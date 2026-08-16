//! Bounds-checked reading and writing of the primitives the protocol uses.
//!
//! Every decode path in this crate goes through `Reader`. That is the whole
//! design: the promise that hostile bytes cannot panic is kept in one small
//! place that can be read in a minute, instead of being re-argued at every
//! slice index in every message type.
//!
//! `Reader` never indexes and never slices with a range it has not first
//! checked, so it cannot panic, and every method returns `Result`. Offsets
//! advance only after a successful read.
//!
//! Byte order is little endian everywhere, matching docs/FORMAT.md. There
//! is no ordering requirement on the wire, so the file format's convention
//! wins over the network one for the sake of having a single answer.

use crate::error::{ProtoError, Result};

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        // pos never passes len, see take, so this cannot underflow.
        self.buf.len() - self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// Error unless the body has been consumed exactly.
    ///
    /// Called at the end of every message decode. Trailing bytes mean the
    /// sender and this decoder disagree about the shape of a message, which
    /// is worth failing on rather than ignoring: silently dropping the tail
    /// is how a version mismatch turns into wrong data instead of an error.
    pub fn finish(self) -> Result<()> {
        match self.remaining() {
            0 => Ok(()),
            n => Err(ProtoError::TrailingBytes(n)),
        }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.pos.checked_add(n).ok_or(ProtoError::Truncated {
            needed: n,
            available: self.remaining(),
        })?;
        if end > self.buf.len() {
            return Err(ProtoError::Truncated {
                needed: n,
                available: self.remaining(),
            });
        }
        let out = &self.buf[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    /// Read exactly `n` bytes with no length prefix, for fixed-width
    /// fields such as the handshake magic.
    pub fn raw(&mut self, n: usize) -> Result<&'a [u8]> {
        self.take(n)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }

    pub fn i64(&mut self) -> Result<i64> {
        Ok(self.u64()? as i64)
    }

    pub fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_bits(self.u64()?))
    }

    pub fn bool(&mut self) -> Result<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(ProtoError::Malformed("bool")),
        }
    }

    /// A 4-byte length followed by that many bytes.
    ///
    /// The length is checked against what is actually left in the buffer
    /// before anything is allocated, so a frame claiming four billion bytes
    /// costs an error and not memory. This is the single most important
    /// check in the crate: the frame cap in `frame.rs` bounds the outer
    /// allocation, and this bounds every inner one.
    pub fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }

    pub fn text(&mut self) -> Result<String> {
        let b = self.bytes()?;
        // Not from_utf8_lossy. Almost-right text is the failure that
        // survives review, so a bad encoding is an error and not a row of
        // replacement characters.
        std::str::from_utf8(b)
            .map(|s| s.to_string())
            .map_err(|_| ProtoError::BadUtf8)
    }

    /// Read a count that will drive a loop, refusing counts the remaining
    /// bytes cannot possibly satisfy.
    ///
    /// `min_bytes_each` is the smallest a single element can encode to. A
    /// count larger than `remaining / min_bytes_each` is a lie regardless
    /// of what follows, and catching it here means `with_capacity` is never
    /// handed a number an attacker chose.
    pub fn count(&mut self, min_bytes_each: usize) -> Result<usize> {
        let n = self.u32()? as usize;
        let ceiling = self.remaining() / min_bytes_each.max(1);
        if n > ceiling {
            return Err(ProtoError::TooLarge {
                declared: n as u64,
                limit: ceiling as u64,
            });
        }
        Ok(n)
    }
}

/// Append helpers, so encoding reads the same way decoding does.
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Writer { buf: Vec::new() }
    }

    pub fn with_capacity(n: usize) -> Self {
        Writer {
            buf: Vec::with_capacity(n),
        }
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn i64(&mut self, v: i64) {
        self.u64(v as u64);
    }

    pub fn f64(&mut self, v: f64) {
        // Bits, not a rendering. NaN and both infinities survive the trip
        // and no value changes meaning by being sent.
        self.u64(v.to_bits());
    }

    pub fn bool(&mut self, v: bool) {
        self.buf.push(v as u8);
    }

    pub fn bytes(&mut self, v: &[u8]) {
        // Truncation here would be silent corruption. Callers bound their
        // payloads against MAX_BODY before building a frame; this cast is
        // safe for anything that passed that check, and a value large
        // enough to wrap would have failed it.
        self.u32(v.len() as u32);
        self.buf.extend_from_slice(v);
    }

    pub fn text(&mut self, v: &str) {
        self.bytes(v.as_bytes());
    }
}

impl Default for Writer {
    fn default() -> Self {
        Writer::new()
    }
}
