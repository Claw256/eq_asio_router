//! Shared-memory status block (`Local\eq_asio_router_status`, `eq_core::StatusBlock` v2).
//!
//! [`StatusWriter`]: the engine creates the mapping once (control path) and the audio
//! thread then only does relaxed atomic stores into the mapped view: no allocation,
//! syscalls or locks. [`StatusReader`]: read-only open for the GUI.
//!
//! If the mapping cannot be created the writer is simply absent (`Option`) and the engine
//! runs without publishing.

use core::mem::offset_of;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};

use eq_core::{StatusBlock, STATUS_MAGIC, STATUS_MAPPING_NAME, STATUS_VERSION};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_READ, FILE_MAP_WRITE,
    MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};

const SIZE: usize = core::mem::size_of::<StatusBlock>();

fn wide(name: &str) -> Vec<u16> {
    name.encode_utf16().chain(Some(0)).collect()
}

/// Static fields written once by [`StatusWriter::init`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StaticInfo {
    pub sample_rate: u32,
    pub buffer_size: u32,
    pub in_latency: u32,
    pub out_latency: u32,
    pub pid: u32,
    /// Rate actually in use (Hz).
    pub effective_rate: u32,
    pub capture_period_us: u32,
    pub safety_buffer_us: u32,
}

/// Per-callback figures published by [`StatusWriter::publish_block`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BlockReport {
    /// Pre-EQ input peaks (L, R) of this block.
    pub peak_in: [f32; 2],
    /// Post-EQ output peaks (L, R) of this block.
    pub peak_out: [f32; 2],
    /// `FaultState::as_u32()`.
    pub fault_state: u32,
    pub non_finite: u32,
    pub ring_fill_frames: u32,
    pub underruns: u32,
    pub overruns: u32,
    pub max_callback_us: u32,
    pub avg_callback_us: u32,
}

/// Writer side.
pub struct StatusWriter {
    base: *mut StatusBlock,
    /// Present when we own a Win32 mapping + view.
    mapping: Option<HANDLE>,
}

// SAFETY: the block is only touched through atomics.
unsafe impl Send for StatusWriter {}
unsafe impl Sync for StatusWriter {}

impl StatusWriter {
    /// Creates (or opens) the standard named mapping. `None` if Win32 refuses.
    pub fn create_mapped() -> Option<Self> {
        Self::create_named(STATUS_MAPPING_NAME)
    }

    /// Creates (or opens) a mapping with a custom name (tests).
    pub fn create_named(name: &str) -> Option<Self> {
        let name = wide(name);
        // SAFETY: plain Win32 calls with valid arguments; handles are closed in Drop.
        unsafe {
            let h = CreateFileMappingW(INVALID_HANDLE_VALUE, None, PAGE_READWRITE, 0, SIZE as u32, PCWSTR(name.as_ptr()))
                .ok()?;
            let view = MapViewOfFile(h, FILE_MAP_WRITE, 0, 0, SIZE);
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

    /// Resets every field and fills the static ones; the magic is stored last so readers
    /// never see a half-initialised block. Control path.
    pub fn init(&self, info: &StaticInfo) {
        self.a32(offset_of!(StatusBlock, magic)).store(0, Relaxed);
        self.a32(offset_of!(StatusBlock, version)).store(STATUS_VERSION, Relaxed);
        self.a32(offset_of!(StatusBlock, sample_rate)).store(info.sample_rate, Relaxed);
        self.a32(offset_of!(StatusBlock, buffer_size)).store(info.buffer_size, Relaxed);
        self.a32(offset_of!(StatusBlock, in_latency)).store(info.in_latency, Relaxed);
        self.a32(offset_of!(StatusBlock, out_latency)).store(info.out_latency, Relaxed);
        self.a64(offset_of!(StatusBlock, callbacks)).store(0, Relaxed);
        self.a64(offset_of!(StatusBlock, xruns)).store(0, Relaxed);
        for off in [
            offset_of!(StatusBlock, peak_in_l),
            offset_of!(StatusBlock, peak_in_r),
            offset_of!(StatusBlock, peak_out_l),
            offset_of!(StatusBlock, peak_out_r),
            offset_of!(StatusBlock, running),
            offset_of!(StatusBlock, fault_state),
            offset_of!(StatusBlock, non_finite),
            offset_of!(StatusBlock, ring_fill_frames),
            offset_of!(StatusBlock, underruns),
            offset_of!(StatusBlock, overruns),
            offset_of!(StatusBlock, max_callback_us),
            offset_of!(StatusBlock, avg_callback_us),
            offset_of!(StatusBlock, dropped_logs),
        ] {
            self.a32(off).store(0, Relaxed);
        }
        self.a32(offset_of!(StatusBlock, pid)).store(info.pid, Relaxed);
        self.a32(offset_of!(StatusBlock, effective_rate)).store(info.effective_rate, Relaxed);
        self.a32(offset_of!(StatusBlock, capture_period_us)).store(info.capture_period_us, Relaxed);
        self.a32(offset_of!(StatusBlock, safety_buffer_us)).store(info.safety_buffer_us, Relaxed);
        self.a32(offset_of!(StatusBlock, magic)).store(STATUS_MAGIC, Relaxed);
    }

    pub fn set_running(&self, on: bool) {
        self.a32(offset_of!(StatusBlock, running)).store(on as u32, Relaxed);
    }
    pub fn set_xruns(&self, n: u64) {
        self.a64(offset_of!(StatusBlock, xruns)).store(n, Relaxed);
    }
    pub fn set_effective_rate(&self, hz: u32) {
        self.a32(offset_of!(StatusBlock, effective_rate)).store(hz, Relaxed);
    }
    pub fn set_capture_period_us(&self, us: u32) {
        self.a32(offset_of!(StatusBlock, capture_period_us)).store(us, Relaxed);
    }
    pub fn set_safety_buffer_us(&self, us: u32) {
        self.a32(offset_of!(StatusBlock, safety_buffer_us)).store(us, Relaxed);
    }
    /// Mirror of `Logger::dropped()` (saturating to u32).
    pub fn set_dropped_logs(&self, n: u64) {
        self.a32(offset_of!(StatusBlock, dropped_logs)).store(n.min(u64::from(u32::MAX)) as u32, Relaxed);
    }

    /// Audio-thread update: bumps `callbacks` and stores this block's figures
    /// (readers see the latest block, no decay). Relaxed atomic stores only.
    #[inline]
    pub fn publish_block(&self, r: &BlockReport) {
        let c = self.a64(offset_of!(StatusBlock, callbacks));
        c.store(c.load(Relaxed).wrapping_add(1), Relaxed);
        self.a32(offset_of!(StatusBlock, peak_in_l)).store(r.peak_in[0].to_bits(), Relaxed);
        self.a32(offset_of!(StatusBlock, peak_in_r)).store(r.peak_in[1].to_bits(), Relaxed);
        self.a32(offset_of!(StatusBlock, peak_out_l)).store(r.peak_out[0].to_bits(), Relaxed);
        self.a32(offset_of!(StatusBlock, peak_out_r)).store(r.peak_out[1].to_bits(), Relaxed);
        self.a32(offset_of!(StatusBlock, fault_state)).store(r.fault_state, Relaxed);
        self.a32(offset_of!(StatusBlock, non_finite)).store(r.non_finite, Relaxed);
        self.a32(offset_of!(StatusBlock, ring_fill_frames)).store(r.ring_fill_frames, Relaxed);
        self.a32(offset_of!(StatusBlock, underruns)).store(r.underruns, Relaxed);
        self.a32(offset_of!(StatusBlock, overruns)).store(r.overruns, Relaxed);
        self.a32(offset_of!(StatusBlock, max_callback_us)).store(r.max_callback_us, Relaxed);
        self.a32(offset_of!(StatusBlock, avg_callback_us)).store(r.avg_callback_us, Relaxed);
    }
}

impl Drop for StatusWriter {
    fn drop(&mut self) {
        self.set_running(false);
        if let Some(h) = self.mapping.take() {
            // SAFETY: view and handle were created in `create_named`.
            unsafe {
                let _ = UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.base as *mut _ });
                let _ = CloseHandle(h);
            }
        }
    }
}

/// Read-only side for the GUI.
pub struct StatusReader {
    base: *const StatusBlock,
    mapping: Option<HANDLE>,
}

// SAFETY: reads go through atomic loads of plain data.
unsafe impl Send for StatusReader {}
unsafe impl Sync for StatusReader {}

impl StatusReader {
    /// Opens the standard mapping read-only; `None` if the engine is not running.
    pub fn open() -> Option<Self> {
        Self::open_named(STATUS_MAPPING_NAME)
    }

    /// Opens a mapping by name.
    pub fn open_named(name: &str) -> Option<Self> {
        let name = wide(name);
        // SAFETY: plain Win32 calls; the handle and view are released in Drop.
        unsafe {
            let h = OpenFileMappingW(FILE_MAP_READ.0, false, PCWSTR(name.as_ptr())).ok()?;
            let view = MapViewOfFile(h, FILE_MAP_READ, 0, 0, SIZE);
            if view.Value.is_null() {
                let _ = CloseHandle(h);
                return None;
            }
            Some(StatusReader { base: view.Value as *const StatusBlock, mapping: Some(h) })
        }
    }

    /// Wraps caller-owned memory (tests).
    ///
    /// # Safety
    /// `p` must be 8-byte aligned, valid for a `StatusBlock` and outlive the reader.
    pub unsafe fn from_raw(p: *const StatusBlock) -> Self {
        StatusReader { base: p, mapping: None }
    }

    /// Snapshot of the block, or `None` if magic/version do not match. Fields are loaded
    /// individually (a block being updated may mix two adjacent callbacks, which is fine
    /// for display).
    pub fn read(&self) -> Option<StatusBlock> {
        // SAFETY: the view is SIZE bytes, 8-byte aligned; copying as 16 atomic u64 loads.
        let words: [u64; SIZE / 8] = core::array::from_fn(|i| unsafe {
            AtomicU64::from_ptr((self.base as *mut u64).add(i)).load(Relaxed)
        });
        // SAFETY: StatusBlock is plain old data (integers/floats, any bit pattern valid).
        let b: StatusBlock = unsafe { core::mem::transmute::<[u64; SIZE / 8], StatusBlock>(words) };
        b.is_valid().then_some(b)
    }
}

impl Drop for StatusReader {
    fn drop(&mut self) {
        if let Some(h) = self.mapping.take() {
            // SAFETY: created in `open_named`.
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

    fn info() -> StaticInfo {
        StaticInfo {
            sample_rate: 48000,
            buffer_size: 512,
            in_latency: 728,
            out_latency: 871,
            pid: 4321,
            effective_rate: 48000,
            capture_period_us: 10_000,
            safety_buffer_us: 2_000,
        }
    }

    #[test]
    fn writes_into_local_block() {
        let mut blk = Box::new(StatusBlock { magic: 0, version: 0, ..StatusBlock::new() });
        let w = unsafe { StatusWriter::from_raw(&mut *blk) };
        w.init(&info());
        w.set_running(true);
        let rep = BlockReport {
            peak_in: [0.5, 0.25],
            peak_out: [1.0, 0.125],
            fault_state: 1,
            non_finite: 7,
            ring_fill_frames: 1088,
            underruns: 2,
            overruns: 3,
            max_callback_us: 900,
            avg_callback_us: 120,
        };
        w.publish_block(&rep);
        w.publish_block(&BlockReport { peak_in: [0.1, 0.2], peak_out: [0.3, 0.4], ..rep });
        w.set_xruns(3);
        w.set_dropped_logs(u64::MAX);
        w.set_capture_period_us(5000);
        w.set_safety_buffer_us(4000);
        w.set_effective_rate(44100);
        assert_eq!(unsafe { core::ptr::read_volatile(&blk.running) }, 1);
        drop(w);
        assert!(blk.is_valid());
        assert_eq!((blk.sample_rate, blk.buffer_size, blk.in_latency, blk.out_latency), (48000, 512, 728, 871));
        assert_eq!(blk.callbacks, 2);
        assert_eq!(blk.xruns, 3);
        assert_eq!((blk.peak_in_l, blk.peak_in_r, blk.peak_out_l, blk.peak_out_r), (0.1, 0.2, 0.3, 0.4));
        assert_eq!(blk.pid, 4321);
        assert_eq!(blk.running, 0, "drop clears running");
        assert_eq!((blk.fault_state, blk.non_finite, blk.ring_fill_frames), (1, 7, 1088));
        assert_eq!((blk.underruns, blk.overruns, blk.max_callback_us, blk.avg_callback_us), (2, 3, 900, 120));
        assert_eq!((blk.capture_period_us, blk.safety_buffer_us, blk.effective_rate), (5000, 4000, 44100));
        assert_eq!(blk.dropped_logs, u32::MAX);
    }

    #[test]
    fn init_resets_previous_run() {
        let mut blk = Box::new(StatusBlock::new());
        let w = unsafe { StatusWriter::from_raw(&mut *blk) };
        w.init(&info());
        w.publish_block(&BlockReport { underruns: 9, fault_state: 2, ..BlockReport::default() });
        w.init(&info());
        let r = unsafe { StatusReader::from_raw(&*blk) };
        let b = r.read().unwrap();
        assert_eq!((b.underruns, b.fault_state, b.callbacks), (0, 0, 0));
    }

    #[test]
    fn reader_validates_magic() {
        let mut blk = Box::new(StatusBlock { magic: 0, ..StatusBlock::new() });
        let r = unsafe { StatusReader::from_raw(&*blk) };
        assert!(r.read().is_none());
        let w = unsafe { StatusWriter::from_raw(&mut *blk) };
        w.init(&info());
        let b = r.read().expect("valid after init");
        assert_eq!(b.buffer_size, 512);
        assert_eq!(b.safety_buffer_us, 2000);
    }

    #[test]
    fn real_mapping_reader_sees_writer() {
        let name = format!("Local\\router_core_test_status_{}", std::process::id());
        assert!(StatusReader::open_named(&name).is_none(), "no mapping yet");
        let w = StatusWriter::create_named(&name).expect("create mapping");
        w.init(&info());
        w.publish_block(&BlockReport { ring_fill_frames: 777, ..BlockReport::default() });
        let r = StatusReader::open_named(&name).expect("open existing");
        let b = r.read().unwrap();
        assert_eq!((b.sample_rate, b.ring_fill_frames, b.callbacks), (48000, 777, 1));
        w.publish_block(&BlockReport { ring_fill_frames: 778, ..BlockReport::default() });
        assert_eq!(r.read().unwrap().ring_fill_frames, 778);
        drop(w);
        // reader keeps the mapping alive; running was cleared by the writer's drop
        assert_eq!(r.read().unwrap().running, 0);
    }
}
