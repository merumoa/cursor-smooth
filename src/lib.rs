//! CursorSmooth: bounded 1000 Hz software cursor smoothing.
//!
//! The crate is split into a platform-independent engine (`core`), the shared
//! fail-open runtime (`runtime`) and thin OS adapters (`platform`).

pub mod cli;
pub mod core;
pub mod platform;
pub mod runtime;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
