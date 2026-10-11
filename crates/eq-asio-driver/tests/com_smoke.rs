//! In-process COM smoke test: no registry, no master driver is touched.

use asio_abi::*;
use core::ffi::c_void;
use eq_asio_driver::ids::*;
use eq_asio_driver::*;

#[repr(C)]
struct FactoryVtbl {
    qi: unsafe extern "system" fn(*mut c_void, *const Guid, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    create: unsafe extern "system" fn(*mut c_void, *mut c_void, *const Guid, *mut *mut c_void) -> i32,
    lock: unsafe extern "system" fn(*mut c_void, i32) -> i32,
}

#[test]
fn factory_creates_driver_and_answers_basic_calls() {
    unsafe {
        let mut f: *mut c_void = std::ptr::null_mut();
        assert_eq!(DllGetClassObject(&CLSID_EQ_ASIO, &IID_ICLASSFACTORY, &mut f), 0);
        let fv = *(f as *mut *const FactoryVtbl);

        // wrong class
        let mut bogus: *mut c_void = std::ptr::null_mut();
        assert_ne!(DllGetClassObject(&IID_IUNKNOWN, &IID_ICLASSFACTORY, &mut bogus), 0);

        // refuses unknown IIDs, accepts the CLSID as IID
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

        // not initialised -> no master
        let (mut i, mut o) = (0, 0);
        assert_eq!((v.get_channels)(a, &mut i, &mut o), ASE_NOT_PRESENT);
        assert_eq!((v.future)(a, K_ASIO_CAN_TIME_INFO, std::ptr::null_mut()), ASE_SUCCESS);
        assert_eq!((v.future)(a, K_ASIO_TRANSPORT, std::ptr::null_mut()), ASE_NOT_PRESENT);
        assert_eq!((v.output_ready)(a), ASE_OK);
        assert_eq!((v.dispose_buffers)(a), ASE_INVALID_MODE);
        let (mut p, mut t) = (AsioInt64::default(), AsioInt64::default());
        assert_eq!((v.get_sample_position)(a, &mut p, &mut t), ASE_SP_NOT_ADVANCING);

        // clock sources
        let mut cs: AsioClockSource = std::mem::zeroed();
        let mut n = 1;
        assert_eq!((v.get_clock_sources)(a, &mut cs, &mut n), ASE_OK);
        assert_eq!(n, 1);

        // QI with unknown IID fails, with IUnknown works
        let mut q: *mut c_void = std::ptr::null_mut();
        assert_ne!((v.query_interface)(a, &IID_ICLASSFACTORY, &mut q), 0);
        assert_eq!((v.query_interface)(a, &IID_IUNKNOWN, &mut q), 0);
        assert_eq!((v.release)(a), 1);
        assert_eq!((v.release)(a), 0);

        ((*fv).release)(f);
        assert_eq!(DllCanUnloadNow(), 0);
    }
}
