//! Wire protocol codec for QuantyDB.
//!
//! Bytes only. This crate encodes and decodes the protocol described in
//! docs/PROTOCOL.md and does no I/O, opens no sockets and starts no
//! threads. That split is deliberate: ADR-022 settled the server on threads
//! and the standard library, and none of the decisions here depend on that
//! answer, so the format can be specified, tested and fuzzed before a
//! single connection exists.
//!
//! The invariant the fuzzer exists to defend: **arbitrary bytes must
//! decode to an `Err`, never to a panic.** Every decode path goes through
//! `codec::Reader`, which is bounds-checked in one place, and every length
//! read from the wire is checked against what remains before anything is
//! allocated for it.
//!
//! No dependencies outside the workspace, see ADR-020.

pub mod codec;
pub mod error;
pub mod frame;
pub mod message;
pub mod value;

pub use error::{ErrorCode, ProtoError, Result};
pub use frame::{
    frame, negotiate, ClientHello, FrameHeader, Refusal, ServerHello, CLIENT_HELLO_LEN, HEADER_LEN,
    MAGIC, MAX_BODY, SERVER_HELLO_LEN, VERSION,
};
pub use message::{batch_rows, ClientMessage, ServerMessage};
pub use value::{read_value, write_value};
