//! Error type for the SQLite reader.
//!
//! Hand rolled like the core's, see ADR-008. The invariant this type exists
//! to serve: every input, including a deliberately hostile one, produces
//! either correct data or one of these errors. Never a panic, never a wrong
//! row.
//!
//! The split between `NotSqlite`, `Unsupported` and `Malformed` is a
//! statement about whose problem it is. `NotSqlite` means the file was never
//! ours to read. `Unsupported` means the file is fine and we are not good
//! enough yet, so it names the feature and is a to do list for us.
//! `Malformed` means the file claims to be a SQLite database and then
//! contradicts itself, which is the case an attacker controls, so those
//! carry the page number to make a fuzz failure reproducible.

use std::fmt;

pub type Result<T> = std::result::Result<T, SqliteError>;

#[derive(Debug)]
pub enum SqliteError {
    Io(std::io::Error),
    /// Not a SQLite database at all: wrong magic, or too short to hold a
    /// header.
    NotSqlite(String),
    /// A real SQLite database using something this reader cannot handle
    /// yet. Named, so the message tells the user what to do.
    Unsupported(String),
    /// The file is internally inconsistent: pointers out of range, sizes
    /// that do not add up, values the format forbids.
    Malformed {
        page: Option<u32>,
        reason: String,
    },
}

impl SqliteError {
    pub(crate) fn malformed(page: impl Into<Option<u32>>, reason: impl Into<String>) -> Self {
        SqliteError::Malformed {
            page: page.into(),
            reason: reason.into(),
        }
    }

    pub(crate) fn unsupported(reason: impl Into<String>) -> Self {
        SqliteError::Unsupported(reason.into())
    }

    pub(crate) fn not_sqlite(reason: impl Into<String>) -> Self {
        SqliteError::NotSqlite(reason.into())
    }
}

impl fmt::Display for SqliteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SqliteError::Io(e) => write!(f, "io error: {e}"),
            SqliteError::NotSqlite(reason) => write!(f, "not a sqlite database: {reason}"),
            SqliteError::Unsupported(reason) => write!(f, "unsupported sqlite file: {reason}"),
            SqliteError::Malformed {
                page: Some(p),
                reason,
            } => write!(f, "malformed sqlite database (page {p}): {reason}"),
            SqliteError::Malformed { page: None, reason } => {
                write!(f, "malformed sqlite database: {reason}")
            }
        }
    }
}

impl std::error::Error for SqliteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SqliteError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for SqliteError {
    fn from(e: std::io::Error) -> Self {
        SqliteError::Io(e)
    }
}
