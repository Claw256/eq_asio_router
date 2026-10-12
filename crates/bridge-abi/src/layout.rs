//! POD shared-memory layout: the [`Header`] plus the two stereo ring regions.
//!
//! Nothing in this module calls the OS; the mapping itself lives in
//! [`crate::Mapping`]. The header is the wire contract: x86 and x64 see the
//! same bytes (all fixed-size types, `#[repr(C)]`, 64-byte aligned hot lines).

use core::mem::{align_of, size_of};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Session-local mapping name; both sides pass the same fixed size.
pub const MAPPING_NAME: &str = "Local\\eq_asio_router_bridge_v2";

/// `"EQRIDGE2"` — reject any other image.
pub const MAGIC: u64 = 0x4551_4252_4944_4732;

/// Layout version of this header/ring image.
pub const VERSION: u32 = 1;

/// ASIO channels exposed by the bridge (in = out); the rings carry the
/// engine pair (channels 0/1) only.
pub const CHANNELS: u32 = 8;

/// Frames per ring (power of two) — 128 KiB of interleaved stereo `f32` each.
pub const RING_FRAMES: usize = 16_384;
/// Index mask (`RING_FRAMES - 1`); positions are `u64` and wrap with `wrapping_*`.
pub const RING_MASK: u64 = (RING_FRAMES as u64) - 1;
/// Bytes of one ring: `RING_FRAMES` interleaved stereo `f32` frames.
pub const RING_BYTES: usize = RING_FRAMES * 2 * size_of::<f32>();
/// The header occupies one whole page so the ring data starts page-aligned.
pub const HEADER_SIZE: usize = 4096;
/// Total mapping size — fixed so 32-bit and 64-bit processes agree.
pub const TOTAL_SIZE: usize = HEADER_SIZE + 2 * RING_BYTES;

/// Shared-memory header. Hot atomics sit on their own 64-byte cache lines:
/// the driver writes line 1 (`driver_beat`, `a_write`, `b_read`) while the app
/// writes line 2 (`app_beat`, `b_write`, `a_read`), so the two processes never
/// false-share.
///
/// Writers per the plan: `magic`/`version`/`channels`/`qpc_freq` = creator,
/// `sample_rate`/`buffer_frames`/`driver_running`/`driver_beat` = driver,
/// `app_beat` = app, ring indices as noted per field.
#[repr(C, align(64))]
pub struct Header {
    // ---- cold line (creator / negotiated values) -------------------------
    /// [`MAGIC`]; stored **last** by the creator, read with `Acquire`.
    pub magic: u64,
    /// [`VERSION`].
    pub version: u32,
    /// [`CHANNELS`].
    pub channels: u32,
    /// Negotiated ASIO sample rate (driver; plain single-writer `f64`).
    pub sample_rate: f64,
    /// Negotiated ASIO buffer size in frames (driver).
    pub buffer_frames: u32,
    /// `1` between `start()` and `stop()` (driver).
    pub driver_running: AtomicU32,
    /// QPC frequency of the creator, for time conversions (Hz).
    pub qpc_freq: i64,
    _pad0: [u8; 24],

    // ---- driver-hot line --------------------------------------------------
    /// +1 per driver callback (driver heartbeat).
    pub driver_beat: AtomicU64,
    /// Ring A write index in frames (driver, producer).
    pub a_write: AtomicU64,
    /// Ring B read index in frames (driver, consumer).
    pub b_read: AtomicU64,
    _pad1: [u8; 40],

    // ---- app-hot line -----------------------------------------------------
    /// +1 per app control tick (100 ms) (app).
    pub app_beat: AtomicU64,
    /// Ring B write index in frames (app, producer).
    pub b_write: AtomicU64,
    /// Ring A read index in frames (app, consumer).
    pub a_read: AtomicU64,
    _pad2: [u8; 40],

    // ---- diagnostics ------------------------------------------------------
    /// Frames dropped by `ring_a_push` (driver).
    pub a_overruns: AtomicU64,
    /// Frames discarded by `ring_a_discard` (app).
    pub a_discarded: AtomicU64,
    /// Frames `ring_b_pop` could not provide (driver; fed as silence).
    pub b_underruns: AtomicU64,
    /// Frames dropped by `ring_b_push` (app).
    pub b_dropped: AtomicU64,
    _pad3: [u8; 32],
}

/// The complete mapping image (header page + both rings). Fields are laid out
/// exactly as mapped; construct one only by mapping memory ([`crate::Mapping`]).
#[repr(C, align(64))]
pub struct Layout {
    pub header: Header,
    _header_pad: [u8; HEADER_SIZE - size_of::<Header>()],
    /// Ring A storage, interleaved `[l0, r0, l1, r1, ...]`.
    pub ring_a: [f32; RING_FRAMES * 2],
    /// Ring B storage, interleaved `[l0, r0, l1, r1, ...]`.
    pub ring_b: [f32; RING_FRAMES * 2],
}

const _: () = {
    assert!(size_of::<Header>() == 256, "header must be exactly 4 cache lines");
    assert!(align_of::<Header>() == 64, "header must be cache-line aligned");
    assert!(size_of::<Layout>() == TOTAL_SIZE, "layout must match the fixed mapping size");
};

/// Why a mapping image was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidateError {
    /// `magic` did not match [`MAGIC`] (or the header was still initialising).
    Magic { found: u64 },
    /// `version` did not match [`VERSION`].
    Version { found: u32 },
    /// `channels` did not match [`CHANNELS`].
    Channels { found: u32 },
}

impl core::fmt::Display for ValidateError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ValidateError::Magic { found } => write!(f, "bad bridge magic {found:#018x}"),
            ValidateError::Version { found } => write!(f, "unsupported bridge version {found}"),
            ValidateError::Channels { found } => write!(f, "unexpected channel count {found}"),
        }
    }
}

impl std::error::Error for ValidateError {}

impl Header {
    /// Checks `magic` (acquire), `version` and `channels`.
    ///
    /// The creator stores `magic` last with `Release`, so a passing validate
    /// guarantees every other field is fully published.
    pub fn validate(&self) -> Result<(), ValidateError> {
        // SAFETY: the header lives in aligned shared memory; the load is a
        // read-only atomic access through the raw pointer (same pattern as
        // `eq_asio_driver::status`). AtomicU64 has the layout of u64.
        let magic = unsafe { AtomicU64::from_ptr(core::ptr::addr_of!(self.magic) as *mut u64) }
            .load(Ordering::Acquire);
        if magic != MAGIC {
            return Err(ValidateError::Magic { found: magic });
        }
        if self.version != VERSION {
            return Err(ValidateError::Version { found: self.version });
        }
        if self.channels != CHANNELS {
            return Err(ValidateError::Channels { found: self.channels });
        }
        Ok(())
    }

    // -- negotiated values (driver writes, volatile: single writer) --------

    /// Publishes the negotiated sample rate. Driver side only.
    pub fn set_sample_rate(&self, rate: f64) {
        // SAFETY: single writer, aligned f64 in mapped memory.
        unsafe { core::ptr::write_volatile(core::ptr::addr_of!(self.sample_rate) as *mut f64, rate) };
    }

    /// Last rate published by [`Self::set_sample_rate`].
    pub fn get_sample_rate(&self) -> f64 {
        // SAFETY: as above.
        unsafe { core::ptr::read_volatile(core::ptr::addr_of!(self.sample_rate)) }
    }

    /// Publishes the negotiated buffer size in frames. Driver side only.
    pub fn set_buffer_frames(&self, frames: u32) {
        // SAFETY: single writer, aligned u32 in mapped memory.
        unsafe { core::ptr::write_volatile(core::ptr::addr_of!(self.buffer_frames) as *mut u32, frames) };
    }

    /// Last buffer size published by [`Self::set_buffer_frames`].
    pub fn get_buffer_frames(&self) -> u32 {
        // SAFETY: as above.
        unsafe { core::ptr::read_volatile(core::ptr::addr_of!(self.buffer_frames)) }
    }

    // -- counters -----------------------------------------------------------

    /// Frames currently in ring A (what the app will consume).
    pub fn fill_a(&self) -> u64 {
        let w = self.a_write.load(Ordering::Acquire);
        let r = self.a_read.load(Ordering::Acquire);
        w.wrapping_sub(r).min(RING_FRAMES as u64)
    }

    /// Frames currently in ring B (what the driver will consume).
    pub fn fill_b(&self) -> u64 {
        let w = self.b_write.load(Ordering::Acquire);
        let r = self.b_read.load(Ordering::Acquire);
        w.wrapping_sub(r).min(RING_FRAMES as u64)
    }

    /// Total ring capacity in frames (both rings).
    pub fn capacity(&self) -> u64 {
        RING_FRAMES as u64
    }

    /// Frames dropped by [`Self::ring_a_push`] (ring A full).
    pub fn a_overruns(&self) -> u64 {
        self.a_overruns.load(Ordering::Relaxed)
    }

    /// Frames dropped by [`Self::ring_a_discard`].
    pub fn a_discarded(&self) -> u64 {
        self.a_discarded.load(Ordering::Relaxed)
    }

    /// Frames [`Self::ring_b_pop`] could not provide.
    pub fn b_underruns(&self) -> u64 {
        self.b_underruns.load(Ordering::Relaxed)
    }

    /// Frames dropped by [`Self::ring_b_push`] (ring B full).
    pub fn b_dropped(&self) -> u64 {
        self.b_dropped.load(Ordering::Relaxed)
    }

    // -- rings --------------------------------------------------------------

    /// Ring A producer (driver): pushes `min(l, r)` frames; the excess *new*
    /// frames are dropped and counted into `a_overruns`. Wait-free.
    ///
    /// # Safety
    /// `self` must be the header of a mapping image (or a [`Layout`]) that
    /// outlives the call; only one producer may call this at a time.
    pub unsafe fn ring_a_push(&self, l: &[f32], r: &[f32]) -> usize {
        push(
            self.ring_a_ptr(),
            &self.a_write,
            &self.a_read,
            l,
            r,
            &self.a_overruns,
        )
    }

    /// Ring A consumer (app): pops up to `min(l, r)` frames; returns how many
    /// were stored. Wait-free.
    ///
    /// # Safety
    /// As [`Self::ring_a_push`]; only one consumer at a time.
    pub unsafe fn ring_a_pop(&self, l: &mut [f32], r: &mut [f32]) -> usize {
        pop(self.ring_a_ptr(), &self.a_write, &self.a_read, l, r, None)
    }

    /// Ring A consumer (app): drops the `n` oldest frames (or all if fewer) to
    /// trim latency; returns the count, also accumulated into `a_discarded`.
    ///
    /// # Safety
    /// As [`Self::ring_a_push`]; only one consumer at a time.
    pub unsafe fn ring_a_discard(&self, n: usize) -> usize {
        discard(&self.a_write, &self.a_read, &self.a_discarded, n)
    }

    /// Ring B producer (app): like [`Self::ring_a_push`] but counted into
    /// `b_dropped` when full.
    ///
    /// # Safety
    /// `self` must be a mapping header; only one producer at a time.
    pub unsafe fn ring_b_push(&self, l: &[f32], r: &[f32]) -> usize {
        push(
            self.ring_b_ptr(),
            &self.b_write,
            &self.b_read,
            l,
            r,
            &self.b_dropped,
        )
    }

    /// Ring B consumer (driver): like [`Self::ring_a_pop`], but the shortfall
    /// of a short pop is accumulated into `b_underruns` (the caller feeds
    /// silence for those frames).
    ///
    /// # Safety
    /// As [`Self::ring_a_push`]; only one consumer at a time.
    pub unsafe fn ring_b_pop(&self, l: &mut [f32], r: &mut [f32]) -> usize {
        pop(
            self.ring_b_ptr(),
            &self.b_write,
            &self.b_read,
            l,
            r,
            Some(&self.b_underruns),
        )
    }

    // -- internal -----------------------------------------------------------

    /// Ring A base: interleaved stereo `f32` right after the header page.
    #[inline]
    fn ring_a_ptr(&self) -> *mut f32 {
        (self as *const Header as *mut u8).wrapping_add(HEADER_SIZE) as *mut f32
    }

    /// Ring B base: right after ring A.
    #[inline]
    fn ring_b_ptr(&self) -> *mut f32 {
        (self as *const Header as *mut u8).wrapping_add(HEADER_SIZE + RING_BYTES) as *mut f32
    }
}

/// Producer half of one ring. `dropped` counts frames that did not fit.
///
/// # Safety
/// `data` must address `RING_FRAMES * 2` `f32`s; the index pair must belong
/// exclusively to this ring's producer/consumer.
#[inline]
unsafe fn push(
    data: *mut f32,
    write: &AtomicU64,
    read: &AtomicU64,
    l: &[f32],
    r: &[f32],
    dropped: &AtomicU64,
) -> usize {
    let n = l.len().min(r.len());
    let w = write.load(Ordering::Relaxed);
    let h = read.load(Ordering::Acquire);
    let used = w.wrapping_sub(h).min(RING_FRAMES as u64);
    let free = RING_FRAMES as u64 - used;
    let k = n.min(free as usize);
    for i in 0..k {
        let idx = (w.wrapping_add(i as u64) & RING_MASK) as usize;
        // SAFETY: idx < RING_FRAMES by the mask; i < k <= n <= l.len()/r.len().
        unsafe {
            *data.add(idx * 2) = l[i];
            *data.add(idx * 2 + 1) = r[i];
        }
    }
    // Release: the frame stores become visible before the index does.
    write.store(w.wrapping_add(k as u64), Ordering::Release);
    if k < n {
        dropped.fetch_add((n - k) as u64, Ordering::Relaxed);
    }
    k
}

/// Consumer half of one ring. `shortfall` (if given) counts requested-but-missing
/// frames.
///
/// # Safety
/// As [`push`].
#[inline]
unsafe fn pop(
    data: *const f32,
    write: &AtomicU64,
    read: &AtomicU64,
    l: &mut [f32],
    r: &mut [f32],
    shortfall: Option<&AtomicU64>,
) -> usize {
    let n = l.len().min(r.len());
    let h = read.load(Ordering::Relaxed);
    let w = write.load(Ordering::Acquire);
    let avail = w.wrapping_sub(h).min(RING_FRAMES as u64);
    let k = n.min(avail as usize);
    for i in 0..k {
        let idx = (h.wrapping_add(i as u64) & RING_MASK) as usize;
        // SAFETY: idx < RING_FRAMES by the mask; i < k <= n.
        unsafe {
            l[i] = *data.add(idx * 2);
            r[i] = *data.add(idx * 2 + 1);
        }
    }
    // Release: consumed frames are freed before the index advances.
    read.store(h.wrapping_add(k as u64), Ordering::Release);
    if k < n {
        if let Some(c) = shortfall {
            c.fetch_add((n - k) as u64, Ordering::Relaxed);
        }
    }
    k
}

/// Consumer-side latency trim: drops `n` oldest frames, counting them.
///
/// # Safety
/// Only the ring's consumer may call this concurrently with [`pop`].
#[inline]
unsafe fn discard(write: &AtomicU64, read: &AtomicU64, counter: &AtomicU64, n: usize) -> usize {
    let h = read.load(Ordering::Relaxed);
    let w = write.load(Ordering::Acquire);
    let avail = w.wrapping_sub(h).min(RING_FRAMES as u64);
    let k = (n as u64).min(avail) as usize;
    read.store(h.wrapping_add(k as u64), Ordering::Release);
    if k > 0 {
        counter.fetch_add(k as u64, Ordering::Relaxed);
    }
    k
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::offset_of;

    /// Allocates a zeroed [`Layout`] without touching the OS mapping layer.
    fn alloc_layout() -> *mut Layout {
        let raw = unsafe {
            let a = std::alloc::alloc(std::alloc::Layout::from_size_align(TOTAL_SIZE, 64).unwrap());
            core::ptr::write_bytes(a, 0, TOTAL_SIZE);
            a
        };
        raw as *mut Layout
    }

    unsafe fn free_layout(p: *mut Layout) {
        std::alloc::dealloc(p as *mut u8, std::alloc::Layout::from_size_align(TOTAL_SIZE, 64).unwrap());
    }

    /// Seeds a freshly allocated image the way a creator would.
    unsafe fn init(p: *mut Layout) {
        let h = p as *mut Header;
        (*h).version = VERSION;
        (*h).channels = CHANNELS;
        (*h).qpc_freq = 10_000_000;
        // magic last, release — same publication order as Mapping::init_header.
        AtomicU64::from_ptr(core::ptr::addr_of_mut!((*h).magic)).store(MAGIC, Ordering::Release);
    }

    #[test]
    fn offsets_and_sizes_are_frozen() {
        // The app-side team builds against these offsets; changing any of them
        // breaks the shared-memory contract.
        assert_eq!(offset_of!(Header, magic), 0);
        assert_eq!(offset_of!(Header, version), 8);
        assert_eq!(offset_of!(Header, channels), 12);
        assert_eq!(offset_of!(Header, sample_rate), 16);
        assert_eq!(offset_of!(Header, buffer_frames), 24);
        assert_eq!(offset_of!(Header, driver_running), 28);
        assert_eq!(offset_of!(Header, qpc_freq), 32);
        assert_eq!(offset_of!(Header, driver_beat), 64);
        assert_eq!(offset_of!(Header, a_write), 72);
        assert_eq!(offset_of!(Header, b_read), 80);
        assert_eq!(offset_of!(Header, app_beat), 128);
        assert_eq!(offset_of!(Header, b_write), 136);
        assert_eq!(offset_of!(Header, a_read), 144);
        assert_eq!(offset_of!(Header, a_overruns), 192);
        assert_eq!(offset_of!(Header, a_discarded), 200);
        assert_eq!(offset_of!(Header, b_underruns), 208);
        assert_eq!(offset_of!(Header, b_dropped), 216);
        assert_eq!(size_of::<Header>(), 256);
        assert_eq!(offset_of!(Layout, ring_a), HEADER_SIZE);
        assert_eq!(offset_of!(Layout, ring_b), HEADER_SIZE + RING_BYTES);
        assert_eq!(size_of::<Layout>(), TOTAL_SIZE);
        assert_eq!(TOTAL_SIZE, 4096 + 2 * 131_072);
        assert_eq!(RING_FRAMES * size_of::<f32>() * 2, 128 * 1024);
    }

    #[test]
    fn validate_accepts_fresh_and_rejects_corruption() {
        let p = alloc_layout();
        unsafe {
            // untouched: magic 0
            assert_eq!((*p).header.validate(), Err(ValidateError::Magic { found: 0 }));
            init(p);
            assert_eq!((*p).header.validate(), Ok(()));
            (*p).header.version = 99;
            assert_eq!((*p).header.validate(), Err(ValidateError::Version { found: 99 }));
            (*p).header.version = VERSION;
            (*p).header.channels = 2;
            assert_eq!((*p).header.validate(), Err(ValidateError::Channels { found: 2 }));
            free_layout(p);
        }
    }

    #[test]
    fn rings_roundtrip_and_share_storage_with_layout() {
        let p = alloc_layout();
        unsafe {
            init(p);
            let h = &(*p).header;
            let l: Vec<f32> = (0..300).map(|i| i as f32 * 0.5).collect();
            let r: Vec<f32> = (0..300).map(|i| -(i as f32)).collect();

            assert_eq!(h.ring_a_push(&l, &r), 300);
            assert_eq!(h.fill_a(), 300);
            // The push went through the header pointer into Layout::ring_a.
            assert_eq!((*p).ring_a[0], 0.0);
            assert_eq!((*p).ring_a[598], 299.0 * 0.5); // frame 299 L
            assert_eq!((*p).ring_a[1], 0.0);
            assert_eq!((*p).ring_a[599], -299.0); // frame 299 R

            let mut ol = vec![0.0f32; 300];
            let mut or_ = vec![0.0f32; 300];
            assert_eq!(h.ring_a_pop(&mut ol, &mut or_), 300);
            assert_eq!(ol, l);
            assert_eq!(or_, r);
            assert_eq!(h.fill_a(), 0);

            // ring B in the other direction
            assert_eq!(h.ring_b_push(&l, &r), 300);
            assert_eq!(h.fill_b(), 300);
            let mut ol = vec![0.0f32; 300];
            let mut or_ = vec![0.0f32; 300];
            assert_eq!(h.ring_b_pop(&mut ol, &mut or_), 300);
            assert_eq!(ol, l);
            assert_eq!((*p).ring_b[599], -299.0); // frame 299 R of ring B
            assert_eq!(h.fill_b(), 0);
            assert_eq!(h.a_overruns(), 0);
            assert_eq!(h.b_underruns(), 0);
            free_layout(p);
        }
    }

    #[test]
    fn overrun_drops_new_and_counts_frames() {
        let p = alloc_layout();
        unsafe {
            init(p);
            let h = &(*p).header;
            let block = vec![1.0f32; 8192];
            // two full pushes fill the ring; the third must drop entirely.
            assert_eq!(h.ring_a_push(&block, &block), 8192);
            assert_eq!(h.ring_a_push(&block, &block), 8192);
            assert_eq!(h.ring_a_push(&block, &block), 0);
            assert_eq!(h.a_overruns(), 8192);
            assert_eq!(h.fill_a(), RING_FRAMES as u64);
            // free 100 frames, then a 200-frame push stores 100, drops 100
            let mut ol = vec![0.0f32; 100];
            let mut orr = vec![0.0f32; 100];
            assert_eq!(h.ring_a_pop(&mut ol, &mut orr), 100);
            let small = vec![2.0f32; 200];
            assert_eq!(h.ring_a_push(&small, &small), 100);
            assert_eq!(h.a_overruns(), 8192 + 100);
            free_layout(p);
        }
    }

    #[test]
    fn short_pop_counts_underrun_and_leaves_silence_to_caller() {
        let p = alloc_layout();
        unsafe {
            init(p);
            let h = &(*p).header;
            let l = vec![0.25f32; 100];
            assert_eq!(h.ring_b_push(&l, &l), 100);
            let mut ol = vec![7.0f32; 256];
            let mut or_ = vec![7.0f32; 256];
            assert_eq!(h.ring_b_pop(&mut ol, &mut or_), 100);
            assert_eq!(h.b_underruns(), 156);
            assert_eq!(ol[0], 0.25);
            assert_eq!(ol[100], 7.0); // untouched tail — caller silences it
            free_layout(p);
        }
    }

    #[test]
    fn discard_trims_oldest_and_counts() {
        let p = alloc_layout();
        unsafe {
            init(p);
            let h = &(*p).header;
            let l: Vec<f32> = (0..500).map(|i| i as f32).collect();
            assert_eq!(h.ring_a_push(&l, &l), 500);
            assert_eq!(h.ring_a_discard(200), 200);
            assert_eq!(h.a_discarded(), 200);
            assert_eq!(h.fill_a(), 300);
            let mut ol = vec![0.0f32; 300];
            let mut or_ = vec![0.0f32; 300];
            assert_eq!(h.ring_a_pop(&mut ol, &mut or_), 300);
            assert_eq!(ol[0], 200.0); // oldest 200 are gone
            assert_eq!(h.ring_a_discard(10), 0);
            free_layout(p);
        }
    }

    #[test]
    fn indices_wrap_like_u64_without_breaking_the_ring() {
        let p = alloc_layout();
        unsafe {
            init(p);
            let h = &(*p).header;
            // Pretend both indices are just below u64::MAX (a wrapping edge).
            let base = u64::MAX - 3;
            h.a_write.store(base, Ordering::Release);
            h.a_read.store(base, Ordering::Release);
            let l = [3.5f32; 8];
            assert_eq!(h.ring_a_push(&l, &l), 8);
            assert_eq!(h.fill_a(), 8);
            let mut ol = [0.0f32; 8];
            let mut or_ = [0.0f32; 8];
            assert_eq!(h.ring_a_pop(&mut ol, &mut or_), 8);
            assert_eq!(ol, l);
            assert_eq!(h.fill_a(), 0);
            // indices wrapped past u64::MAX
            assert!(h.a_write.load(Ordering::Acquire) < 16);
            free_layout(p);
        }
    }

    #[test]
    fn negotiated_accessors_roundtrip() {
        let p = alloc_layout();
        unsafe {
            init(p);
            let h = &(*p).header;
            h.set_sample_rate(176_400.0);
            h.set_buffer_frames(512);
            assert_eq!(h.get_sample_rate(), 176_400.0);
            assert_eq!(h.get_buffer_frames(), 512);
            assert_eq!((*p).header.get_sample_rate(), 176_400.0);
            free_layout(p);
        }
    }
}
