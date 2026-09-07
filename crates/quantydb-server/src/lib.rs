//! Reactor and connection handling for QuantyDB.
//!
//! The reactor is built where there is a readiness kernel to build it on,
//! which is Linux and macOS today. Windows compiles the parts that do not
//! touch a descriptor and waits for IOCP (ADR-037). The condition is
//! spelled out on every item rather than hidden behind an alias, because
//! an attribute binds to one item and the last time that was forgotten
//! the whole crate quietly left the Windows build.

#![deny(unsafe_code)]
#![deny(missing_docs)]

/// The syscall boundary moved out to a crate of its own, so the unsafe
/// lines live in one place for every platform rather than one per
/// reactor.
#[cfg(target_os = "linux")]
use quantydb_sys::linux as sys;

#[cfg(target_os = "macos")]
use quantydb_sys::bsd as sys;

pub mod conn;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod listener;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod poll;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod dispatch;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod registry;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub mod worker;

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use dispatch::{ConnId, Dispatch, Idle, Job, Outbox, Reply};
// An attribute covers one item, and this one used to sit above the line
// before it, so the crate did not compile off Linux at all.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use poll::{Event, Interest, Poller, Token, Waker, WAKE_TOKEN};

pub use conn::{Conn, Step};
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use listener::bind_reuseport;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub use worker::{Turn, Worker};
