//! Cells: what a b-tree page actually stores, and where a payload goes when
//! it does not fit.
//!
//! The four page kinds have four different cell layouts, and only three of
//! them carry a payload at all. A table interior cell is just a child
//! pointer and a rowid, which is why a table b-tree can be walked by key
//! without touching a single record.
//!
//! The interesting part is the spill rule. A payload larger than the page
//! keeps a prefix on the page and puts the rest in a chain of overflow
//! pages, but the size of that prefix is not simply "as much as fits". The
//! format picks it so that a payload never leaves an almost empty overflow
//! page behind, using these definitions, all with truncating integer
//! division:
//!
//! ```text
//! U = usable page size
//! X = U - 35                     for table leaf pages
//! X = ((U - 12) * 64 / 255) - 23 for index pages
//! M = ((U - 12) * 32 / 255) - 23
//!
//! P <= X          -> all P bytes stay local
//! otherwise K = M + ((P - M) % (U - 4))
//!   K <= X        -> K bytes stay local
//!   K > X         -> M bytes stay local
//! ```
//!
//! Getting this wrong does not fail loudly. It reads a payload that is off
//! by a few bytes, which then decodes into a record that looks plausible
//! and is wrong, so the fixture in tests/data deliberately holds values on
//! both sides of every boundary this rule has.

use crate::error::{Result, SqliteError};
use crate::page::{BtreePage, PageKind};
use crate::varint;

/// A cell with its payload assembled, overflow pages included.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    /// Child pointer plus the largest rowid in that subtree.
    TableInterior { child: u32, rowid: i64 },
    /// A row: its rowid and the record that holds its columns.
    TableLeaf { rowid: i64, payload: Vec<u8> },
    /// Child pointer plus the separator key that lives on this page.
    IndexInterior { child: u32, payload: Vec<u8> },
    /// An index entry: the indexed values and the rowid they point at.
    IndexLeaf { payload: Vec<u8> },
}

/// Where a cell's payload lives before overflow pages are followed.
pub(crate) struct PayloadRef<'a> {
    pub total: usize,
    pub local: &'a [u8],
    pub overflow: Option<u32>,
}

pub(crate) enum CellLayout<'a> {
    TableInterior { child: u32, rowid: i64 },
    TableLeaf { rowid: i64, payload: PayloadRef<'a> },
    IndexInterior { child: u32, payload: PayloadRef<'a> },
    IndexLeaf { payload: PayloadRef<'a> },
}

/// How many bytes of a `total` byte payload stay on the page.
pub(crate) fn local_len(kind: PageKind, usable: usize, total: usize) -> usize {
    // the header validation guarantees usable >= 480, so none of these
    // subtractions can wrap
    let max_local = if kind == PageKind::LeafTable {
        usable - 35
    } else {
        ((usable - 12) * 64 / 255) - 23
    };
    if total <= max_local {
        return total;
    }
    let min_local = ((usable - 12) * 32 / 255) - 23;
    let k = min_local + (total - min_local) % (usable - 4);
    if k <= max_local {
        k
    } else {
        min_local
    }
}

/// Parse cell `index` of `page`. `payload_limit` is the largest payload the
/// file could possibly hold, which stops a hostile length varint from
/// asking for an allocation the file cannot back.
pub(crate) fn parse<'a>(
    page: &'a BtreePage,
    index: usize,
    usable: usize,
    payload_limit: u64,
) -> Result<CellLayout<'a>> {
    let bytes = page.cell(index)?;
    let number = page.number;
    let malformed = |reason: String| SqliteError::malformed(number, reason);

    Ok(match page.kind {
        PageKind::InteriorTable => {
            if bytes.len() < 4 {
                return Err(malformed(format!(
                    "cell {index} is too short for a child pointer"
                )));
            }
            let child = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            if child == 0 {
                return Err(malformed(format!("cell {index} points at page 0")));
            }
            let (rowid, _) = varint::read(&bytes[4..])?;
            CellLayout::TableInterior { child, rowid }
        }
        PageKind::LeafTable => {
            let (total, size_len) = varint::read_unsigned(bytes)?;
            let (rowid, rowid_len) = varint::read(&bytes[size_len..])?;
            let payload = payload_ref(
                page.kind,
                &bytes[size_len + rowid_len..],
                total,
                usable,
                payload_limit,
                &malformed,
            )?;
            CellLayout::TableLeaf { rowid, payload }
        }
        PageKind::InteriorIndex => {
            if bytes.len() < 4 {
                return Err(malformed(format!(
                    "cell {index} is too short for a child pointer"
                )));
            }
            let child = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            if child == 0 {
                return Err(malformed(format!("cell {index} points at page 0")));
            }
            let (total, size_len) = varint::read_unsigned(&bytes[4..])?;
            let payload = payload_ref(
                page.kind,
                &bytes[4 + size_len..],
                total,
                usable,
                payload_limit,
                &malformed,
            )?;
            CellLayout::IndexInterior { child, payload }
        }
        PageKind::LeafIndex => {
            let (total, size_len) = varint::read_unsigned(bytes)?;
            let payload = payload_ref(
                page.kind,
                &bytes[size_len..],
                total,
                usable,
                payload_limit,
                &malformed,
            )?;
            CellLayout::IndexLeaf { payload }
        }
    })
}

fn payload_ref<'a>(
    kind: PageKind,
    after_header: &'a [u8],
    total: u64,
    usable: usize,
    payload_limit: u64,
    malformed: &dyn Fn(String) -> SqliteError,
) -> Result<PayloadRef<'a>> {
    if total > payload_limit {
        return Err(malformed(format!(
            "a cell claims a {total} byte payload, more than the {payload_limit} bytes the file holds"
        )));
    }
    let total = total as usize;
    let local = local_len(kind, usable, total);
    if local > after_header.len() {
        return Err(malformed(format!(
            "a cell keeps {local} bytes on the page but only {} are left on it",
            after_header.len()
        )));
    }

    let overflow = if local < total {
        // the four bytes right after the local part are the first overflow
        // page number
        let rest = &after_header[local..];
        if rest.len() < 4 {
            return Err(malformed(
                "a spilled cell has no room for its overflow pointer".to_string(),
            ));
        }
        let first = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]);
        if first == 0 {
            return Err(malformed(
                "a spilled cell points at overflow page 0".to_string(),
            ));
        }
        Some(first)
    } else {
        None
    };

    Ok(PayloadRef {
        total,
        local: &after_header[..local],
        overflow,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payloads_that_fit_stay_whole() {
        // u = 512: a table leaf keeps up to 477 bytes
        assert_eq!(local_len(PageKind::LeafTable, 512, 0), 0);
        assert_eq!(local_len(PageKind::LeafTable, 512, 476), 476);
        assert_eq!(local_len(PageKind::LeafTable, 512, 477), 477);
        // u = 4096, the default page size
        assert_eq!(local_len(PageKind::LeafTable, 4096, 4061), 4061);
    }

    #[test]
    fn index_pages_keep_far_less_than_table_pages() {
        // ((512 - 12) * 64 / 255) - 23 = 102
        assert_eq!(local_len(PageKind::LeafIndex, 512, 102), 102);
        assert_eq!(local_len(PageKind::InteriorIndex, 512, 102), 102);
        assert!(local_len(PageKind::LeafIndex, 512, 103) < 103);
    }

    #[test]
    fn spilled_payloads_follow_the_k_rule() {
        // u = 512, so m = 39, x = 477 and u - 4 = 508
        let m = 39;
        for total in [478usize, 500, 546, 1000, 5000, 50000] {
            let k = m + (total - m) % 508;
            let expected = if k <= 477 { k } else { m };
            assert_eq!(
                local_len(PageKind::LeafTable, 512, total),
                expected,
                "payload of {total} bytes"
            );
        }
    }

    #[test]
    fn the_local_part_never_exceeds_the_page() {
        for usable in [480usize, 512, 1024, 4096, 65536] {
            for total in [0usize, 1, 100, 479, 480, 1000, 65535, 65536, 1 << 20] {
                for kind in [
                    PageKind::LeafTable,
                    PageKind::LeafIndex,
                    PageKind::InteriorIndex,
                ] {
                    let local = local_len(kind, usable, total);
                    assert!(local <= total, "{kind:?} {usable} {total}");
                    // room for the four byte overflow pointer has to remain
                    assert!(local + 4 <= usable, "{kind:?} {usable} {total}");
                }
            }
        }
    }
}
