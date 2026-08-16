//! Wire protocol codec for QuantyDB.
//!
//! Bytes only. This crate encodes and decodes the protocol described in
//! docs/PROTOCOL.md and does no I/O, opens no sockets and starts no
//! threads. That split is deliberate: none of the decisions here depend on
//! how the server is built, so the format can be specified, tested and
//! fuzzed before a single connection exists.
//!
//! Three invariants, each defended by a test rather than by assertion:
//!
//! - **No panics.** Arbitrary bytes decode to an `Err`. Every decode path
//!   goes through `codec::Reader`, bounds-checked in one place.
//! - **Bounded memory.** Memory a decoder can be made to commit is bounded
//!   by a constant, not by a multiple of the input. Counts are capped by
//!   `limits`, because an element costs more memory than wire and any
//!   uncapped count hands the sender that ratio as a multiplier.
//!   tests/allocation.rs measures this rather than trusting it.
//! - **Canonical encoding.** One message has exactly one encoding, so
//!   `encode . decode . encode == encode`. tests/proto_fuzz.rs checks it on
//!   every input it accepts.
//!
//! No dependencies outside the workspace, see ADR-020, and no unsafe.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod codec;
pub mod error;
pub mod frame;
pub mod limits;
pub mod message;
pub mod value;

pub use error::{ErrorCode, ProtoError, Result};
pub use frame::{
    frame, negotiate, ClientHello, FrameHeader, Refusal, ServerHello, CLIENT_HELLO_LEN, HEADER_LEN,
    MAGIC, SERVER_HELLO_LEN, VERSION,
};
pub use limits::{MAX_BODY, MAX_LINES, MAX_ROWS_PER_BATCH, MAX_VALUES_PER_ROW};
pub use message::{batch_rows, ClientMessage, RowBatcher, ServerMessage};
pub use value::{encoded_row_len, encoded_value_len, read_value, write_value};
