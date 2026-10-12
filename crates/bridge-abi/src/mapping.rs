//! Named mapping open/create helpers (Windows API via the workspace `windows`
//! crate). This is the only module of `bridge-abi` that touches the OS.

use core::sync::atomic::{AtomicU64, Ordering};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, SetLastError, HANDLE, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, INVALID_HANDLE_VALUE,
};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
    MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};
use windows::Win32::System::Performance::QueryPerformanceFrequency;

use crate::layout::{Header, Layout, MAGIC, TOTAL_SIZE, VERSION, CHANNELS};

/// A live view of the bridge mapping: owns the Win32 handle + mapped view and
/// unmaps/closes both on drop.
pub struct Mapping {
    handle: HANDLE,
    base: *mut u8,
    /// True when this call created the mapping object (header initialised here).
    created: bool,
}

// The view is just memory; all mutation goes through the header's atomics /
// single-writer fields. The handle is independent of the creating thread.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    /// Creates (first comer) or opens the fixed-name mapping.
    pub fn open_or_create() -> Option<Mapping> {
        super::open_or_create()
    }

    /// Opens the fixed-name mapping if it exists.
    pub fn open() -> Option<Mapping> {
        super::open()
    }

    /// As [`Self::open_or_create`] with an explicit name (tests / tooling).
    pub fn open_or_create_named(name: &str) -> Option<Mapping> {
        open_or_create_named(name)
    }

    /// As [`Self::open`] with an explicit name (tests / tooling).
    pub fn open_named(name: &str) -> Option<Mapping> {
        open_named(name)
    }

    /// True when this process created the mapping (and initialised its header).
    pub fn created(&self) -> bool {
        self.created
    }

    /// Base address of the mapped image.
    pub fn base(&self) -> *mut u8 {
        self.base
    }

    /// The header at the base of the image, validated by the open call.
    ///
    /// # Safety
    /// The returned reference must not outlive this `Mapping`.
    pub unsafe fn header(&self) -> &Header {
        &*(self.base as *const Header)
    }

    /// The complete mapping image.
    ///
    /// # Safety
    /// The returned reference must not outlive this `Mapping`.
    pub unsafe fn layout(&self) -> &Layout {
        &*(self.base as *const Layout)
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: view + handle were created together in the open helpers.
        unsafe {
            let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.base as *mut _ });
            let _ = CloseHandle(self.handle);
        }
    }
}

fn wide(name: &str) -> Vec<u16> {
    name.encode_utf16().chain(core::iter::once(0)).collect()
}

/// Opens the view for a mapping handle; `None` if the view cannot cover
/// [`TOTAL_SIZE`] bytes (e.g. a foreign mapping of a different size).
unsafe fn map_view(handle: HANDLE) -> Option<*mut u8> {
    let view = MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, TOTAL_SIZE);
    if view.Value.is_null() {
        None
    } else {
        Some(view.Value as *mut u8)
    }
}

/// Initialises a freshly created header. `magic` is stored **last** with
/// `Release` so racing readers either see an invalid magic (and retry) or a
/// fully published header.
///
/// # Safety
/// `base` must be a mapped view of [`TOTAL_SIZE`] bytes.
unsafe fn init_header(base: *mut u8) {
    let h = base as *mut Header;
    // Zero the header page (fresh page-file mappings are already zero; this
    // also makes a defensive re-init deterministic).
    core::ptr::write_bytes(base, 0, crate::layout::HEADER_SIZE);
    (*h).version = VERSION;
    (*h).channels = CHANNELS;
    let mut freq = 0i64;
    let _ = QueryPerformanceFrequency(&mut freq);
    (*h).qpc_freq = freq;
    // Negotiated values start unset; the driver publishes them at init().
    // atomics (beat/index/counter fields) are already zero.
    AtomicU64::from_ptr(core::ptr::addr_of_mut!((*h).magic)).store(MAGIC, Ordering::Release);
}

/// Validates an opened image; invalid images are dropped and reported as `None`.
unsafe fn validated_view(base: *mut u8, created: bool, handle: HANDLE) -> Option<Mapping> {
    let m = Mapping { handle, base, created };
    if m.created || unsafe { (*m.base.cast::<Header>()).validate() }.is_ok() {
        Some(m)
    } else {
        // Foreign image with bad magic/version — refuse to use it.
        None
    }
}

/// Creates-or-opens `name` with the fixed [`TOTAL_SIZE`].
pub(crate) fn open_or_create_named(name: &str) -> Option<Mapping> {
    let wname = wide(name);
    // SAFETY: valid NUL-terminated name; handles closed in Mapping::drop.
    unsafe {
        SetLastError(ERROR_SUCCESS);
        let handle = CreateFileMappingW(
            INVALID_HANDLE_VALUE,
            None,
            PAGE_READWRITE,
            0,
            TOTAL_SIZE as u32,
            PCWSTR(wname.as_ptr()),
        )
        .ok()?;
        let created = GetLastError() != ERROR_ALREADY_EXISTS;
        let Some(base) = map_view(handle) else {
            let _ = CloseHandle(handle);
            return None;
        };
        if created {
            init_header(base);
        }
        validated_view(base, created, handle)
    }
}

/// Opens the existing mapping `name`; `None` when it does not exist.
pub(crate) fn open_named(name: &str) -> Option<Mapping> {
    let wname = wide(name);
    // SAFETY: as above.
    unsafe {
        let handle = OpenFileMappingW(FILE_MAP_ALL_ACCESS.0, false, PCWSTR(wname.as_ptr())).ok()?;
        let Some(base) = map_view(handle) else {
            let _ = CloseHandle(handle);
            return None;
        };
        validated_view(base, false, handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::MAPPING_NAME;

    fn unique_name() -> String {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("Local\\eq_asio_router_bridge_v2_test_{}_{}", std::process::id(), t + SEQ.fetch_add(1, Ordering::Relaxed) as u128)
    }

    #[test]
    fn create_then_open_shares_the_same_image() {
        let name = unique_name();
        let a = open_or_create_named(&name).expect("create");
        assert!(a.created());
        unsafe {
            a.header().set_sample_rate(96_000.0);
            a.header().driver_beat.store(42, Ordering::Release);
        }
        let b = open_named(&name).expect("open existing");
        assert!(!b.created());
        unsafe {
            assert_eq!(b.header().validate(), Ok(()));
            assert_eq!(b.header().get_sample_rate(), 96_000.0);
            assert_eq!(b.header().driver_beat.load(Ordering::Acquire), 42);
        }
        // A second create-or-open attaches, never re-initialises.
        let c = open_or_create_named(&name).expect("reopen");
        assert!(!c.created());
        unsafe { assert_eq!(c.header().driver_beat.load(Ordering::Acquire), 42) };
        drop(a);
        drop(b);
        drop(c);
    }

    #[test]
    fn open_missing_returns_none() {
        assert!(open_named(&unique_name()).is_none());
    }

    #[test]
    fn fixed_name_roundtrips() {
        // Uses the real contract name; sequential with other test binaries.
        let Some(a) = crate::open_or_create() else { return };
        unsafe { assert_eq!(a.header().validate(), Ok(())) };
        let Some(b) = crate::open() else { panic!("fixed-name mapping should exist while a holds it") };
        unsafe { assert_eq!(b.header().validate(), Ok(())) };
        assert!(MAPPING_NAME.starts_with("Local\\"));
    }
}
