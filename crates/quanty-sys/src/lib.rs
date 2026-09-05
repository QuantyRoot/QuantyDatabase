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

#[cfg(target_os = "linux")]
pub mod linux;
