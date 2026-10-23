//! Thin operating-system adapters.
//!
//! The adapters own threads, native hooks and clocks; all smoothing decisions
//! live in the platform-independent engine.

use crate::cli::RunOptions;

/// Process exit codes other tooling relies on.
pub const EXIT_OK: i32 = 0;
pub const EXIT_FAILURE: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_PERMISSION: i32 = 3;

#[cfg(windows)]
pub mod windows;

#[cfg(target_os = "macos")]
pub mod macos;

/// Runs the smoother until Ctrl+C, the requested duration or an emergency stop.
#[cfg(windows)]
pub fn run(options: &RunOptions) -> i32 {
    windows::run(options)
}

#[cfg(target_os = "macos")]
pub fn run(options: &RunOptions) -> i32 {
    macos::run(options)
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn run(_options: &RunOptions) -> i32 {
    eprintln!("cursor-smooth: smoothing is implemented for Windows and macOS only");
    EXIT_FAILURE
}

/// Reports platform readiness without installing a suppressing hook.
#[cfg(windows)]
pub fn doctor() -> i32 {
    windows::doctor()
}

#[cfg(target_os = "macos")]
pub fn doctor() -> i32 {
    macos::doctor()
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn doctor() -> i32 {
    println!("os: unsupported");
    println!("cursor-smooth implements Windows and macOS. Core engines are still testable.");
    EXIT_FAILURE
}
