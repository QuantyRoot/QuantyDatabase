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
        // pos never passes len, see take, so this cannot underflow.
        self.buf.len() - self.pos
    }

    /// Whether the buffer is fully consumed.
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
    /// Read exactly `n` bytes with no length prefix, for fixed-width
    /// fields such as the handshake magic.
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

    /// A length-prefixed UTF-8 string.
    pub fn text(&mut self) -> Result<String> {
        let b = self.bytes()?;
        // Not from_utf8_lossy. Almost-right text is the failure that
        // survives review, so a bad encoding is an error and not a row of
        // replacement characters.
        std::str::from_utf8(b)
            .map(|s| s.to_string())
            .map_err(|_| ProtoError::BadUtf8)
    }

    /// Read a count that will drive a loop, refusing anything the
    /// protocol does not permit.
    ///
    /// Two separate limits, and both are needed.
    ///
    /// `hard_max` is the protocol's own cap on how many elements a message
    /// of this kind may carry. It is the one that matters. An element costs
    /// more memory than it costs wire: an empty row is four bytes sent and
    /// 24 bytes held, an empty value one byte sent and 32 held. A limit
    /// expressed only as "what fits in the frame" therefore hands the
    /// sender a multiplier, and 16 MiB of `Null` tags becomes half a
    /// gigabyte of resident memory. Capping the count itself makes the
    /// memory a decoder can be made to commit a constant instead of a
    /// multiple of `MAX_BODY`.
    ///
    /// `min_bytes_each` then rejects counts the remaining bytes could not
    /// satisfy anyway, which catches the lie earlier and with a better
    /// error than reading elements until the body runs out.
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
/// Appends primitives, refusing to write a length it cannot represent.
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
    ///
    /// Errors are held until here so that encoding reads as a straight
    /// sequence of writes rather than a question after every field, while
    /// still having exactly one place the caller must handle failure.
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
        // Bits, not a rendering. NaN and both infinities survive the trip
        // and no value changes meaning by being sent.
        self.u64(v.to_bits());
    }

    /// Write a boolean as one byte.
    pub fn bool(&mut self, v: bool) {
        self.buf.push(v as u8);
    }

    /// Write a length-prefixed byte string.
    ///
    /// A payload too large for the four byte prefix is refused, not
    /// truncated. Truncating would write a wrong length in front of a right
    /// payload: a corrupt frame that looks like a successful send, where
    /// the receiver reads a short field and then finds the rest of it where
    /// the next field should be. The previous version of this function
    /// carried a comment arguing the cast was safe because callers check
    /// against MAX_BODY before framing. They check after this runs.
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
    ///
    /// The mirror of `Reader::count`. An encoder able to emit a count no
    /// decoder will accept produces frames that fail only at the far end,
    /// which is the worst place to find out.
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
/// letting the sender choose the number.
///
/// The cap has passed, so the worst case is already bounded in absolute
/// terms. This exists because "bounded by a constant" and "reserved up
/// front" are different promises: a message legitimately carrying two rows
/// should not reserve room for sixty-five thousand.
pub fn prealloc<T>(n: usize) -> Vec<T> {
    Vec::with_capacity(n.min(crate::limits::PREALLOC_ELEMS))
}

impl Default for Writer {
    fn default() -> Self {
        Writer::new()
    }
}
