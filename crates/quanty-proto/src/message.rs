//! The messages themselves.

use quanty_core::Value;

use crate::codec::{prealloc, Reader, Writer};
use crate::error::{ErrorCode, ProtoError, Result};
use crate::frame::frame;
use crate::limits::{
    MAX_BODY, MAX_LINES, MAX_ROWS_PER_BATCH, MAX_VALUES_PER_ROW, MIN_ROW_LEN, MIN_TEXT_LEN,
};
use crate::value::{encoded_row_len, read_row, write_row};

/// Opaque token, client to server.
pub const T_AUTH: u8 = 0x10;
/// One QQL statement.
pub const T_QUERY: u8 = 0x11;
/// One SQL statement.
pub const T_QUERY_SQL: u8 = 0x12;
/// Orderly shutdown.
pub const T_CLOSE: u8 = 0x13;

/// Auth accepted, or none required.
pub const T_READY: u8 = 0x20;
/// Statement produced no rows.
pub const T_OK: u8 = 0x21;
/// A verb and a number.
pub const T_COUNT: u8 = 0x22;
/// Column names, opening a result.
pub const T_ROWS_BEGIN: u8 = 0x23;
/// A chunk of rows.
pub const T_ROW_BATCH: u8 = 0x24;
/// End of a result.
pub const T_ROWS_END: u8 = 0x25;
/// A list of strings.
pub const T_LINES: u8 = 0x26;
/// A code and a message.
pub const T_ERROR: u8 = 0x27;

/// What a client may send.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientMessage {
    /// An opaque token. What it means, where it is stored and how it is
    Auth(Vec<u8>),
    /// One QQL statement.
    Query(String),
    /// One SQL statement.
    QuerySql(String),
    /// Close the connection.
    Close,
}

/// What a server may send.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerMessage {
    /// Authenticated, or no authentication required.
    Ready,
    /// The statement succeeded and produced nothing.
    Ok,
    /// The statement affected `n` rows.
    Count {
        /// The verb to render, e.g. "put".
        verb: String,
        /// How many rows.
        n: u64,
    },
    /// A result set opens with its column names.
    RowsBegin {
        /// One name per column.
        columns: Vec<String>,
    },
    /// One frame's worth of rows.
    RowBatch {
        /// The rows in this batch, never empty in a well-formed sequence.
        rows: Vec<Vec<Value>>,
    },
    /// The result set is complete.
    RowsEnd,
    /// A list of lines, for statements that render text.
    Lines(Vec<String>),
    /// The statement failed.
    Error {
        /// Stable code, see `ErrorCode`.
        code: u16,
        /// Human-readable detail, not a contract.
        message: String,
    },
}

impl ServerMessage {
    /// Whether this message ends a request.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            ServerMessage::Ok
                | ServerMessage::Count { .. }
                | ServerMessage::RowsEnd
                | ServerMessage::Lines(_)
                | ServerMessage::Error { .. }
        )
    }

    /// Build an error message from a stable code.
    pub fn error(code: ErrorCode, message: impl Into<String>) -> Self {
        ServerMessage::Error {
            code: code.as_u16(),
            message: message.into(),
        }
    }
}

impl ClientMessage {
    /// The type byte this message travels under.
    pub fn msg_type(&self) -> u8 {
        match self {
            ClientMessage::Auth(_) => T_AUTH,
            ClientMessage::Query(_) => T_QUERY,
            ClientMessage::QuerySql(_) => T_QUERY_SQL,
            ClientMessage::Close => T_CLOSE,
        }
    }

    /// Encode the body, without the frame header.
    pub fn body(&self) -> Result<Vec<u8>> {
        let mut w = Writer::new();
        match self {
            ClientMessage::Auth(t) => w.bytes(t),
            ClientMessage::Query(s) | ClientMessage::QuerySql(s) => w.text(s),
            ClientMessage::Close => {}
        }
        w.finish()
    }

    /// Encode a complete frame.
    pub fn encode(&self) -> Result<Vec<u8>> {
        frame(self.msg_type(), &self.body()?)
    }

    /// Decode a body that arrived under `msg_type`.
    pub fn decode(msg_type: u8, body: &[u8]) -> Result<Self> {
        let mut r = Reader::new(body);
        let msg = match msg_type {
            T_AUTH => ClientMessage::Auth(r.bytes()?.to_vec()),
            T_QUERY => ClientMessage::Query(r.text()?),
            T_QUERY_SQL => ClientMessage::QuerySql(r.text()?),
            T_CLOSE => ClientMessage::Close,
            other => return Err(ProtoError::UnknownTag(other)),
        };
        r.finish()?;
        Ok(msg)
    }
}

impl ServerMessage {
    /// The type byte this message travels under.
    pub fn msg_type(&self) -> u8 {
        match self {
            ServerMessage::Ready => T_READY,
            ServerMessage::Ok => T_OK,
            ServerMessage::Count { .. } => T_COUNT,
            ServerMessage::RowsBegin { .. } => T_ROWS_BEGIN,
            ServerMessage::RowBatch { .. } => T_ROW_BATCH,
            ServerMessage::RowsEnd => T_ROWS_END,
            ServerMessage::Lines(_) => T_LINES,
            ServerMessage::Error { .. } => T_ERROR,
        }
    }

    /// Encode the body, without the frame header.
    pub fn body(&self) -> Result<Vec<u8>> {
        let mut w = Writer::new();
        match self {
            ServerMessage::Ready | ServerMessage::Ok | ServerMessage::RowsEnd => {}
            ServerMessage::Count { verb, n } => {
                w.text(verb);
                w.u64(*n);
            }
            ServerMessage::RowsBegin { columns } => {
                w.count(columns.len(), MAX_VALUES_PER_ROW);
                for c in columns {
                    w.text(c);
                }
            }
            ServerMessage::RowBatch { rows } => {
                w.count(rows.len(), MAX_ROWS_PER_BATCH);
                for row in rows {
                    write_row(&mut w, row);
                }
            }
            ServerMessage::Lines(lines) => {
                w.count(lines.len(), MAX_LINES);
                for l in lines {
                    w.text(l);
                }
            }
            ServerMessage::Error { code, message } => {
                w.u16(*code);
                w.text(message);
            }
        }
        w.finish()
    }

    /// Encode a complete frame.
    pub fn encode(&self) -> Result<Vec<u8>> {
        frame(self.msg_type(), &self.body()?)
    }

    /// Decode a body that arrived under `msg_type`.
    pub fn decode(msg_type: u8, body: &[u8]) -> Result<Self> {
        let mut r = Reader::new(body);
        let msg = match msg_type {
            T_READY => ServerMessage::Ready,
            T_OK => ServerMessage::Ok,
            T_COUNT => ServerMessage::Count {
                verb: r.text()?,
                n: r.u64()?,
            },
            T_ROWS_BEGIN => {
                let n = r.count(MIN_TEXT_LEN, MAX_VALUES_PER_ROW)?;
                let mut columns = prealloc(n);
                for _ in 0..n {
                    columns.push(r.text()?);
                }
                ServerMessage::RowsBegin { columns }
            }
            T_ROW_BATCH => {
                let n = r.count(MIN_ROW_LEN, MAX_ROWS_PER_BATCH)?;
                let mut rows = prealloc(n);
                for _ in 0..n {
                    rows.push(read_row(&mut r)?);
                }
                ServerMessage::RowBatch { rows }
            }
            T_ROWS_END => ServerMessage::RowsEnd,
            T_LINES => {
                let n = r.count(MIN_TEXT_LEN, MAX_LINES)?;
                let mut lines = prealloc(n);
                for _ in 0..n {
                    lines.push(r.text()?);
                }
                ServerMessage::Lines(lines)
            }
            T_ERROR => ServerMessage::Error {
                code: r.u16()?,
                message: r.text()?,
            },
            other => return Err(ProtoError::UnknownTag(other)),
        };
        r.finish()?;
        Ok(msg)
    }
}

/// Room a `RowBatch` body has for rows, once its own count is deducted.
const BATCH_BUDGET: usize = MAX_BODY - 4;

/// Splits rows into batches that each fit inside a frame.
pub struct RowBatcher<I> {
    rows: I,
    pending: Vec<Vec<Value>>,
    used: usize,
    done: bool,
}

impl<I> RowBatcher<I> {
    /// Batch the rows produced by `rows`.
    pub fn new<S>(rows: S) -> Self
    where
        S: IntoIterator<IntoIter = I>,
    {
        RowBatcher {
            rows: rows.into_iter(),
            pending: Vec::new(),
            used: 0,
            done: false,
        }
    }
}

impl<I: Iterator<Item = Vec<Value>>> Iterator for RowBatcher<I> {
    type Item = Result<ServerMessage>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        loop {
            let Some(row) = self.rows.next() else {
                self.done = true;
                return if self.pending.is_empty() {
                    None
                } else {
                    Some(Ok(ServerMessage::RowBatch {
                        rows: std::mem::take(&mut self.pending),
                    }))
                };
            };

            let size = encoded_row_len(&row);
            if size > BATCH_BUDGET || row.len() > MAX_VALUES_PER_ROW {
                self.done = true;
                return Some(Err(ProtoError::TooLarge {
                    declared: size as u64,
                    limit: BATCH_BUDGET as u64,
                }));
            }

            let full = self.used + size > BATCH_BUDGET || self.pending.len() >= MAX_ROWS_PER_BATCH;
            if !self.pending.is_empty() && full {
                let batch = std::mem::take(&mut self.pending);
                self.used = size;
                self.pending.push(row);
                return Some(Ok(ServerMessage::RowBatch { rows: batch }));
            }

            self.used += size;
            self.pending.push(row);
        }
    }
}

/// Batch a materialized result set, failing on the first row that cannot
pub fn batch_rows(rows: Vec<Vec<Value>>) -> Result<Vec<ServerMessage>> {
    RowBatcher::new(rows).collect()
}
