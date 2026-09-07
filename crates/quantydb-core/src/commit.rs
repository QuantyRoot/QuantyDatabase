//! Commit records.
//!
//! Every commit writes one commit record page describing itself and linking
//! to its parent's record. History is a DAG exactly like git: records are
//! the objects, branch heads in the refs tree are the pointers into it.
//! Lookups walk parent edges from branch heads down to each branch's
//! retention floor, never further, so pruned records are never touched.
//!
//! Body layout after the 16 byte page header (see docs/FORMAT.md):
//!
//! ```text
//! offset  size  field
//! 16      8     commit id (equals the txid that sealed this page)
//! 24      8     parent commit id (0 = the empty initial state)
//! 32      8     data root page at this commit
//! 40      8     catalog root page at this commit
//! 48      8     page of the parent's commit record (0 = none)
//! 56      8     wall clock, unix milliseconds
//! ```

use crate::error::{Error, Result};
use crate::page::{self, PageId, PageType, NIL_PAGE, PAGE_HEADER_LEN};
use crate::pager::WriteBatch;
use crate::storage::Storage;

use crate::btree::ReadPages;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitInfo {
    pub commit_id: u64,
    pub parent_id: u64,
    pub data_root: PageId,
    pub catalog_root: PageId,
    /// Page of the parent's commit record, the DAG edge.
    pub parent_page: PageId,
    pub unix_ts_ms: u64,
}

const OFF_ID: usize = PAGE_HEADER_LEN;
const OFF_PARENT: usize = OFF_ID + 8;
const OFF_DATA_ROOT: usize = OFF_PARENT + 8;
const OFF_CATALOG_ROOT: usize = OFF_DATA_ROOT + 8;
const OFF_PARENT_PAGE: usize = OFF_CATALOG_ROOT + 8;
const OFF_TS: usize = OFF_PARENT_PAGE + 8;

/// Write the commit record for the transaction the batch is about to
/// commit. The parent is the head of the branch being committed to, which
/// is the base txid on a linear history but an older commit when writing
/// on a branch that is not the newest.
pub(crate) fn write_record<S: Storage>(
    batch: &mut WriteBatch<'_, S>,
    parent_id: u64,
    parent_page: PageId,
    data_root: PageId,
    catalog_root: PageId,
    unix_ts_ms: u64,
) -> Result<PageId> {
    let commit_id = batch.base_meta().txid + 1;

    let id = batch.allocate(PageType::Commit);
    let buf = batch.page_mut(id)?;
    buf[OFF_ID..OFF_ID + 8].copy_from_slice(&commit_id.to_le_bytes());
    buf[OFF_PARENT..OFF_PARENT + 8].copy_from_slice(&parent_id.to_le_bytes());
    buf[OFF_DATA_ROOT..OFF_DATA_ROOT + 8].copy_from_slice(&data_root.to_le_bytes());
    buf[OFF_CATALOG_ROOT..OFF_CATALOG_ROOT + 8].copy_from_slice(&catalog_root.to_le_bytes());
    buf[OFF_PARENT_PAGE..OFF_PARENT_PAGE + 8].copy_from_slice(&parent_page.to_le_bytes());
    buf[OFF_TS..OFF_TS + 8].copy_from_slice(&unix_ts_ms.to_le_bytes());
    Ok(id)
}

pub(crate) fn read_record<P: ReadPages>(src: &P, page_id: PageId) -> Result<CommitInfo> {
    let buf = src.read(page_id)?;
    if page::page_type(&buf)? != PageType::Commit {
        return Err(Error::corrupted(page_id, "expected a commit record page"));
    }
    let u64_at =
        |off: usize| u64::from_le_bytes(buf[off..off + 8].try_into().expect("commit record slice"));
    Ok(CommitInfo {
        commit_id: u64_at(OFF_ID),
        parent_id: u64_at(OFF_PARENT),
        data_root: u64_at(OFF_DATA_ROOT),
        catalog_root: u64_at(OFF_CATALOG_ROOT),
        parent_page: u64_at(OFF_PARENT_PAGE),
        unix_ts_ms: u64_at(OFF_TS),
    })
}

/// Walk parent edges from a branch head and return the record for
/// `commit_id`, stopping at the branch's retention floor. `floor` is the
/// oldest retained commit id; 0 means the walk may reach the root. The
/// floor exists so a walk never follows a parent edge into a record that
/// garbage collection has reclaimed.
pub(crate) fn find<P: ReadPages>(
    src: &P,
    head: PageId,
    floor: u64,
    commit_id: u64,
) -> Result<Option<CommitInfo>> {
    let mut at = head;
    while at != NIL_PAGE {
        let rec = read_record(src, at)?;
        if rec.commit_id == commit_id {
            return Ok(Some(rec));
        }
        if rec.commit_id < commit_id || rec.commit_id <= floor {
            return Ok(None); // ids only shrink down the edge; floor is the wall
        }
        at = rec.parent_page;
    }
    Ok(None)
}

/// Newest record on a branch with a timestamp at or before `unix_ts_ms`,
/// respecting the retention floor. Returns `None` when every retained
/// commit is newer than the requested time.
pub(crate) fn find_at_time<P: ReadPages>(
    src: &P,
    head: PageId,
    floor: u64,
    unix_ts_ms: u64,
) -> Result<Option<CommitInfo>> {
    let mut at = head;
    while at != NIL_PAGE {
        let rec = read_record(src, at)?;
        if rec.unix_ts_ms <= unix_ts_ms {
            return Ok(Some(rec));
        }
        if rec.commit_id <= floor {
            return Ok(None);
        }
        at = rec.parent_page;
    }
    Ok(None)
}
