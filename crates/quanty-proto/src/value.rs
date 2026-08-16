//! Values on the wire.
//!
//! This is **not** `quanty_core::encoding`, and the difference is worth
//! stating because the two look similar enough to be merged by someone in a
//! hurry. That one exists to make memcmp match logical order, which is why
//! it escapes zero bytes and flips sign bits. This one exists to round-trip
//! a value across a socket, where nothing is ever compared as bytes.
//!
//! The tags below are defined here rather than imported from core on
//! purpose. They happen to agree with core's today. Sharing the constants
//! would mean a change to the on-disk format silently became a change to
//! the protocol, and those two have separate version histories and separate
//! compatibility promises. See docs/PROTOCOL.md.

use quanty_core::Value;

use crate::codec::{Reader, Writer};
use crate::error::{ProtoError, Result};

pub const TAG_NULL: u8 = 0x01;
pub const TAG_BOOL: u8 = 0x02;
pub const TAG_INT: u8 = 0x03;
pub const TAG_FLOAT: u8 = 0x04;
pub const TAG_TEXT: u8 = 0x05;
pub const TAG_BYTES: u8 = 0x06;

/// Smallest number of bytes a value can encode to: the tag of a `Null`.
///
/// Used to bound row and column counts before allocating for them.
pub const MIN_VALUE_LEN: usize = 1;

pub fn write_value(w: &mut Writer, v: &Value) {
    match v {
        Value::Null => w.u8(TAG_NULL),
        Value::Bool(b) => {
            w.u8(TAG_BOOL);
            w.bool(*b);
        }
        Value::Int(i) => {
            w.u8(TAG_INT);
            w.i64(*i);
        }
        Value::Float(f) => {
            w.u8(TAG_FLOAT);
            w.f64(*f);
        }
        Value::Text(s) => {
            w.u8(TAG_TEXT);
            w.text(s);
        }
        Value::Bytes(b) => {
            w.u8(TAG_BYTES);
            w.bytes(b);
        }
    }
}

pub fn read_value(r: &mut Reader<'_>) -> Result<Value> {
    let tag = r.u8()?;
    Ok(match tag {
        TAG_NULL => Value::Null,
        TAG_BOOL => Value::Bool(r.bool()?),
        TAG_INT => Value::Int(r.i64()?),
        TAG_FLOAT => Value::Float(r.f64()?),
        TAG_TEXT => Value::Text(r.text()?),
        TAG_BYTES => Value::Bytes(r.bytes()?.to_vec()),
        other => return Err(ProtoError::UnknownTag(other)),
    })
}

pub fn write_row(w: &mut Writer, row: &[Value]) {
    w.u32(row.len() as u32);
    for v in row {
        write_value(w, v);
    }
}

pub fn read_row(r: &mut Reader<'_>) -> Result<Vec<Value>> {
    let n = r.count(MIN_VALUE_LEN)?;
    let mut row = Vec::with_capacity(n);
    for _ in 0..n {
        row.push(read_value(r)?);
    }
    Ok(row)
}
