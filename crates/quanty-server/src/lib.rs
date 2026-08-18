//! Reactor and connection handling for QuantyDB.

#![deny(unsafe_code)]
#![deny(missing_docs)]

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod sys;

pub mod conn;

#[cfg(target_os = "linux")]
pub mod listener;

#[cfg(target_os = "linux")]
pub mod poll;

#[cfg(target_os = "linux")]
pub mod registry;

#[cfg(target_os = "linux")]
pub mod worker;

#[cfg(target_os = "linux")]
pub use poll::{Event, Interest, Poller, Token, Waker, WAKE_TOKEN};

pub use conn::{Conn, Service, Step};
#[cfg(target_os = "linux")]
pub use listener::bind_reuseport;
#[cfg(target_os = "linux")]
pub use worker::{Idle, Turn, Worker};
