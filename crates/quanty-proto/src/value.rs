//! Values on the wire.

use quanty_core::Value;

use crate::codec::{prealloc, Reader, Writer};
use crate::error::{ProtoError, Result};
use crate::limits::{MAX_VALUES_PER_ROW, MIN_VALUE_LEN};

/// Tag for the absence of a value.
pub const TAG_NULL: u8 = 0x01;
/// Tag for a boolean.
pub const TAG_BOOL: u8 = 0x02;
/// Tag for a signed 64 bit integer.
pub const TAG_INT: u8 = 0x03;
/// Tag for a 64 bit float, by its bits.
pub const TAG_FLOAT: u8 = 0x04;
/// Tag for length-prefixed UTF-8.
pub const TAG_TEXT: u8 = 0x05;
/// Tag for length-prefixed bytes.
pub const TAG_BYTES: u8 = 0x06;

/// Append one value: its tag, then its payload.
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

/// Read one value, refusing a tag this version does not define.
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

/// Bytes `write_value` will append for this value.
pub fn encoded_value_len(v: &Value) -> usize {
    1 + match v {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Int(_) | Value::Float(_) => 8,
        Value::Text(s) => 4 + s.len(),
        Value::Bytes(b) => 4 + b.len(),
    }
}

/// Bytes `write_row` will append for this row.
pub fn encoded_row_len(row: &[Value]) -> usize {
    4 + row.iter().map(encoded_value_len).sum::<usize>()
}

/// Write a row: a capped value count, then the values.
pub fn write_row(w: &mut Writer, row: &[Value]) {
    w.count(row.len(), MAX_VALUES_PER_ROW);
    for v in row {
        write_value(w, v);
    }
}

/// Read a row, committing bounded memory whatever the count claims.
pub fn read_row(r: &mut Reader<'_>) -> Result<Vec<Value>> {
    let n = r.count(MIN_VALUE_LEN, MAX_VALUES_PER_ROW)?;
    let mut row = prealloc(n);
    for _ in 0..n {
        row.push(read_value(r)?);
    }
    Ok(row)
}
