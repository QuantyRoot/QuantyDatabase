//! Page layout.
//!
//! Every page in the file, meta pages included, starts with the same 16 byte
//! header:
//!
//! ```text
//! offset  size  field
//! 0       4     crc32c checksum of bytes [4..page_size]
//! 4       1     page type
//! 5       1     flags
//! 6       2     entry count / used bytes (owner-defined, little endian)
//! 8       8     lsn: txid of the commit that sealed this page (LE)
//! ```
//!
//! The pager owns the checksum and the lsn (both written at commit time).
//! Whoever builds the page owns type, flags and count.

use crate::error::{Error, Result};

pub type PageId = u64;

/// Page id 0 doubles as the nil pointer for roots, which works out because
/// page 0 is always meta slot A and never a valid data target.
pub const NIL_PAGE: PageId = 0;

pub const PAGE_HEADER_LEN: usize = 16;
pub const MIN_PAGE_SIZE: u32 = 512;
pub const MAX_PAGE_SIZE: u32 = 65536;
pub const DEFAULT_PAGE_SIZE: u32 = 4096;

const OFF_CHECKSUM: usize = 0;
const OFF_TYPE: usize = 4;
const OFF_FLAGS: usize = 5;
const OFF_COUNT: usize = 6;
const OFF_LSN: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PageType {
    Meta = 0,
    Branch = 1,
    Leaf = 2,
    Overflow = 3,
    FreeList = 4,
    Blob = 5,
    Commit = 6,
}

impl TryFrom<u8> for PageType {
    type Error = u8;

    fn try_from(v: u8) -> std::result::Result<Self, u8> {
        Ok(match v {
            0 => PageType::Meta,
            1 => PageType::Branch,
            2 => PageType::Leaf,
            3 => PageType::Overflow,
            4 => PageType::FreeList,
            5 => PageType::Blob,
            6 => PageType::Commit,
            other => return Err(other),
        })
    }
}

pub fn valid_page_size(page_size: u32) -> bool {
    (MIN_PAGE_SIZE..=MAX_PAGE_SIZE).contains(&page_size) && page_size.is_power_of_two()
}

/// Initialize the header of a freshly allocated page buffer.
pub fn init_header(buf: &mut [u8], page_type: PageType) {
    debug_assert!(buf.len() >= PAGE_HEADER_LEN);
    buf[OFF_TYPE] = page_type as u8;
    buf[OFF_FLAGS] = 0;
    buf[OFF_COUNT..OFF_COUNT + 2].fill(0);
}

pub fn page_type(buf: &[u8]) -> Result<PageType> {
    PageType::try_from(buf[OFF_TYPE])
        .map_err(|v| Error::corrupted(None, format!("unknown page type {v}")))
}

pub fn lsn(buf: &[u8]) -> u64 {
    u64::from_le_bytes(buf[OFF_LSN..OFF_LSN + 8].try_into().expect("header slice"))
}

/// Stamp the lsn and compute the checksum. Call once, right before the page
/// hits storage. After sealing the buffer must not change.
pub fn seal(buf: &mut [u8], txid: u64) {
    buf[OFF_LSN..OFF_LSN + 8].copy_from_slice(&txid.to_le_bytes());
    let sum = crc32c::crc32c(&buf[OFF_TYPE..]);
    buf[OFF_CHECKSUM..OFF_CHECKSUM + 4].copy_from_slice(&sum.to_le_bytes());
}

/// Verify the checksum of a page read from storage.
pub fn verify(buf: &[u8], id: PageId) -> Result<()> {
    let stored = u32::from_le_bytes(buf[OFF_CHECKSUM..OFF_CHECKSUM + 4].try_into().expect("hdr"));
    let actual = crc32c::crc32c(&buf[OFF_TYPE..]);
    if stored != actual {
        return Err(Error::corrupted(
            id,
            format!("checksum mismatch (stored {stored:#010x}, computed {actual:#010x})"),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_then_verify_roundtrips() {
        let mut page = vec![0u8; 512];
        init_header(&mut page, PageType::Leaf);
        page[100] = 42;
        seal(&mut page, 7);
        verify(&page, 3).unwrap();
        assert_eq!(lsn(&page), 7);
        assert_eq!(page_type(&page).unwrap(), PageType::Leaf);
    }

    #[test]
    fn verify_catches_a_single_flipped_bit() {
        let mut page = vec![0u8; 512];
        init_header(&mut page, PageType::Leaf);
        seal(&mut page, 1);
        page[300] ^= 0x01;
        assert!(verify(&page, 3).is_err());
    }
}
