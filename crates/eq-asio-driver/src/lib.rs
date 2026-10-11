//! "EQ ASIO Router": a virtual ASIO driver (COM in-proc server) that hosts a
//! hardware master ASIO driver (Yamaha by default), lets the ASIO host (e.g. the
//! VB-Audio ASIO Bridge) fill its outputs 0/1 inside the master's callback,
//! runs them through a [`engine::Processor`] and writes them to the master.
//!
//! CLSID: `{D9568DC8-2E04-429B-ABD0-03852E2931DF}` (see [`ids`]).

pub mod com;
pub mod config;
pub mod convert;
mod driver;
pub mod engine;
pub mod ids;
pub mod log;
pub mod processor;
pub mod status;
pub mod watcher;

pub use com::{DllCanUnloadNow, DllGetClassObject, DllRegisterServer, DllUnregisterServer};
