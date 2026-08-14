//! Walking a table b-tree.
//!
//! Two things about this file are defences rather than features, and both
//! exist because the tree comes out of a file we did not write.
//!
//! It does not recurse. A b-tree of sensible shape is a handful of levels
//! deep, but nothing in the file stops the pointers from forming a chain
//! thousands of levels long, and a recursive walker meets that with a stack
//! overflow, which aborts the process. An explicit stack meets it by
//! running.
//!
//! It refuses to visit a page twice. Pointers in the file can form a cycle,
//! by corruption or by design, and a walker that follows them faithfully
//! never returns. Since a well formed b-tree reaches every one of its pages
//! exactly once, remembering what has been visited costs one bit per page
//! and turns a hang into an error.
//!
//! Rows come out in rowid order, because that is the order the tree is in.
//! Nothing sorts them.

use std::collections::HashSet;

use crate::cell::Cell;
use crate::error::{Result, SqliteError};
use crate::page::{BtreePage, PageKind};
use crate::record::{self, SqliteValue};
use crate::source::Source;
use crate::Reader;

/// One row of a table: its rowid and the values of its columns.
///
/// A column declared `integer primary key` is an alias for the rowid, and
/// SQLite stores it as NULL in the record itself. The rowid on this struct
/// is the real value.
#[derive(Debug, Clone, PartialEq)]
pub struct TableRow {
    pub rowid: i64,
    pub values: Vec<SqliteValue>,
}

/// Where the walk currently is on one level of the tree.
struct Frame {
    page: u32,
    /// Next cell to visit. On an interior page, the index one past the last
    /// cell means the right most child.
    next: usize,
}

/// A depth first walk of a table b-tree, yielding rows in rowid order.
///
/// The iterator stops after its first error. Continuing past one would mean
/// guessing which part of a broken file is still trustworthy, and a partial
/// import that looks complete is exactly the outcome this crate exists to
/// avoid.
pub struct TableScan<'a, S: Source> {
    reader: &'a Reader<S>,
    stack: Vec<Frame>,
    visited: HashSet<u32>,
    /// The page the walk is currently reading. Depth first means the next
    /// row almost always comes from the same page as the last one, so one
    /// slot of cache turns a page read per row into a page read per page.
    current: Option<BtreePage>,
    /// The last rowid handed out. A table b-tree is ordered by rowid, so a
    /// scan of one is strictly ascending; anything else means the tree
    /// contradicts its own shape and the file is corrupt.
    last_rowid: Option<i64>,
    finished: bool,
}

impl<'a, S: Source> TableScan<'a, S> {
    pub(crate) fn new(reader: &'a Reader<S>, root: u32) -> Result<TableScan<'a, S>> {
        let page = reader.btree_page(root)?;
        if !page.kind.is_table() {
            return Err(SqliteError::unsupported(format!(
                "page {root} is a {:?}, which means the table is a without rowid table; \
                 those are stored in an index b-tree and this reader cannot read them yet",
                page.kind
            )));
        }
        let mut visited = HashSet::new();
        visited.insert(root);
        Ok(TableScan {
            reader,
            stack: vec![Frame {
                page: root,
                next: 0,
            }],
            visited,
            current: Some(page),
            last_rowid: None,
            finished: false,
        })
    }

    /// Make sure `number` is the page in hand, reading it if it is not.
    ///
    /// This fills the cache rather than returning the page, so that reading
    /// it afterwards borrows `self` immutably and leaves the reader
    /// reachable in the same expression.
    fn ensure_page(&mut self, number: u32) -> Result<()> {
        if !matches!(&self.current, Some(page) if page.number == number) {
            self.current = Some(self.reader.btree_page(number)?);
        }
        Ok(())
    }

    fn page_in_hand(&self) -> &BtreePage {
        self.current
            .as_ref()
            .expect("ensure_page runs before every read")
    }

    fn descend(&mut self, child: u32) -> Result<()> {
        if !self.visited.insert(child) {
            return Err(SqliteError::malformed(
                child,
                "the b-tree visits this page twice, so its pointers form a cycle",
            ));
        }
        let page = self.reader.btree_page(child)?;
        if !page.kind.is_table() {
            return Err(SqliteError::malformed(
                child,
                format!("a table b-tree points at a {:?}", page.kind),
            ));
        }
        self.current = Some(page);
        self.stack.push(Frame {
            page: child,
            next: 0,
        });
        Ok(())
    }

    /// One step of the walk: a row, a descent, or the end of a page.
    fn step(&mut self) -> Result<Option<TableRow>> {
        let Some(frame) = self.stack.last() else {
            self.finished = true;
            return Ok(None);
        };
        let (number, next) = (frame.page, frame.next);
        self.ensure_page(number)?;
        let page = self.page_in_hand();
        let (kind, cell_count, right_most) = (page.kind, page.cell_count(), page.right_most);

        if kind == PageKind::LeafTable {
            if next >= cell_count {
                self.stack.pop();
                return Ok(None);
            }
            let cell = self.reader.cell(self.page_in_hand(), next)?;
            self.bump();
            return match cell {
                Cell::TableLeaf { rowid, payload } => {
                    // the tree is ordered by rowid, so a scan of it is
                    // strictly ascending. a file where it is not has a
                    // b-tree that is not one, and passing those rows on
                    // would mean handing a caller an order it cannot trust.
                    if let Some(previous) = self.last_rowid {
                        if rowid <= previous {
                            return Err(SqliteError::malformed(
                                number,
                                format!(
                                    "rowid {rowid} follows {previous}, so the table b-tree \
                                     is not in key order"
                                ),
                            ));
                        }
                    }
                    self.last_rowid = Some(rowid);
                    Ok(Some(TableRow {
                        rowid,
                        values: record::decode(&payload)?,
                    }))
                }
                other => Err(SqliteError::malformed(
                    number,
                    format!("a table leaf page holds {other:?}"),
                )),
            };
        }

        // interior page: visit every child, then the right most one
        if next < cell_count {
            let cell = self.reader.cell(self.page_in_hand(), next)?;
            self.bump();
            match cell {
                Cell::TableInterior { child, .. } => self.descend(child)?,
                other => {
                    return Err(SqliteError::malformed(
                        number,
                        format!("a table interior page holds {other:?}"),
                    ))
                }
            }
        } else if next == cell_count {
            self.bump();
            let child = right_most.ok_or_else(|| {
                SqliteError::malformed(number, "an interior page without a right most child")
            })?;
            self.descend(child)?;
        } else {
            self.stack.pop();
        }
        Ok(None)
    }

    fn bump(&mut self) {
        if let Some(frame) = self.stack.last_mut() {
            frame.next += 1;
        }
    }
}

impl<S: Source> Iterator for TableScan<'_, S> {
    type Item = Result<TableRow>;

    fn next(&mut self) -> Option<Result<TableRow>> {
        while !self.finished {
            if self.stack.is_empty() {
                self.finished = true;
                return None;
            }
            match self.step() {
                Ok(Some(row)) => return Some(Ok(row)),
                Ok(None) => continue,
                Err(e) => {
                    self.finished = true;
                    return Some(Err(e));
                }
            }
        }
        None
    }
}
