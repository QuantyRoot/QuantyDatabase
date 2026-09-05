//! Reactor and connection handling for QuantyDB.

#![deny(unsafe_code)]
#![deny(missing_docs)]

/// The syscall boundary moved out to a crate of its own, so the unsafe
/// lines live in one place for every platform rather than one per
/// reactor.
#[cfg(target_os = "linux")]
use quanty_sys::linux as sys;

pub mod conn;

#[cfg(target_os = "linux")]
pub mod listener;

#[cfg(target_os = "linux")]
pub mod poll;

#[cfg(target_os = "linux")]
pub mod dispatch;

#[cfg(target_os = "linux")]
pub mod registry;

#[cfg(target_os = "linux")]
pub mod worker;

#[cfg(target_os = "linux")]
pub use dispatch::{ConnId, Dispatch, Idle, Job, Outbox, Reply};
// An attribute covers one item, and this one used to sit above the line
// before it, so the crate did not compile off Linux at all.
#[cfg(target_os = "linux")]
pub use poll::{Event, Interest, Poller, Token, Waker, WAKE_TOKEN};

pub use conn::{Conn, Step};
#[cfg(target_os = "linux")]
pub use listener::bind_reuseport;
#[cfg(target_os = "linux")]
pub use worker::{Turn, Worker};
