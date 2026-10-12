//! Shared-memory status block layout (driver writes, GUI reads).
//!
//! Layout only; mapping the memory is done by the driver/GUI via Win32.

/// Name of the file mapping used to share the block.
pub const STATUS_MAPPING_NAME: &str = "Local\\eq_asio_router_status";
/// Magic value ("EQST" little endian).
pub const STATUS_MAGIC: u32 = u32::from_le_bytes(*b"EQST");
/// Layout version.
/// v2: fault/effective-rate/fill/latency fields.
/// v3: grew to 192 bytes, added the VBMatrix bridge block (`bridge_attached` .. `b_dropped`).
/// Old readers reject v3 (`is_valid`) and old v2 mappings are rejected by v3 readers.
pub const STATUS_VERSION: u32 = 3;

/// Plain-old-data status block, exactly 192 bytes (3 cache lines).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StatusBlock {
    pub magic: u32,
    pub version: u32,
    pub sample_rate: u32,
    pub buffer_size: u32,
    pub in_latency: u32,
    pub out_latency: u32,
    pub callbacks: u64,
    pub xruns: u64,
    pub peak_in_l: f32,
    pub peak_in_r: f32,
    pub peak_out_l: f32,
    pub peak_out_r: f32,
    /// Non-zero while the driver is streaming.
    pub running: u32,
    pub pid: u32,
    /// v2: 0 = Ok, 1 = Warn, 2 = Muted (see `dsp::FaultState`).
    pub fault_state: u32,
    /// v2: sample rate actually in use (Hz).
    pub effective_rate: u32,
    /// v2: non-finite samples sanitised so far.
    pub non_finite: u32,
    /// v2: current ring fill in frames.
    pub ring_fill_frames: u32,
    /// v2: ring underruns.
    pub underruns: u32,
    /// v2: ring overruns.
    pub overruns: u32,
    /// v2: worst callback duration in microseconds.
    pub max_callback_us: u32,
    /// v2: average callback duration in microseconds.
    pub avg_callback_us: u32,
    /// v2: capture period in microseconds.
    pub capture_period_us: u32,
    /// v2: effective safety buffer in microseconds.
    pub safety_buffer_us: u32,
    /// v2: log lines dropped by the logger ring.
    pub dropped_logs: u32,
    /// v3: the engine is attached to the bridge mapping.
    pub bridge_attached: u32,
    /// v3: the driver slot is running and its heartbeat is fresh (`driver_running` + beat).
    pub vbmatrix_running: u32,
    /// v3: sample rate negotiated by the driver with VBMatrix (Hz).
    pub bridge_sample_rate: u32,
    /// v3: ASIO buffer size negotiated by the driver (frames).
    pub bridge_buffer_frames: u32,
    /// v3: frames currently in ring A (source, driver -> app).
    pub fill_a: u32,
    /// v3: frames currently in ring B (loopback, app -> driver).
    pub fill_b: u32,
    /// v3: frames dropped by the driver's `ring_a_push` (ring A full).
    pub a_overruns: u32,
    /// v3: frames discarded by the app's consumer-side latency trim.
    pub a_discarded: u32,
    /// v3: frames the driver's `ring_b_pop` could not provide (fed as silence).
    pub b_underruns: u32,
    /// v3: frames dropped by the app's `ring_b_push` (ring B full).
    pub b_dropped: u32,
    pub _reserved: [u8; 44],
}

const _: () = assert!(std::mem::size_of::<StatusBlock>() == 192);
const _: () = assert!(std::mem::align_of::<StatusBlock>() == 8);

impl StatusBlock {
    /// Zeroed block with magic and version set.
    pub const fn new() -> Self {
        StatusBlock {
            magic: STATUS_MAGIC,
            version: STATUS_VERSION,
            sample_rate: 0,
            buffer_size: 0,
            in_latency: 0,
            out_latency: 0,
            callbacks: 0,
            xruns: 0,
            peak_in_l: 0.0,
            peak_in_r: 0.0,
            peak_out_l: 0.0,
            peak_out_r: 0.0,
            running: 0,
            pid: 0,
            fault_state: 0,
            effective_rate: 0,
            non_finite: 0,
            ring_fill_frames: 0,
            underruns: 0,
            overruns: 0,
            max_callback_us: 0,
            avg_callback_us: 0,
            capture_period_us: 0,
            safety_buffer_us: 0,
            dropped_logs: 0,
            bridge_attached: 0,
            vbmatrix_running: 0,
            bridge_sample_rate: 0,
            bridge_buffer_frames: 0,
            fill_a: 0,
            fill_b: 0,
            a_overruns: 0,
            a_discarded: 0,
            b_underruns: 0,
            b_dropped: 0,
            _reserved: [0; 44],
        }
    }

    /// True if magic and version match this build.
    pub fn is_valid(&self) -> bool {
        self.magic == STATUS_MAGIC && self.version == STATUS_VERSION
    }
}

impl Default for StatusBlock {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    #[test]
    fn layout_is_stable() {
        assert_eq!(size_of::<StatusBlock>(), 192);
        assert_eq!(offset_of!(StatusBlock, callbacks), 24);
        assert_eq!(offset_of!(StatusBlock, xruns), 32);
        assert_eq!(offset_of!(StatusBlock, peak_in_l), 40);
        assert_eq!(offset_of!(StatusBlock, running), 56);
        assert_eq!(offset_of!(StatusBlock, pid), 60);
        assert_eq!(offset_of!(StatusBlock, fault_state), 64);
        assert_eq!(offset_of!(StatusBlock, ring_fill_frames), 76);
        assert_eq!(offset_of!(StatusBlock, safety_buffer_us), 100);
        assert_eq!(offset_of!(StatusBlock, dropped_logs), 104);
        assert_eq!(offset_of!(StatusBlock, bridge_attached), 108);
        assert_eq!(offset_of!(StatusBlock, vbmatrix_running), 112);
        assert_eq!(offset_of!(StatusBlock, bridge_sample_rate), 116);
        assert_eq!(offset_of!(StatusBlock, bridge_buffer_frames), 120);
        assert_eq!(offset_of!(StatusBlock, fill_a), 124);
        assert_eq!(offset_of!(StatusBlock, fill_b), 128);
        assert_eq!(offset_of!(StatusBlock, a_overruns), 132);
        assert_eq!(offset_of!(StatusBlock, a_discarded), 136);
        assert_eq!(offset_of!(StatusBlock, b_underruns), 140);
        assert_eq!(offset_of!(StatusBlock, b_dropped), 144);
        assert_eq!(offset_of!(StatusBlock, _reserved), 148);
    }

    #[test]
    fn defaults() {
        let s = StatusBlock::default();
        assert!(s.is_valid());
        assert_eq!(STATUS_MAPPING_NAME, "Local\\eq_asio_router_status");
    }
}
