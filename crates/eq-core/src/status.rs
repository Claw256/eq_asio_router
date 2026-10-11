//! Shared-memory status block layout (driver writes, GUI reads).
//!
//! Layout only; mapping the memory is done by the driver/GUI via Win32.

/// Name of the file mapping used to share the block.
pub const STATUS_MAPPING_NAME: &str = "Local\\eq_asio_router_status";
/// Magic value ("EQST" little endian).
pub const STATUS_MAGIC: u32 = u32::from_le_bytes(*b"EQST");
/// Layout version.
pub const STATUS_VERSION: u32 = 2;

/// Plain-old-data status block, exactly 128 bytes.
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
    pub _reserved: [u8; 20],
}

const _: () = assert!(std::mem::size_of::<StatusBlock>() == 128);
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
            _reserved: [0; 20],
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
        assert_eq!(size_of::<StatusBlock>(), 128);
        assert_eq!(offset_of!(StatusBlock, callbacks), 24);
        assert_eq!(offset_of!(StatusBlock, xruns), 32);
        assert_eq!(offset_of!(StatusBlock, peak_in_l), 40);
        assert_eq!(offset_of!(StatusBlock, running), 56);
        assert_eq!(offset_of!(StatusBlock, pid), 60);
        assert_eq!(offset_of!(StatusBlock, fault_state), 64);
        assert_eq!(offset_of!(StatusBlock, ring_fill_frames), 76);
        assert_eq!(offset_of!(StatusBlock, safety_buffer_us), 100);
        assert_eq!(offset_of!(StatusBlock, dropped_logs), 104);
        assert_eq!(offset_of!(StatusBlock, _reserved), 108);
    }

    #[test]
    fn defaults() {
        let s = StatusBlock::default();
        assert!(s.is_valid());
        assert_eq!(STATUS_MAPPING_NAME, "Local\\eq_asio_router_status");
    }
}
