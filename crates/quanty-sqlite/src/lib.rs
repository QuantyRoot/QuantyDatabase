//! quanty-sqlite: a reader for the SQLite 3 file format.
//!
//! This crate exists so that importing a `.sqlite` file needs no SQLite
//! library, no C toolchain and no ffi (ADR-005). It only ever reads, and
//! the type it reads through cannot write, so the file being imported
//! cannot be damaged by us.
//!
//! The bar it is held to is the same one our own format reader meets: any
//! input at all, including one built specifically to break it, produces
//! either correct data or an error. Never a panic, never a row that is not
//! in the file.
//!
//! ```no_run
//! use quanty_sqlite::{FileSource, Reader};
//!
//! let reader = Reader::open(FileSource::open("chinook.sqlite")?)?;
//! println!("{} pages of {} bytes", reader.page_count(), reader.header().page_size);
//! # Ok::<(), quanty_sqlite::SqliteError>(())
//! ```

mod error;
mod header;
mod page;
mod source;

pub use error::{Result, SqliteError};
pub use header::{Header, TextEncoding};
pub use page::{BtreePage, PageKind};
pub use source::{FileSource, SliceSource, Source};

/// A SQLite database file, opened for reading.
pub struct Reader<S: Source> {
    source: S,
    header: Header,
    page_count: u32,
}

impl<S: Source> Reader<S> {
    /// Read and validate the header, then work out how many pages the file
    /// really has.
    pub fn open(source: S) -> Result<Reader<S>> {
        let file_len = source.len()?;
        if file_len < header::HEADER_LEN as u64 {
            return Err(SqliteError::not_sqlite(format!(
                "file is {file_len} bytes, shorter than the {} byte header",
                header::HEADER_LEN
            )));
        }
        let mut buf = [0u8; header::HEADER_LEN];
        source.read_at(0, &mut buf)?;
        let header = Header::parse(&buf)?;

        // the header may state the page count, and the file itself implies
        // one. where they disagree the file is truncated, and finding that
        // out here gives a clear message instead of an unexpected eof in
        // the middle of a scan later on.
        let pages_in_file = file_len / header.page_size as u64;
        let page_count = match header.header_page_count {
            Some(claimed) => {
                if claimed as u64 > pages_in_file {
                    return Err(SqliteError::malformed(
                        None,
                        format!(
                            "the header says {claimed} pages of {} bytes, but the file only \
                             holds {pages_in_file}",
                            header.page_size
                        ),
                    ));
                }
                claimed
            }
            None => u32::try_from(pages_in_file).unwrap_or(u32::MAX),
        };
        if page_count == 0 {
            return Err(SqliteError::malformed(
                None,
                "the file holds no complete page",
            ));
        }

        Ok(Reader {
            source,
            header,
            page_count,
        })
    }

    pub fn header(&self) -> &Header {
        &self.header
    }

    /// Pages in the database. Page numbers run from 1 to this value.
    pub fn page_count(&self) -> u32 {
        self.page_count
    }

    /// Read one whole page, reserved trailer included.
    pub fn read_page(&self, number: u32) -> Result<Vec<u8>> {
        if number == 0 || number > self.page_count {
            return Err(SqliteError::malformed(
                number,
                format!(
                    "page {number} is outside the file's 1..={}",
                    self.page_count
                ),
            ));
        }
        let size = self.header.page_size as usize;
        let offset = (number as u64 - 1) * self.header.page_size as u64;
        let mut bytes = vec![0u8; size];
        self.source.read_at(offset, &mut bytes)?;
        Ok(bytes)
    }

    /// Read a page and parse it as a b-tree page.
    pub fn btree_page(&self, number: u32) -> Result<BtreePage> {
        let bytes = self.read_page(number)?;
        // page 1 carries the database header in front of its b-tree header
        let body_offset = if number == 1 { header::HEADER_LEN } else { 0 };
        BtreePage::parse(
            number,
            bytes,
            self.header.usable_size() as usize,
            body_offset,
        )
    }
}
