//! Real-time core: runs inside the master driver's buffer-switch callback, plus
//! the reset-forwarding thread. Nothing here allocates, locks or logs on the
//! audio path (`Core::on_buffer_switch`).

use core::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use asio_abi::*;
use asio_host::{AsioDriver, Handler};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

use crate::convert;
use crate::processor::{peak, Settings};
use crate::status::StatusWriter;

/// Stereo DSP stage running inside the audio callback.
pub trait Processor: Send {
    /// Block start: pick up pending configuration (wait-free, allocation-free).
    fn update(&mut self) {}
    /// Current realtime settings (bypass, output mapping, ...).
    fn settings(&self) -> Settings {
        Settings::default()
    }
    fn process(&mut self, l: &mut [f32], r: &mut [f32]);
}

/// Converts a QPC reading to nanoseconds without 128-bit math.
pub fn qpc_to_ns(counts: i64, freq: i64) -> u64 {
    if freq <= 0 || counts < 0 {
        return 0;
    }
    let (c, f) = (counts as u64, freq as u64);
    (c / f) * 1_000_000_000 + (c % f) * 1_000_000_000 / f
}

// ---------------------------------------------------------------- reset thread

#[derive(Clone, Copy)]
struct HostCallbacks(AsioCallbacks);
// Plain function pointers supplied by the host.
unsafe impl Send for HostCallbacks {}

pub enum ResetMsg {
    Reset,
    Rate(f64),
    Quit,
}

pub struct ResetShared {
    host: Mutex<Option<HostCallbacks>>,
    in_callback: AtomicBool,
}

pub struct ResetThread {
    tx: Sender<ResetMsg>,
    shared: Arc<ResetShared>,
    handle: Option<JoinHandle<()>>,
}

fn reset_loop(rx: Receiver<ResetMsg>, shared: Arc<ResetShared>) {
    while let Ok(msg) = rx.recv() {
        if matches!(msg, ResetMsg::Quit) {
            break;
        }
        let cb = shared.host.lock().ok().and_then(|g| *g);
        let Some(cb) = cb else { continue };
        shared.in_callback.store(true, Ordering::Release);
        // SAFETY: host-provided callbacks, valid while buffers exist; we copy
        // the pointers out so the lock is not held across the call.
        unsafe {
            match msg {
                ResetMsg::Reset => {
                    let f = cb.0.asio_message;
                    f(K_ASIO_RESET_REQUEST, 0, core::ptr::null_mut(), core::ptr::null_mut());
                }
                ResetMsg::Rate(r) => {
                    let f = cb.0.sample_rate_did_change;
                    f(r);
                }
                ResetMsg::Quit => {}
            }
        }
        shared.in_callback.store(false, Ordering::Release);
    }
}

impl ResetThread {
    pub fn spawn() -> Option<Self> {
        let (tx, rx) = channel();
        let shared = Arc::new(ResetShared { host: Mutex::new(None), in_callback: AtomicBool::new(false) });
        let s2 = shared.clone();
        let handle = std::thread::Builder::new().name("eq-asio-reset".into()).spawn(move || reset_loop(rx, s2)).ok()?;
        Some(Self { tx, shared, handle: Some(handle) })
    }
    pub fn sender(&self) -> Sender<ResetMsg> {
        self.tx.clone()
    }
    pub fn post(&self, m: ResetMsg) {
        let _ = self.tx.send(m);
    }
    pub fn set_host(&self, cb: Option<AsioCallbacks>) {
        if let Ok(mut g) = self.shared.host.lock() {
            *g = cb.map(HostCallbacks);
        }
    }
    /// Stops the thread. If it is currently inside a host callback (which may
    /// itself be waiting on us) it is detached instead of joined.
    pub fn shutdown(&mut self) {
        self.set_host(None);
        let _ = self.tx.send(ResetMsg::Quit);
        if let Some(h) = self.handle.take() {
            if !self.shared.in_callback.load(Ordering::Acquire) {
                let _ = h.join();
            }
        }
    }
}

// ---------------------------------------------------------------- audio core

/// Master input -> our input buffer copy description.
pub struct InChan {
    pub src: [*const u8; 2],
    pub ty: AsioSampleType,
    /// Our Int32 LSB buffer, `2 * frames` samples (both halves).
    pub dst: *mut i32,
}

pub struct OutChan {
    pub ptrs: [*mut u8; 2],
    pub ty: AsioSampleType,
}

/// Cheap xrun heuristic: the gap between two callback entries exceeds 1.5x the
/// nominal buffer period (measured with QPC). `prev_ns == 0` means "no previous
/// callback" (first block after start). Drivers that report `kAsioOverload`
/// are counted separately via [`Handler::overload`].
pub fn is_xrun(prev_ns: u64, now_ns: u64, period_ns: u64) -> bool {
    prev_ns != 0 && now_ns.saturating_sub(prev_ns) > period_ns + period_ns / 2
}

/// Nominal buffer period in nanoseconds.
pub fn period_ns(frames: usize, rate: f64) -> u64 {
    if rate > 0.0 {
        (frames as f64 / rate * 1e9) as u64
    } else {
        0
    }
}

pub struct Core {
    pub frames: usize,
    pub host: AsioCallbacks,
    pub use_time_info: bool,
    pub active: AtomicBool,
    pub sample_pos: AtomicU64,
    pub sys_time_ns: AtomicU64,
    pub rate_bits: AtomicU64,
    pub qpc_freq: i64,
    pub ins: Vec<InChan>,
    /// Our output channels 0/1 (Int32 LSB, `2 * frames` samples), null if the host did not request them.
    pub out_l: *const i32,
    pub out_r: *const i32,
    /// All master output channels (so the output mapping can change at runtime).
    pub master_out: Vec<OutChan>,
    pub processor: Box<dyn Processor>,
    /// Preallocated f32 scratch, `frames` each.
    pub l: Vec<f32>,
    pub r: Vec<f32>,
    pub master: *const AsioDriver,
    pub output_ready: bool,
    pub reset: Sender<ResetMsg>,
    pub status: Option<StatusWriter>,
    pub xruns: AtomicU64,
    last_cb_ns: AtomicU64,
}

impl Core {
    pub fn new(
        frames: usize,
        host: AsioCallbacks,
        rate: f64,
        reset: Sender<ResetMsg>,
        processor: Box<dyn Processor>,
        status: Option<StatusWriter>,
    ) -> Box<Core> {
        let mut f = 0i64;
        // SAFETY: valid out pointer.
        unsafe {
            let _ = QueryPerformanceFrequency(&mut f);
        }
        Box::new(Core {
            frames,
            host,
            use_time_info: false,
            active: AtomicBool::new(false),
            sample_pos: AtomicU64::new(0),
            sys_time_ns: AtomicU64::new(0),
            rate_bits: AtomicU64::new(rate.to_bits()),
            qpc_freq: f,
            ins: Vec::new(),
            out_l: core::ptr::null(),
            out_r: core::ptr::null(),
            master_out: Vec::new(),
            processor,
            l: vec![0.0; frames],
            r: vec![0.0; frames],
            master: core::ptr::null(),
            output_ready: true,
            reset,
            status,
            xruns: AtomicU64::new(0),
            last_cb_ns: AtomicU64::new(0),
        })
    }

    /// Marks streaming on/off (also published in the status block).
    pub fn set_active(&self, on: bool) {
        self.last_cb_ns.store(0, Ordering::Relaxed);
        self.active.store(on, Ordering::Release);
        if let Some(s) = &self.status {
            s.set_running(on);
        }
    }

    fn now_ns(&self) -> u64 {
        let mut c = 0i64;
        // SAFETY: valid out pointer.
        unsafe {
            let _ = QueryPerformanceCounter(&mut c);
        }
        qpc_to_ns(c, self.qpc_freq)
    }

    /// Called from the master's buffer-switch callback. Allocation-free.
    ///
    /// # Safety
    /// All stored pointers must refer to live buffers of `frames` samples per half.
    pub unsafe fn on_buffer_switch(&mut self, index: i32) {
        if !self.active.load(Ordering::Acquire) || !(index == 0 || index == 1) {
            return;
        }
        let idx = index as usize;
        let n = self.frames;

        // 0. pick up hot-reloaded settings, xrun heuristic
        self.processor.update();
        let st = self.processor.settings();
        let t = self.now_ns();
        let rate = f64::from_bits(self.rate_bits.load(Ordering::Relaxed));
        if is_xrun(self.last_cb_ns.load(Ordering::Relaxed), t, period_ns(n, rate)) {
            self.xruns.fetch_add(1, Ordering::Relaxed);
        }
        self.last_cb_ns.store(t, Ordering::Relaxed);

        // 1. fresh inputs for the host (format-converted into Int32 LSB), or silence
        for c in &self.ins {
            let dst = core::slice::from_raw_parts_mut(c.dst.add(idx * n), n);
            if st.passthrough_inputs {
                convert::read_i32(c.src[idx], c.ty, dst);
            } else {
                dst.fill(0);
            }
        }

        // 2. let the host fill our output half `index`
        let pos = self.sample_pos.load(Ordering::Relaxed);
        self.sys_time_ns.store(t, Ordering::Release);
        if self.use_time_info {
            let mut ti = AsioTime::zeroed();
            ti.time_info.speed = 1.0;
            ti.time_info.system_time = AsioInt64::from_u64(t);
            ti.time_info.sample_position = AsioInt64::from_u64(pos);
            ti.time_info.sample_rate = rate;
            ti.time_info.flags = K_SYSTEM_TIME_VALID | K_SAMPLE_POSITION_VALID | K_SAMPLE_RATE_VALID;
            let f = self.host.buffer_switch_time_info;
            f(&mut ti, index, ASIO_TRUE);
        } else {
            let f = self.host.buffer_switch;
            f(index, ASIO_TRUE);
        }

        // 3. our out 0/1 -> f32 -> (EQ unless bypassed) -> master outputs
        for (src, dst) in [(self.out_l, &mut self.l), (self.out_r, &mut self.r)] {
            if src.is_null() {
                dst.fill(0.0);
            } else {
                let s = core::slice::from_raw_parts(src.add(idx * n), n);
                convert::i32_slice_to_f32(s, dst);
            }
        }
        let peak_in = [peak(&self.l), peak(&self.r)];
        self.processor.process(&mut self.l, &mut self.r);
        let peak_out = [peak(&self.l), peak(&self.r)];
        for (ch, o) in self.master_out.iter().enumerate() {
            let p = o.ptrs[idx];
            if p.is_null() {
                continue;
            }
            if ch == st.outputs[0] {
                convert::write_f32(&self.l, p, o.ty);
            } else if ch == st.outputs[1] {
                convert::write_f32(&self.r, p, o.ty);
            } else {
                // Unmapped master outputs stay silent (all-zero bytes is silence for every PCM/float type).
                core::ptr::write_bytes(p, 0, n * bytes_per_sample(o.ty));
            }
        }
        if self.output_ready && !self.master.is_null() && !(*self.master).output_ready() {
            self.output_ready = false;
        }
        self.sample_pos.store(pos + n as u64, Ordering::Release);
        if let Some(s) = &self.status {
            s.on_block(peak_in, peak_out);
            s.set_xruns(self.xruns.load(Ordering::Relaxed));
        }
    }
}

/// Handler installed into the hosted master driver.
pub struct MasterHandler {
    pub core: *mut Core,
}
// The core is only mutated from the master's audio thread while active.
unsafe impl Send for MasterHandler {}

impl Handler for MasterHandler {
    fn buffer_switch(&mut self, index: i32) {
        // SAFETY: `core` outlives the master's buffers (disposed first).
        unsafe { (*self.core).on_buffer_switch(index) }
    }
    fn reset_request(&mut self) {
        // SAFETY: as above; only the Sender is touched.
        let _ = unsafe { &(*self.core).reset }.send(ResetMsg::Reset);
    }
    fn sample_rate_changed(&mut self, rate: f64) {
        // SAFETY: as above.
        unsafe {
            (*self.core).rate_bits.store(rate.to_bits(), Ordering::Relaxed);
            let _ = (*self.core).reset.send(ResetMsg::Rate(rate));
        }
    }
    fn overload(&mut self) {
        // SAFETY: as above; atomics only.
        unsafe { (*self.core).xruns.fetch_add(1, Ordering::Relaxed) };
    }
}

#[allow(dead_code)]
fn _assert_types(_: *mut c_void) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::processor::{EqProcessor, Handoff};
    use eq_core::{Config, StatusBlock};
    use std::sync::atomic::AtomicPtr;
    use std::sync::Arc;

    #[test]
    fn qpc_conversion() {
        assert_eq!(qpc_to_ns(10_000_000, 10_000_000), 1_000_000_000);
        assert_eq!(qpc_to_ns(15_000_000, 10_000_000), 1_500_000_000);
        assert_eq!(qpc_to_ns(1, 10_000_000), 100);
        assert_eq!(qpc_to_ns(5, 0), 0);
        // large uptime: 30 days at 10 MHz
        let c = 30i64 * 86_400 * 10_000_000;
        assert_eq!(qpc_to_ns(c, 10_000_000), 30u64 * 86_400 * 1_000_000_000);
    }

    #[test]
    fn xrun_heuristic() {
        let p = period_ns(128, 48000.0); // 2.666 ms
        assert_eq!(p, 2_666_666);
        assert!(!is_xrun(0, 10_000_000_000, p)); // first block
        assert!(!is_xrun(1_000, 1_000 + p, p));
        assert!(!is_xrun(1_000, 1_000 + p * 3 / 2, p)); // exactly 1.5x is fine
        assert!(is_xrun(1_000, 1_000 + p * 3 / 2 + 1, p));
        assert!(!is_xrun(5_000, 1_000, p)); // clock went backwards: no panic
    }

    static OUT_L: AtomicPtr<i32> = AtomicPtr::new(core::ptr::null_mut());
    static OUT_R: AtomicPtr<i32> = AtomicPtr::new(core::ptr::null_mut());
    const N: usize = 16;

    unsafe extern "C" fn fake_switch(index: i32, _d: AsioBool) {
        let off = index as usize * N;
        for i in 0..N {
            *OUT_L.load(Ordering::Relaxed).add(off + i) = 0x4000_0000; // 0.5
            *OUT_R.load(Ordering::Relaxed).add(off + i) = 0x2000_0000; // 0.25
        }
    }
    unsafe extern "C" fn fake_rate(_r: f64) {}
    unsafe extern "C" fn fake_msg(_s: i32, _v: i32, _m: *mut c_void, _o: *mut f64) -> i32 {
        0
    }
    unsafe extern "C" fn fake_time(t: *mut AsioTime, i: i32, d: AsioBool) -> *mut AsioTime {
        fake_switch(i, d);
        t
    }

    #[test]
    fn core_block_maps_outputs_and_publishes_status() {
        let host = AsioCallbacks {
            buffer_switch: fake_switch,
            sample_rate_did_change: fake_rate,
            asio_message: fake_msg,
            buffer_switch_time_info: fake_time,
        };
        let mut out_l = vec![0i32; 2 * N];
        let mut out_r = vec![0i32; 2 * N];
        OUT_L.store(out_l.as_mut_ptr(), Ordering::Relaxed);
        OUT_R.store(out_r.as_mut_ptr(), Ordering::Relaxed);
        let mut master: Vec<Vec<i32>> = (0..4).map(|_| vec![0x7777i32; 2 * N]).collect();
        let master_in_src = [0x1234_0000i32; 2 * N];
        let mut our_in = vec![0x55i32; 2 * N];

        // Output pair [2, 1] (swapped order, ch 0 and 3 must be silent), limiter off.
        let cfg = Config { output_channels: [2, 1], limiter: false, passthrough_inputs: false, ..Config::default() };
        let proc = EqProcessor::new(48000.0, &cfg, 4, Arc::new(Handoff::new()));
        let mut blk = Box::new(StatusBlock { magic: 0, ..StatusBlock::new() });
        let status = unsafe { StatusWriter::from_raw(&mut *blk) };
        status.init(48000, N as u32, 0, 0, 1);
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut core = Core::new(N, host, 48000.0, tx, Box::new(proc), Some(status));
        core.out_l = out_l.as_ptr();
        core.out_r = out_r.as_ptr();
        for m in master.iter_mut() {
            let p = m.as_mut_ptr();
            core.master_out.push(OutChan { ptrs: [p as *mut u8, unsafe { p.add(N) } as *mut u8], ty: ASIOST_INT32_LSB });
        }
        core.ins.push(InChan {
            src: [master_in_src.as_ptr() as *const u8, master_in_src.as_ptr() as *const u8],
            ty: ASIOST_INT32_LSB,
            dst: our_in.as_mut_ptr(),
        });
        core.set_active(true);
        unsafe {
            core.on_buffer_switch(1);
            core.on_buffer_switch(1);
        }
        // half 1 only
        assert!(master[2][N..].iter().all(|&s| (s - 0x4000_0000).abs() < 64), "L on ch2");
        assert!(master[1][N..].iter().all(|&s| (s - 0x2000_0000).abs() < 64), "R on ch1");
        assert!(master[0][N..].iter().all(|&s| s == 0) && master[3][N..].iter().all(|&s| s == 0));
        assert!(master[0][..N].iter().all(|&s| s == 0x7777), "other half untouched");
        // passthrough off: our inputs silent
        assert!(our_in[N..].iter().all(|&s| s == 0));
        assert!(our_in[..N].iter().all(|&s| s == 0x55));
        assert_eq!(core.sample_pos.load(Ordering::Relaxed), 2 * N as u64);
        core.status = None; // drops the writer (clears running) before reading the block
        assert_eq!(blk.callbacks, 2);
        assert!((blk.peak_in_l - 0.5).abs() < 1e-6 && (blk.peak_out_r - 0.25).abs() < 1e-3);
        assert_eq!(blk.running, 0);
    }
}
