//! COM plumbing: DllMain, class factory, DLL exports, registration.

use core::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicI32, AtomicIsize, AtomicU32, Ordering};

use asio_abi::Guid;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HMODULE, MAX_PATH};
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_WRITE,
    REG_OPTION_NON_VOLATILE, REG_SZ,
};

use crate::driver;
use crate::ids::*;

pub const S_OK: i32 = 0;
pub const S_FALSE: i32 = 1;
pub const E_NOINTERFACE: i32 = 0x8000_4002u32 as i32;
pub const E_POINTER: i32 = 0x8000_4003u32 as i32;
pub const E_FAIL: i32 = 0x8000_4005u32 as i32;
pub const E_OUTOFMEMORY: i32 = 0x8007_000Eu32 as i32;
pub const E_UNEXPECTED: i32 = 0x8000_FFFFu32 as i32;
pub const CLASS_E_NOAGGREGATION: i32 = 0x8004_0110u32 as i32;
pub const CLASS_E_CLASSNOTAVAILABLE: i32 = 0x8004_0111u32 as i32;
pub const SELFREG_E_CLASS: i32 = 0x8004_0201u32 as i32;

/// Live COM objects + server locks; `DllCanUnloadNow` is `S_OK` when zero.
pub static OBJECT_COUNT: AtomicI32 = AtomicI32::new(0);
pub static LOCK_COUNT: AtomicI32 = AtomicI32::new(0);
static MODULE: AtomicIsize = AtomicIsize::new(0);

/// Runs `f`, returning `fail` if it panics (no-op under `panic = "abort"`).
pub fn guard<R>(fail: R, f: impl FnOnce() -> R) -> R {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(fail)
}

#[no_mangle]
pub extern "system" fn DllMain(hinst: *mut c_void, reason: u32, _reserved: *mut c_void) -> i32 {
    const DLL_PROCESS_ATTACH: u32 = 1;
    if reason == DLL_PROCESS_ATTACH {
        MODULE.store(hinst as isize, Ordering::Relaxed);
    }
    1
}

/// Full path of this DLL.
pub fn module_path() -> Option<String> {
    let h = MODULE.load(Ordering::Relaxed);
    if h == 0 {
        return None;
    }
    let mut buf = [0u16; MAX_PATH as usize * 2];
    // SAFETY: `buf` is a valid mutable buffer.
    let n = unsafe { GetModuleFileNameW(Some(HMODULE(h as *mut c_void)), &mut buf) } as usize;
    if n == 0 || n >= buf.len() {
        return None;
    }
    Some(String::from_utf16_lossy(&buf[..n]))
}

// ---------------------------------------------------------------- class factory

#[repr(C)]
struct FactoryVtbl {
    query_interface: unsafe extern "system" fn(*mut Factory, *const Guid, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut Factory) -> u32,
    release: unsafe extern "system" fn(*mut Factory) -> u32,
    create_instance: unsafe extern "system" fn(*mut Factory, *mut c_void, *const Guid, *mut *mut c_void) -> i32,
    lock_server: unsafe extern "system" fn(*mut Factory, i32) -> i32,
}

#[repr(C)]
struct Factory {
    vtbl: *const FactoryVtbl,
    refs: AtomicU32,
}

static FACTORY_VTBL: FactoryVtbl = FactoryVtbl {
    query_interface: factory_qi,
    add_ref: factory_add_ref,
    release: factory_release,
    create_instance: factory_create_instance,
    lock_server: factory_lock_server,
};

unsafe extern "system" fn factory_qi(this: *mut Factory, riid: *const Guid, ppv: *mut *mut c_void) -> i32 {
    guard(E_UNEXPECTED, || {
        if ppv.is_null() || riid.is_null() {
            return E_POINTER;
        }
        let iid = *riid;
        if iid == IID_IUNKNOWN || iid == IID_ICLASSFACTORY {
            factory_add_ref(this);
            *ppv = this as *mut c_void;
            S_OK
        } else {
            *ppv = core::ptr::null_mut();
            E_NOINTERFACE
        }
    })
}

unsafe extern "system" fn factory_add_ref(this: *mut Factory) -> u32 {
    (*this).refs.fetch_add(1, Ordering::AcqRel) + 1
}

unsafe extern "system" fn factory_release(this: *mut Factory) -> u32 {
    let n = (*this).refs.fetch_sub(1, Ordering::AcqRel) - 1;
    if n == 0 {
        drop(Box::from_raw(this));
        OBJECT_COUNT.fetch_sub(1, Ordering::AcqRel);
    }
    n
}

unsafe extern "system" fn factory_create_instance(
    _this: *mut Factory,
    outer: *mut c_void,
    riid: *const Guid,
    ppv: *mut *mut c_void,
) -> i32 {
    guard(E_UNEXPECTED, || {
        if ppv.is_null() || riid.is_null() {
            return E_POINTER;
        }
        *ppv = core::ptr::null_mut();
        if !outer.is_null() {
            return CLASS_E_NOAGGREGATION;
        }
        let obj = driver::create_object();
        if obj.is_null() {
            return E_OUTOFMEMORY;
        }
        let hr = driver::query_interface(obj, riid, ppv);
        // Drop the creation reference; on success QI added its own.
        driver::release(obj);
        hr
    })
}

unsafe extern "system" fn factory_lock_server(_this: *mut Factory, lock: i32) -> i32 {
    if lock != 0 {
        LOCK_COUNT.fetch_add(1, Ordering::AcqRel);
    } else {
        LOCK_COUNT.fetch_sub(1, Ordering::AcqRel);
    }
    S_OK
}

// ---------------------------------------------------------------- DLL exports

/// # Safety
/// COM entry point; pointers must be valid per the COM contract.
#[no_mangle]
pub unsafe extern "system" fn DllGetClassObject(rclsid: *const Guid, riid: *const Guid, ppv: *mut *mut c_void) -> i32 {
    guard(E_UNEXPECTED, || {
        if rclsid.is_null() || riid.is_null() || ppv.is_null() {
            return E_POINTER;
        }
        *ppv = core::ptr::null_mut();
        if *rclsid != CLSID_EQ_ASIO {
            return CLASS_E_CLASSNOTAVAILABLE;
        }
        let f = Box::into_raw(Box::new(Factory { vtbl: &FACTORY_VTBL, refs: AtomicU32::new(1) }));
        OBJECT_COUNT.fetch_add(1, Ordering::AcqRel);
        let hr = factory_qi(f, riid, ppv);
        factory_release(f);
        hr
    })
}

#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> i32 {
    if OBJECT_COUNT.load(Ordering::Acquire) == 0 && LOCK_COUNT.load(Ordering::Acquire) == 0 {
        S_OK
    } else {
        S_FALSE
    }
}

// ---------------------------------------------------------------- registration

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn set_string(key: HKEY, name: Option<&str>, value: &str) -> bool {
    let v = wide(value);
    let n = name.map(wide);
    let pname = n.as_ref().map(|n| PCWSTR(n.as_ptr())).unwrap_or(PCWSTR::null());
    // SAFETY: `v` outlives the call; the byte view covers the whole NUL-terminated buffer.
    let bytes = unsafe { core::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 2) };
    // SAFETY: valid key handle and buffers.
    unsafe { RegSetValueExW(key, pname, None, REG_SZ, Some(bytes)) }.is_ok()
}

/// Creates `HKLM\<path>` and writes `values` (name `None` = default value).
fn write_key(path: &str, values: &[(Option<&str>, &str)]) -> bool {
    let p = wide(path);
    let mut key = HKEY::default();
    // SAFETY: valid path and out pointer.
    let rc = unsafe {
        RegCreateKeyExW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(p.as_ptr()),
            None,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut key,
            None,
        )
    };
    if rc.is_err() {
        crate::dlog!("RegCreateKeyExW({path}) failed: {rc:?}");
        return false;
    }
    let mut ok = true;
    for (n, v) in values {
        ok &= set_string(key, *n, v);
    }
    // SAFETY: opened above.
    unsafe {
        let _ = RegCloseKey(key);
    }
    ok
}

/// Registers in the registry view of this DLL's bitness (WOW6432Node for a
/// 32-bit DLL on 64-bit Windows).
#[no_mangle]
pub extern "system" fn DllRegisterServer() -> i32 {
    guard(E_UNEXPECTED, || {
        let Some(dll) = module_path() else { return SELFREG_E_CLASS };
        let clsid = clsid_string();
        let ok = write_key(&clsid_key_path(), &[(None, DRIVER_DESCRIPTION)])
            && write_key(&inproc_key_path(), &[(None, &dll), (Some("ThreadingModel"), "Apartment")])
            && write_key(&asio_key_path(), &[(Some("CLSID"), &clsid), (Some("Description"), DRIVER_NAME)]);
        crate::dlog!("DllRegisterServer({dll}) -> {ok}");
        if ok {
            S_OK
        } else {
            SELFREG_E_CLASS
        }
    })
}

#[no_mangle]
pub extern "system" fn DllUnregisterServer() -> i32 {
    guard(E_UNEXPECTED, || {
        for path in [asio_key_path(), clsid_key_path()] {
            let p = wide(&path);
            // SAFETY: valid NUL-terminated path. Missing keys are fine.
            let _ = unsafe { RegDeleteTreeW(HKEY_LOCAL_MACHINE, PCWSTR(p.as_ptr())) };
        }
        crate::dlog!("DllUnregisterServer");
        S_OK
    })
}
