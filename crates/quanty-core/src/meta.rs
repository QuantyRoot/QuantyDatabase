//! Meta pages.
//!
//! Pages 0 and 1 are the two meta slots. A commit writes its meta into slot
//! `txid % 2`, so the previous commit's meta always survives a torn write.
//! Recovery reads both slots and picks the valid one with the highest txid.
//!
//! Body layout, immediately after the 16 byte page header:
//!
//! ```text
//! offset  size  field
//! 16      8     magic "QUANTYDB"
//! 24      4     format version (LE)
//! 28      4     page size (LE)
//! 32      8     txid
//! 40      8     data root page (0 = none)
//! 48      8     catalog root page (0 = none)
//! 56      8     freelist root page (0 = none)
//! 64      8     page count (total pages in file, metas included)
//! 72      8     unix timestamp in ms
//! 80      8     newest commit record page (0 = none)
//! ```

use crate::error::{Error, Result};
use crate::page::{self, PageId, PageType, PAGE_HEADER_LEN};
use crate::storage::Storage;

pub const MAGIC: [u8; 8] = *b"QUANTYDB";
pub const FORMAT_VERSION: u32 = 1;

const OFF_MAGIC: usize = PAGE_HEADER_LEN;
const OFF_VERSION: usize = OFF_MAGIC + 8;
const OFF_PAGE_SIZE: usize = OFF_VERSION + 4;
const OFF_TXID: usize = OFF_PAGE_SIZE + 4;
const OFF_DATA_ROOT: usize = OFF_TXID + 8;
const OFF_CATALOG_ROOT: usize = OFF_DATA_ROOT + 8;
const OFF_FREELIST_ROOT: usize = OFF_CATALOG_ROOT + 8;
const OFF_PAGE_COUNT: usize = OFF_FREELIST_ROOT + 8;
const OFF_TIMESTAMP: usize = OFF_PAGE_COUNT + 8;
const OFF_COMMIT_PAGE: usize = OFF_TIMESTAMP + 8;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    pub page_size: u32,
    pub txid: u64,
    pub data_root: PageId,
    pub catalog_root: PageId,
    pub freelist_root: PageId,
    pub page_count: u64,
    pub unix_ts_ms: u64,
    /// Page holding the newest commit record, the head of the commit chain.
    pub commit_page: PageId,
}

impl Meta {
    pub(crate) fn encode(&self, buf: &mut [u8]) {
        debug_assert_eq!(buf.len(), self.page_size as usize);
        buf.fill(0);
        page::init_header(buf, PageType::Meta);
        buf[OFF_MAGIC..OFF_MAGIC + 8].copy_from_slice(&MAGIC);
        put_u32(buf, OFF_VERSION, FORMAT_VERSION);
        put_u32(buf, OFF_PAGE_SIZE, self.page_size);
        put_u64(buf, OFF_TXID, self.txid);
        put_u64(buf, OFF_DATA_ROOT, self.data_root);
        put_u64(buf, OFF_CATALOG_ROOT, self.catalog_root);
        put_u64(buf, OFF_FREELIST_ROOT, self.freelist_root);
        put_u64(buf, OFF_PAGE_COUNT, self.page_count);
        put_u64(buf, OFF_TIMESTAMP, self.unix_ts_ms);
        put_u64(buf, OFF_COMMIT_PAGE, self.commit_page);
        page::seal(buf, self.txid);
    }

    /// Decode and fully validate one meta page buffer.
    pub(crate) fn decode(buf: &[u8], slot: PageId) -> Result<Meta> {
        page::verify(buf, slot)?;
        if buf[OFF_MAGIC..OFF_MAGIC + 8] != MAGIC {
            return Err(Error::InvalidFormat(
                "bad magic, not a quanty database".into(),
            ));
        }
        let version = get_u32(buf, OFF_VERSION);
        if version != FORMAT_VERSION {
            return Err(Error::InvalidFormat(format!(
                "format version {version} is not supported (this build reads {FORMAT_VERSION})"
            )));
        }
        let page_size = get_u32(buf, OFF_PAGE_SIZE);
        if !page::valid_page_size(page_size) {
            return Err(Error::corrupted(
                slot,
                format!("implausible page size {page_size}"),
            ));
        }
        if page_size as usize != buf.len() {
            return Err(Error::corrupted(
                slot,
                "page size does not match meta location",
            ));
        }
        let meta = Meta {
            page_size,
            txid: get_u64(buf, OFF_TXID),
            data_root: get_u64(buf, OFF_DATA_ROOT),
            catalog_root: get_u64(buf, OFF_CATALOG_ROOT),
            freelist_root: get_u64(buf, OFF_FREELIST_ROOT),
            page_count: get_u64(buf, OFF_PAGE_COUNT),
            unix_ts_ms: get_u64(buf, OFF_TIMESTAMP),
            commit_page: get_u64(buf, OFF_COMMIT_PAGE),
        };
        if meta.page_count < 2 {
            return Err(Error::corrupted(
                slot,
                "page count below the two meta pages",
            ));
        }
        for root in [
            meta.data_root,
            meta.catalog_root,
            meta.freelist_root,
            meta.commit_page,
        ] {
            if root != page::NIL_PAGE && (root < 2 || root >= meta.page_count) {
                return Err(Error::corrupted(
                    slot,
                    format!("root pointer {root} out of range"),
                ));
            }
        }
        Ok(meta)
    }
}

/// Find the newest valid meta in an existing database.
///
/// The page size lives inside the meta itself, so this brute-forces the
/// small set of legal page sizes instead of trusting a possibly corrupted
/// size field. Eight candidate sizes, two slots each, cheap and hard to fool.
pub(crate) fn recover(storage: &dyn Storage) -> Result<Meta> {
    let mut best: Option<Meta> = None;
    let mut saw_magic = false;
    let len = storage.len()?;

    let mut ps = page::MIN_PAGE_SIZE;
    while ps <= page::MAX_PAGE_SIZE {
        for slot in 0..2u64 {
            let offset = slot * ps as u64;
            if offset + ps as u64 > len {
                continue;
            }
            let mut buf = vec![0u8; ps as usize];
            if storage.read_at(offset, &mut buf).is_err() {
                continue;
            }
            if buf[OFF_MAGIC..OFF_MAGIC + 8] == MAGIC {
                saw_magic = true;
            }
            match Meta::decode(&buf, slot) {
                Ok(meta) if best.as_ref().map_or(true, |b| meta.txid > b.txid) => best = Some(meta),
                _ => {}
            }
        }
        ps *= 2;
    }

    match best {
        Some(meta) => Ok(meta),
        None if saw_magic => Err(Error::corrupted(None, "no valid meta page in either slot")),
        None => Err(Error::InvalidFormat("not a quanty database".into())),
    }
}

fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

fn put_u64(buf: &mut [u8], off: usize, v: u64) {
    buf[off..off + 8].copy_from_slice(&v.to_le_bytes());
}

fn get_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(buf[off..off + 4].try_into().expect("meta slice"))
}

fn get_u64(buf: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(buf[off..off + 8].try_into().expect("meta slice"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(txid: u64) -> Meta {
        Meta {
            page_size: 512,
            txid,
            data_root: 0,
            catalog_root: 0,
            freelist_root: 0,
            page_count: 2,
            unix_ts_ms: 123,
            commit_page: 0,
        }
    }

    #[test]
    fn encode_decode_roundtrips() {
        let meta = sample(9);
        let mut buf = vec![0u8; 512];
        meta.encode(&mut buf);
        assert_eq!(Meta::decode(&buf, 0).unwrap(), meta);
    }

    #[test]
    fn decode_rejects_wrong_magic() {
        let mut buf = vec![0u8; 512];
        sample(1).encode(&mut buf);
        buf[OFF_MAGIC] = b'X';
        // magic is covered by the checksum, so re-seal to isolate the check
        page::seal(&mut buf, 1);
        assert!(matches!(
            Meta::decode(&buf, 0),
            Err(Error::InvalidFormat(_))
        ));
    }

    #[test]
    fn decode_rejects_flipped_bit() {
        let mut buf = vec![0u8; 512];
        sample(1).encode(&mut buf);
        buf[OFF_TXID] ^= 0x80;
        assert!(matches!(
            Meta::decode(&buf, 0),
            Err(Error::Corrupted { .. })
        ));
    }
}
