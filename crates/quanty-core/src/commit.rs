//! Commit records.
//!
//! Every commit writes one commit record page describing itself and linking
//! to its predecessor. The meta points at the newest record, so the file
//! carries its full history as a chain. Snapshots of old commits are looked
//! up here; branches turn this chain into a DAG in phase 3.
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
//! 56      8     wall clock, unix milliseconds (informational)
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
    pub prev_commit_page: PageId,
    pub unix_ts_ms: u64,
}

const OFF_ID: usize = PAGE_HEADER_LEN;
const OFF_PARENT: usize = OFF_ID + 8;
const OFF_DATA_ROOT: usize = OFF_PARENT + 8;
const OFF_CATALOG_ROOT: usize = OFF_DATA_ROOT + 8;
const OFF_PREV_PAGE: usize = OFF_CATALOG_ROOT + 8;
const OFF_TS: usize = OFF_PREV_PAGE + 8;

/// Write the commit record for the transaction the batch is about to
/// commit. Returns the record's page id for `WriteBatch::set_commit_page`.
pub(crate) fn write_record<S: Storage>(
    batch: &mut WriteBatch<'_, S>,
    data_root: PageId,
    catalog_root: PageId,
    unix_ts_ms: u64,
) -> Result<PageId> {
    let commit_id = batch.base_meta().txid + 1;
    let parent_id = batch.base_meta().txid;
    let prev_page = batch.base_meta().commit_page;

    let id = batch.allocate(PageType::Commit);
    let buf = batch.page_mut(id)?;
    buf[OFF_ID..OFF_ID + 8].copy_from_slice(&commit_id.to_le_bytes());
    buf[OFF_PARENT..OFF_PARENT + 8].copy_from_slice(&parent_id.to_le_bytes());
    buf[OFF_DATA_ROOT..OFF_DATA_ROOT + 8].copy_from_slice(&data_root.to_le_bytes());
    buf[OFF_CATALOG_ROOT..OFF_CATALOG_ROOT + 8].copy_from_slice(&catalog_root.to_le_bytes());
    buf[OFF_PREV_PAGE..OFF_PREV_PAGE + 8].copy_from_slice(&prev_page.to_le_bytes());
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
        prev_commit_page: u64_at(OFF_PREV_PAGE),
        unix_ts_ms: u64_at(OFF_TS),
    })
}

/// Walk the chain from the newest record and return the record for
/// `commit_id`. Linear for now; a commit index tree arrives with branches.
pub(crate) fn find<P: ReadPages>(src: &P, head: PageId, commit_id: u64) -> Result<CommitInfo> {
    let mut at = head;
    while at != NIL_PAGE {
        let rec = read_record(src, at)?;
        if rec.commit_id == commit_id {
            return Ok(rec);
        }
        if rec.commit_id < commit_id {
            break; // the chain is ordered, no point walking further back
        }
        at = rec.prev_commit_page;
    }
    Err(Error::InvalidArgument("no such commit id"))
}
