//! The syscall boundary.
//!
//! Every unsafe line in this workspace is in this crate, and every one of
//! them is a call into the platform. Nothing else here is allowed any, so
//! "is this safe?" is a question about one crate rather than about
//! thirteen.
//!
//! There is no `libc` dependency. The symbols are declared here and
//! resolved against the C library the standard library already links, so
//! the dependency count stays at zero (ADR-020).

#![deny(missing_docs)]
#![deny(unsafe_code)]

pub mod lock;
pub mod random;

// Windows asks the same question through SetConsoleCtrlHandler, which is
// a different shape entirely, and `quanty serve` does not run there yet.
#[cfg(unix)]
pub mod signal;

#[cfg(target_os = "linux")]
pub mod linux;

// One attribute, one item. The Windows job found three places where this
// was written above the wrong line, so it is worth saying again.
#[cfg(target_os = "macos")]
pub mod bsd;
