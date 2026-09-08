//! Pages and b-tree page headers.
//!
//! Pages are numbered from 1 and are all the same size. Page 1 is special
//! in exactly one way: the 100 byte database header sits in front of its
//! b-tree header, so everything on page 1 is shifted by 100 bytes. Getting
//! that wrong is the classic first bug in a SQLite reader, so the offset
//! lives in one place here and nowhere else.
//!
//! Every offset read out of a page is checked against the usable size
//! before it is used. A cell pointer that points into the page header, past
//! the end of the page, or into the reserved trailer is a malformed file,
//! not a slice to be taken on faith.

use crate::error::{Result, SqliteError};

/// What a b-tree page holds. Table pages are keyed by rowid, index pages by
/// the indexed values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageKind {
    InteriorIndex,
    InteriorTable,
    LeafIndex,
    LeafTable,
}

impl PageKind {
    fn from_byte(b: u8) -> Option<PageKind> {
        match b {
            2 => Some(PageKind::InteriorIndex),
            5 => Some(PageKind::InteriorTable),
            10 => Some(PageKind::LeafIndex),
            13 => Some(PageKind::LeafTable),
            _ => None,
        }
    }

    pub fn is_interior(self) -> bool {
        matches!(self, PageKind::InteriorIndex | PageKind::InteriorTable)
    }

    pub fn is_table(self) -> bool {
        matches!(self, PageKind::InteriorTable | PageKind::LeafTable)
    }

    /// Interior pages carry a right most child pointer, which makes their
    /// header four bytes longer.
    fn header_len(self) -> usize {
        if self.is_interior() {
            12
        } else {
            8
        }
    }
}

/// One b-tree page, with its header parsed and its cell pointers validated.
pub struct BtreePage {
    pub number: u32,
    pub kind: PageKind,
    /// Child page holding keys greater than every key on this page.
    /// Interior pages only.
    pub right_most: Option<u32>,
    bytes: Vec<u8>,
    usable: usize,
    cell_pointers: Vec<u16>,
}

impl BtreePage {
    /// Parse `bytes` as page `number`. `usable` is the page size minus the
    /// reserved trailer, `body_offset` is 100 on page 1 and 0 elsewhere.
    pub(crate) fn parse(
        number: u32,
        bytes: Vec<u8>,
        usable: usize,
        body_offset: usize,
    ) -> Result<BtreePage> {
        // an empty database has a page 1 that has been allocated but not
        // written; every other page must carry a real header
        if body_offset + 8 > usable {
            return Err(SqliteError::malformed(
                number,
                "page is too small to hold a b-tree header",
            ));
        }

        let kind = PageKind::from_byte(bytes[body_offset]).ok_or_else(|| {
            SqliteError::malformed(
                number,
                format!(
                    "page type {} is not one of 2, 5, 10 or 13",
                    bytes[body_offset]
                ),
            )
        })?;
        let header_len = kind.header_len();
        if body_offset + header_len > usable {
            return Err(SqliteError::malformed(
                number,
                "page is too small to hold an interior b-tree header",
            ));
        }

        let cell_count = be16(&bytes, body_offset + 3) as usize;
        let pointers_end = body_offset + header_len + cell_count * 2;
        if pointers_end > usable {
            return Err(SqliteError::malformed(
                number,
                format!("{cell_count} cell pointers do not fit in {usable} usable bytes"),
            ));
        }

        // 0 means 65536, which is the one page size that does not fit the
        // field. an empty page points its content area at the very end.
        let content_start = match be16(&bytes, body_offset + 5) {
            0 => 65536,
            n => n as usize,
        };
        if content_start > usable {
            return Err(SqliteError::malformed(
                number,
                format!("cell content area starts at {content_start}, past the usable {usable}"),
            ));
        }
        if cell_count > 0 && content_start < pointers_end {
            return Err(SqliteError::malformed(
                number,
                format!(
                    "cell content area starts at {content_start}, inside the header and pointer \
                     array which end at {pointers_end}"
                ),
            ));
        }

        let mut cell_pointers = Vec::with_capacity(cell_count);
        for i in 0..cell_count {
            let at = be16(&bytes, body_offset + header_len + i * 2) as usize;
            if at < pointers_end || at >= usable {
                return Err(SqliteError::malformed(
                    number,
                    format!(
                        "cell {i} points at offset {at}, outside the content area \
                         {pointers_end}..{usable}"
                    ),
                ));
            }
            cell_pointers.push(at as u16);
        }

        let right_most = if kind.is_interior() {
            let child = be32(&bytes, body_offset + 8);
            if child == 0 {
                return Err(SqliteError::malformed(
                    number,
                    "interior page has a right most child pointer of 0",
                ));
            }
            Some(child)
        } else {
            None
        };

        Ok(BtreePage {
            number,
            kind,
            right_most,
            bytes,
            usable,
            cell_pointers,
        })
    }

    pub fn cell_count(&self) -> usize {
        self.cell_pointers.len()
    }

    /// The bytes of cell `index`, from where it starts to the end of the
    /// usable area. Cells are variable length and their real end is only
    /// known once the payload header has been read, so this is the widest
    /// slice a cell may occupy, not its exact extent.
    pub fn cell(&self, index: usize) -> Result<&[u8]> {
        let at = *self.cell_pointers.get(index).ok_or_else(|| {
            SqliteError::malformed(
                self.number,
                format!("cell {index} requested, page has {}", self.cell_count()),
            )
        })? as usize;
        Ok(&self.bytes[at..self.usable])
    }
}

pub(crate) fn be16(bytes: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([bytes[at], bytes[at + 1]])
}

pub(crate) fn be32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}
