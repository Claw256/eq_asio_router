//! Shared-memory status block writer (`Local\eq_asio_router_status`).
//!
//! All stores are relaxed atomics into the mapped view (single logical writer,
//! the audio thread, except `xruns` which the core mirrors from its own
//! counter). No allocation or syscalls after construction. If the mapping
//! cannot be created the writer is simply absent (`Option`) and the driver
//! runs without publishing.

use core::mem::offset_of;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};

use eq_core::{StatusBlock, STATUS_MAGIC, STATUS_MAPPING_NAME, STATUS_VERSION};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, UnmapViewOfFile, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};

pub struct StatusWriter {
    base: *mut StatusBlock,
    /// Present when we own a Win32 mapping + view.
    mapping: Option<HANDLE>,
}

// The block is only touched through atomics.
unsafe impl Send for StatusWriter {}
unsafe impl Sync for StatusWriter {}

impl StatusWriter {
    /// Creates (or opens) the named mapping. `None` if Win32 refuses.
    pub fn create_mapped() -> Option<Self> {
        let name: Vec<u16> = STATUS_MAPPING_NAME.encode_utf16().chain(Some(0)).collect();
        // SAFETY: plain Win32 calls with valid arguments; handles are closed in Drop.
        unsafe {
            let h = CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                0,
                core::mem::size_of::<StatusBlock>() as u32,
                PCWSTR(name.as_ptr()),
            )
            .ok()?;
            let view = MapViewOfFile(h, FILE_MAP_WRITE, 0, 0, core::mem::size_of::<StatusBlock>());
            if view.Value.is_null() {
                let _ = CloseHandle(h);
                return None;
            }
            Some(StatusWriter { base: view.Value as *mut StatusBlock, mapping: Some(h) })
        }
    }

    /// Wraps caller-owned memory (tests).
    ///
    /// # Safety
    /// `p` must be 8-byte aligned, valid for a `StatusBlock` and outlive the writer.
    pub unsafe fn from_raw(p: *mut StatusBlock) -> Self {
        StatusWriter { base: p, mapping: None }
    }

    #[inline]
    fn a32(&self, off: usize) -> &AtomicU32 {
        // SAFETY: base is a valid, aligned block; offsets come from offset_of!.
        unsafe { AtomicU32::from_ptr((self.base as *mut u8).add(off) as *mut u32) }
    }
    #[inline]
    fn a64(&self, off: usize) -> &AtomicU64 {
        // SAFETY: as above; the u64 fields are 8-byte aligned.
        unsafe { AtomicU64::from_ptr((self.base as *mut u8).add(off) as *mut u64) }
    }

    /// Fills the static fields; the magic is stored last.
    pub fn init(&self, sample_rate: u32, buffer_size: u32, in_latency: u32, out_latency: u32, pid: u32) {
        self.a32(offset_of!(StatusBlock, magic)).store(0, Relaxed);
        self.a32(offset_of!(StatusBlock, version)).store(STATUS_VERSION, Relaxed);
        self.a32(offset_of!(StatusBlock, sample_rate)).store(sample_rate, Relaxed);
        self.a32(offset_of!(StatusBlock, buffer_size)).store(buffer_size, Relaxed);
        self.a32(offset_of!(StatusBlock, in_latency)).store(in_latency, Relaxed);
        self.a32(offset_of!(StatusBlock, out_latency)).store(out_latency, Relaxed);
        self.a64(offset_of!(StatusBlock, callbacks)).store(0, Relaxed);
        self.a64(offset_of!(StatusBlock, xruns)).store(0, Relaxed);
        for off in [
            offset_of!(StatusBlock, peak_in_l),
            offset_of!(StatusBlock, peak_in_r),
            offset_of!(StatusBlock, peak_out_l),
            offset_of!(StatusBlock, peak_out_r),
            offset_of!(StatusBlock, running),
        ] {
            self.a32(off).store(0, Relaxed);
        }
        self.a32(offset_of!(StatusBlock, pid)).store(pid, Relaxed);
        self.a32(offset_of!(StatusBlock, magic)).store(STATUS_MAGIC, Relaxed);
    }

    pub fn set_running(&self, on: bool) {
        self.a32(offset_of!(StatusBlock, running)).store(on as u32, Relaxed);
    }

    pub fn set_xruns(&self, n: u64) {
        self.a64(offset_of!(StatusBlock, xruns)).store(n, Relaxed);
    }

    /// Per-callback update: bumps `callbacks` and stores this block's peaks
    /// (pre-EQ in, post-EQ out; readers see the latest block, no decay).
    pub fn on_block(&self, peak_in: [f32; 2], peak_out: [f32; 2]) {
        let c = self.a64(offset_of!(StatusBlock, callbacks));
        c.store(c.load(Relaxed).wrapping_add(1), Relaxed);
        self.a32(offset_of!(StatusBlock, peak_in_l)).store(peak_in[0].to_bits(), Relaxed);
        self.a32(offset_of!(StatusBlock, peak_in_r)).store(peak_in[1].to_bits(), Relaxed);
        self.a32(offset_of!(StatusBlock, peak_out_l)).store(peak_out[0].to_bits(), Relaxed);
        self.a32(offset_of!(StatusBlock, peak_out_r)).store(peak_out[1].to_bits(), Relaxed);
    }
}

impl Drop for StatusWriter {
    fn drop(&mut self) {
        self.set_running(false);
        if let Some(h) = self.mapping.take() {
            // SAFETY: view and handle were created in `create_mapped`.
            unsafe {
                let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.base as *mut _ });
                let _ = CloseHandle(h);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_into_local_block() {
        let mut blk = Box::new(StatusBlock { magic: 0, version: 0, ..StatusBlock::new() });
        let w = unsafe { StatusWriter::from_raw(&mut *blk) };
        w.init(48000, 128, 10, 20, 4321);
        w.set_running(true);
        w.on_block([0.5, 0.25], [1.0, 0.125]);
        w.on_block([0.1, 0.2], [0.3, 0.4]);
        w.set_xruns(3);
        assert_eq!(unsafe { core::ptr::read_volatile(&blk.running) }, 1);
        drop(w);
        assert!(blk.is_valid());
        assert_eq!((blk.sample_rate, blk.buffer_size, blk.in_latency, blk.out_latency), (48000, 128, 10, 20));
        assert_eq!(blk.callbacks, 2);
        assert_eq!(blk.xruns, 3);
        assert_eq!((blk.peak_in_l, blk.peak_in_r, blk.peak_out_l, blk.peak_out_r), (0.1, 0.2, 0.3, 0.4));
        assert_eq!(blk.pid, 4321);
        assert_eq!(blk.running, 0);
    }

    #[test]
    fn real_mapping_second_opener_sees_data() {
        let Some(w) = StatusWriter::create_mapped() else { return };
        w.init(44100, 64, 1, 2, 7);
        let w2 = StatusWriter::create_mapped().expect("open existing");
        // SAFETY: valid mapped view for the lifetime of w2.
        let rate = unsafe { core::ptr::read_volatile(&(*w2.base).sample_rate) };
        assert_eq!(rate, 44100);
    }
}
