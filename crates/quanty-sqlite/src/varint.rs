//! SQLite's variable length integers.
//!
//! One to nine bytes, big endian, and the encoding is not the usual
//! LEB128: the first eight bytes carry seven bits each with the high bit as
//! a continuation flag, and the ninth byte, if it is reached, carries all
//! eight of its bits. That last part is the piece people get wrong, and it
//! matters because it is exactly how a rowid near `i64::MAX` is stored.
//!
//! The result is signed. A varint is a 64 bit two's complement integer, so
//! large values come back negative on purpose, and rowids near the top of
//! the range round trip through this without a special case.

use crate::error::{Result, SqliteError};

/// Read a varint from the front of `bytes`, returning it and how many bytes
/// it took.
pub(crate) fn read(bytes: &[u8]) -> Result<(i64, usize)> {
    let mut value: u64 = 0;
    for (index, byte) in bytes.iter().take(8).enumerate() {
        value = (value << 7) | (byte & 0x7f) as u64;
        if byte & 0x80 == 0 {
            return Ok((value as i64, index + 1));
        }
    }
    // eight continuation bytes means a ninth byte follows and contributes
    // all eight of its bits, for 56 + 8 = 64
    match bytes.get(8) {
        Some(byte) => Ok((((value << 8) | *byte as u64) as i64, 9)),
        None => Err(SqliteError::malformed(
            None,
            "a varint runs past the end of its cell",
        )),
    }
}

/// Read a varint that is used as a length or a count, where a negative
/// value is always a malformed file rather than a large one.
pub(crate) fn read_unsigned(bytes: &[u8]) -> Result<(u64, usize)> {
    let (value, len) = read(bytes)?;
    if value < 0 {
        return Err(SqliteError::malformed(
            None,
            format!("a length varint holds the negative value {value}"),
        ));
    }
    Ok((value as u64, len))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_byte_values() {
        for n in 0..=0x7fu8 {
            assert_eq!(read(&[n]).unwrap(), (n as i64, 1));
        }
    }

    #[test]
    fn multi_byte_values() {
        // the examples the format documentation works through
        assert_eq!(read(&[0x81, 0x00]).unwrap(), (128, 2));
        assert_eq!(read(&[0x81, 0x01]).unwrap(), (129, 2));
        assert_eq!(read(&[0xfe, 0x7f]).unwrap(), (16255, 2));
        assert_eq!(read(&[0xff, 0x7f]).unwrap(), (16383, 2));
        assert_eq!(read(&[0x81, 0x80, 0x00]).unwrap(), (1 << 14, 3));
    }

    #[test]
    fn the_ninth_byte_carries_eight_bits() {
        let max = [0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff];
        assert_eq!(read(&max).unwrap(), (-1, 9));
        // the top bit of the value is carried by the first byte's payload,
        // so i64::MIN starts 0xc0 and not 0x81
        let min = [0xc0, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00];
        assert_eq!(read(&min).unwrap(), (i64::MIN, 9));
        let bit_57 = [0x81, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00];
        assert_eq!(read(&bit_57).unwrap(), (1 << 57, 9));
    }

    #[test]
    fn a_varint_stops_at_nine_bytes() {
        // all continuation bits set, so a decoder without the nine byte cap
        // would keep reading into whatever follows
        let bytes = [0xff; 12];
        assert_eq!(read(&bytes).unwrap().1, 9);
    }

    #[test]
    fn truncation_is_an_error_not_a_panic() {
        assert!(read(&[]).is_err());
        assert!(read(&[0x81]).is_err());
        assert!(read(&[0xff; 8]).is_err());
    }

    #[test]
    fn lengths_refuse_negative_values() {
        assert!(read_unsigned(&[0xff; 9]).is_err());
        assert_eq!(read_unsigned(&[0x7f]).unwrap(), (127, 1));
    }
}
