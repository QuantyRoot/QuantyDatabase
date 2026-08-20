//! What answers a protocol request: one thread, one session, one queue.
//!
//! ADR-003 gives the database one writer and ADR-021 makes an open
//! transaction a suspended write batch. ADR-024 says what a server does
//! with that: statements go to a queue served by one thread, and a
//! connection waiting on it is parked rather than blocking its worker.
//! ADR-027 records the two things that follow from the code as it stands.
//!
//! **One session, transactions parked per connection.** `Session::execute`
//! takes `&mut self` and `Db::gc` takes `&mut Db` on purpose, so a session
//! cannot be shared and a database behind an `Arc` would lose `gc`
//! entirely. So there is one session, and the per-connection part of it,
//! the open transaction, is parked in and out around each statement.
//!
//! **Reads serialize too, and that is the price.** Every statement crosses
//! this thread, so a long read delays the next statement on every other
//! connection. ADR-016 forbids optimizing that away before a measurement
//! says how much it costs, and no such measurement exists yet.
//!
//! Group commit is the same story from the other side. The shape ADR-024
//! wants is here, a queue drained by one writer, but batching several
//! statements into one write and one fsync is a change to `drain` that
//! belongs with the number that justifies it.
//!
//! This crate is where the reactor meets the engine: `quanty-server` knows
//! nothing about statements and `quanty-exec` knows nothing about sockets.

#![deny(unsafe_code)]
#![deny(missing_docs)]

mod answer;

#[cfg(target_os = "linux")]
mod executor;

#[cfg(target_os = "linux")]
pub use executor::{Deadlines, Executor, Handle, Stats, BUSY_TIMEOUT, IDLE_IN_TXN, MAX_BATCH};
