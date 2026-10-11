//! Host a third-party ASIO driver: registry enumeration, COM instantiation and
//! a safe-ish wrapper over the `IASIO` vtable.

pub mod busy;

use std::ffi::c_void;

pub use busy::{ask_holder_to_close, describe_holders, find_driver_holders, Holder};
use std::sync::atomic::{AtomicPtr, Ordering};

use asio_abi::*;
use thiserror::Error;
use windows::core::{Interface, GUID, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CoGetClassObject, CoInitializeEx, IClassFactory, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegEnumKeyExW, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ,
    REG_SZ, REG_VALUE_TYPE,
};

#[derive(Debug, Error)]
pub enum HostError {
    #[error("registry error: {0}")]
    Registry(String),
    #[error("COM error: {0}")]
    Com(#[from] windows::core::Error),
    #[error("driver init failed")]
    InitFailed,
    #[error("ASIO call `{call}` failed with code {code}")]
    Asio { call: &'static str, code: i32 },
    #[error("driver `{0}` not found")]
    NotFound(String),
    #[error("driver is {}", busy::describe_holders(holders))]
    DriverBusy { holders: Vec<Holder> },
}

pub type Result<T> = std::result::Result<T, HostError>;

#[derive(Clone, Debug)]
pub struct DriverInfo {
    pub name: String,
    pub clsid: GUID,
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn read_reg_string(key: HKEY, value: &str) -> Option<String> {
    let name = to_wide(value);
    let mut ty = REG_VALUE_TYPE(0);
    let mut buf = [0u16; 512];
    let mut len = (buf.len() * 2) as u32;
    // SAFETY: buffers are valid for the sizes passed.
    let rc = unsafe {
        RegQueryValueExW(
            key,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut ty),
            Some(buf.as_mut_ptr() as *mut u8),
            Some(&mut len),
        )
    };
    if rc.is_err() || ty != REG_SZ {
        return None;
    }
    let chars = (len as usize / 2).min(buf.len());
    let s = String::from_utf16_lossy(&buf[..chars]);
    Some(s.trim_end_matches('\0').to_string())
}

/// Enumerates `HKLM\SOFTWARE\ASIO` in the registry view matching this
/// process's bitness (a 32-bit process transparently sees WOW6432Node).
pub fn enumerate() -> Result<Vec<DriverInfo>> {
    let path = to_wide("SOFTWARE\\ASIO");
    let mut root = HKEY::default();
    // SAFETY: valid NUL-terminated path, valid out pointer.
    let rc = unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, PCWSTR(path.as_ptr()), None, KEY_READ, &mut root) };
    if rc.is_err() {
        return Err(HostError::Registry(format!("open HKLM\\SOFTWARE\\ASIO: {rc:?}")));
    }
    let mut out = Vec::new();
    let mut index = 0u32;
    loop {
        let mut name = [0u16; 256];
        let mut len = name.len() as u32;
        // SAFETY: buffer/len describe `name`.
        let rc = unsafe {
            RegEnumKeyExW(root, index, Some(windows::core::PWSTR(name.as_mut_ptr())), &mut len, None, None, None, None)
        };
        if rc.is_err() {
            break;
        }
        index += 1;
        let subkey = String::from_utf16_lossy(&name[..len as usize]);
        let sub_w = to_wide(&subkey);
        let mut k = HKEY::default();
        // SAFETY: as above.
        if unsafe { RegOpenKeyExW(root, PCWSTR(sub_w.as_ptr()), None, KEY_READ, &mut k) }.is_err() {
            continue;
        }
        if let Some(clsid_str) = read_reg_string(k, "CLSID") {
            let w = to_wide(&clsid_str);
            // SAFETY: NUL-terminated wide string.
            if let Ok(clsid) = unsafe { windows::Win32::System::Com::CLSIDFromString(PCWSTR(w.as_ptr())) } {
                out.push(DriverInfo { name: subkey, clsid });
            }
        }
        // SAFETY: `k` was opened above.
        unsafe {
            let _ = RegCloseKey(k);
        }
    }
    // SAFETY: `root` was opened above.
    unsafe {
        let _ = RegCloseKey(root);
    }
    Ok(out)
}

pub fn find_driver(name: &str) -> Result<DriverInfo> {
    enumerate()?
        .into_iter()
        .find(|d| d.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| HostError::NotFound(name.to_string()))
}

/// Initialise COM as a single-threaded apartment for the calling thread
/// (ASIO drivers are registered as `ThreadingModel=Apartment`).
pub fn init_com_sta() {
    // SAFETY: plain COM init; S_FALSE / RPC_E_CHANGED_MODE are tolerated.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
}

/// Receives driver callbacks. Called on the driver's audio thread.
pub trait Handler: Send {
    /// `index` is the half (0/1) of the double buffer the host may now process.
    fn buffer_switch(&mut self, index: i32);
    /// Driver asked for a reset (e.g. buffer size / rate changed).
    fn reset_request(&mut self) {}
    fn sample_rate_changed(&mut self, _rate: f64) {}
    /// Driver reported `kAsioOverload` (it missed a deadline).
    fn overload(&mut self) {}
}

/// Single global callback slot: ASIO callbacks carry no user pointer, so one
/// hosted driver per process (per copy of this crate) is supported.
static HANDLER: AtomicPtr<Box<dyn Handler>> = AtomicPtr::new(std::ptr::null_mut());

unsafe extern "C" fn cb_buffer_switch(index: i32, _direct: AsioBool) {
    let p = HANDLER.load(Ordering::Acquire);
    if !p.is_null() {
        (**p).buffer_switch(index);
    }
}
unsafe extern "C" fn cb_sample_rate(rate: f64) {
    let p = HANDLER.load(Ordering::Acquire);
    if !p.is_null() {
        (**p).sample_rate_changed(rate);
    }
}
unsafe extern "C" fn cb_message(selector: i32, value: i32, _msg: *mut c_void, _opt: *mut f64) -> i32 {
    match selector {
        K_ASIO_SELECTOR_SUPPORTED => matches!(
            value,
            K_ASIO_RESET_REQUEST | K_ASIO_ENGINE_VERSION | K_ASIO_RESYNC_REQUEST | K_ASIO_LATENCIES_CHANGED | K_ASIO_OVERLOAD
        ) as i32,
        K_ASIO_ENGINE_VERSION => 2,
        K_ASIO_SUPPORTS_TIME_INFO => 0,
        K_ASIO_RESET_REQUEST | K_ASIO_BUFFER_SIZE_CHANGE => {
            let p = HANDLER.load(Ordering::Acquire);
            if !p.is_null() {
                (**p).reset_request();
            }
            1
        }
        K_ASIO_OVERLOAD => {
            let p = HANDLER.load(Ordering::Acquire);
            if !p.is_null() {
                (**p).overload();
            }
            1
        }
        K_ASIO_RESYNC_REQUEST | K_ASIO_LATENCIES_CHANGED => 1,
        _ => 0,
    }
}
unsafe extern "C" fn cb_buffer_switch_time(time: *mut AsioTime, index: i32, direct: AsioBool) -> *mut AsioTime {
    cb_buffer_switch(index, direct);
    time
}

static CALLBACKS: AsioCallbacks = AsioCallbacks {
    buffer_switch: cb_buffer_switch,
    sample_rate_did_change: cb_sample_rate,
    asio_message: cb_message,
    buffer_switch_time_info: cb_buffer_switch_time,
};

#[derive(Clone, Copy, Debug)]
pub struct BufferSizes {
    pub min: i32,
    pub max: i32,
    pub preferred: i32,
    pub granularity: i32,
}

#[derive(Clone, Debug)]
pub struct ChannelInfo {
    pub channel: i32,
    pub is_input: bool,
    pub sample_type: AsioSampleType,
    pub name: String,
}

/// One hosted ASIO driver instance.
pub struct AsioDriver {
    ptr: *mut IAsio,
    buffers: Vec<AsioBufferInfo>,
    handler: *mut Box<dyn Handler>,
    pub name: String,
}

// The driver object is only used from the thread that created it, except for
// the audio callbacks which are driven by the driver itself.
unsafe impl Send for AsioDriver {}

macro_rules! call {
    ($self:ident, $name:literal, $method:ident $(, $arg:expr)*) => {{
        // SAFETY: `ptr` is a live IASIO object; vtable layout mirrors the ABI.
        let code = unsafe { ((*(*$self.ptr).vtbl).$method)($self.ptr $(, $arg)*) };
        check($name, code)
    }};
}

fn check(call: &'static str, code: AsioError) -> Result<()> {
    if code == ASE_OK || code == ASE_SUCCESS {
        Ok(())
    } else {
        Err(HostError::Asio { call, code })
    }
}

impl AsioDriver {
    /// Instantiates and initialises a driver. Call on an STA thread.
    pub fn load(info: &DriverInfo, hwnd: HWND) -> Result<Self> {
        // Drivers answer QueryInterface only for their own CLSID used as IID.
        // SAFETY: standard COM class-factory creation.
        let factory: IClassFactory =
            unsafe { CoGetClassObject(&info.clsid, CLSCTX_INPROC_SERVER, None)? };
        let mut raw: *mut c_void = std::ptr::null_mut();
        // SAFETY: valid factory, out pointer; IID is the driver CLSID.
        unsafe {
            (Interface::vtable(&factory).CreateInstance)(
                factory.as_raw(),
                std::ptr::null_mut(),
                &info.clsid,
                &mut raw,
            )
            .ok()?;
        }
        let ptr = raw as *mut IAsio;
        if ptr.is_null() {
            return Err(HostError::InitFailed);
        }
        let this = Self { ptr, buffers: Vec::new(), handler: std::ptr::null_mut(), name: info.name.clone() };
        // SAFETY: live object.
        let ok = unsafe { ((*(*ptr).vtbl).init)(ptr, hwnd.0) };
        if ok == 0 {
            // `this` drop releases the object.
            return Err(HostError::InitFailed);
        }
        Ok(this)
    }

    /// As [`load`](Self::load), but when it fails with a init/COM error and another process
    /// has the driver DLL loaded, returns [`HostError::DriverBusy`] naming the holders.
    pub fn load_diagnosed(info: &DriverInfo, hwnd: HWND) -> Result<Self> {
        match Self::load(info, hwnd) {
            Err(e @ (HostError::InitFailed | HostError::Com(_))) => {
                let holders = find_driver_holders(info);
                if holders.is_empty() {
                    Err(e)
                } else {
                    Err(HostError::DriverBusy { holders })
                }
            }
            other => other,
        }
    }

    pub fn driver_name(&self) -> String {
        let mut buf = [0 as std::ffi::c_char; 64];
        // SAFETY: buffer is >= 32 bytes as the ABI requires.
        unsafe { ((*(*self.ptr).vtbl).get_driver_name)(self.ptr, buf.as_mut_ptr()) };
        cstr_to_string(&buf)
    }

    pub fn driver_version(&self) -> i32 {
        // SAFETY: live object.
        unsafe { ((*(*self.ptr).vtbl).get_driver_version)(self.ptr) }
    }

    pub fn error_message(&self) -> String {
        let mut buf = [0 as std::ffi::c_char; 128];
        // SAFETY: buffer is >= 124 bytes as the ABI requires.
        unsafe { ((*(*self.ptr).vtbl).get_error_message)(self.ptr, buf.as_mut_ptr()) };
        cstr_to_string(&buf)
    }

    pub fn channels(&self) -> Result<(i32, i32)> {
        let (mut i, mut o) = (0, 0);
        call!(self, "getChannels", get_channels, &mut i, &mut o)?;
        Ok((i, o))
    }

    pub fn latencies(&self) -> Result<(i32, i32)> {
        let (mut i, mut o) = (0, 0);
        call!(self, "getLatencies", get_latencies, &mut i, &mut o)?;
        Ok((i, o))
    }

    pub fn buffer_sizes(&self) -> Result<BufferSizes> {
        let (mut mn, mut mx, mut pf, mut gr) = (0, 0, 0, 0);
        call!(self, "getBufferSize", get_buffer_size, &mut mn, &mut mx, &mut pf, &mut gr)?;
        Ok(BufferSizes { min: mn, max: mx, preferred: pf, granularity: gr })
    }

    pub fn sample_rate(&self) -> Result<f64> {
        let mut r = 0.0;
        call!(self, "getSampleRate", get_sample_rate, &mut r)?;
        Ok(r)
    }

    pub fn set_sample_rate(&self, rate: f64) -> Result<()> {
        call!(self, "setSampleRate", set_sample_rate, rate)
    }

    pub fn can_sample_rate(&self, rate: f64) -> bool {
        // SAFETY: live object.
        let code = unsafe { ((*(*self.ptr).vtbl).can_sample_rate)(self.ptr, rate) };
        code == ASE_OK || code == ASE_SUCCESS
    }

    pub fn channel_info(&self, channel: i32, is_input: bool) -> Result<ChannelInfo> {
        // SAFETY: all-zero is valid for this POD.
        let mut ci: AsioChannelInfo = unsafe { std::mem::zeroed() };
        ci.channel = channel;
        ci.is_input = is_input as AsioBool;
        call!(self, "getChannelInfo", get_channel_info, &mut ci)?;
        let name = cstr_to_string(&{ ci.name });
        Ok(ChannelInfo { channel, is_input, sample_type: { ci.sample_type }, name })
    }

    /// Creates buffers for `(is_input, channel)` pairs and installs `handler`.
    pub fn create_buffers(
        &mut self,
        channels: &[(bool, i32)],
        buffer_size: i32,
        handler: Box<dyn Handler>,
    ) -> Result<()> {
        self.buffers = channels
            .iter()
            .map(|&(is_input, ch)| AsioBufferInfo {
                is_input: is_input as AsioBool,
                channel_num: ch,
                buffers: [std::ptr::null_mut(); 2],
            })
            .collect();
        let boxed: *mut Box<dyn Handler> = Box::into_raw(Box::new(handler));
        self.handler = boxed;
        HANDLER.store(boxed, Ordering::Release);
        let n = self.buffers.len() as i32;
        let bufs = self.buffers.as_mut_ptr();
        let r = call!(self, "createBuffers", create_buffers, bufs, n, buffer_size, &CALLBACKS);
        if r.is_err() {
            self.release_handler();
        }
        r
    }

    /// Buffer pointers (`[half0, half1]`) for the i-th channel passed to `create_buffers`.
    pub fn buffer_ptrs(&self, i: usize) -> [*mut c_void; 2] {
        self.buffers[i].buffers
    }

    pub fn start(&self) -> Result<()> {
        call!(self, "start", start)
    }
    pub fn stop(&self) -> Result<()> {
        call!(self, "stop", stop)
    }
    pub fn output_ready(&self) -> bool {
        // SAFETY: live object.
        unsafe { ((*(*self.ptr).vtbl).output_ready)(self.ptr) == ASE_OK }
    }
    pub fn control_panel(&self) -> Result<()> {
        call!(self, "controlPanel", control_panel)
    }

    pub fn dispose_buffers(&mut self) -> Result<()> {
        let r = call!(self, "disposeBuffers", dispose_buffers);
        self.buffers.clear();
        self.release_handler();
        r
    }

    fn release_handler(&mut self) {
        HANDLER.store(std::ptr::null_mut(), Ordering::Release);
        if !self.handler.is_null() {
            // SAFETY: allocated by `create_buffers` via Box::into_raw; the driver
            // is stopped/disposed so no callback can still use it.
            drop(unsafe { Box::from_raw(self.handler) });
            self.handler = std::ptr::null_mut();
        }
    }
}

/// Copyable handle that can call `outputReady` from the audio callback.
///
/// Valid only while the driver it came from is alive (the engine drops the
/// callback owner by stopping/disposing the driver first).
#[derive(Clone, Copy)]
pub struct OutputReady(*mut IAsio);

// SAFETY: `outputReady` is documented to be callable from the buffer-switch callback.
unsafe impl Send for OutputReady {}

impl OutputReady {
    /// True if the driver implements `outputReady` and returned `ASE_OK`.
    pub fn call(&self) -> bool {
        // SAFETY: the pointer is a live IASIO object while streaming.
        unsafe { ((*(*self.0).vtbl).output_ready)(self.0) == ASE_OK }
    }
}

impl AsioDriver {
    /// Handle for calling `outputReady` from the callback thread.
    pub fn output_ready_handle(&self) -> OutputReady {
        OutputReady(self.ptr)
    }
}

impl Drop for AsioDriver {
    fn drop(&mut self) {
        if !self.buffers.is_empty() {
            let _ = self.stop();
            let _ = self.dispose_buffers();
        }
        self.release_handler();
        // SAFETY: we own one reference.
        unsafe { ((*(*self.ptr).vtbl).release)(self.ptr) };
    }
}
