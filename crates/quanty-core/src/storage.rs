//! Storage backends.
//!
//! The pager talks to a `Storage` trait, not to a file. That keeps the core
//! testable against an in-memory backend, keeps mmap optional (ADR-007) and
//! keeps a WASM build possible later. Offsets are absolute byte offsets,
//! callers are responsible for page alignment.

use std::fs::{File, OpenOptions};
use std::path::Path;

use crate::sync::RwLock;

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
    /// Whether `claim` got the lock. Kept for the message it allows, not
    /// as licence to skip the commit guard: a locked writer and an
    /// unlocked reader coexist by design, so a locked handle is not
    /// provably alone and skipping the guard would let it overwrite the
    /// other's commit in silence.
    #[allow(dead_code)]
    locked: bool,
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
        Self::claim(file)
    }

    /// Open an existing file read/write, taking the writer lock.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Self::claim(file)
    }

    /// Open without taking the writer lock.
    ///
    /// For readers, which need no lock at all: a snapshot pins a commit
    /// and copy-on-write means the pages under it are never rewritten, so
    /// many readers alongside one writer is the model and always was. A
    /// shared lock would be worse than none, because it would conflict
    /// with the writer's.
    ///
    /// Also the way the commit guard is exercised, since with the lock in
    /// place two writing handles are no longer reachable by accident.
    /// This crate carries no stability promise (ADR-030), which is why an
    /// escape hatch may live here rather than behind a feature.
    pub fn open_unlocked(path: impl AsRef<Path>) -> Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Ok(FileStorage {
            file,
            locked: false,
        })
    }

    /// Take the writer lock, or say who has it.
    ///
    /// One writer per file is the model (ADR-035). Before this existed,
    /// two handles on one file each believed they were alone, computed
    /// the same transaction id and wrote the same meta slot, and the
    /// second silently replaced a commit that had been acknowledged.
    ///
    /// The lock is advisory and held for as long as the handle lives; the
    /// operating system drops it when the process ends, including when
    /// the process is killed, so a crash does not leave a database
    /// unopenable. It is also not everywhere: some network filesystems
    /// treat locking as a suggestion. The guard in `Pager::commit` stays
    /// for exactly that, and costs a page read per commit to turn a lost
    /// commit into a refused one where this cannot help.
    fn claim(file: File) -> Result<Self> {
        match file.try_lock() {
            Ok(()) => Ok(FileStorage { file, locked: true }),
            Err(std::fs::TryLockError::WouldBlock) => Err(Error::AlreadyOpen),
            // Locking is unsupported here, or the filesystem refused. That
            // is not a reason to refuse the database; it is a reason to
            // rely on the commit guard, which does not need the kernel's
            // help.
            Err(std::fs::TryLockError::Error(_)) => Ok(FileStorage {
                file,
                locked: false,
            }),
        }
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
