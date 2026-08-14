//! Where the bytes come from.
//!
//! This is deliberately not `quanty_core::Storage`. That trait can write,
//! and the file we are reading belongs to somebody else: it is their
//! database, produced by a different engine, and we are only ever a guest
//! in it. A trait without a write method makes damaging the source
//! impossible to express, rather than merely something we remember not to
//! do. It also lets the file backend open read only, so importing from a
//! read only mount or a file we lack write permission on works.
//!
//! The slice backend is what the fuzz harness drives, so the two paths that
//! matter are covered by the same code above this line.

use std::fs::File;
use std::path::Path;

use crate::error::{Result, SqliteError};

pub trait Source {
    /// Read exactly `buf.len()` bytes at `offset`. Reading past the end is
    /// an error; short reads are never returned.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()>;

    /// Size in bytes.
    fn len(&self) -> Result<u64>;

    fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Whether this source has already accounted for a write-ahead log.
    ///
    /// A wal mode database is only complete in its main file once the log
    /// has been checkpointed into it, and bytes alone cannot say whether
    /// that happened. A source that knows, because it looked next to the
    /// file or because it reads the log itself, says so here; everything
    /// else stays conservative and a wal mode database read through it is
    /// refused rather than silently truncated to an older version.
    fn accounts_for_wal(&self) -> bool {
        false
    }
}

/// A file opened read only.
pub struct FileSource {
    file: File,
    /// True when there is no log to account for: either no `-wal` file
    /// sits next to this one, or the one that does is empty, which is what
    /// sqlite leaves behind after a checkpoint.
    no_log_beside_it: bool,
}

impl FileSource {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let file = File::open(path)?;
        // sqlite names the log after the database, suffix and all
        let mut log = path.as_os_str().to_os_string();
        log.push("-wal");
        let no_log_beside_it = match std::fs::metadata(Path::new(&log)) {
            Ok(meta) => meta.len() == 0,
            Err(_) => true,
        };
        Ok(FileSource {
            file,
            no_log_beside_it,
        })
    }
}

#[cfg(unix)]
impl Source for FileSource {
    fn accounts_for_wal(&self) -> bool {
        self.no_log_beside_it
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        use std::os::unix::fs::FileExt;
        self.file.read_exact_at(buf, offset)?;
        Ok(())
    }

    fn len(&self) -> Result<u64> {
        Ok(self.file.metadata()?.len())
    }
}

#[cfg(windows)]
impl Source for FileSource {
    fn accounts_for_wal(&self) -> bool {
        self.no_log_beside_it
    }

    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        use std::os::windows::fs::FileExt;
        let mut done = 0;
        while done < buf.len() {
            let n = self
                .file
                .seek_read(&mut buf[done..], offset + done as u64)?;
            if n == 0 {
                return Err(SqliteError::Io(std::io::Error::from(
                    std::io::ErrorKind::UnexpectedEof,
                )));
            }
            done += n;
        }
        Ok(())
    }

    fn len(&self) -> Result<u64> {
        Ok(self.file.metadata()?.len())
    }
}

/// Bytes already in memory. Used by the tests and by the fuzz harness.
pub struct SliceSource<'a> {
    bytes: &'a [u8],
}

impl<'a> SliceSource<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        SliceSource { bytes }
    }
}

impl Source for SliceSource<'_> {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let start = usize::try_from(offset).map_err(|_| {
            SqliteError::Io(std::io::Error::from(std::io::ErrorKind::UnexpectedEof))
        })?;
        let end = start.checked_add(buf.len()).ok_or_else(|| {
            SqliteError::Io(std::io::Error::from(std::io::ErrorKind::UnexpectedEof))
        })?;
        if end > self.bytes.len() {
            return Err(SqliteError::Io(std::io::Error::from(
                std::io::ErrorKind::UnexpectedEof,
            )));
        }
        buf.copy_from_slice(&self.bytes[start..end]);
        Ok(())
    }

    fn len(&self) -> Result<u64> {
        Ok(self.bytes.len() as u64)
    }
}
