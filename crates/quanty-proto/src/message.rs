//! The messages themselves.
//!
//! Two enums rather than one, because the direction of travel is part of
//! the type: a server cannot accidentally send a `Query` and a client
//! cannot accidentally send `Rows`. The type bytes are listed in
//! docs/PROTOCOL.md and the gaps between them are reserved.

use quanty_core::Value;

use crate::codec::{Reader, Writer};
use crate::error::{ErrorCode, ProtoError, Result};
use crate::frame::{frame, MAX_BODY};
use crate::value::{read_row, write_row};

pub const T_AUTH: u8 = 0x10;
pub const T_QUERY: u8 = 0x11;
pub const T_QUERY_SQL: u8 = 0x12;
pub const T_CLOSE: u8 = 0x13;

pub const T_READY: u8 = 0x20;
pub const T_OK: u8 = 0x21;
pub const T_COUNT: u8 = 0x22;
pub const T_ROWS_BEGIN: u8 = 0x23;
pub const T_ROW_BATCH: u8 = 0x24;
pub const T_ROWS_END: u8 = 0x25;
pub const T_LINES: u8 = 0x26;
pub const T_ERROR: u8 = 0x27;

/// Smallest a `String` can encode to: a four byte length and no bytes.
const MIN_TEXT_LEN: usize = 4;

#[derive(Debug, Clone, PartialEq)]
pub enum ClientMessage {
    /// An opaque token. What it means, where it is stored and how it is
    /// revoked is deliberately not decided here; see docs/PROTOCOL.md.
    Auth(Vec<u8>),
    Query(String),
    QuerySql(String),
    Close,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ServerMessage {
    Ready,
    Ok,
    Count { verb: String, n: u64 },
    RowsBegin { columns: Vec<String> },
    RowBatch { rows: Vec<Vec<Value>> },
    RowsEnd,
    Lines(Vec<String>),
    Error { code: u16, message: String },
}

impl ServerMessage {
    /// Whether this message ends a request.
    ///
    /// A client sends one statement and reads until this returns true, which
    /// is the whole of the "one request in flight" rule from
    /// docs/PROTOCOL.md expressed as a function.
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

    pub fn error(code: ErrorCode, message: impl Into<String>) -> Self {
        ServerMessage::Error {
            code: code.as_u16(),
            message: message.into(),
        }
    }
}

impl ClientMessage {
    pub fn msg_type(&self) -> u8 {
        match self {
            ClientMessage::Auth(_) => T_AUTH,
            ClientMessage::Query(_) => T_QUERY,
            ClientMessage::QuerySql(_) => T_QUERY_SQL,
            ClientMessage::Close => T_CLOSE,
        }
    }

    pub fn body(&self) -> Vec<u8> {
        let mut w = Writer::new();
        match self {
            ClientMessage::Auth(t) => w.bytes(t),
            ClientMessage::Query(s) | ClientMessage::QuerySql(s) => w.text(s),
            ClientMessage::Close => {}
        }
        w.into_vec()
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        frame(self.msg_type(), &self.body())
    }

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

    pub fn body(&self) -> Vec<u8> {
        let mut w = Writer::new();
        match self {
            ServerMessage::Ready | ServerMessage::Ok | ServerMessage::RowsEnd => {}
            ServerMessage::Count { verb, n } => {
                w.text(verb);
                w.u64(*n);
            }
            ServerMessage::RowsBegin { columns } => {
                w.u32(columns.len() as u32);
                for c in columns {
                    w.text(c);
                }
            }
            ServerMessage::RowBatch { rows } => {
                w.u32(rows.len() as u32);
                for row in rows {
                    write_row(&mut w, row);
                }
            }
            ServerMessage::Lines(lines) => {
                w.u32(lines.len() as u32);
                for l in lines {
                    w.text(l);
                }
            }
            ServerMessage::Error { code, message } => {
                w.u16(*code);
                w.text(message);
            }
        }
        w.into_vec()
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        frame(self.msg_type(), &self.body())
    }

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
                let n = r.count(MIN_TEXT_LEN)?;
                let mut columns = Vec::with_capacity(n);
                for _ in 0..n {
                    columns.push(r.text()?);
                }
                ServerMessage::RowsBegin { columns }
            }
            T_ROW_BATCH => {
                // A row is at least a four byte value count.
                let n = r.count(4)?;
                let mut rows = Vec::with_capacity(n);
                for _ in 0..n {
                    rows.push(read_row(&mut r)?);
                }
                ServerMessage::RowBatch { rows }
            }
            T_ROWS_END => ServerMessage::RowsEnd,
            T_LINES => {
                let n = r.count(MIN_TEXT_LEN)?;
                let mut lines = Vec::with_capacity(n);
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

/// Split rows into batches that each fit inside a frame.
///
/// `Output::Rows` is one Rust value of any size, so it cannot be one
/// message. The sender chooses the split; the receiver only sees a
/// sequence. A single row larger than the cap cannot be sent at all and is
/// reported rather than silently dropped, because a result set that is
/// quietly missing a row is worse than one that failed.
pub fn batch_rows(rows: Vec<Vec<Value>>) -> Result<Vec<ServerMessage>> {
    // Leave room for the batch's own row count.
    let budget = MAX_BODY - 4;
    let mut out = Vec::new();
    let mut current: Vec<Vec<Value>> = Vec::new();
    let mut used = 0usize;

    for row in rows {
        let size = encoded_row_len(&row);
        if size > budget {
            return Err(ProtoError::TooLarge {
                declared: size as u64,
                limit: budget as u64,
            });
        }
        if !current.is_empty() && used + size > budget {
            out.push(ServerMessage::RowBatch {
                rows: std::mem::take(&mut current),
            });
            used = 0;
        }
        used += size;
        current.push(row);
    }
    if !current.is_empty() {
        out.push(ServerMessage::RowBatch { rows: current });
    }
    Ok(out)
}

fn encoded_row_len(row: &[Value]) -> usize {
    let mut n = 4;
    for v in row {
        n += 1 + match v {
            Value::Null => 0,
            Value::Bool(_) => 1,
            Value::Int(_) | Value::Float(_) => 8,
            Value::Text(s) => 4 + s.len(),
            Value::Bytes(b) => 4 + b.len(),
        };
    }
    n
}
