//! The 100 byte database header at the start of page 1.
//!
//! Everything here is big endian, and every field that the format says must
//! hold a particular value is checked rather than assumed. That is the
//! difference between a reader and a parser of hostile input: a file that
//! lies about its own page size is the cheapest way to make a naive reader
//! index out of bounds, and the check costs one comparison at open time.
//!
//! Two rejections are worth explaining because they refuse files that other
//! tools would happily open.
//!
//! Wal mode is recorded here rather than judged here. Whether a wal mode
//! database can be read from its main file alone depends on something this
//! parser cannot see: whether a `-wal` file exists next to it with commits
//! in it. A database that was checkpointed and closed cleanly is complete
//! and perfectly readable, and refusing it on the strength of a flag would
//! turn away a large share of the databases in the world. The decision
//! belongs to `Reader::open`, which can ask the source what it accounts
//! for.
//!
//! A database whose text encoding is not utf-8 is refused rather than
//! transcoded. Getting utf-16 wrong produces text that looks almost right,
//! which is the failure mode hardest to notice downstream.

use crate::error::{Result, SqliteError};

pub const HEADER_LEN: usize = 100;
const MAGIC: &[u8; 16] = b"SQLite format 3\0";

/// Text encoding of every text value in the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextEncoding {
    Utf8,
    Utf16Le,
    Utf16Be,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// Bytes per page, 512 to 65536, always a power of two.
    pub page_size: u32,
    /// Unused bytes at the end of every page. Usually zero; extensions use
    /// it for per page trailers.
    pub reserved_space: u8,
    /// Whether the database uses a write-ahead log. On its own this says
    /// nothing about whether the main file is complete: see the note at the
    /// top of this file.
    pub wal_mode: bool,
    pub file_change_counter: u32,
    /// Page count as stated in the header, only present when the header
    /// says it is current. Callers should prefer `Reader::page_count`.
    pub header_page_count: Option<u32>,
    pub first_freelist_trunk: u32,
    pub freelist_page_count: u32,
    pub schema_cookie: u32,
    /// 1 to 4. Format 4 is what any SQLite since 2006 writes.
    pub schema_format: u32,
    pub text_encoding: TextEncoding,
    pub user_version: i32,
    pub application_id: i32,
    /// Non zero when the database is in auto vacuum mode, in which case
    /// pointer map pages are interleaved with the data.
    pub largest_root_page: u32,
    pub sqlite_version_number: u32,
}

impl Header {
    /// Bytes of each page the b-tree layer may use. Everything in the file
    /// that indexes into a page is bounded by this, not by the page size.
    pub fn usable_size(&self) -> u32 {
        self.page_size - self.reserved_space as u32
    }

    pub fn parse(bytes: &[u8]) -> Result<Header> {
        if bytes.len() < HEADER_LEN {
            return Err(SqliteError::not_sqlite(format!(
                "file is {} bytes, shorter than the {HEADER_LEN} byte header",
                bytes.len()
            )));
        }
        if &bytes[0..16] != MAGIC {
            return Err(SqliteError::not_sqlite(
                "the file does not start with the sqlite format 3 magic string",
            ));
        }

        // a page size of 1 encodes 65536, which does not fit the u16 field
        let raw_page_size = be16(bytes, 16);
        let page_size: u32 = if raw_page_size == 1 {
            65536
        } else {
            raw_page_size as u32
        };
        if page_size < 512 || !page_size.is_power_of_two() {
            return Err(SqliteError::malformed(
                1,
                format!("page size {page_size} is not a power of two of at least 512"),
            ));
        }

        let write_version = bytes[18];
        let read_version = bytes[19];
        if read_version > 2 || write_version > 2 {
            return Err(SqliteError::unsupported(format!(
                "file format versions {write_version}/{read_version} are newer than this reader"
            )));
        }
        let wal_mode = write_version == 2 || read_version == 2;

        let reserved_space = bytes[20];
        let usable = page_size as i64 - reserved_space as i64;
        if usable < 480 {
            return Err(SqliteError::malformed(
                1,
                format!(
                    "reserved space {reserved_space} leaves {usable} usable bytes per page, \
                     the format requires at least 480"
                ),
            ));
        }

        // the format fixes these three; anything else means the file was not
        // written by a sqlite that agrees with the documented format
        if bytes[21] != 64 || bytes[22] != 32 || bytes[23] != 32 {
            return Err(SqliteError::malformed(
                1,
                format!(
                    "payload fractions are {}/{}/{}, the format requires 64/32/32",
                    bytes[21], bytes[22], bytes[23]
                ),
            ));
        }

        let file_change_counter = be32(bytes, 24);
        let claimed_page_count = be32(bytes, 28);
        let version_valid_for = be32(bytes, 92);
        // the in-header page count only counts when it was written by the
        // same transaction that last changed the file; older sqlite versions
        // left it stale on purpose
        let header_page_count =
            if claimed_page_count != 0 && version_valid_for == file_change_counter {
                Some(claimed_page_count)
            } else {
                None
            };

        let schema_format = be32(bytes, 44);
        if !(1..=4).contains(&schema_format) {
            return Err(SqliteError::malformed(
                1,
                format!("schema format {schema_format} is not one of 1 to 4"),
            ));
        }

        let text_encoding = match be32(bytes, 56) {
            1 => TextEncoding::Utf8,
            2 => TextEncoding::Utf16Le,
            3 => TextEncoding::Utf16Be,
            other => {
                return Err(SqliteError::malformed(
                    1,
                    format!("text encoding {other} is not one of 1, 2 or 3"),
                ))
            }
        };
        if text_encoding != TextEncoding::Utf8 {
            return Err(SqliteError::unsupported(format!(
                "text encoding is {text_encoding:?}, this reader only handles utf-8"
            )));
        }

        // reserved for expansion, and the format says it must be zero. a
        // non zero value means either a newer format or a file that has been
        // tampered with, and both deserve a stop rather than a guess
        if bytes[72..92].iter().any(|b| *b != 0) {
            return Err(SqliteError::malformed(
                1,
                "the reserved header range 72..92 is not zero",
            ));
        }

        Ok(Header {
            page_size,
            reserved_space,
            file_change_counter,
            header_page_count,
            wal_mode,
            first_freelist_trunk: be32(bytes, 32),
            freelist_page_count: be32(bytes, 36),
            schema_cookie: be32(bytes, 40),
            schema_format,
            text_encoding,
            user_version: be32(bytes, 60) as i32,
            application_id: be32(bytes, 68) as i32,
            largest_root_page: be32(bytes, 52),
            sqlite_version_number: be32(bytes, 96),
        })
    }
}

fn be16(bytes: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([bytes[at], bytes[at + 1]])
}

fn be32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}
