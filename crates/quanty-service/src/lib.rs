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
//! **Everything crosses this one thread, and that is the price.** A long
//! statement delays the next one on every other connection. ADR-016 forbids
//! optimizing that away before a measurement says what it costs, and the
//! measurement that would settle it needs more than one core.
//!
//! What did get fixed is the sharper version of it: a connection holding an
//! open transaction used to make every other connection wait, reads
//! included. Reads go past now, because a read commits nothing and so
//! cannot invalidate the parked batch (ADR-029).
//!
//! Group commit is here, measured into existence by ADR-028: a turn of
//! statements shares one transaction, one write and one fsync, each with a
//! savepoint of its own so one failure does not take the rest down.
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
