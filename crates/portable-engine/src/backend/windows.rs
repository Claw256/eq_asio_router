//! Windows output backend — **STUB**. Phase C wires `asio-host` here
//! (`AsioDriver` + `Handler::buffer_switch`, `asio-host/src/lib.rs:141-146`).
//!
//! Every entry point is `unimplemented!()` on purpose: the seam types exist so the
//! shared skeleton compiles against them, but no Windows stream is opened until
//! Phase C. Do not fill this in during Phase B (A6 ownership).

use crate::seams::{OutputCallback, OutputSink, SinkError, StreamRequest, StreamSpec};

/// Master output over the Windows ASIO host. Stub until Phase C.
pub struct WindowsSink {
    #[allow(dead_code)] // read by `OutputSink::spec` once Phase C constructs the sink
    spec: StreamSpec,
}

impl WindowsSink {
    /// TODO(Phase C): open `asio_host::AsioDriver`, negotiate buffers
    /// (`logic::choose_buffer_size` semantics), keep the driver alive here.
    pub fn open(_want: &StreamRequest) -> Result<WindowsSink, SinkError> {
        unimplemented!("TODO(Phase C): wire asio-host")
    }
}

/// The callback type the Windows sink drives; TODO(Phase C): wrap the portable
/// `PipelineCallback` in an `asio_host::Handler` (`buffer_switch` half-buffer loop).
pub struct WindowsCallback;

impl OutputCallback for WindowsCallback {
    fn render(&mut self, _out: &mut [&mut [f32]], _now_ticks: i64) {
        unimplemented!("TODO(Phase C): wire asio-host")
    }
}

impl OutputSink for WindowsSink {
    type Callback = WindowsCallback;

    fn negotiate(&mut self, _want: &StreamRequest) -> Result<StreamSpec, SinkError> {
        unimplemented!("TODO(Phase C): wire asio-host")
    }

    fn start(&mut self, _cb: Self::Callback) -> Result<(), SinkError> {
        unimplemented!("TODO(Phase C): wire asio-host")
    }

    fn stop(&mut self) -> Result<(), SinkError> {
        unimplemented!("TODO(Phase C): wire asio-host")
    }

    fn spec(&self) -> &StreamSpec {
        &self.spec
    }
}
