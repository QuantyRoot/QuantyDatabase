//! Order-preserving key encoding.
//!
//! The B-tree compares raw bytes and nothing else. This module guarantees
//! that byte order equals logical order for every supported type, so the
//! tree never needs a comparator, a schema or a type tag lookup.
//!
//! The scheme is the well trodden tuple encoding (FoundationDB popularized
//! it): every element starts with a type tag, tags are ordered so that
//! values of different types have a stable total order, and each type
//! encodes so that memcmp agrees with its logical order:
//!
//! - int: i64 with the sign bit flipped, big endian
//! - float: f64 bits, sign-dependent flip, big endian. Matches IEEE total
//!   order, so -NaN < -inf < ... < -0.0 < 0.0 < ... < +inf < +NaN
//! - text and bytes: raw bytes with 0x00 escaped as 0x00 0xFF, terminated
//!   by a single 0x00
//!
//! Tuples are just concatenated elements, which gives lexicographic tuple
//! order and makes every encoded tuple a valid range-scan prefix of its
//! extensions.

use crate::error::{Error, Result};

const TAG_NULL: u8 = 0x01;
const TAG_BOOL: u8 = 0x02;
const TAG_INT: u8 = 0x03;
const TAG_FLOAT: u8 = 0x04;
const TAG_TEXT: u8 = 0x05;
const TAG_BYTES: u8 = 0x06;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
}

impl Value {
    fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            Value::Null => out.push(TAG_NULL),
            Value::Bool(b) => {
                out.push(TAG_BOOL);
                out.push(*b as u8);
            }
            Value::Int(i) => {
                out.push(TAG_INT);
                out.extend_from_slice(&((*i as u64) ^ (1 << 63)).to_be_bytes());
            }
            Value::Float(f) => {
                out.push(TAG_FLOAT);
                let bits = f.to_bits();
                let ordered = if bits & (1 << 63) != 0 {
                    !bits
                } else {
                    bits ^ (1 << 63)
                };
                out.extend_from_slice(&ordered.to_be_bytes());
            }
            Value::Text(s) => {
                out.push(TAG_TEXT);
                escape_into(s.as_bytes(), out);
            }
            Value::Bytes(b) => {
                out.push(TAG_BYTES);
                escape_into(b, out);
            }
        }
    }
}

/// Encode a tuple of values into a single order-preserving key.
pub fn encode_key(values: &[Value]) -> Vec<u8> {
    let mut out = Vec::with_capacity(values.len() * 10);
    for v in values {
        v.encode_into(&mut out);
    }
    out
}

/// Decode a key back into its tuple. The exact inverse of [`encode_key`];
/// anything that does not parse cleanly and completely is an error.
pub fn decode_key(mut buf: &[u8]) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    while !buf.is_empty() {
        let (value, rest) = decode_one(buf)?;
        out.push(value);
        buf = rest;
    }
    Ok(out)
}

fn decode_one(buf: &[u8]) -> Result<(Value, &[u8])> {
    let bad = |what: &str| Error::corrupted(None, format!("key decode: {what}"));
    let (&tag, rest) = buf.split_first().ok_or_else(|| bad("empty"))?;
    Ok(match tag {
        TAG_NULL => (Value::Null, rest),
        TAG_BOOL => {
            let (&b, rest) = rest.split_first().ok_or_else(|| bad("truncated bool"))?;
            match b {
                0 => (Value::Bool(false), rest),
                1 => (Value::Bool(true), rest),
                _ => return Err(bad("bad bool byte")),
            }
        }
        TAG_INT => {
            if rest.len() < 8 {
                return Err(bad("truncated int"));
            }
            let raw = u64::from_be_bytes(rest[..8].try_into().expect("len checked"));
            (Value::Int((raw ^ (1 << 63)) as i64), &rest[8..])
        }
        TAG_FLOAT => {
            if rest.len() < 8 {
                return Err(bad("truncated float"));
            }
            let ordered = u64::from_be_bytes(rest[..8].try_into().expect("len checked"));
            let bits = if ordered & (1 << 63) != 0 {
                ordered ^ (1 << 63)
            } else {
                !ordered
            };
            (Value::Float(f64::from_bits(bits)), &rest[8..])
        }
        TAG_TEXT => {
            let (bytes, rest) = unescape(rest).ok_or_else(|| bad("unterminated text"))?;
            let s = String::from_utf8(bytes).map_err(|_| bad("text is not utf-8"))?;
            (Value::Text(s), rest)
        }
        TAG_BYTES => {
            let (bytes, rest) = unescape(rest).ok_or_else(|| bad("unterminated bytes"))?;
            (Value::Bytes(bytes), rest)
        }
        _ => return Err(bad("unknown tag")),
    })
}

fn escape_into(data: &[u8], out: &mut Vec<u8>) {
    for &b in data {
        out.push(b);
        if b == 0x00 {
            out.push(0xFF);
        }
    }
    out.push(0x00);
}

fn unescape(buf: &[u8]) -> Option<(Vec<u8>, &[u8])> {
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        let b = *buf.get(i)?;
        if b == 0x00 {
            match buf.get(i + 1) {
                Some(0xFF) => {
                    out.push(0x00);
                    i += 2;
                }
                _ => return Some((out, &buf[i + 1..])),
            }
        } else {
            out.push(b);
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Ordering;

    /// Logical order, defined independently of the encoding so the property
    /// test cannot cheat.
    fn logical_cmp(a: &[Value], b: &[Value]) -> Ordering {
        fn rank(v: &Value) -> u8 {
            match v {
                Value::Null => 1,
                Value::Bool(_) => 2,
                Value::Int(_) => 3,
                Value::Float(_) => 4,
                Value::Text(_) => 5,
                Value::Bytes(_) => 6,
            }
        }
        for (x, y) in a.iter().zip(b.iter()) {
            let ord = match (x, y) {
                (Value::Bool(p), Value::Bool(q)) => p.cmp(q),
                (Value::Int(p), Value::Int(q)) => p.cmp(q),
                (Value::Float(p), Value::Float(q)) => p.total_cmp(q),
                (Value::Text(p), Value::Text(q)) => p.as_bytes().cmp(q.as_bytes()),
                (Value::Bytes(p), Value::Bytes(q)) => p.cmp(q),
                _ => rank(x).cmp(&rank(y)),
            };
            if ord != Ordering::Equal {
                return ord;
            }
        }
        a.len().cmp(&b.len())
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
    }

    fn random_value(rng: &mut Rng) -> Value {
        let nasty_floats = [
            0.0f64,
            -0.0,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
            -f64::NAN,
            f64::MIN_POSITIVE,
            -f64::MIN_POSITIVE,
            1.5e-310, // subnormal
        ];
        match rng.next() % 6 {
            0 => Value::Null,
            1 => Value::Bool(rng.next() % 2 == 0),
            2 => Value::Int(match rng.next() % 4 {
                0 => i64::MIN,
                1 => i64::MAX,
                2 => (rng.next() % 100) as i64 - 50,
                _ => rng.next() as i64,
            }),
            3 => Value::Float(match rng.next() % 3 {
                0 => nasty_floats[(rng.next() % nasty_floats.len() as u64) as usize],
                1 => (rng.next() as i64 as f64) / 1e6,
                _ => f64::from_bits(rng.next()),
            }),
            4 => {
                let pool = ["", "a", "ab", "a\u{0}b", "\u{0}", "zz", "grüezi", "🦀"];
                Value::Text(pool[(rng.next() % pool.len() as u64) as usize].to_string())
            }
            _ => {
                let len = (rng.next() % 6) as usize;
                let mut b = Vec::with_capacity(len);
                for _ in 0..len {
                    // heavy on 0x00 and 0xFF to stress the escaping
                    b.push(match rng.next() % 4 {
                        0 => 0x00,
                        1 => 0xFF,
                        _ => rng.next() as u8,
                    });
                }
                Value::Bytes(b)
            }
        }
    }

    fn random_tuple(rng: &mut Rng) -> Vec<Value> {
        let len = (rng.next() % 4) as usize;
        (0..len).map(|_| random_value(rng)).collect()
    }

    #[test]
    fn byte_order_equals_logical_order() {
        let mut rng = Rng(0xDEAD_BEEF_CAFE_1234);
        for i in 0..200_000u64 {
            let a = random_tuple(&mut rng);
            let b = random_tuple(&mut rng);
            let ea = encode_key(&a);
            let eb = encode_key(&b);
            assert_eq!(
                ea.cmp(&eb),
                logical_cmp(&a, &b),
                "order mismatch at case {i}: {a:?} vs {b:?} ({ea:02x?} vs {eb:02x?})",
            );
        }
    }

    #[test]
    fn encode_decode_roundtrips() {
        let mut rng = Rng(0x0123_4567_89AB_CDEF);
        for i in 0..100_000u64 {
            let tuple = random_tuple(&mut rng);
            let encoded = encode_key(&tuple);
            let decoded = decode_key(&encoded).unwrap_or_else(|e| panic!("case {i}: {e}"));
            // NaN != NaN under PartialEq, so compare via the total order
            assert_eq!(
                logical_cmp(&tuple, &decoded),
                Ordering::Equal,
                "roundtrip mismatch at case {i}: {tuple:?} vs {decoded:?}",
            );
        }
    }

    #[test]
    fn truncated_and_garbage_keys_error_cleanly() {
        for bad in [
            &[TAG_INT][..],
            &[TAG_INT, 1, 2, 3][..],
            &[TAG_TEXT, b'a'][..],
            &[TAG_TEXT, 0xC0, 0x00][..], // invalid utf-8
            &[TAG_BOOL, 7][..],
            &[0x77][..],
        ] {
            assert!(decode_key(bad).is_err(), "accepted garbage {bad:02x?}");
        }
    }

    #[test]
    fn tuple_prefix_property_holds() {
        // an encoded tuple is a strict byte prefix of its extensions,
        // which is what makes prefix range scans work
        let base = encode_key(&[Value::Int(42)]);
        let ext = encode_key(&[Value::Int(42), Value::Text("x".into())]);
        assert!(ext.starts_with(&base));
    }
}
