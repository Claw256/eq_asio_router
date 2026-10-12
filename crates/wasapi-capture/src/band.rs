//! Band-limited click detector DSP (moved to `router_core::band` so `router-engine` needs no
//! WASAPI dependency). Re-exported here so `wasapi_capture::band::*` paths keep working.

pub use router_core::band::*;
