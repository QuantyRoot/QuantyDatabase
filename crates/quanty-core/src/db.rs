//! The key-value database API on top of the pager and the B-tree.
//!
//! This is the surface QQL runs on: a transactional, snapshot-capable
//! ordered map with git-shaped history. Commits form a DAG, branches are
//! named pointers into it, any retained commit can be read as a snapshot,
//! and garbage collection reclaims whatever falls out of retention.
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
//!
//! db.create_branch("experiment", None).unwrap();
//! db.switch_branch("experiment").unwrap();
//! ```

use std::collections::HashSet;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::RwLock;

use crate::btree::{self, Scan};
use crate::commit::{self, CommitInfo};
use crate::error::{Error, Result};
use crate::page::{PageId, NIL_PAGE};
use crate::pager::{Pager, PagerOptions, WriteBatch};
use crate::refs::{self, BranchRef, DEFAULT_BRANCH};
use crate::storage::{FileStorage, MemStorage, Storage};

pub struct Db<S: Storage> {
    pager: Pager<S>,
    /// Name of the branch new transactions commit to. Cached here, the
    /// durable copy lives under the "head" key in the refs tree.
    current: RwLock<String>,
}

/// What a garbage collection run did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GcReport {
    /// Commits that fell out of retention this run.
    pub pruned_commits: u64,
    /// Pages returned to the free list.
    pub freed_pages: u64,
    /// Total pages in the file. GC does not shrink the file; freed pages
    /// get reused by later commits, which is what stops it growing.
    pub page_count: u64,
}

/// Diagnostic counters, the raw material of a future `quanty check`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DbStats {
    pub page_count: u64,
    /// Pages reachable from the current branch head (data + catalog trees
    /// plus the head's commit record).
    pub head_pages: u64,
    /// Pages currently listed in the free list.
    pub free_pages: u64,
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
        let pager = Pager::create(storage, options)?;
        Ok(Db {
            pager,
            current: RwLock::new(DEFAULT_BRANCH.to_string()),
        })
    }

    pub fn open(storage: S, options: PagerOptions) -> Result<Self> {
        let pager = Pager::open(storage, options)?;
        let current = read_head_name(&pager)?.unwrap_or_else(|| DEFAULT_BRANCH.to_string());
        Ok(Db {
            pager,
            current: RwLock::new(current),
        })
    }

    // -----------------------------------------------------------------
    // branches
    // -----------------------------------------------------------------

    /// Name of the branch new transactions commit to.
    pub fn current_branch(&self) -> String {
        self.current.read().clone()
    }

    /// Id of the newest commit on the current branch. 0 means the branch
    /// sits at the empty initial state.
    pub fn head_commit(&self) -> u64 {
        self.branch_head(&self.current_branch())
            .map(|r| r.head_id)
            .unwrap_or(0)
    }

    /// All branches with their heads, sorted by name. A database that
    /// never branched reports the single implicit branch "main".
    pub fn branches(&self) -> Result<Vec<(String, BranchRef)>> {
        let meta = self.pager.committed_meta();
        if meta.refs_root == NIL_PAGE {
            return Ok(vec![(DEFAULT_BRANCH.to_string(), self.implicit_ref())]);
        }
        let prefix = refs::branches_prefix();
        let end = prefix_successor(&prefix);
        let mut out = Vec::new();
        for item in Scan::new(&self.pager, meta.refs_root, Some(&prefix), end.as_deref())? {
            let (key, value) = item?;
            let mut parts = crate::encoding::decode_key(&key)?;
            let Some(crate::encoding::Value::Text(name)) = parts.pop() else {
                return Err(Error::corrupted(None, "branch key does not end in a name"));
            };
            out.push((name, BranchRef::decode(&value)?));
        }
        Ok(out)
    }

    /// Create a branch pointing at commit `at`, or at the current branch
    /// head. The new branch is not switched to.
    pub fn create_branch(&self, name: &str, at: Option<u64>) -> Result<()> {
        refs::validate_name(name)?;
        let target = match at {
            None => self.branch_head(&self.current_branch())?,
            Some(0) => BranchRef {
                head_id: 0,
                head_page: NIL_PAGE,
                floor_id: 0,
            },
            Some(id) => {
                let found = self.find_commit(id)?;
                BranchRef {
                    head_id: id,
                    head_page: found.page,
                    floor_id: found.floor,
                }
            }
        };
        let mut batch = self.pager.begin();
        let mut root = self.materialize_refs(&mut batch)?;
        if btree::get(&batch, root, &refs::branch_key(name))?.is_some() {
            return Err(Error::InvalidArgument(
                "a branch with this name already exists",
            ));
        }
        root = btree::put(&mut batch, root, &refs::branch_key(name), &target.encode())?;
        batch.set_refs_root(root);
        batch.commit()?;
        Ok(())
    }

    /// Make `name` the branch new transactions commit to.
    pub fn switch_branch(&self, name: &str) -> Result<()> {
        refs::validate_name(name)?;
        let mut batch = self.pager.begin();
        let mut root = self.materialize_refs(&mut batch)?;
        if btree::get(&batch, root, &refs::branch_key(name))?.is_none() {
            return Err(Error::InvalidArgument("no branch with this name"));
        }
        root = btree::put(&mut batch, root, &refs::head_key(), name.as_bytes())?;
        batch.set_refs_root(root);
        batch.commit()?;
        *self.current.write() = name.to_string();
        Ok(())
    }

    /// Delete a branch pointer. Its exclusive commits become unreachable
    /// and are reclaimed by the next garbage collection.
    pub fn drop_branch(&self, name: &str) -> Result<()> {
        if name == self.current_branch() {
            return Err(Error::InvalidArgument("cannot drop the current branch"));
        }
        let mut batch = self.pager.begin();
        let root = self.materialize_refs(&mut batch)?;
        let (root, existed) = btree::remove(&mut batch, root, &refs::branch_key(name))?;
        if !existed {
            return Err(Error::InvalidArgument("no branch with this name"));
        }
        batch.set_refs_root(root);
        batch.commit()?;
        Ok(())
    }

    /// Fast-forward the current branch to the head of `from`. Errors when
    /// the branches have diverged; real merges need conflict handling and
    /// are out of scope until then.
    pub fn merge_ff(&self, from: &str) -> Result<u64> {
        let current_name = self.current_branch();
        if from == current_name {
            return Err(Error::InvalidArgument("cannot merge a branch into itself"));
        }
        let ours = self.branch_head(&current_name)?;
        let theirs = self.branch_head(from)?;
        if ours.head_id == theirs.head_id {
            return Ok(ours.head_id); // already up to date
        }
        if !self.is_ancestor(ours.head_id, &theirs)? {
            return Err(Error::InvalidArgument(
                "branches have diverged; only fast-forward merges are supported for now",
            ));
        }
        let mut batch = self.pager.begin();
        let mut root = self.materialize_refs(&mut batch)?;
        let updated = BranchRef {
            head_id: theirs.head_id,
            head_page: theirs.head_page,
            floor_id: ours.floor_id.max(theirs.floor_id),
        };
        root = btree::put(
            &mut batch,
            root,
            &refs::branch_key(&current_name),
            &updated.encode(),
        )?;
        batch.set_refs_root(root);
        batch.commit()?;
        Ok(updated.head_id)
    }

    /// Is `ancestor_id` on the parent chain of `head`, or equal to it?
    fn is_ancestor(&self, ancestor_id: u64, head: &BranchRef) -> Result<bool> {
        if ancestor_id == 0 {
            return Ok(true); // the empty state is everyone's ancestor
        }
        Ok(commit::find(&self.pager, head.head_page, head.floor_id, ancestor_id)?.is_some())
    }

    // -----------------------------------------------------------------
    // transactions and snapshots
    // -----------------------------------------------------------------

    /// Start a write transaction on the current branch. Single writer:
    /// blocks while another transaction is open.
    pub fn begin(&self) -> WriteTx<'_, S> {
        let branch = self.current_branch();
        let batch = self.pager.begin();
        let head = match self.branch_ref_in(&batch, &branch) {
            Ok(Some(r)) => r,
            _ => self.implicit_ref(),
        };
        // roots come from the branch head, which equals the newest meta
        // only when this branch was the last one written to
        let (root, catalog_root) = if head.head_id == batch.base_meta().txid {
            (batch.base_meta().data_root, batch.base_meta().catalog_root)
        } else if head.head_id == 0 {
            (NIL_PAGE, NIL_PAGE)
        } else {
            match commit::read_record(&batch, head.head_page) {
                Ok(rec) => (rec.data_root, rec.catalog_root),
                Err(_) => (batch.base_meta().data_root, batch.base_meta().catalog_root),
            }
        };
        WriteTx {
            root,
            catalog_root,
            branch,
            parent: head,
            batch,
        }
    }

    /// A read snapshot of the current branch head.
    pub fn snapshot(&self) -> Snapshot<'_, S> {
        let meta = self.pager.committed_meta();
        let head = self
            .branch_head(&self.current_branch())
            .unwrap_or_else(|_| self.implicit_ref());
        if head.head_id != meta.txid && head.head_id != 0 {
            if let Ok(rec) = commit::read_record(&self.pager, head.head_page) {
                return Snapshot {
                    pager: &self.pager,
                    root: rec.data_root,
                    catalog_root: rec.catalog_root,
                    commit_id: head.head_id,
                };
            }
        }
        if head.head_id == 0 {
            return Snapshot {
                pager: &self.pager,
                root: NIL_PAGE,
                catalog_root: NIL_PAGE,
                commit_id: 0,
            };
        }
        Snapshot {
            pager: &self.pager,
            root: meta.data_root,
            catalog_root: meta.catalog_root,
            commit_id: meta.txid,
        }
    }

    /// A read snapshot of an arbitrary retained commit, searched across
    /// all branches. Commit id 0 is the empty database every file starts
    /// as.
    pub fn snapshot_at(&self, commit_id: u64) -> Result<Snapshot<'_, S>> {
        if commit_id == 0 {
            return Ok(Snapshot {
                pager: &self.pager,
                root: NIL_PAGE,
                catalog_root: NIL_PAGE,
                commit_id: 0,
            });
        }
        let found = self.find_commit(commit_id)?;
        Ok(Snapshot {
            pager: &self.pager,
            root: found.info.data_root,
            catalog_root: found.info.catalog_root,
            commit_id,
        })
    }

    /// The newest commit on the current branch with a timestamp at or
    /// before `unix_ts_ms`. Resolves to the empty state when the branch
    /// has no commit that old and its full history is retained.
    pub fn snapshot_at_time(&self, unix_ts_ms: u64) -> Result<Snapshot<'_, S>> {
        let head = self.branch_head(&self.current_branch())?;
        if head.head_id == 0 {
            return self.snapshot_at(0);
        }
        match commit::find_at_time(&self.pager, head.head_page, head.floor_id, unix_ts_ms)? {
            Some(rec) => Ok(Snapshot {
                pager: &self.pager,
                root: rec.data_root,
                catalog_root: rec.catalog_root,
                commit_id: rec.commit_id,
            }),
            None if head.floor_id == 0 => self.snapshot_at(0),
            None => Err(Error::InvalidArgument(
                "no commit that old is retained on this branch (raise the gc retention)",
            )),
        }
    }

    /// History of the current branch, newest first, down to its retention
    /// floor.
    pub fn log(&self) -> Result<Vec<CommitInfo>> {
        let head = self.branch_head(&self.current_branch())?;
        let mut out = Vec::new();
        let mut at = head.head_page;
        while at != NIL_PAGE {
            let rec = commit::read_record(&self.pager, at)?;
            let stop = head.floor_id != 0 && rec.commit_id <= head.floor_id;
            at = rec.parent_page;
            out.push(rec);
            if stop {
                break;
            }
        }
        Ok(out)
    }

    // -----------------------------------------------------------------
    // garbage collection
    // -----------------------------------------------------------------

    /// Reclaim pages of commits outside retention. `retain` is the number
    /// of commits kept per branch, counting the head; at least one.
    ///
    /// Takes `&mut self` on purpose: outstanding snapshots and
    /// transactions borrow the database, so the borrow checker proves
    /// reader quiescence at compile time and a reused page can never pull
    /// the rug out from under a live snapshot.
    pub fn gc(&mut self, retain: usize) -> Result<GcReport> {
        if retain == 0 {
            return Err(Error::InvalidArgument(
                "gc must retain at least one commit per branch",
            ));
        }
        let base = self.pager.committed_meta();
        let branches = self.branches()?;

        // mark: everything the retained commits can reach stays
        let mut marked: HashSet<PageId> = HashSet::new();
        let mut new_floors: Vec<(String, BranchRef)> = Vec::new();
        let mut pruned = 0u64;
        for (name, branch) in &branches {
            let mut at = branch.head_page;
            let mut kept = 0usize;
            let mut floor = branch.floor_id;
            while at != NIL_PAGE {
                let rec = commit::read_record(&self.pager, at)?;
                if kept < retain {
                    // seen from another branch already? marking is
                    // idempotent, floors stay per branch
                    marked.insert(at);
                    btree::collect_pages(&self.pager, rec.data_root, &mut marked)?;
                    btree::collect_pages(&self.pager, rec.catalog_root, &mut marked)?;
                    kept += 1;
                    floor = rec.commit_id;
                } else {
                    pruned += 1;
                }
                if branch.floor_id != 0 && rec.commit_id <= branch.floor_id {
                    break;
                }
                at = rec.parent_page;
            }
            new_floors.push((
                name.clone(),
                BranchRef {
                    head_id: branch.head_id,
                    head_page: branch.head_page,
                    floor_id: floor,
                },
            ));
        }

        // sweep: within the committed file, everything unmarked is free.
        // set_freelist turns reuse off for this whole batch, so every page
        // the batch itself writes (refs updates, the new chain) lands past
        // base.page_count and can never collide with the swept set.
        let mut batch = self.pager.begin();
        let free: Vec<PageId> = (2..base.page_count)
            .filter(|id| !marked.contains(id))
            .collect();
        let freed_pages = free.len() as u64;
        batch.set_freelist(free);

        // persist the raised floors so no lookup ever walks into a
        // reclaimed record
        let mut root = self.materialize_refs_with(&mut batch, &branches)?;
        for (name, updated) in &new_floors {
            root = btree::put(&mut batch, root, &refs::branch_key(name), &updated.encode())?;
        }
        batch.set_refs_root(root);
        batch.commit()?;

        Ok(GcReport {
            pruned_commits: pruned,
            freed_pages,
            page_count: base.page_count,
        })
    }

    /// Diagnostic counters.
    pub fn stats(&self) -> Result<DbStats> {
        let meta = self.pager.committed_meta();
        let head = self
            .branch_head(&self.current_branch())
            .unwrap_or_else(|_| self.implicit_ref());
        let mut pages: HashSet<PageId> = HashSet::new();
        if head.head_id == meta.txid {
            btree::collect_pages(&self.pager, meta.data_root, &mut pages)?;
            btree::collect_pages(&self.pager, meta.catalog_root, &mut pages)?;
            if meta.commit_page != NIL_PAGE {
                pages.insert(meta.commit_page);
            }
        } else if head.head_id != 0 {
            let rec = commit::read_record(&self.pager, head.head_page)?;
            pages.insert(head.head_page);
            btree::collect_pages(&self.pager, rec.data_root, &mut pages)?;
            btree::collect_pages(&self.pager, rec.catalog_root, &mut pages)?;
        }
        let mut free_pages = 0u64;
        let mut chain = meta.freelist_root;
        while chain != NIL_PAGE {
            let buf = self.pager.read_page(chain)?;
            let (next, ids) = crate::freelist::decode_page(&buf, chain)?;
            free_pages += ids.len() as u64;
            chain = next;
        }
        Ok(DbStats {
            page_count: meta.page_count,
            head_pages: pages.len() as u64,
            free_pages,
        })
    }

    // -----------------------------------------------------------------
    // internals
    // -----------------------------------------------------------------

    /// The single implicit branch of a database that never branched: the
    /// newest commit, full history retained.
    fn implicit_ref(&self) -> BranchRef {
        let meta = self.pager.committed_meta();
        BranchRef {
            head_id: meta.txid,
            head_page: meta.commit_page,
            floor_id: 0,
        }
    }

    fn branch_head(&self, name: &str) -> Result<BranchRef> {
        let refs_root = self.pager.committed_meta().refs_root;
        if refs_root == NIL_PAGE {
            if name == DEFAULT_BRANCH {
                return Ok(self.implicit_ref());
            }
            return Err(Error::InvalidArgument("no branch with this name"));
        }
        match btree::get(&self.pager, refs_root, &refs::branch_key(name))? {
            Some(bytes) => BranchRef::decode(&bytes),
            None => Err(Error::InvalidArgument("no branch with this name")),
        }
    }

    fn branch_ref_in(&self, batch: &WriteBatch<'_, S>, name: &str) -> Result<Option<BranchRef>> {
        let refs_root = batch.base_meta().refs_root;
        if refs_root == NIL_PAGE {
            return Ok(None);
        }
        match btree::get(batch, refs_root, &refs::branch_key(name))? {
            Some(bytes) => Ok(Some(BranchRef::decode(&bytes)?)),
            None => Ok(None),
        }
    }

    /// Ensure the refs tree exists inside this batch, seeding it with the
    /// implicit branch and the head pointer on first use. Returns the
    /// working refs root for further writes in the same batch.
    fn materialize_refs(&self, batch: &mut WriteBatch<'_, S>) -> Result<PageId> {
        let branches = self.branches()?;
        self.materialize_refs_with(batch, &branches)
    }

    fn materialize_refs_with(
        &self,
        batch: &mut WriteBatch<'_, S>,
        branches: &[(String, BranchRef)],
    ) -> Result<PageId> {
        let mut root = batch.base_meta().refs_root;
        if root != NIL_PAGE {
            return Ok(root);
        }
        for (name, branch) in branches {
            root = btree::put(batch, root, &refs::branch_key(name), &branch.encode())?;
        }
        let current = self.current_branch();
        root = btree::put(batch, root, &refs::head_key(), current.as_bytes())?;
        Ok(root)
    }

    /// Locate a commit record by id across all branches.
    fn find_commit(&self, commit_id: u64) -> Result<FoundCommit> {
        for (_, branch) in self.branches()? {
            if branch.head_id == 0 {
                continue;
            }
            let mut at = branch.head_page;
            while at != NIL_PAGE {
                let rec = commit::read_record(&self.pager, at)?;
                if rec.commit_id == commit_id {
                    return Ok(FoundCommit {
                        info: rec,
                        page: at,
                        floor: branch.floor_id,
                    });
                }
                if rec.commit_id < commit_id
                    || (branch.floor_id != 0 && rec.commit_id <= branch.floor_id)
                {
                    break; // ids only shrink down the edge; floor is the wall
                }
                at = rec.parent_page;
            }
        }
        Err(Error::InvalidArgument(
            "no such commit id (it may have been garbage collected)",
        ))
    }
}

struct FoundCommit {
    info: CommitInfo,
    page: PageId,
    floor: u64,
}

fn read_head_name<S: Storage>(pager: &Pager<S>) -> Result<Option<String>> {
    let refs_root = pager.committed_meta().refs_root;
    if refs_root == NIL_PAGE {
        return Ok(None);
    }
    match btree::get(pager, refs_root, &refs::head_key())? {
        Some(bytes) => Ok(Some(String::from_utf8(bytes).map_err(|_| {
            Error::corrupted(None, "head ref is not a valid branch name")
        })?)),
        None => Ok(None),
    }
}

/// Smallest byte string greater than every extension of `prefix`.
fn prefix_successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut out = prefix.to_vec();
    while let Some(&last) = out.last() {
        if last == 0xFF {
            out.pop();
        } else {
            *out.last_mut().expect("non-empty") += 1;
            return Some(out);
        }
    }
    None
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

/// A write transaction on one branch. Reads see the transaction's own
/// writes. Dropping without committing discards everything.
pub struct WriteTx<'db, S: Storage> {
    batch: WriteBatch<'db, S>,
    root: PageId,
    catalog_root: PageId,
    branch: String,
    parent: BranchRef,
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

    /// Write the commit record, advance the branch, make everything
    /// durable. Returns the commit id.
    pub fn commit(mut self) -> Result<u64> {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        let record = commit::write_record(
            &mut self.batch,
            self.parent.head_id,
            self.parent.head_page,
            self.root,
            self.catalog_root,
            ts,
        )?;
        let commit_id = self.pending_commit_id();
        self.batch.set_data_root(self.root);
        self.batch.set_catalog_root(self.catalog_root);
        self.batch.set_commit_page(record);

        // once refs exist, the branch pointer advances in the same commit
        let refs_root = self.batch.base_meta().refs_root;
        if refs_root != NIL_PAGE {
            let updated = BranchRef {
                head_id: commit_id,
                head_page: record,
                floor_id: self.parent.floor_id,
            };
            let new_root = btree::put(
                &mut self.batch,
                refs_root,
                &refs::branch_key(&self.branch),
                &updated.encode(),
            )?;
            self.batch.set_refs_root(new_root);
        }
        self.batch.commit()
    }
}
