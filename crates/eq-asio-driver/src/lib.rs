//! "EQ ASIO Router" v2: a **virtual ASIO bridge** (COM in-proc server) loaded
//! by an ASIO host such as VBMatrix. The driver hosts nothing and runs no DSP:
//! it shuttles samples through `bridge-abi` shared memory only — outputs 0/1
//! go to ring A (towards the EQ Router app), ring B (post-EQ loopback from the
//! app) feeds inputs 0/1, everything else is silence/discarded.
//!
//! Pacing: the driver has no hardware, so [`pacing`] fires the host's
//! `bufferSwitch` callbacks on a QPC-anchored absolute schedule (high-resolution
//! waitable timer + MMCSS "Pro Audio").
//!
//! CLSID: `{D9568DC8-2E04-429B-ABD0-03852E2931DF}` (see [`ids`]).
//! Registration: `HKLM\SOFTWARE\ASIO\EQ ASIO Router` (see [`com`]).

pub mod com;
pub mod convert;
mod driver;
pub mod ids;
pub mod log;
pub mod pacing;

pub use com::{DllCanUnloadNow, DllGetClassObject, DllRegisterServer, DllUnregisterServer};
