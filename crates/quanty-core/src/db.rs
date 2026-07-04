//! The key-value database API on top of the pager and the B-tree.
//!
//! This is the surface phase 2 grows the typed catalog and QQL on top of.
//! For now it is a transactional, snapshot-capable ordered map:
//!
//! ```
//! use quanty_core::{Db, MemStorage, PagerOptions};
//!
//! let db = Db::create(MemStorage::new(), PagerOptions::default()).unwrap();
//! let mut tx = db.begin();
//! tx.put(b"hello", b"world").unwrap();
//! let commit_id = tx.commit().unwrap();
//!
//! assert_eq!(db.snapshot().get(b"hello").unwrap().as_deref(), Some(&b"world"[..]));
//! assert!(db.snapshot_at(commit_id).is_ok());
//! ```

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::btree::{self, Scan};
use crate::commit::{self, CommitInfo};
use crate::error::Result;
use crate::page::{PageId, NIL_PAGE};
use crate::pager::{Pager, PagerOptions, WriteBatch};
use crate::storage::{FileStorage, MemStorage, Storage};

pub struct Db<S: Storage> {
    pager: Pager<S>,
}

impl Db<FileStorage> {
    /// Create a new single-file database. Fails if the path exists.
    pub fn create_file(path: impl AsRef<Path>) -> Result<Self> {
        Db::create(FileStorage::create(path)?, PagerOptions::default())
    }

    /// Open an existing database file.
    pub fn open_file(path: impl AsRef<Path>) -> Result<Self> {
        Db::open(FileStorage::open(path)?, PagerOptions::default())
    }
}

impl Db<MemStorage> {
    /// A fresh in-memory database, gone when dropped. Handy for tests.
    pub fn in_memory() -> Result<Self> {
        Db::create(MemStorage::new(), PagerOptions::default())
    }
}

impl<S: Storage> Db<S> {
    pub fn create(storage: S, options: PagerOptions) -> Result<Self> {
        Ok(Db {
            pager: Pager::create(storage, options)?,
        })
    }

    pub fn open(storage: S, options: PagerOptions) -> Result<Self> {
        Ok(Db {
            pager: Pager::open(storage, options)?,
        })
    }

    /// Id of the newest commit. 0 means the database is freshly created.
    pub fn head_commit(&self) -> u64 {
        self.pager.committed_meta().txid
    }

    /// Start a write transaction. Single writer: blocks while another
    /// transaction is open.
    pub fn begin(&self) -> WriteTx<'_, S> {
        let batch = self.pager.begin();
        let root = batch.base_meta().data_root;
        let catalog_root = batch.base_meta().catalog_root;
        WriteTx {
            root,
            catalog_root,
            batch,
        }
    }

    /// A read snapshot of the newest commit.
    pub fn snapshot(&self) -> Snapshot<'_, S> {
        let meta = self.pager.committed_meta();
        Snapshot {
            pager: &self.pager,
            root: meta.data_root,
            catalog_root: meta.catalog_root,
            commit_id: meta.txid,
        }
    }

    /// A read snapshot of an arbitrary past commit. Commit id 0 is the
    /// empty database every file starts as.
    pub fn snapshot_at(&self, commit_id: u64) -> Result<Snapshot<'_, S>> {
        if commit_id == 0 {
            return Ok(Snapshot {
                pager: &self.pager,
                root: NIL_PAGE,
                catalog_root: NIL_PAGE,
                commit_id: 0,
            });
        }
        let meta = self.pager.committed_meta();
        let rec = commit::find(&self.pager, meta.commit_page, commit_id)?;
        Ok(Snapshot {
            pager: &self.pager,
            root: rec.data_root,
            catalog_root: rec.catalog_root,
            commit_id,
        })
    }

    /// The commit history, newest first.
    pub fn log(&self) -> Result<Vec<CommitInfo>> {
        let mut out = Vec::new();
        let mut at = self.pager.committed_meta().commit_page;
        while at != NIL_PAGE {
            let rec = commit::read_record(&self.pager, at)?;
            at = rec.prev_commit_page;
            out.push(rec);
        }
        Ok(out)
    }
}

/// A read-only view of one commit. Cheap to create, never blocks writers,
/// stays valid for the lifetime of the `Db` borrow.
pub struct Snapshot<'db, S: Storage> {
    pager: &'db Pager<S>,
    root: PageId,
    catalog_root: PageId,
    commit_id: u64,
}

impl<S: Storage> Snapshot<'_, S> {
    pub fn commit_id(&self) -> u64 {
        self.commit_id
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        btree::get(self.pager, self.root, key)
    }

    /// Iterate `[start, end)` in key order; `None` means unbounded.
    pub fn scan(&self, start: Option<&[u8]>, end: Option<&[u8]>) -> Result<Scan<'_, Pager<S>>> {
        Scan::new(self.pager, self.root, start, end)
    }

    /// Point read in the catalog tree.
    pub fn catalog_get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        btree::get(self.pager, self.catalog_root, key)
    }

    /// Range scan over the catalog tree, same bounds semantics as `scan`.
    pub fn catalog_scan(
        &self,
        start: Option<&[u8]>,
        end: Option<&[u8]>,
    ) -> Result<Scan<'_, Pager<S>>> {
        Scan::new(self.pager, self.catalog_root, start, end)
    }
}

/// A write transaction. Reads see the transaction's own writes. Dropping
/// without committing discards everything.
pub struct WriteTx<'db, S: Storage> {
    batch: WriteBatch<'db, S>,
    root: PageId,
    catalog_root: PageId,
}

impl<S: Storage> WriteTx<'_, S> {
    /// The commit id this transaction will get if it commits.
    pub fn pending_commit_id(&self) -> u64 {
        self.batch.base_meta().txid + 1
    }

    pub fn put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.root = btree::put(&mut self.batch, self.root, key, value)?;
        Ok(())
    }

    /// Returns whether the key existed.
    pub fn delete(&mut self, key: &[u8]) -> Result<bool> {
        let (root, existed) = btree::remove(&mut self.batch, self.root, key)?;
        self.root = root;
        Ok(existed)
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        btree::get(&self.batch, self.root, key)
    }

    /// Iterate `[start, end)` including this transaction's own writes.
    pub fn scan(
        &self,
        start: Option<&[u8]>,
        end: Option<&[u8]>,
    ) -> Result<Scan<'_, WriteBatch<'_, S>>> {
        Scan::new(&self.batch, self.root, start, end)
    }

    /// The catalog tree, same operations as the data tree. Schema and data
    /// commit atomically together since both roots land in one meta.
    pub fn catalog_put(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.catalog_root = btree::put(&mut self.batch, self.catalog_root, key, value)?;
        Ok(())
    }

    pub fn catalog_delete(&mut self, key: &[u8]) -> Result<bool> {
        let (root, existed) = btree::remove(&mut self.batch, self.catalog_root, key)?;
        self.catalog_root = root;
        Ok(existed)
    }

    pub fn catalog_get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        btree::get(&self.batch, self.catalog_root, key)
    }

    pub fn catalog_scan(
        &self,
        start: Option<&[u8]>,
        end: Option<&[u8]>,
    ) -> Result<Scan<'_, WriteBatch<'_, S>>> {
        Scan::new(&self.batch, self.catalog_root, start, end)
    }

    /// Write the commit record and make everything durable. Returns the
    /// commit id.
    pub fn commit(mut self) -> Result<u64> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let record = commit::write_record(&mut self.batch, self.root, self.catalog_root, ts)?;
        self.batch.set_data_root(self.root);
        self.batch.set_catalog_root(self.catalog_root);
        self.batch.set_commit_page(record);
        self.batch.commit()
    }
}
