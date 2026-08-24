//! The record format: how a row's values are laid out inside a cell.
//!
//! A record is a header followed by a body. The header is a varint holding
//! its own total length, then one varint per column giving that column's
//! serial type. The body is the values, back to back, in the same order.
//! Serial types encode both the type and the width, and two of them encode
//! the value as well: type 8 is the integer 0 and type 9 is the integer 1,
//! neither of which occupies a single byte in the body.
//!
//! Values come back as the five storage classes SQLite actually has. This
//! crate deliberately does not map them onto QuantyDB's value type: SQLite
//! is dynamically typed and QuantyDB is not, so that mapping is a decision
//! with its own trade-offs and it belongs to the importer, not to a format
//! reader.

use crate::error::{Result, SqliteError};
use crate::header::TextEncoding;
use crate::varint;

/// One of SQLite's five storage classes.
///
/// `PartialEq` is the derived one, so `Real` values follow IEEE rules and a
/// NaN is not equal to itself. That matters because a file can hold any 64
/// bit pattern in a float field: SQLite's own API turns a NaN into NULL on
/// the way in, but a corrupted or hand written file is under no such
/// obligation. Code asking whether two reads produced the same bytes should
/// compare `to_bits` rather than the values.
#[derive(Debug, Clone, PartialEq)]
pub enum SqliteValue {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl SqliteValue {
    /// The name SQLite's own `typeof()` gives this value, which is what the
    /// import tests compare against.
    pub fn type_name(&self) -> &'static str {
        match self {
            SqliteValue::Null => "null",
            SqliteValue::Integer(_) => "integer",
            SqliteValue::Real(_) => "real",
            SqliteValue::Text(_) => "text",
            SqliteValue::Blob(_) => "blob",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SerialType {
    Null,
    Int(u8),
    Float,
    /// The value 0, carried entirely by the serial type.
    Zero,
    /// The value 1, likewise.
    One,
    Blob(usize),
    Text(usize),
}

impl SerialType {
    fn from_code(code: u64) -> Result<SerialType> {
        Ok(match code {
            0 => SerialType::Null,
            1 => SerialType::Int(1),
            2 => SerialType::Int(2),
            3 => SerialType::Int(3),
            4 => SerialType::Int(4),
            5 => SerialType::Int(6),
            6 => SerialType::Int(8),
            7 => SerialType::Float,
            8 => SerialType::Zero,
            9 => SerialType::One,
            // sqlite reserves these for its own internal record formats;
            // they never appear in a database file
            10 | 11 => {
                return Err(SqliteError::malformed(
                    None,
                    format!("serial type {code} is reserved for internal use"),
                ))
            }
            even if even % 2 == 0 => {
                let len = (even - 12) / 2;
                SerialType::Blob(usize::try_from(len).map_err(|_| {
                    SqliteError::malformed(None, format!("blob of {len} bytes is impossibly large"))
                })?)
            }
            odd => {
                let len = (odd - 13) / 2;
                SerialType::Text(usize::try_from(len).map_err(|_| {
                    SqliteError::malformed(None, format!("text of {len} bytes is impossibly large"))
                })?)
            }
        })
    }

    /// Bytes this type occupies in the record body.
    fn width(self) -> usize {
        match self {
            SerialType::Null | SerialType::Zero | SerialType::One => 0,
            SerialType::Int(n) => n as usize,
            SerialType::Float => 8,
            SerialType::Blob(n) | SerialType::Text(n) => n,
        }
    }
}

/// Decode one record into its values.
///
/// `payload` must be the whole record, overflow pages already stitched back
/// on. Anything that does not add up, a header that overruns the payload, a
/// value that runs past the end, a reserved serial type, is a malformed
/// file and comes back as an error.
///
/// `encoding` is the one the database header states, and it applies to
/// every text value in the file. Text comes back as a Rust `String`
/// whatever the file stores, so the encoding is a fact about the bytes on
/// disk and not something callers have to carry around afterwards.
pub fn decode(payload: &[u8], encoding: TextEncoding) -> Result<Vec<SqliteValue>> {
    let (header_len, varint_len) = varint::read_unsigned(payload)?;
    let header_len = usize::try_from(header_len)
        .map_err(|_| SqliteError::malformed(None, "record header length does not fit in memory"))?;
    if header_len < varint_len || header_len > payload.len() {
        return Err(SqliteError::malformed(
            None,
            format!(
                "record header claims {header_len} bytes of a {} byte payload",
                payload.len()
            ),
        ));
    }

    // first pass: read the serial types and work out where the body starts
    let mut types = Vec::new();
    let mut at = varint_len;
    while at < header_len {
        let (code, len) = varint::read_unsigned(&payload[at..header_len])?;
        types.push(SerialType::from_code(code)?);
        at += len;
    }

    // second pass: cut the body into values
    let mut values = Vec::with_capacity(types.len());
    let mut at = header_len;
    for ty in types {
        let width = ty.width();
        let end = at.checked_add(width).ok_or_else(|| {
            SqliteError::malformed(None, "a record value overflows the payload length")
        })?;
        if end > payload.len() {
            return Err(SqliteError::malformed(
                None,
                format!(
                    "a {width} byte value at offset {at} runs past the {} byte payload",
                    payload.len()
                ),
            ));
        }
        let bytes = &payload[at..end];
        values.push(match ty {
            SerialType::Null => SqliteValue::Null,
            SerialType::Zero => SqliteValue::Integer(0),
            SerialType::One => SqliteValue::Integer(1),
            SerialType::Int(_) => SqliteValue::Integer(signed_be(bytes)),
            SerialType::Float => {
                let mut eight = [0u8; 8];
                eight.copy_from_slice(bytes);
                SqliteValue::Real(f64::from_bits(u64::from_be_bytes(eight)))
            }
            SerialType::Blob(_) => SqliteValue::Blob(bytes.to_vec()),
            SerialType::Text(_) => SqliteValue::Text(text_from(bytes, encoding)?),
        });
        at = end;
    }

    Ok(values)
}

/// Turn the stored bytes of a text value into a string.
///
/// Both utf-16 encodings are decoded rather than transcoded loosely: an
/// unpaired surrogate is an error, not a replacement character. Text that
/// is almost right is the failure mode that survives an import and shows up
/// months later in somebody's name field, so it stops here.
fn text_from(bytes: &[u8], encoding: TextEncoding) -> Result<String> {
    match encoding {
        TextEncoding::Utf8 => match std::str::from_utf8(bytes) {
            Ok(text) => Ok(text.to_string()),
            Err(e) => Err(SqliteError::malformed(
                None,
                format!("a text value is not valid utf-8: {e}"),
            )),
        },
        TextEncoding::Utf16Le | TextEncoding::Utf16Be => {
            if !bytes.len().is_multiple_of(2) {
                return Err(SqliteError::malformed(
                    None,
                    format!(
                        "a utf-16 text value is {} bytes long, which is not a whole number of \
                         code units",
                        bytes.len()
                    ),
                ));
            }
            let big_endian = encoding == TextEncoding::Utf16Be;
            let units: Vec<u16> = bytes
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| {
                    if big_endian {
                        u16::from_be_bytes([pair[0], pair[1]])
                    } else {
                        u16::from_le_bytes([pair[0], pair[1]])
                    }
                })
                .collect();
            String::from_utf16(&units).map_err(|_| {
                SqliteError::malformed(
                    None,
                    "a utf-16 text value holds an unpaired surrogate, so it is not text",
                )
            })
        }
    }
}

/// Big endian two's complement of 1, 2, 3, 4, 6 or 8 bytes.
fn signed_be(bytes: &[u8]) -> i64 {
    let mut value: u64 = 0;
    for byte in bytes {
        value = (value << 8) | *byte as u64;
    }
    let bits = 8 * bytes.len() as u32;
    if bits < 64 && (value >> (bits - 1)) & 1 == 1 {
        // sign extend the widths that are not a whole i64
        (value | (!0u64 << bits)) as i64
    } else {
        value as i64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_extension_covers_every_width() {
        assert_eq!(signed_be(&[0x7f]), 127);
        assert_eq!(signed_be(&[0x80]), -128);
        assert_eq!(signed_be(&[0xff]), -1);
        assert_eq!(signed_be(&[0x80, 0x00]), -32768);
        assert_eq!(signed_be(&[0x80, 0x00, 0x00]), -8388608);
        assert_eq!(signed_be(&[0x7f, 0xff, 0xff]), 8388607);
        assert_eq!(signed_be(&[0x80, 0, 0, 0]), -2147483648);
        assert_eq!(signed_be(&[0x80, 0, 0, 0, 0, 0]), -140737488355328);
        assert_eq!(
            signed_be(&[0x7f, 0xff, 0xff, 0xff, 0xff, 0xff]),
            140737488355327
        );
        assert_eq!(signed_be(&[0x80, 0, 0, 0, 0, 0, 0, 0]), i64::MIN);
        assert_eq!(signed_be(&[0xff; 8]), -1);
    }

    #[test]
    fn a_record_of_every_cheap_type() {
        // header: length 6, then serial types null, 0, 1, int8, text(2).
        // a text of n characters is serial type 2n + 13, so two is 17.
        let payload = [6u8, 0, 8, 9, 1, 17, 0x2a, b'h', b'i'];
        let values = decode(&payload, TextEncoding::Utf8).unwrap();
        assert_eq!(
            values,
            vec![
                SqliteValue::Null,
                SqliteValue::Integer(0),
                SqliteValue::Integer(1),
                SqliteValue::Integer(42),
                SqliteValue::Text("hi".to_string()),
            ]
        );
    }

    #[test]
    fn an_empty_record_has_no_values() {
        assert_eq!(decode(&[1u8], TextEncoding::Utf8).unwrap(), vec![]);
    }

    #[test]
    fn reserved_serial_types_are_refused() {
        assert!(decode(&[2u8, 10], TextEncoding::Utf8).is_err());
        assert!(decode(&[2u8, 11], TextEncoding::Utf8).is_err());
    }

    #[test]
    fn a_header_longer_than_the_payload_is_refused() {
        assert!(decode(&[99u8, 0], TextEncoding::Utf8).is_err());
        assert!(decode(&[], TextEncoding::Utf8).is_err());
    }

    #[test]
    fn a_value_running_past_the_payload_is_refused() {
        // header says one text of 4 bytes, body holds one
        assert!(decode(&[2u8, 21, b'a'], TextEncoding::Utf8).is_err());
    }

    #[test]
    fn invalid_utf8_in_a_text_value_is_refused() {
        assert!(decode(&[2u8, 15, 0xff, 0xfe], TextEncoding::Utf8).is_err());
    }

    #[test]
    fn floats_round_trip_bit_for_bit() {
        let mut payload = vec![2u8, 7];
        payload.extend_from_slice(&(-2.25f64).to_be_bytes());
        assert_eq!(
            decode(&payload, TextEncoding::Utf8).unwrap(),
            vec![SqliteValue::Real(-2.25)]
        );
    }
}
