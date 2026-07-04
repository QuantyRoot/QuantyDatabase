//! Error type for the storage core.
//!
//! Hand rolled instead of pulling in thiserror, see ADR-008. The important
//! invariant: a corrupted or hostile file must always surface as
//! `Error::Corrupted` or `Error::InvalidFormat`, never as a panic.

use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug)]
pub enum Error {
    /// Underlying I/O failed.
    Io(std::io::Error),
    /// The file claims to be a Quanty database but its content does not
    /// check out (bad checksum, impossible pointers, truncation).
    Corrupted { page: Option<u64>, reason: String },
    /// The file is not a Quanty database, or a version we cannot read.
    InvalidFormat(String),
    /// Attempted to write a page that was not allocated in the current
    /// write batch. Committed pages are immutable, that is the whole point.
    PageNotWritable(u64),
    /// Page id is outside the committed file, or points at a meta page.
    PageOutOfBounds(u64),
    /// Bad caller-supplied option, e.g. an invalid page size.
    InvalidArgument(&'static str),
}

impl Error {
    pub(crate) fn corrupted(page: impl Into<Option<u64>>, reason: impl Into<String>) -> Self {
        Error::Corrupted {
            page: page.into(),
            reason: reason.into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::Corrupted {
                page: Some(p),
                reason,
            } => {
                write!(f, "corrupted database (page {p}): {reason}")
            }
            Error::Corrupted { page: None, reason } => {
                write!(f, "corrupted database: {reason}")
            }
            Error::InvalidFormat(reason) => write!(f, "invalid format: {reason}"),
            Error::PageNotWritable(p) => {
                write!(
                    f,
                    "page {p} is not writable in this batch (committed pages are immutable)"
                )
            }
            Error::PageOutOfBounds(p) => write!(f, "page {p} is out of bounds"),
            Error::InvalidArgument(reason) => write!(f, "invalid argument: {reason}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
