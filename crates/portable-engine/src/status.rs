//! [`StatusTransport`] implementations for the portable engine: a no-op transport
//! and an in-process, atomics-backed snapshot transport.
//!
//! Both are RT-safe on the `publish_block`/`set_xruns` path (relaxed atomics only,
//! docs/architecture.md §3), matching the shm `StatusWriter`'s contract so the same
//! call sites work against any transport.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};

use router_core::status_writer::{BlockReport, BridgeReport, StaticInfo};

use crate::seams::StatusTransport;

/// No-op transport: every call is discarded. The default for headless runs and tests
/// that do not assert on status.
#[derive(Clone, Copy, Debug, Default)]
pub struct NullTransport;

impl StatusTransport for NullTransport {
    fn init(&self, _info: &StaticInfo) {}
    fn set_running(&self, _on: bool) {}
    fn set_xruns(&self, _n: u64) {}
    fn publish_block(&self, _r: &BlockReport) {}
    fn set_bridge(&self, _r: &BridgeReport) {}
}

/// In-process snapshot transport: keeps the latest status in relaxed atomics so a
/// test (or the CLI's status line) can read it without shared memory or locks.
/// Wrap in `Arc` and hand out as `Arc<dyn StatusTransport>`.
#[derive(Default)]
pub struct InProcTransport {
    running: AtomicBool,
    xruns: AtomicU64,
    blocks: AtomicU64,
    // StaticInfo (u32 fields).
    sample_rate: AtomicU32,
    buffer_size: AtomicU32,
    in_latency: AtomicU32,
    out_latency: AtomicU32,
    pid: AtomicU32,
    effective_rate: AtomicU32,
    capture_period_us: AtomicU32,
    safety_buffer_us: AtomicU32,
    // BlockReport (peaks as f32 bits).
    peak_in: [AtomicU32; 2],
    peak_out: [AtomicU32; 2],
    fault_state: AtomicU32,
    non_finite: AtomicU32,
    ring_fill_frames: AtomicU32,
    underruns: AtomicU32,
    overruns: AtomicU32,
    max_callback_us: AtomicU32,
    avg_callback_us: AtomicU32,
    // BridgeReport.
    bridge: BridgeFields,
}

#[derive(Default)]
struct BridgeFields {
    attached: AtomicU32,
    vbmatrix_running: AtomicU32,
    sample_rate: AtomicU32,
    buffer_frames: AtomicU32,
    fill_a: AtomicU32,
    fill_b: AtomicU32,
    a_overruns: AtomicU32,
    a_discarded: AtomicU32,
    b_underruns: AtomicU32,
    b_dropped: AtomicU32,
}

impl InProcTransport {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn running(&self) -> bool {
        self.running.load(Relaxed)
    }

    pub fn xruns(&self) -> u64 {
        self.xruns.load(Relaxed)
    }

    /// Number of published blocks.
    pub fn blocks(&self) -> u64 {
        self.blocks.load(Relaxed)
    }

    /// Last published block report.
    pub fn block_report(&self) -> BlockReport {
        BlockReport {
            peak_in: [f32::from_bits(self.peak_in[0].load(Relaxed)), f32::from_bits(self.peak_in[1].load(Relaxed))],
            peak_out: [f32::from_bits(self.peak_out[0].load(Relaxed)), f32::from_bits(self.peak_out[1].load(Relaxed))],
            fault_state: self.fault_state.load(Relaxed),
            non_finite: self.non_finite.load(Relaxed),
            ring_fill_frames: self.ring_fill_frames.load(Relaxed),
            underruns: self.underruns.load(Relaxed),
            overruns: self.overruns.load(Relaxed),
            max_callback_us: self.max_callback_us.load(Relaxed),
            avg_callback_us: self.avg_callback_us.load(Relaxed),
        }
    }

    /// Last published bridge report.
    pub fn bridge_report(&self) -> BridgeReport {
        BridgeReport {
            attached: self.bridge.attached.load(Relaxed),
            vbmatrix_running: self.bridge.vbmatrix_running.load(Relaxed),
            sample_rate: self.bridge.sample_rate.load(Relaxed),
            buffer_frames: self.bridge.buffer_frames.load(Relaxed),
            fill_a: self.bridge.fill_a.load(Relaxed),
            fill_b: self.bridge.fill_b.load(Relaxed),
            a_overruns: self.bridge.a_overruns.load(Relaxed),
            a_discarded: self.bridge.a_discarded.load(Relaxed),
            b_underruns: self.bridge.b_underruns.load(Relaxed),
            b_dropped: self.bridge.b_dropped.load(Relaxed),
        }
    }

    /// Static info published by `init`.
    pub fn static_info(&self) -> StaticInfo {
        StaticInfo {
            sample_rate: self.sample_rate.load(Relaxed),
            buffer_size: self.buffer_size.load(Relaxed),
            in_latency: self.in_latency.load(Relaxed),
            out_latency: self.out_latency.load(Relaxed),
            pid: self.pid.load(Relaxed),
            effective_rate: self.effective_rate.load(Relaxed),
            capture_period_us: self.capture_period_us.load(Relaxed),
            safety_buffer_us: self.safety_buffer_us.load(Relaxed),
        }
    }
}

impl StatusTransport for InProcTransport {
    fn init(&self, info: &StaticInfo) {
        self.sample_rate.store(info.sample_rate, Relaxed);
        self.buffer_size.store(info.buffer_size, Relaxed);
        self.in_latency.store(info.in_latency, Relaxed);
        self.out_latency.store(info.out_latency, Relaxed);
        self.pid.store(info.pid, Relaxed);
        self.effective_rate.store(info.effective_rate, Relaxed);
        self.capture_period_us.store(info.capture_period_us, Relaxed);
        self.safety_buffer_us.store(info.safety_buffer_us, Relaxed);
    }

    fn set_running(&self, on: bool) {
        self.running.store(on, Relaxed);
    }

    fn set_xruns(&self, n: u64) {
        self.xruns.store(n, Relaxed);
    }

    fn publish_block(&self, r: &BlockReport) {
        self.peak_in[0].store(r.peak_in[0].to_bits(), Relaxed);
        self.peak_in[1].store(r.peak_in[1].to_bits(), Relaxed);
        self.peak_out[0].store(r.peak_out[0].to_bits(), Relaxed);
        self.peak_out[1].store(r.peak_out[1].to_bits(), Relaxed);
        self.fault_state.store(r.fault_state, Relaxed);
        self.non_finite.store(r.non_finite, Relaxed);
        self.ring_fill_frames.store(r.ring_fill_frames, Relaxed);
        self.underruns.store(r.underruns, Relaxed);
        self.overruns.store(r.overruns, Relaxed);
        self.max_callback_us.store(r.max_callback_us, Relaxed);
        self.avg_callback_us.store(r.avg_callback_us, Relaxed);
        self.blocks.fetch_add(1, Relaxed);
    }

    fn set_bridge(&self, r: &BridgeReport) {
        self.bridge.attached.store(r.attached, Relaxed);
        self.bridge.vbmatrix_running.store(r.vbmatrix_running, Relaxed);
        self.bridge.sample_rate.store(r.sample_rate, Relaxed);
        self.bridge.buffer_frames.store(r.buffer_frames, Relaxed);
        self.bridge.fill_a.store(r.fill_a, Relaxed);
        self.bridge.fill_b.store(r.fill_b, Relaxed);
        self.bridge.a_overruns.store(r.a_overruns, Relaxed);
        self.bridge.a_discarded.store(r.a_discarded, Relaxed);
        self.bridge.b_underruns.store(r.b_underruns, Relaxed);
        self.bridge.b_dropped.store(r.b_dropped, Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_proc_roundtrip() {
        let t = InProcTransport::new();
        t.init(&StaticInfo { sample_rate: 48000, buffer_size: 512, pid: 7, ..StaticInfo::default() });
        assert_eq!(t.static_info().sample_rate, 48000);
        assert_eq!(t.static_info().pid, 7);
        t.set_running(true);
        t.set_xruns(3);
        let b = BlockReport { peak_in: [0.5, 0.25], underruns: 2, avg_callback_us: 300, ..BlockReport::default() };
        t.publish_block(&b);
        let got = t.block_report();
        assert_eq!(got.peak_in[0], 0.5);
        assert_eq!(got.underruns, 2);
        assert_eq!(got.avg_callback_us, 300);
        assert_eq!(t.blocks(), 1);
        assert!(t.running());
        assert_eq!(t.xruns(), 3);
        let br = BridgeReport { attached: 1, fill_a: 42, ..BridgeReport::default() };
        t.set_bridge(&br);
        assert_eq!(t.bridge_report().fill_a, 42);
    }

    #[test]
    fn null_transport_is_inert() {
        let t = NullTransport;
        t.init(&StaticInfo::default());
        t.set_running(true);
        t.set_xruns(1);
        t.publish_block(&BlockReport::default());
        t.set_bridge(&BridgeReport::default());
    }
}
