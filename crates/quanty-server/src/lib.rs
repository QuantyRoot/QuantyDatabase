//! Reactor and connection handling for QuantyDB.
//!
//! ADR-023 settled the shape: N worker threads, each with its own epoll
//! instance and its own listening socket via `SO_REUSEPORT`, each owning a
//! disjoint set of connections. Nothing is shared between workers, so there
//! is no work stealing and no cross-thread wakeup on the common path.
//!
//! **Unsafe lives in exactly one module and the compiler holds it there.**
//! This crate denies `unsafe_code` and `sys` is the single `allow`, so
//! introducing unsafe anywhere else means deleting a line that says what is
//! being done. That is the whole of what ADR-023 bought: a boundary six
//! functions wide, with everything above it in safe Rust.
//!
//! `epoll` is Linux. ADR-023 keeps thread per connection as the portable
//! fallback and as the differential test partner, since two implementations
//! behind one interface make a divergence a bug by definition.
//!
//! No dependencies outside the workspace, see ADR-020.

#![deny(unsafe_code)]
#![deny(missing_docs)]

#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
mod sys;

#[cfg(target_os = "linux")]
pub mod poll;

#[cfg(target_os = "linux")]
pub use poll::{Event, Interest, Poller, Token, Waker, WAKE_TOKEN};
