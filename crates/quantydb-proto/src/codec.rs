//! Bounds-checked reading and writing of the primitives the protocol uses.

use crate::error::{ProtoError, Result};

/// Reads primitives out of a message body without ever trusting it.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// Start reading at the front of `buf`.
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    /// Bytes not yet consumed.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// Whether the buffer is fully consumed.
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// Error unless the body has been consumed exactly.
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
    pub fn raw(&mut self, n: usize) -> Result<&'a [u8]> {
        self.take(n)
    }

    /// Read one byte.
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    /// Read a little endian `u16`.
    pub fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    /// Read a little endian `u32`.
    pub fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Read a little endian `u64`.
    pub fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }

    /// Read a little endian `i64`.
    pub fn i64(&mut self) -> Result<i64> {
        Ok(self.u64()? as i64)
    }

    /// Read an `f64` by its bits.
    pub fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_bits(self.u64()?))
    }

    /// Read a boolean, refusing any byte that is neither 0 nor 1.
    pub fn bool(&mut self) -> Result<bool> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(ProtoError::Malformed("bool")),
        }
    }

    /// A 4-byte length followed by that many bytes.
    pub fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.u32()? as usize;
        self.take(n)
    }

    /// A length-prefixed UTF-8 string.
    pub fn text(&mut self) -> Result<String> {
        let b = self.bytes()?;
        std::str::from_utf8(b)
            .map(|s| s.to_string())
            .map_err(|_| ProtoError::BadUtf8)
    }

    /// Read a count that will drive a loop, refusing anything the
    pub fn count(&mut self, min_bytes_each: usize, hard_max: usize) -> Result<usize> {
        let n = self.u32()? as usize;
        if n > hard_max {
            return Err(ProtoError::TooManyElements {
                declared: n as u64,
                limit: hard_max as u64,
            });
        }
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
    err: Option<ProtoError>,
}

impl Writer {
    /// A new, empty writer.
    pub fn new() -> Self {
        Writer {
            buf: Vec::new(),
            err: None,
        }
    }

    /// A new writer with room for `n` bytes.
    pub fn with_capacity(n: usize) -> Self {
        Writer {
            buf: Vec::with_capacity(n),
            err: None,
        }
    }

    /// Take the bytes, or the first error that happened while writing them.
    pub fn finish(self) -> Result<Vec<u8>> {
        match self.err {
            Some(e) => Err(e),
            None => Ok(self.buf),
        }
    }

    fn fail(&mut self, e: ProtoError) {
        if self.err.is_none() {
            self.err = Some(e);
        }
    }

    /// Bytes written so far.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether nothing has been written.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Write one byte.
    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    /// Write a little endian `u16`.
    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Write a little endian `u32`.
    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Write a little endian `u64`.
    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Write a little endian `i64`.
    pub fn i64(&mut self, v: i64) {
        self.u64(v as u64);
    }

    /// Write an `f64` by its bits.
    pub fn f64(&mut self, v: f64) {
        self.u64(v.to_bits());
    }

    /// Write a boolean as one byte.
    pub fn bool(&mut self, v: bool) {
        self.buf.push(v as u8);
    }

    /// Write a length-prefixed byte string.
    pub fn bytes(&mut self, v: &[u8]) {
        if v.len() > crate::limits::MAX_BODY {
            self.fail(ProtoError::TooLarge {
                declared: v.len() as u64,
                limit: crate::limits::MAX_BODY as u64,
            });
            return;
        }
        self.u32(v.len() as u32);
        self.buf.extend_from_slice(v);
    }

    /// Write a length-prefixed string.
    pub fn text(&mut self, v: &str) {
        self.bytes(v.as_bytes());
    }

    /// Write an element count, refusing one the protocol does not permit.
    pub fn count(&mut self, n: usize, hard_max: usize) {
        if n > hard_max {
            self.fail(ProtoError::TooManyElements {
                declared: n as u64,
                limit: hard_max as u64,
            });
            return;
        }
        self.u32(n as u32);
    }
}

/// Allocate for a count that has already passed `Reader::count`, without
pub fn prealloc<T>(n: usize) -> Vec<T> {
    Vec::with_capacity(n.min(crate::limits::PREALLOC_ELEMS))
}

impl Default for Writer {
    fn default() -> Self {
        Writer::new()
    }
}
