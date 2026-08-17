//! Wire protocol codec for QuantyDB.

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
