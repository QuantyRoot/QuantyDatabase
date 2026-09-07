//! Free list encoding.
//!
//! The free list is a chain of FreeList pages, each holding page ids that
//! no retained commit references. Body after the 16 byte header:
//!
//! ```text
//! 8       next chain page (0 = last)
//! 2       number of ids in this page (u16), mirrored in the header count
//! n * 8   page ids
//! ```
//!
//! The invariant that makes reuse crash safe: a page id listed in the free
//! list is referenced by nothing at all, including the free list itself.
//! Chain pages are never listed in the chain they belong to; when a batch
//! consumes a chain page it defers that page into the next free list
//! instead of reusing it, because the previous commit's meta still points
//! at it until the commit point.

use crate::error::{Error, Result};
use crate::page::{self, PageId, PAGE_HEADER_LEN};

pub(crate) fn ids_per_page(page_size: u32) -> usize {
    (page_size as usize - PAGE_HEADER_LEN - 8 - 2) / 8
}

/// Fill a freshly allocated FreeList page buffer.
pub(crate) fn encode_page(buf: &mut [u8], next: PageId, ids: &[PageId]) {
    debug_assert!(ids.len() <= ids_per_page(buf.len() as u32));
    let count = u16::try_from(ids.len()).expect("free list ids fit u16");
    buf[6..8].copy_from_slice(&count.to_le_bytes());
    let body = PAGE_HEADER_LEN;
    buf[body..body + 8].copy_from_slice(&next.to_le_bytes());
    buf[body + 8..body + 10].copy_from_slice(&count.to_le_bytes());
    for (i, id) in ids.iter().enumerate() {
        let at = body + 10 + i * 8;
        buf[at..at + 8].copy_from_slice(&id.to_le_bytes());
    }
}

/// Read one chain page: `(next page, listed ids)`.
pub(crate) fn decode_page(buf: &[u8], id: PageId) -> Result<(PageId, Vec<PageId>)> {
    if page::page_type(buf)? != page::PageType::FreeList {
        return Err(Error::corrupted(
            id,
            "free list chain hit a non free list page",
        ));
    }
    let body = PAGE_HEADER_LEN;
    let next = u64::from_le_bytes(buf[body..body + 8].try_into().expect("hdr"));
    let count = u16::from_le_bytes(buf[body + 8..body + 10].try_into().expect("hdr")) as usize;
    if count > ids_per_page(buf.len() as u32) {
        return Err(Error::corrupted(
            id,
            "free list page claims impossible count",
        ));
    }
    let mut ids = Vec::with_capacity(count);
    for i in 0..count {
        let at = body + 10 + i * 8;
        ids.push(u64::from_le_bytes(buf[at..at + 8].try_into().expect("len")));
    }
    Ok((next, ids))
}
