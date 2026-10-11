//! The `IASIO` COM object ("EQ ASIO Router").

use core::cell::UnsafeCell;
use core::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

use asio_abi::*;
use asio_host::{AsioDriver, ChannelInfo};
use windows::Win32::Foundation::HWND;

use crate::com::{guard, E_NOINTERFACE, E_POINTER, E_UNEXPECTED, OBJECT_COUNT, S_OK};
use crate::engine::{Core, InChan, MasterHandler, OutChan, ResetMsg, ResetThread};
use crate::processor::{EqProcessor, Handoff, Snapshot};
use crate::status::StatusWriter;
use crate::watcher::ConfigWatcher;
use crate::ids::*;
use crate::{config, convert, dlog};
use std::sync::Arc;

/// Defines an `IASIO` method with the right ABI (`thiscall` on x86, `system`
/// elsewhere) whose body cannot unwind into the host.
macro_rules! asio_method {
    (fn $name:ident($($arg:ident : $t:ty),*) -> $ret:ty = $fail:expr, $body:block) => {
        #[cfg(target_arch = "x86")]
        unsafe extern "thiscall" fn $name($($arg: $t),*) -> $ret {
            guard($fail, || $body)
        }
        #[cfg(not(target_arch = "x86"))]
        unsafe extern "system" fn $name($($arg: $t),*) -> $ret {
            guard($fail, || $body)
        }
    };
}

struct Inner {
    master: Option<Box<AsioDriver>>,
    error: String,
    num_in: i32,
    buffer_size: i32,
    in_info: Vec<ChannelInfo>,
    num_out: i32,
    out_types: Vec<AsioSampleType>,
    in_latency: i32,
    out_latency: i32,
    active_in: Vec<bool>,
    active_out: Vec<bool>,
    core: *mut Core,
    our_bufs: Vec<Box<[i32]>>,
    reset: Option<ResetThread>,
    watcher: Option<ConfigWatcher>,
    buffers_created: bool,
    started: bool,
}

#[repr(C)]
pub struct Driver {
    vtbl: *const IAsioVtbl,
    refs: AtomicU32,
    /// Mirror of `inner.core` for `getSamplePosition` (atomics only).
    core_ptr: AtomicPtr<Core>,
    inner: UnsafeCell<Inner>,
}

static VTBL: IAsioVtbl = IAsioVtbl {
    query_interface: m_query_interface,
    add_ref: m_add_ref,
    release: m_release,
    init: m_init,
    get_driver_name: m_get_driver_name,
    get_driver_version: m_get_driver_version,
    get_error_message: m_get_error_message,
    start: m_start,
    stop: m_stop,
    get_channels: m_get_channels,
    get_latencies: m_get_latencies,
    get_buffer_size: m_get_buffer_size,
    can_sample_rate: m_can_sample_rate,
    get_sample_rate: m_get_sample_rate,
    set_sample_rate: m_set_sample_rate,
    get_clock_sources: m_get_clock_sources,
    set_clock_source: m_set_clock_source,
    get_sample_position: m_get_sample_position,
    get_channel_info: m_get_channel_info,
    create_buffers: m_create_buffers,
    dispose_buffers: m_dispose_buffers,
    control_panel: m_control_panel,
    future: m_future,
    output_ready: m_output_ready,
};

pub fn create_object() -> *mut IAsio {
    let d = Box::new(Driver {
        vtbl: &VTBL,
        refs: AtomicU32::new(1),
        core_ptr: AtomicPtr::new(core::ptr::null_mut()),
        inner: UnsafeCell::new(Inner {
            master: None,
            error: String::new(),
            num_in: 0,
            buffer_size: 0,
            in_info: Vec::new(),
            num_out: 0,
            out_types: Vec::new(),
            in_latency: 0,
            out_latency: 0,
            active_in: Vec::new(),
            active_out: vec![false; NUM_OUTPUTS as usize],
            core: core::ptr::null_mut(),
            our_bufs: Vec::new(),
            reset: None,
            watcher: None,
            buffers_created: false,
            started: false,
        }),
    });
    OBJECT_COUNT.fetch_add(1, Ordering::AcqRel);
    Box::into_raw(d) as *mut IAsio
}

/// # Safety
/// `this` must be a live driver object; pointers per COM contract.
pub unsafe fn query_interface(this: *mut IAsio, riid: *const Guid, ppv: *mut *mut c_void) -> i32 {
    m_query_interface(this, riid, ppv)
}

/// # Safety
/// `this` must be a live driver object.
pub unsafe fn release(this: *mut IAsio) -> u32 {
    m_release(this)
}

unsafe fn drv<'a>(this: *mut IAsio) -> &'a Driver {
    &*(this as *const Driver)
}

#[allow(clippy::mut_from_ref)]
unsafe fn me<'a>(this: *mut IAsio) -> &'a mut Inner {
    &mut *drv(this).inner.get()
}

// ---------------------------------------------------------------- IUnknown

unsafe extern "system" fn m_query_interface(this: *mut IAsio, riid: *const Guid, ppv: *mut *mut c_void) -> i32 {
    guard(E_UNEXPECTED, || {
        if ppv.is_null() || riid.is_null() {
            return E_POINTER;
        }
        let iid = *riid;
        // ASIO quirk: hosts ask for IASIO using the driver's own CLSID as IID.
        if iid == IID_IUNKNOWN || iid == CLSID_EQ_ASIO {
            m_add_ref(this);
            *ppv = this as *mut c_void;
            S_OK
        } else {
            *ppv = core::ptr::null_mut();
            E_NOINTERFACE
        }
    })
}

unsafe extern "system" fn m_add_ref(this: *mut IAsio) -> u32 {
    drv(this).refs.fetch_add(1, Ordering::AcqRel) + 1
}

unsafe extern "system" fn m_release(this: *mut IAsio) -> u32 {
    let n = drv(this).refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if n == 0 {
        guard((), || {
            dlog!("driver object released, tearing down");
            // Drop runs teardown: stop master, dispose master buffers, drop
            // master driver, stop reset thread -- all before the object goes away.
            drop(Box::from_raw(this as *mut Driver));
        });
        OBJECT_COUNT.fetch_sub(1, Ordering::AcqRel);
    }
    n
}

impl Drop for Driver {
    fn drop(&mut self) {
        let inner = self.inner.get_mut();
        inner.dispose();
        inner.master = None;
        if let Some(mut r) = inner.reset.take() {
            r.shutdown();
        }
    }
}

// ---------------------------------------------------------------- helpers

fn copy_cstr(dst: *mut c_char, s: &str, cap: usize) {
    if dst.is_null() || cap == 0 {
        return;
    }
    let b = s.as_bytes();
    let n = b.len().min(cap - 1);
    // SAFETY: caller guarantees `cap` writable bytes.
    unsafe {
        for (i, &c) in b[..n].iter().enumerate() {
            *dst.add(i) = c as c_char;
        }
        *dst.add(n) = 0;
    }
}

fn name_array(s: &str) -> [c_char; 32] {
    let mut a = [0 as c_char; 32];
    for (i, &c) in s.as_bytes().iter().take(31).enumerate() {
        a[i] = c as c_char;
    }
    a
}

impl Inner {
    fn fail(&mut self, msg: impl Into<String>) -> AsioBool {
        self.error = msg.into();
        dlog!("init failed: {}", self.error);
        ASIO_FALSE
    }

    fn init(&mut self, sys_handle: *mut c_void) -> AsioBool {
        if self.master.is_some() {
            return ASIO_TRUE;
        }
        asio_host::init_com_sta();
        let name = config::effective_master(&config::load());
        dlog!("init: master driver = {name:?}");
        if config::is_self(&name) {
            return self.fail("master_driver must not be EQ ASIO Router itself");
        }
        let info = match asio_host::find_driver(&name) {
            Ok(i) => i,
            Err(e) => return self.fail(format!("master driver lookup: {e}")),
        };
        let m = match AsioDriver::load_diagnosed(&info, HWND(sys_handle)) {
            Ok(m) => Box::new(m),
            Err(e) => return self.fail(format!("master driver load: {e}")),
        };
        let (n_in, n_out) = match m.channels() {
            Ok(c) => c,
            Err(e) => return self.fail(format!("master getChannels: {e}")),
        };
        if n_out < 2 {
            return self.fail("master driver has fewer than 2 outputs");
        }
        let bs = match m.buffer_sizes() {
            Ok(b) => b,
            Err(e) => return self.fail(format!("master getBufferSize: {e}")),
        };
        let mut in_info = Vec::new();
        for i in 0..n_in {
            match m.channel_info(i, true) {
                Ok(c) => in_info.push(c),
                Err(e) => return self.fail(format!("master getChannelInfo(in {i}): {e}")),
            }
        }
        let mut types = vec![ASIOST_INT32_LSB; n_out as usize];
        for (o, t) in types.iter_mut().enumerate() {
            match m.channel_info(o as i32, false) {
                Ok(c) => *t = c.sample_type,
                Err(e) => return self.fail(format!("master getChannelInfo(out {o}): {e}")),
            }
        }
        if !types.iter().all(|&t| convert::is_supported(t)) || in_info.iter().any(|c| !convert::is_supported(c.sample_type)) {
            dlog!("warning: master uses sample types we cannot convert: out {types:?}");
        }
        let (il, ol) = m.latencies().unwrap_or((0, 0));
        dlog!(
            "master ok: {} in / {} out, buffer {} (min {} max {}), rate {:?}, out types {:?}, latency {il}/{ol}",
            n_in,
            n_out,
            bs.preferred,
            bs.min,
            bs.max,
            m.sample_rate().ok(),
            types
        );
        self.num_in = n_in;
        self.buffer_size = bs.preferred;
        self.in_info = in_info;
        self.num_out = n_out;
        self.out_types = types;
        self.in_latency = il;
        self.out_latency = ol;
        self.active_in = vec![false; n_in as usize];
        self.master = Some(m);
        self.reset = ResetThread::spawn();
        ASIO_TRUE
    }

    fn create_buffers(
        &mut self,
        core_slot: &AtomicPtr<Core>,
        infos: *mut AsioBufferInfo,
        num: i32,
        size: i32,
        callbacks: *const AsioCallbacks,
    ) -> AsioError {
        let Some(master) = self.master.as_mut() else { return ASE_NOT_PRESENT };
        if self.buffers_created {
            return ASE_INVALID_MODE;
        }
        if infos.is_null() || callbacks.is_null() || num <= 0 {
            return ASE_INVALID_PARAMETER;
        }
        if size != self.buffer_size {
            self.error = format!("unsupported buffer size {size} (driver uses {})", self.buffer_size);
            return ASE_INVALID_MODE;
        }
        // SAFETY: the host passes `num` entries.
        let reqs = unsafe { core::slice::from_raw_parts_mut(infos, num as usize) };
        for r in reqs.iter() {
            let (is_in, ch) = ({ r.is_input } != 0, { r.channel_num });
            let limit = if is_in { self.num_in } else { NUM_OUTPUTS };
            if ch < 0 || ch >= limit {
                return ASE_INVALID_PARAMETER;
            }
        }
        let host = unsafe { *callbacks };
        let frames = size as usize;

        // Our own double buffers (Int32 LSB): [half0 | half1] per channel.
        let mut bufs: Vec<Box<[i32]>> = Vec::with_capacity(reqs.len());
        for r in reqs.iter_mut() {
            let mut b = vec![0i32; frames * 2].into_boxed_slice();
            let p = b.as_mut_ptr();
            // SAFETY: p..p+2*frames is the allocation.
            r.buffers = [p as *mut c_void, unsafe { p.add(frames) } as *mut c_void];
            bufs.push(b);
        }

        let rate = master.sample_rate().unwrap_or(48000.0);
        let reset_tx = match self.reset.as_ref() {
            Some(r) => r.sender(),
            None => return ASE_HW_MALFUNCTION,
        };

        // Config -> processor (EQ at the master's rate) + status block (best effort).
        let cfg = config::load();
        let n_out = self.num_out as usize;
        let handoff: Arc<Handoff<Snapshot>> = Arc::new(Handoff::new());
        let processor = EqProcessor::new(rate, &cfg, n_out, handoff.clone());
        let status = StatusWriter::create_mapped();
        match &status {
            Some(s) => s.init(rate as u32, size as u32, self.in_latency.max(0) as u32, self.out_latency.max(0) as u32, std::process::id()),
            None => dlog!("status mapping unavailable, continuing without it"),
        }
        let core = Box::into_raw(Core::new(frames, host, rate, reset_tx, Box::new(processor), status));

        // Master buffers: every master output (the EQ pair is routed per config at runtime), then each requested input.
        let mut mch: Vec<(bool, i32)> = (0..self.num_out).map(|o| (false, o)).collect();
        let mut in_map: Vec<(usize, usize, i32)> = Vec::new(); // (request idx, master buf idx, master ch)
        for (i, r) in reqs.iter().enumerate() {
            if { r.is_input } != 0 {
                in_map.push((i, mch.len(), r.channel_num));
                mch.push((true, r.channel_num));
            }
        }
        if let Err(e) = master.create_buffers(&mch, size, Box::new(MasterHandler { core })) {
            // SAFETY: allocated by Box::into_raw above, never shared.
            drop(unsafe { Box::from_raw(core) });
            self.error = format!("master createBuffers: {e}");
            dlog!("{}", self.error);
            return ASE_HW_MALFUNCTION;
        }

        // SAFETY: master is stopped, so nothing else touches the core yet.
        let c = unsafe { &mut *core };
        c.master = &**master as *const AsioDriver;
        for o in 0..n_out {
            let p = master.buffer_ptrs(o);
            c.master_out.push(OutChan { ptrs: [p[0] as *mut u8, p[1] as *mut u8], ty: self.out_types[o] });
        }
        for &(ri, mi, mch_num) in &in_map {
            let p = master.buffer_ptrs(mi);
            let ty = self.in_info[mch_num as usize].sample_type;
            c.ins.push(InChan {
                src: [p[0] as *const u8, p[1] as *const u8],
                ty,
                dst: bufs[ri].as_mut_ptr(),
            });
        }
        self.active_in.iter_mut().for_each(|a| *a = false);
        self.active_out.iter_mut().for_each(|a| *a = false);
        for (i, r) in reqs.iter().enumerate() {
            let ch = { r.channel_num };
            if { r.is_input } != 0 {
                self.active_in[ch as usize] = true;
            } else {
                self.active_out[ch as usize] = true;
                let p = bufs[i].as_ptr();
                if ch == 0 {
                    c.out_l = p;
                } else if ch == 1 {
                    c.out_r = p;
                }
            }
        }

        // Does the host want time info?
        // SAFETY: host callbacks are valid for the duration of the buffers.
        c.use_time_info = unsafe {
            let msg = host.asio_message;
            msg(K_ASIO_SELECTOR_SUPPORTED, K_ASIO_SUPPORTS_TIME_INFO, core::ptr::null_mut(), core::ptr::null_mut()) == 1
                && msg(K_ASIO_SUPPORTS_TIME_INFO, 0, core::ptr::null_mut(), core::ptr::null_mut()) == 1
        };

        if let Some(r) = self.reset.as_ref() {
            r.set_host(Some(host));
        }
        self.watcher = ConfigWatcher::spawn(config::config_path(), handoff, n_out);
        self.core = core;
        core_slot.store(core, Ordering::Release);
        self.our_bufs = bufs;
        self.buffers_created = true;
        dlog!(
            "createBuffers ok: {} requested, size {}, time info {}",
            num,
            size,
            if c.use_time_info { "yes" } else { "no" }
        );
        ASE_OK
    }

    fn start(&mut self) -> AsioError {
        let Some(master) = self.master.as_ref() else { return ASE_NOT_PRESENT };
        if !self.buffers_created {
            return ASE_NOT_PRESENT;
        }
        if self.started {
            return ASE_OK;
        }
        // SAFETY: core is alive while buffers exist; master is stopped.
        let c = unsafe { &*self.core };
        c.sample_pos.store(0, Ordering::Release);
        c.set_active(true);
        if let Err(e) = master.start() {
            c.set_active(false);
            self.error = format!("master start: {e}");
            dlog!("{}", self.error);
            return ASE_HW_MALFUNCTION;
        }
        self.started = true;
        dlog!("started");
        ASE_OK
    }

    fn stop(&mut self) -> AsioError {
        if !self.started {
            return ASE_OK;
        }
        // SAFETY: core alive while started.
        unsafe { &*self.core }.set_active(false);
        if let Some(m) = self.master.as_ref() {
            if let Err(e) = m.stop() {
                dlog!("master stop: {e}");
            }
        }
        self.started = false;
        dlog!("stopped");
        ASE_OK
    }

    /// Stop, dispose master buffers, then free our state (in that order).
    fn dispose(&mut self) -> AsioError {
        if !self.buffers_created {
            return ASE_OK;
        }
        self.stop();
        if let Some(m) = self.master.as_mut() {
            if let Err(e) = m.dispose_buffers() {
                dlog!("master disposeBuffers: {e}");
            }
        }
        if let Some(r) = self.reset.as_ref() {
            r.set_host(None);
        }
        if let Some(mut w) = self.watcher.take() {
            w.shutdown();
        }
        if !self.core.is_null() {
            // SAFETY: allocated by Box::into_raw; master no longer calls us.
            drop(unsafe { Box::from_raw(self.core) });
            self.core = core::ptr::null_mut();
        }
        self.our_bufs.clear();
        self.active_in.iter_mut().for_each(|a| *a = false);
        self.active_out.iter_mut().for_each(|a| *a = false);
        self.buffers_created = false;
        dlog!("buffers disposed");
        ASE_OK
    }
}

// ---------------------------------------------------------------- IASIO methods

asio_method!(fn m_init(this: *mut IAsio, sys: *mut c_void) -> AsioBool = ASIO_FALSE, {
    me(this).init(sys)
});

asio_method!(fn m_get_driver_name(_this: *mut IAsio, name: *mut c_char) -> () = (), {
    copy_cstr(name, DRIVER_NAME, 32);
});

asio_method!(fn m_get_driver_version(_this: *mut IAsio) -> i32 = 0, { DRIVER_VERSION });

asio_method!(fn m_get_error_message(this: *mut IAsio, msg: *mut c_char) -> () = (), {
    let e = &me(this).error;
    copy_cstr(msg, if e.is_empty() { "No error" } else { e }, 124);
});

asio_method!(fn m_start(this: *mut IAsio) -> AsioError = ASE_HW_MALFUNCTION, { me(this).start() });

asio_method!(fn m_stop(this: *mut IAsio) -> AsioError = ASE_HW_MALFUNCTION, { me(this).stop() });

asio_method!(fn m_get_channels(this: *mut IAsio, n_in: *mut i32, n_out: *mut i32) -> AsioError = ASE_HW_MALFUNCTION, {
    let s = me(this);
    if s.master.is_none() {
        return ASE_NOT_PRESENT;
    }
    if n_in.is_null() || n_out.is_null() {
        return ASE_INVALID_PARAMETER;
    }
    *n_in = s.num_in;
    *n_out = NUM_OUTPUTS;
    ASE_OK
});

asio_method!(fn m_get_latencies(this: *mut IAsio, l_in: *mut i32, l_out: *mut i32) -> AsioError = ASE_HW_MALFUNCTION, {
    let s = me(this);
    let Some(m) = s.master.as_ref() else { return ASE_NOT_PRESENT };
    if l_in.is_null() || l_out.is_null() {
        return ASE_INVALID_PARAMETER;
    }
    if let Ok((i, o)) = m.latencies() {
        s.in_latency = i;
        s.out_latency = o;
    }
    // EQ is processed in place within the same cycle: no added latency.
    *l_in = s.in_latency;
    *l_out = s.out_latency;
    ASE_OK
});

asio_method!(fn m_get_buffer_size(this: *mut IAsio, mn: *mut i32, mx: *mut i32, pf: *mut i32, gr: *mut i32) -> AsioError = ASE_HW_MALFUNCTION, {
    let s = me(this);
    if s.master.is_none() {
        return ASE_NOT_PRESENT;
    }
    if mn.is_null() || mx.is_null() || pf.is_null() || gr.is_null() {
        return ASE_INVALID_PARAMETER;
    }
    *mn = s.buffer_size;
    *mx = s.buffer_size;
    *pf = s.buffer_size;
    *gr = 0;
    ASE_OK
});

asio_method!(fn m_can_sample_rate(this: *mut IAsio, rate: AsioSampleRate) -> AsioError = ASE_HW_MALFUNCTION, {
    match me(this).master.as_ref() {
        Some(m) => if m.can_sample_rate(rate) { ASE_OK } else { ASE_NO_CLOCK },
        None => ASE_NOT_PRESENT,
    }
});

asio_method!(fn m_get_sample_rate(this: *mut IAsio, rate: *mut AsioSampleRate) -> AsioError = ASE_HW_MALFUNCTION, {
    let Some(m) = me(this).master.as_ref() else { return ASE_NOT_PRESENT };
    if rate.is_null() {
        return ASE_INVALID_PARAMETER;
    }
    match m.sample_rate() {
        Ok(r) => {
            *rate = r;
            ASE_OK
        }
        Err(_) => ASE_NO_CLOCK,
    }
});

asio_method!(fn m_set_sample_rate(this: *mut IAsio, rate: AsioSampleRate) -> AsioError = ASE_HW_MALFUNCTION, {
    let s = me(this);
    let Some(m) = s.master.as_ref() else { return ASE_NOT_PRESENT };
    let current = m.sample_rate().unwrap_or(0.0);
    if (current - rate).abs() < 0.5 {
        return ASE_OK;
    }
    if !m.can_sample_rate(rate) {
        return ASE_NO_CLOCK;
    }
    match m.set_sample_rate(rate) {
        Ok(()) => {
            dlog!("setSampleRate({rate}) ok (was {current})");
            // Buffers already exist: the host must reload us, like USBAsio's reset thread.
            if s.buffers_created {
                if let Some(r) = s.reset.as_ref() {
                    r.post(ResetMsg::Reset);
                }
            }
            ASE_OK
        }
        Err(e) => {
            dlog!("setSampleRate({rate}) failed: {e}");
            ASE_NO_CLOCK
        }
    }
});

asio_method!(fn m_get_clock_sources(_this: *mut IAsio, clocks: *mut AsioClockSource, n: *mut i32) -> AsioError = ASE_HW_MALFUNCTION, {
    if clocks.is_null() || n.is_null() {
        return ASE_INVALID_PARAMETER;
    }
    if *n < 1 {
        return ASE_INVALID_PARAMETER;
    }
    (*clocks).index = 0;
    (*clocks).associated_channel = -1;
    (*clocks).associated_group = -1;
    (*clocks).is_current_source = ASIO_TRUE;
    (*clocks).name = name_array("Internal");
    *n = 1;
    ASE_OK
});

asio_method!(fn m_set_clock_source(_this: *mut IAsio, reference: i32) -> AsioError = ASE_HW_MALFUNCTION, {
    if reference == 0 { ASE_OK } else { ASE_INVALID_PARAMETER }
});

asio_method!(fn m_get_sample_position(this: *mut IAsio, pos: *mut AsioSamples, ts: *mut AsioTimeStamp) -> AsioError = ASE_HW_MALFUNCTION, {
    if pos.is_null() || ts.is_null() {
        return ASE_INVALID_PARAMETER;
    }
    let c = drv(this).core_ptr.load(Ordering::Acquire);
    if c.is_null() {
        return ASE_SP_NOT_ADVANCING;
    }
    *pos = AsioInt64::from_u64((*c).sample_pos.load(Ordering::Acquire));
    *ts = AsioInt64::from_u64((*c).sys_time_ns.load(Ordering::Acquire));
    ASE_OK
});

asio_method!(fn m_get_channel_info(this: *mut IAsio, info: *mut AsioChannelInfo) -> AsioError = ASE_HW_MALFUNCTION, {
    let s = me(this);
    if s.master.is_none() {
        return ASE_NOT_PRESENT;
    }
    if info.is_null() {
        return ASE_INVALID_PARAMETER;
    }
    let (ch, is_in) = ({ (*info).channel }, { (*info).is_input } != 0);
    if is_in {
        let Some(ci) = s.in_info.get(ch.max(0) as usize).filter(|_| ch >= 0) else { return ASE_INVALID_PARAMETER };
        (*info).is_active = s.active_in[ch as usize] as AsioBool;
        (*info).name = name_array(&ci.name);
    } else {
        if !(0..NUM_OUTPUTS).contains(&ch) {
            return ASE_INVALID_PARAMETER;
        }
        (*info).is_active = s.active_out[ch as usize] as AsioBool;
        (*info).name = name_array(&format!("EQ Out {}", ch + 1));
    }
    (*info).channel_group = 0;
    (*info).sample_type = ASIOST_INT32_LSB;
    ASE_OK
});

asio_method!(fn m_create_buffers(this: *mut IAsio, infos: *mut AsioBufferInfo, num: i32, size: i32, cb: *const AsioCallbacks) -> AsioError = ASE_HW_MALFUNCTION, {
    me(this).create_buffers(&drv(this).core_ptr, infos, num, size, cb)
});

asio_method!(fn m_dispose_buffers(this: *mut IAsio) -> AsioError = ASE_HW_MALFUNCTION, {
    let s = me(this);
    if !s.buffers_created {
        return ASE_INVALID_MODE;
    }
    // Hide the core from getSamplePosition before it is freed.
    drv(this).core_ptr.store(core::ptr::null_mut(), Ordering::Release);
    s.dispose()
});

asio_method!(fn m_control_panel(_this: *mut IAsio) -> AsioError = ASE_OK, {
    // Fire-and-forget launch of the GUI; always succeed.
    if let Some(dir) = crate::com::module_path().and_then(|p| std::path::Path::new(&p).parent().map(|d| d.to_path_buf())) {
        // Installed layout first (next to the DLL), then dev layouts where the
        // 32-bit DLL lives in target\i686-pc-windows-msvc\<profile>\ and the
        // 64-bit GUI in target\<profile>\.
        let mut candidates = vec![dir.join(CONTROL_PANEL_EXE), dir.join("..").join(CONTROL_PANEL_EXE)];
        if let (Some(profile), Some(target)) = (dir.file_name(), dir.parent().and_then(|p| p.parent())) {
            candidates.push(target.join(profile).join(CONTROL_PANEL_EXE));
            candidates.push(target.join("release").join(CONTROL_PANEL_EXE));
            candidates.push(target.join("debug").join(CONTROL_PANEL_EXE));
        }
        match candidates.iter().find(|p| p.exists()) {
            Some(exe) => match std::process::Command::new(exe).spawn() {
                Ok(_) => dlog!("controlPanel: launched {}", exe.display()),
                Err(e) => dlog!("controlPanel: launch of {} failed: {e}", exe.display()),
            },
            None => dlog!("controlPanel: {} not found (searched {} locations)", CONTROL_PANEL_EXE, candidates.len()),
        }
    }
    ASE_OK
});

asio_method!(fn m_future(_this: *mut IAsio, selector: i32, _opt: *mut c_void) -> AsioError = ASE_NOT_PRESENT, {
    match selector {
        K_ASIO_CAN_TIME_INFO => ASE_SUCCESS,
        _ => ASE_NOT_PRESENT,
    }
});

asio_method!(fn m_output_ready(_this: *mut IAsio) -> AsioError = ASE_OK, { ASE_OK });
