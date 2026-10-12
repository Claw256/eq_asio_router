//! In-process COM smoke test for the **v2 bridge driver**: no registry, no
//! admin, no third-party driver. Proves the COM surface, the directProcess
//! gate, the shared-memory shuttle (in-process) and the pacing thread
//! end-to-end: fake host writes outputs → ring A, ring B → fake host inputs.

use asio_abi::*;
use core::ffi::c_void;
use eq_asio_driver::ids::*;
use eq_asio_driver::*;
use eq_asio_driver::convert;

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const N: usize = 256; // preferred buffer size; the test negotiates it

// ------------------------------------------------------------- fake host state

/// Driver-owned buffer bases (half0 pointers), published before `start`.
static OUT_L: AtomicPtr<i32> = AtomicPtr::new(core::ptr::null_mut());
static OUT_R: AtomicPtr<i32> = AtomicPtr::new(core::ptr::null_mut());
static IN_L: AtomicPtr<i32> = AtomicPtr::new(core::ptr::null_mut());
static IN_R: AtomicPtr<i32> = AtomicPtr::new(core::ptr::null_mut());
/// First-callback input capture (allocated by the test).
static CAP_L: AtomicPtr<i32> = AtomicPtr::new(core::ptr::null_mut());
static CAP_R: AtomicPtr<i32> = AtomicPtr::new(core::ptr::null_mut());
static CAPTURED: AtomicBool = AtomicBool::new(false);
/// Host-side callback count + the last `directProcess` we were handed.
static HOST_BEATS: AtomicU64 = AtomicU64::new(0);
static LAST_DIRECT: AtomicI32 = AtomicI32::new(-1);
/// When true, the fake host answers 1 to the deferred-processing query
/// (the plan's `directProcess = FALSE` request) — `createBuffers` must refuse.
static DEFER: AtomicBool = AtomicBool::new(false);

/// Ramp the host "renders" into its outputs: Int32 `0x1000_0000 + i` /
/// `0x2000_0000 + i` per block (0.125 / 0.25 after the f32 shuttle).
unsafe extern "C" fn host_switch(index: i32, direct_process: AsioBool) {
    LAST_DIRECT.store(direct_process, Ordering::Relaxed);
    let b = HOST_BEATS.fetch_add(1, Ordering::Relaxed);
    let off = index as usize * N;
    for (ptr, base) in [(OUT_L.load(Ordering::Relaxed), 0x1000_0000i32), (OUT_R.load(Ordering::Relaxed), 0x2000_0000i32)] {
        if ptr.is_null() {
            continue;
        }
        for i in 0..N {
            *ptr.add(off + i) = base + i as i32;
        }
    }
    if b == 0 {
        CAPTURED.store(true, Ordering::Release);
        for (dst, src) in [(CAP_L.load(Ordering::Relaxed), IN_L.load(Ordering::Relaxed)),
                           (CAP_R.load(Ordering::Relaxed), IN_R.load(Ordering::Relaxed))]
        {
            if dst.is_null() || src.is_null() {
                continue;
            }
            for i in 0..N {
                *dst.add(i) = *src.add(off + i);
            }
        }
    }
}

unsafe extern "C" fn host_switch_time_info(t: *mut AsioTime, index: i32, direct_process: AsioBool) -> *mut AsioTime {
    host_switch(index, direct_process);
    t
}

unsafe extern "C" fn host_rate_changed(_rate: AsioSampleRate) {}

unsafe extern "C" fn host_message(selector: i32, value: i32, _m: *mut c_void, _o: *mut f64) -> i32 {
    match selector {
        K_ASIO_SELECTOR_SUPPORTED => match value {
            K_ASIO_SUPPORTS_TIME_INFO => 1,
            K_ASIO_DEFER_PROCESS => DEFER.load(Ordering::Relaxed) as i32,
            _ => 0,
        },
        K_ASIO_SUPPORTS_TIME_INFO => 1,
        _ => 0,
    }
}

static CALLBACKS: AsioCallbacks = AsioCallbacks {
    buffer_switch: host_switch,
    sample_rate_did_change: host_rate_changed,
    asio_message: host_message,
    buffer_switch_time_info: host_switch_time_info,
};

#[repr(C)]
struct FactoryVtbl {
    qi: unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    create: unsafe extern "system" fn(*mut c_void, *mut c_void, *const Guid, *mut *mut c_void) -> i32,
    lock: unsafe extern "system" fn(*mut c_void, i32) -> i32,
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < Duration::from_secs(10), "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn bridge_driver_com_smoke() {
    unsafe {
        // ---------------------------------------------------------- COM surface
        let mut f: *mut c_void = std::ptr::null_mut();
        assert_eq!(DllGetClassObject(&CLSID_EQ_ASIO, &IID_ICLASSFACTORY, &mut f), 0);
        let fv = *(f as *mut *const FactoryVtbl);

        let mut bogus: *mut c_void = std::ptr::null_mut();
        assert_ne!(DllGetClassObject(&IID_IUNKNOWN, &IID_ICLASSFACTORY, &mut bogus), 0);

        // refuses unknown IIDs, accepts the CLSID as IID (ASIO quirk)
        let mut obj: *mut c_void = std::ptr::null_mut();
        assert_ne!(((*fv).create)(f, std::ptr::null_mut(), &IID_ICLASSFACTORY, &mut obj), 0);
        assert!(obj.is_null());
        assert_eq!(((*fv).create)(f, std::ptr::null_mut(), &CLSID_EQ_ASIO, &mut obj), 0);
        let a = obj as *mut IAsio;
        let v = &*(*a).vtbl;

        let mut name = [0 as std::ffi::c_char; 64];
        (v.get_driver_name)(a, name.as_mut_ptr());
        assert_eq!(cstr_to_string(&name), "EQ ASIO Router");
        assert_eq!((v.get_driver_version)(a), DRIVER_VERSION);
        assert_eq!(DRIVER_VERSION, 2, "v2 bridge");

        // static topology — v2 answers before init (no master to probe)
        let (mut i, mut o) = (0, 0);
        assert_eq!((v.get_channels)(a, &mut i, &mut o), ASE_OK);
        assert_eq!((i, o), (NUM_INPUTS, NUM_OUTPUTS));
        assert_eq!((i, o), (8, 8));

        let (mut mn, mut mx, mut pf, mut gr) = (0, 0, 0, 0);
        assert_eq!((v.get_buffer_size)(a, &mut mn, &mut mx, &mut pf, &mut gr), ASE_OK);
        assert_eq!((mn, mx, pf, gr), (64, 2048, 256, 64));

        assert_eq!((v.can_sample_rate)(a, 48_000.0), ASE_OK);
        assert_eq!((v.can_sample_rate)(a, 192_000.0), ASE_OK);
        assert_eq!((v.can_sample_rate)(a, 12_345.0), ASE_NO_CLOCK);
        let mut rate = 0.0;
        assert_eq!((v.get_sample_rate)(a, &mut rate), ASE_OK);
        assert_eq!(rate, 48_000.0);
        let (mut li, mut lo) = (0, 0);
        assert_eq!((v.get_latencies)(a, &mut li, &mut lo), ASE_OK);
        assert_eq!((li, lo), (256, 256));

        assert_eq!((v.future)(a, K_ASIO_CAN_TIME_INFO, std::ptr::null_mut()), ASE_SUCCESS);
        assert_eq!((v.future)(a, K_ASIO_TRANSPORT, std::ptr::null_mut()), ASE_NOT_PRESENT);
        assert_eq!((v.output_ready)(a), ASE_OK);
        assert_eq!((v.control_panel)(a), ASE_OK);
        assert_eq!((v.dispose_buffers)(a), ASE_INVALID_MODE);
        let (mut p, mut t) = (AsioInt64::default(), AsioInt64::default());
        assert_eq!((v.get_sample_position)(a, &mut p, &mut t), ASE_SP_NOT_ADVANCING);

        // clock sources: internal only
        let mut cs: AsioClockSource = std::mem::zeroed();
        let mut n = 1;
        assert_eq!((v.get_clock_sources)(a, &mut cs, &mut n), ASE_OK);
        assert_eq!(n, 1);
        assert_eq!((v.set_clock_source)(a, 0), ASE_OK);
        assert_ne!((v.set_clock_source)(a, 3), ASE_OK);

        // channel info: Int32LSB everywhere, names, range checks
        let mut ci: AsioChannelInfo = std::mem::zeroed();
        ci.channel = 0;
        ci.is_input = ASIO_FALSE;
        assert_eq!((v.get_channel_info)(a, &mut ci), ASE_OK);
        assert_eq!(cstr_to_string(&{ ci.name }), "EQ Out 1");
        assert_eq!({ ci.sample_type }, ASIOST_INT32_LSB);
        assert_eq!({ ci.is_active }, ASIO_FALSE);
        ci.channel = 7;
        ci.is_input = ASIO_TRUE;
        assert_eq!((v.get_channel_info)(a, &mut ci), ASE_OK);
        assert_eq!(cstr_to_string(&{ ci.name }), "EQ In 8");
        ci.channel = 8;
        assert_ne!((v.get_channel_info)(a, &mut ci), ASE_OK);

        // ---------------------------------------------------------- init + shm
        assert_eq!((v.init)(a, std::ptr::null_mut()), ASIO_TRUE);
        assert_eq!((v.init)(a, std::ptr::null_mut()), ASIO_TRUE); // idempotent
        let mapping = bridge_abi::open().expect("init must create the bridge mapping");
        let hdr = mapping.header();
        assert_eq!(hdr.validate(), Ok(()));
        assert_eq!(hdr.get_sample_rate(), 48_000.0);
        assert_eq!(hdr.get_buffer_frames(), 256);

        // ------------------------------------------- directProcess gate (M0)
        DEFER.store(true, Ordering::Relaxed);
        let mut probe: [AsioBufferInfo; 1] =
            [AsioBufferInfo { is_input: 0, channel_num: 0, buffers: [std::ptr::null_mut(); 2] }];
        assert_eq!(
            (v.create_buffers)(a, probe.as_mut_ptr(), 1, N as i32, &CALLBACKS),
            ASE_INVALID_MODE,
            "host asking for directProcess = FALSE must be rejected"
        );
        DEFER.store(false, Ordering::Relaxed);
        assert_eq!(hdr.driver_running.load(Ordering::Acquire), 0, "not started yet");

        // ---------------------------------------------------------- create
        let mut infos: [AsioBufferInfo; 5] = [
            AsioBufferInfo { is_input: 0, channel_num: 0, buffers: [std::ptr::null_mut(); 2] },
            AsioBufferInfo { is_input: 0, channel_num: 1, buffers: [std::ptr::null_mut(); 2] },
            AsioBufferInfo { is_input: 1, channel_num: 0, buffers: [std::ptr::null_mut(); 2] },
            AsioBufferInfo { is_input: 1, channel_num: 1, buffers: [std::ptr::null_mut(); 2] },
            AsioBufferInfo { is_input: 1, channel_num: 2, buffers: [std::ptr::null_mut(); 2] },
        ];
        // out of range → ASE_INVALID_PARAMETER
        let mut bad = AsioBufferInfo { is_input: 0, channel_num: 9, buffers: [std::ptr::null_mut(); 2] };
        assert_eq!((v.create_buffers)(a, &mut bad, 1, N as i32, &CALLBACKS), ASE_INVALID_PARAMETER);
        // off-granularity size → ASE_INVALID_MODE
        assert_eq!((v.create_buffers)(a, infos.as_mut_ptr(), 5, 100, &CALLBACKS), ASE_INVALID_MODE);

        assert_eq!((v.create_buffers)(a, infos.as_mut_ptr(), 5, N as i32, &CALLBACKS), ASE_OK);
        assert_eq!((v.create_buffers)(a, infos.as_mut_ptr(), 5, N as i32, &CALLBACKS), ASE_INVALID_MODE, "twice");

        // publish the driver's buffers to the fake host
        OUT_L.store(infos[0].buffers[0] as *mut i32, Ordering::Release);
        OUT_R.store(infos[1].buffers[0] as *mut i32, Ordering::Release);
        IN_L.store(infos[2].buffers[0] as *mut i32, Ordering::Release);
        IN_R.store(infos[3].buffers[0] as *mut i32, Ordering::Release);
        let cap_l: Box<[i32]> = vec![0i32; N].into_boxed_slice();
        let cap_r: Box<[i32]> = vec![0i32; N].into_boxed_slice();
        CAP_L.store(Box::into_raw(cap_l) as *mut i32, Ordering::Release);
        CAP_R.store(Box::into_raw(cap_r) as *mut i32, Ordering::Release);
        // channel 2 inputs must come back silent: prefill with junk, the driver
        // zeroes them every tick.
        for half in 0..2 {
            let p = infos[4].buffers[half] as *mut i32;
            for i in 0..N {
                *p.add(i) = 0x7777_7777;
            }
        }
        // inputs are active now
        let mut ci: AsioChannelInfo = std::mem::zeroed();
        ci.channel = 0;
        ci.is_input = ASIO_TRUE;
        assert_eq!((v.get_channel_info)(a, &mut ci), ASE_OK);
        assert_eq!({ ci.is_active }, ASIO_TRUE);

        assert_eq!((v.get_latencies)(a, &mut li, &mut lo), ASE_OK);
        assert_eq!((li, lo), (N as i32, N as i32));
        assert_eq!(hdr.get_buffer_frames(), N as u32);

        // ------------------------------------------------ prefill ring B (loopback)
        let want_l: Vec<f32> = (0..N).map(|i| 0.001 * i as f32 + 0.25).collect();
        let want_r: Vec<f32> = (0..N).map(|i| -(0.002 * i as f32) - 0.5).collect();
        assert_eq!(hdr.ring_b_push(&want_l, &want_r), N);
        assert_eq!(hdr.fill_b(), N as u64);

        // ---------------------------------------------------------- run
        assert_eq!((v.start)(a), ASE_OK);
        assert_eq!((v.start)(a), ASE_OK, "double start is a no-op");
        wait_until("4 driver beats", || {
            hdr.driver_running.load(Ordering::Acquire) == 1
                && hdr.driver_beat.load(Ordering::Acquire) >= 4
        });

        // sample position advances while running
        assert_eq!((v.get_sample_position)(a, &mut p, &mut t), ASE_OK);
        assert!(p.to_u64() > 0, "sample position must advance");
        assert!(t.to_u64() > 0, "timestamp must be monotonic");

        assert_eq!((v.stop)(a), ASE_OK);
        assert_eq!((v.stop)(a), ASE_OK, "double stop is a no-op");
        assert_eq!(hdr.driver_running.load(Ordering::Acquire), 0);
        assert_eq!((v.get_sample_position)(a, &mut p, &mut t), ASE_SP_NOT_ADVANCING);

        let beats_after_first_run = hdr.driver_beat.load(Ordering::Acquire);
        assert!(beats_after_first_run >= 4, "pacing thread never ticked");
        assert!(HOST_BEATS.load(Ordering::Relaxed) >= 4, "host callback never fired");
        assert_eq!(LAST_DIRECT.load(Ordering::Relaxed), ASIO_TRUE, "directProcess must be TRUE");

        // restart cycle: stop() joined, start() must work again
        assert_eq!((v.start)(a), ASE_OK);
        wait_until("beat after restart", || {
            hdr.driver_beat.load(Ordering::Acquire) > beats_after_first_run
        });
        assert_eq!((v.stop)(a), ASE_OK);

        // ---------------------------------------------------------- bridge contents
        // ring A: the fake host's ramp (converted Int32 -> f32). The very first
        // pushed block is the untouched half1 (silence), then every block.
        let fill = hdr.fill_a();
        assert!(fill >= 4 * N as u64, "ring A should hold the shuttled blocks, fill = {fill}");
        let mut ol = vec![0.0f32; fill as usize];
        let mut orr = vec![0.0f32; fill as usize];
        assert_eq!(hdr.ring_a_pop(&mut ol, &mut orr), fill as usize);
        assert_eq!(hdr.fill_a(), 0);
        let ramp_l = (0x1000_0000i32 as f32) * (1.0 / 2147483648.0); // 0.125
        let ramp_r = (0x2000_0000i32 as f32) * (1.0 / 2147483648.0); // 0.25
        let ramp_frames = ol
            .iter()
            .zip(&orr)
            .filter(|&(a, b)| (a - ramp_l).abs() < 1e-6 && (b - ramp_r).abs() < 1e-6)
            .count();
        assert!(
            ramp_frames >= 3 * N,
            "expected at least 3 full ramp blocks in ring A, got {ramp_frames} frames"
        );

        // counters: no A overruns (we are well under capacity), B underruns
        // after the prefill was consumed by the first tick.
        assert_eq!(hdr.a_overruns(), 0);
        assert_eq!(hdr.b_dropped(), 0);
        assert!(hdr.b_underruns() >= N as u64, "ring B was drained after the prefill");
        assert_eq!(hdr.fill_b(), 0);

        // ring B -> inputs: the first callback must have seen the prefill
        assert!(CAPTURED.load(Ordering::Acquire), "first callback never ran");
        let cap_l = core::slice::from_raw_parts(CAP_L.load(Ordering::Relaxed), N);
        let cap_r = core::slice::from_raw_parts(CAP_R.load(Ordering::Relaxed), N);
        for i in 0..N {
            assert_eq!(cap_l[i], convert::f32_to_i32(want_l[i]), "input L[{i}] roundtrip");
            assert_eq!(cap_r[i], convert::f32_to_i32(want_r[i]), "input R[{i}] roundtrip");
        }

        // inputs 2..8 are silence even though the host prefilled them with junk
        for half in 0..2 {
            let p = infos[4].buffers[half] as *const i32;
            for i in 0..N {
                assert_eq!(*p.add(i), 0, "input ch2 half{half}[{i}] must be silence");
            }
        }

        // ---------------------------------------------------------- teardown
        assert_eq!((v.dispose_buffers)(a), ASE_OK);
        assert_eq!((v.dispose_buffers)(a), ASE_INVALID_MODE);
        assert_eq!((v.get_sample_position)(a, &mut p, &mut t), ASE_SP_NOT_ADVANCING);
        // the mapping survives dispose; attach stays live across re-create
        assert_eq!(hdr.validate(), Ok(()));

        // QI with unknown IID fails, with IUnknown works
        let mut q: *mut c_void = std::ptr::null_mut();
        assert_ne!((v.query_interface)(a, &IID_ICLASSFACTORY, &mut q), 0);
        assert_eq!((v.query_interface)(a, &IID_IUNKNOWN, &mut q), 0);
        assert_eq!((v.release)(a), 1);
        assert_eq!((v.release)(a), 0);

        // leaked capture boxes (pointers published to the fake host)
        drop(Box::from_raw(core::ptr::slice_from_raw_parts_mut(CAP_L.load(Ordering::Relaxed), N)));
        drop(Box::from_raw(core::ptr::slice_from_raw_parts_mut(CAP_R.load(Ordering::Relaxed), N)));

        ((*fv).release)(f);
        assert_eq!(DllCanUnloadNow(), 0);
    }
}
