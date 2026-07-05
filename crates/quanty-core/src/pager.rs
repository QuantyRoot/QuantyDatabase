//! The pager.
//!
//! Owns the file layout, the commit protocol and the copy-on-write
//! discipline. Everything above this layer (B-tree, catalog, blobs) only
//! ever sees three operations: read a committed page, allocate a fresh page
//! inside a write batch, commit the batch.
//!
//! Commit protocol (see docs/FORMAT.md):
//!
//! 1. seal every dirty page (stamp lsn, compute checksum), write it out
//! 2. fsync
//! 3. encode the new meta with txid+1 into slot `txid % 2`, write it
//! 4. fsync
//!
//! A crash before step 4 completes leaves the previous meta untouched and
//! the previous commit fully intact, because dirty pages only ever go to
//! page ids nothing references: fresh ids past the end of the file, or ids
//! listed in the free list, which by construction (see freelist.rs) are
//! referenced by nothing at all. That rule is enforced here, not merely
//! hoped for: `WriteBatch::page_mut` refuses to touch anything that was
//! not allocated in the current batch.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::{Mutex, MutexGuard, RwLock};

use crate::cache::PageCache;
use crate::error::{Error, Result};
use crate::freelist;
use crate::meta::{self, Meta};
use crate::page::{self, PageId, PageType, DEFAULT_PAGE_SIZE};
use crate::storage::Storage;

#[derive(Debug, Clone)]
pub struct PagerOptions {
    /// Page size in bytes. Power of two, 512 to 65536. Fixed at creation
    /// time and stored in the file, ignored when opening.
    pub page_size: u32,
    /// Page cache capacity in pages.
    pub cache_pages: usize,
}

impl Default for PagerOptions {
    fn default() -> Self {
        PagerOptions {
            page_size: DEFAULT_PAGE_SIZE,
            cache_pages: 1024,
        }
    }
}

pub struct Pager<S: Storage> {
    storage: S,
    page_size: u32,
    cache: PageCache,
    /// Meta of the most recent commit. Readers snapshot this.
    committed: RwLock<Meta>,
    /// Single writer for now (ADR-003). Holding this token is what makes a
    /// `WriteBatch` exclusive.
    writer: Mutex<()>,
}

impl<S: Storage> Pager<S> {
    /// Initialize a brand new database on empty storage.
    pub fn create(storage: S, options: PagerOptions) -> Result<Self> {
        if !page::valid_page_size(options.page_size) {
            return Err(Error::InvalidArgument(
                "page size must be a power of two between 512 and 65536",
            ));
        }
        if !storage.is_empty()? {
            return Err(Error::InvalidArgument(
                "refusing to create over non-empty storage",
            ));
        }

        let meta = Meta {
            page_size: options.page_size,
            txid: 0,
            data_root: page::NIL_PAGE,
            catalog_root: page::NIL_PAGE,
            freelist_root: page::NIL_PAGE,
            page_count: 2,
            unix_ts_ms: now_ms(),
            commit_page: page::NIL_PAGE,
            refs_root: page::NIL_PAGE,
        };

        // Both slots start out identical so recovery always finds a valid
        // meta no matter which slot the first real commit lands in.
        let mut buf = vec![0u8; options.page_size as usize];
        meta.encode(&mut buf);
        storage.write_at(0, &buf)?;
        storage.write_at(options.page_size as u64, &buf)?;
        storage.sync()?;

        Ok(Pager {
            storage,
            page_size: options.page_size,
            cache: PageCache::new(options.cache_pages),
            committed: RwLock::new(meta),
            writer: Mutex::new(()),
        })
    }

    /// Open an existing database, recovering the newest valid meta.
    pub fn open(storage: S, options: PagerOptions) -> Result<Self> {
        let meta = meta::recover(&storage)?;
        Ok(Pager {
            page_size: meta.page_size,
            cache: PageCache::new(options.cache_pages),
            committed: RwLock::new(meta),
            storage,
            writer: Mutex::new(()),
        })
    }

    pub fn page_size(&self) -> u32 {
        self.page_size
    }

    /// Meta of the most recent commit.
    pub fn committed_meta(&self) -> Meta {
        self.committed.read().clone()
    }

    /// Read a committed page, checksum verified, served from cache when hot.
    pub fn read_page(&self, id: PageId) -> Result<Arc<[u8]>> {
        let page_count = self.committed.read().page_count;
        if id < 2 || id >= page_count {
            return Err(Error::PageOutOfBounds(id));
        }
        if let Some(buf) = self.cache.get(id) {
            return Ok(buf);
        }
        let mut buf = vec![0u8; self.page_size as usize];
        self.storage.read_at(id * self.page_size as u64, &mut buf)?;
        page::verify(&buf, id)?;
        let buf: Arc<[u8]> = Arc::from(buf.into_boxed_slice());
        self.cache.insert(id, Arc::clone(&buf));
        Ok(buf)
    }

    /// Start a write batch. Blocks until any other writer is done.
    pub fn begin(&self) -> WriteBatch<'_, S> {
        let guard = self.writer.lock();
        let base = self.committed.read().clone();
        WriteBatch {
            pager: self,
            _guard: guard,
            next_page: base.page_count,
            data_root: base.data_root,
            catalog_root: base.catalog_root,
            freelist_root: base.freelist_root,
            commit_page: base.commit_page,
            refs_root: base.refs_root,
            reuse_pool: Vec::new(),
            chain_cursor: base.freelist_root,
            deferred_free: Vec::new(),
            touched_freelist: false,
            allow_reuse: true,
            freelist_override: None,
            base,
            dirty: BTreeMap::new(),
        }
    }
}

/// An exclusive write transaction at the page level.
///
/// Dropping a batch without committing discards it completely: nothing it
/// allocated or wrote is reachable from any meta, so it never existed as far
/// as the database is concerned.
pub struct WriteBatch<'p, S: Storage> {
    pager: &'p Pager<S>,
    _guard: MutexGuard<'p, ()>,
    base: Meta,
    next_page: PageId,
    dirty: BTreeMap<PageId, Box<[u8]>>,
    data_root: PageId,
    catalog_root: PageId,
    freelist_root: PageId,
    commit_page: PageId,
    refs_root: PageId,
    /// Page ids ready for reuse, taken from consumed free list pages.
    reuse_pool: Vec<PageId>,
    /// Next unread page of the committed free list chain.
    chain_cursor: PageId,
    /// Chain pages this batch consumed. They are still referenced by the
    /// previous meta until the commit point, so they must not be reused
    /// now; they get listed in the free list this batch writes instead.
    deferred_free: Vec<PageId>,
    touched_freelist: bool,
    /// Garbage collection turns reuse off so its own writes cannot land
    /// on pages whose freedom it is in the middle of deciding.
    allow_reuse: bool,
    /// When set, the free list written at commit is exactly this list.
    freelist_override: Option<Vec<PageId>>,
}

impl<S: Storage> WriteBatch<'_, S> {
    /// Allocate a page with an initialized header, reusing a free page
    /// when one is available and extending the file otherwise. Only pages
    /// allocated here are writable in this batch.
    pub fn allocate(&mut self, page_type: PageType) -> PageId {
        let id = self.take_page_id();
        let mut buf = vec![0u8; self.pager.page_size as usize].into_boxed_slice();
        page::init_header(&mut buf, page_type);
        self.dirty.insert(id, buf);
        id
    }

    fn take_page_id(&mut self) -> PageId {
        if self.allow_reuse {
            if self.reuse_pool.is_empty() && self.chain_cursor != page::NIL_PAGE {
                // pull the next chain page into the pool; the chain page
                // itself becomes free, but only for later transactions
                if let Ok(buf) = self.pager.read_page(self.chain_cursor) {
                    if let Ok((next, ids)) = freelist::decode_page(&buf, self.chain_cursor) {
                        self.reuse_pool.extend(ids);
                        self.deferred_free.push(self.chain_cursor);
                        self.chain_cursor = next;
                        self.touched_freelist = true;
                    } else {
                        // a broken chain is corruption, but allocation has
                        // no error path; stop consuming and let the next
                        // read of that page report it properly
                        self.chain_cursor = page::NIL_PAGE;
                    }
                } else {
                    self.chain_cursor = page::NIL_PAGE;
                }
            }
            if let Some(id) = self.reuse_pool.pop() {
                return id;
            }
        }
        let id = self.next_page;
        self.next_page += 1;
        id
    }

    /// Replace the committed free list wholesale. Garbage collection uses
    /// this after computing the exact free set. The batch keeps allocating
    /// normally; at commit time the final list is the given set minus
    /// every page this batch ended up writing, so nothing in-use can be
    /// listed as free.
    pub fn set_freelist(&mut self, ids: Vec<PageId>) {
        self.freelist_override = Some(ids);
        self.touched_freelist = true;
    }

    /// Mutable access to a page allocated in this batch. Refuses committed
    /// pages: copy-on-write is a hard rule, not a convention.
    pub fn page_mut(&mut self, id: PageId) -> Result<&mut [u8]> {
        match self.dirty.get_mut(&id) {
            Some(buf) => Ok(buf),
            None => Err(Error::PageNotWritable(id)),
        }
    }

    /// Read a page as visible to this batch: its own dirty pages first,
    /// committed pages otherwise.
    pub fn read_page(&self, id: PageId) -> Result<Arc<[u8]>> {
        if let Some(buf) = self.dirty.get(&id) {
            return Ok(Arc::from(buf.clone()));
        }
        if id < 2 || id >= self.base.page_count {
            return Err(Error::PageOutOfBounds(id));
        }
        self.pager.read_page(id)
    }

    pub fn set_data_root(&mut self, root: PageId) {
        self.data_root = root;
    }

    pub fn set_catalog_root(&mut self, root: PageId) {
        self.catalog_root = root;
    }

    pub fn set_freelist_root(&mut self, root: PageId) {
        self.freelist_root = root;
    }

    /// Point the meta at the commit record page for this transaction.
    pub fn set_commit_page(&mut self, page: PageId) {
        self.commit_page = page;
    }

    pub fn set_refs_root(&mut self, root: PageId) {
        self.refs_root = root;
    }

    /// The committed meta this batch is building on.
    pub fn base_meta(&self) -> &Meta {
        &self.base
    }

    /// Does this batch own (and may therefore rewrite) the given page?
    pub fn owns(&self, id: PageId) -> bool {
        self.dirty.contains_key(&id)
    }

    /// Page size of the underlying database.
    pub fn page_size(&self) -> u32 {
        self.pager.page_size
    }

    /// Number of pages allocated by this batch so far.
    pub fn allocated(&self) -> u64 {
        self.next_page - self.base.page_count
    }

    /// Persist the free list. Two sources feed it: the batch's own
    /// leftovers (unused reuse pool plus consumed chain pages) or, when
    /// garbage collection set an override, the full swept set minus every
    /// page this batch wrote.
    ///
    /// Chain pages holding the list are taken from ids that are both in
    /// the list and safe to write right now, meaning ids out of the reuse
    /// pool, which by the free list invariant nothing references. Only
    /// when those run out does the chain extend the file. Deferred pages
    /// (the previous chain) are listed but never written, because the
    /// previous meta still references them until the commit point.
    fn settle_freelist(&mut self) -> Result<()> {
        if !self.touched_freelist
            && self.deferred_free.is_empty()
            && self.freelist_override.is_none()
        {
            return Ok(()); // never looked at it, meta keeps the old root
        }
        let writable: Vec<PageId> = std::mem::take(&mut self.reuse_pool);
        let (mut remaining, tail) = match self.freelist_override.take() {
            Some(mut list) => {
                // the swept set includes the old chain and old refs pages;
                // they are listable (unreferenced once this meta lands)
                // but everything the batch wrote is in use and must go
                list.retain(|id| !self.dirty.contains_key(id));
                (list, page::NIL_PAGE)
            }
            None => {
                let mut ids = writable.clone();
                ids.append(&mut self.deferred_free);
                (ids, self.chain_cursor)
            }
        };
        // from here on allocation is settling only
        self.allow_reuse = false;
        self.chain_cursor = page::NIL_PAGE;
        self.deferred_free.clear();

        let writable: std::collections::HashSet<PageId> = writable.into_iter().collect();
        let per_page = freelist::ids_per_page(self.pager.page_size);
        let mut head = tail;
        while !remaining.is_empty() {
            // a chain page from the list itself when one is writable,
            // a fresh page otherwise
            let page_id = match remaining.iter().position(|id| writable.contains(id)) {
                Some(i) => {
                    let id = remaining.swap_remove(i);
                    let mut buf = vec![0u8; self.pager.page_size as usize].into_boxed_slice();
                    page::init_header(&mut buf, PageType::FreeList);
                    self.dirty.insert(id, buf);
                    id
                }
                None => self.allocate(PageType::FreeList),
            };
            let take = remaining.len().min(per_page);
            let chunk: Vec<PageId> = remaining.split_off(remaining.len() - take);
            freelist::encode_page(self.page_mut(page_id)?, head, &chunk);
            head = page_id;
        }
        self.freelist_root = head;
        Ok(())
    }

    /// Make the batch durable. Returns the new txid.
    pub fn commit(mut self) -> Result<u64> {
        let txid = self.base.txid + 1;
        let ps = self.pager.page_size as u64;

        self.settle_freelist()?;

        for root in [
            self.data_root,
            self.catalog_root,
            self.freelist_root,
            self.commit_page,
            self.refs_root,
        ] {
            if root != page::NIL_PAGE && (root < 2 || root >= self.next_page) {
                return Err(Error::InvalidArgument("root pointer outside the file"));
            }
        }

        // 1. seal and write every dirty page
        for (&id, buf) in self.dirty.iter_mut() {
            page::seal(buf, txid);
            self.pager.storage.write_at(id * ps, buf)?;
        }

        // 2. data must be durable before any meta points at it
        self.pager.storage.sync()?;

        // 3. write the new meta into the slot the previous commit is not in
        let new_meta = Meta {
            page_size: self.pager.page_size,
            txid,
            data_root: self.data_root,
            catalog_root: self.catalog_root,
            freelist_root: self.freelist_root,
            page_count: self.next_page,
            unix_ts_ms: now_ms(),
            commit_page: self.commit_page,
            refs_root: self.refs_root,
        };
        let mut meta_buf = vec![0u8; self.pager.page_size as usize];
        new_meta.encode(&mut meta_buf);
        self.pager.storage.write_at((txid % 2) * ps, &meta_buf)?;

        // 4. the commit point
        self.pager.storage.sync()?;

        // publish: cache the sealed pages, swap the committed meta
        for (id, buf) in std::mem::take(&mut self.dirty) {
            self.pager.cache.insert(id, Arc::from(buf));
        }
        *self.pager.committed.write() = new_meta;

        Ok(txid)
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
