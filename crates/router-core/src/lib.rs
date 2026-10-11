//! Shared building blocks of the standalone router engine: SPSC frame ring,
//! drift-compensating resampler, EQ processor with fault handling, config
//! watcher, status shared memory, ASIO sample conversion, non-blocking logger
//! and a watchdog.
//!
//! Everything marked "audio thread safe" is lock-free and allocation-free.

pub mod analyze;
pub mod arrival;
pub mod asio_convert;
pub mod logger;
pub mod processor;
pub mod resample;
pub mod ring;
pub mod status_writer;
pub mod watchdog;
pub mod watcher;
pub mod wav;
