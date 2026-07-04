//! Storage backends.
//!
//! The pager talks to a `Storage` trait, not to a file. That keeps the core
//! testable against an in-memory backend, keeps mmap optional (ADR-007) and
//! keeps a WASM build possible later. Offsets are absolute byte offsets,
//! callers are responsible for page alignment.

use std::fs::{File, OpenOptions};
use std::path::Path;

use parking_lot::RwLock;

use crate::error::{Error, Result};

pub trait Storage: Send + Sync {
    /// Read exactly `buf.len()` bytes at `offset`. Reading past the end of
    /// the storage is an error, short reads are never returned.
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()>;

    /// Write all of `buf` at `offset`, growing the storage if needed.
    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()>;

    /// Make everything written so far durable. Commit correctness depends
    /// on this actually reaching stable storage.
    fn sync(&self) -> Result<()>;

    /// Current size in bytes.
    fn len(&self) -> Result<u64>;

    fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }
}

// ---------------------------------------------------------------------------
// File backend
// ---------------------------------------------------------------------------

/// Plain file backend using positioned reads/writes. No mmap, no seeking,
/// safe to share across threads.
pub struct FileStorage {
    file: File,
}

impl FileStorage {
    /// Create a new file. Fails if the path already exists, on purpose:
    /// silently truncating a database is how data dies.
    pub fn create(path: impl AsRef<Path>) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        Ok(FileStorage { file })
    }

    /// Open an existing file read/write.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Ok(FileStorage { file })
    }
}

#[cfg(unix)]
impl Storage for FileStorage {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        use std::os::unix::fs::FileExt;
        self.file.read_exact_at(buf, offset)?;
        Ok(())
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        use std::os::unix::fs::FileExt;
        self.file.write_all_at(buf, offset)?;
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        self.file.sync_data()?;
        Ok(())
    }

    fn len(&self) -> Result<u64> {
        Ok(self.file.metadata()?.len())
    }
}

#[cfg(windows)]
impl Storage for FileStorage {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        use std::os::windows::fs::FileExt;
        let mut done = 0;
        while done < buf.len() {
            let n = self
                .file
                .seek_read(&mut buf[done..], offset + done as u64)?;
            if n == 0 {
                return Err(Error::Io(std::io::Error::from(
                    std::io::ErrorKind::UnexpectedEof,
                )));
            }
            done += n;
        }
        Ok(())
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        use std::os::windows::fs::FileExt;
        let mut done = 0;
        while done < buf.len() {
            let n = self.file.seek_write(&buf[done..], offset + done as u64)?;
            done += n;
        }
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        self.file.sync_data()?;
        Ok(())
    }

    fn len(&self) -> Result<u64> {
        Ok(self.file.metadata()?.len())
    }
}

// ---------------------------------------------------------------------------
// Memory backend
// ---------------------------------------------------------------------------

/// In-memory backend for tests and, later, ephemeral databases.
#[derive(Default)]
pub struct MemStorage {
    data: RwLock<Vec<u8>>,
}

impl MemStorage {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Storage for MemStorage {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let data = self.data.read();
        let start = usize::try_from(offset).map_err(|_| Error::PageOutOfBounds(offset))?;
        let end = start
            .checked_add(buf.len())
            .ok_or(Error::PageOutOfBounds(offset))?;
        if end > data.len() {
            return Err(Error::Io(std::io::Error::from(
                std::io::ErrorKind::UnexpectedEof,
            )));
        }
        buf.copy_from_slice(&data[start..end]);
        Ok(())
    }

    fn write_at(&self, offset: u64, buf: &[u8]) -> Result<()> {
        let mut data = self.data.write();
        let start = usize::try_from(offset).map_err(|_| Error::PageOutOfBounds(offset))?;
        let end = start
            .checked_add(buf.len())
            .ok_or(Error::PageOutOfBounds(offset))?;
        if end > data.len() {
            data.resize(end, 0);
        }
        data[start..end].copy_from_slice(buf);
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        Ok(())
    }

    fn len(&self) -> Result<u64> {
        Ok(self.data.read().len() as u64)
    }
}
