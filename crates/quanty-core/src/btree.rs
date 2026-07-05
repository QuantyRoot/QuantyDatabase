//! The copy-on-write B-tree.
//!
//! Every operation that changes a node writes the change to a page owned by
//! the current write batch. Nodes on committed pages are never touched, so
//! every committed root keeps describing a complete, frozen tree forever
//! (until GC exists, and then only outside the retention window).
//!
//! Within a batch, nodes the batch already owns are rewritten in place, so
//! a batch grows with its working set and not with its operation count.
//!
//! Deletes merge underfull nodes with a neighbor when the pair fits into
//! one page (ADR-010). There is no key borrowing between siblings: a lone
//! underfull node between two full neighbors stays as it is, which keeps
//! the logic small and bounds waste well enough in practice. Emptied nodes
//! are unlinked and single-child branches collapse into their child.

use std::collections::HashSet;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::node::{leaf_cell_size, Node, ValueRef};
use crate::page::{PageId, PageType, NIL_PAGE, PAGE_HEADER_LEN};
use crate::pager::{Pager, WriteBatch};
use crate::storage::Storage;

/// Read access to pages, implemented by the pager (committed state) and by
/// a write batch (committed state plus the batch's own pages).
pub trait ReadPages {
    fn read(&self, id: PageId) -> Result<Arc<[u8]>>;
    fn page_size(&self) -> u32;
}

impl<S: Storage> ReadPages for Pager<S> {
    fn read(&self, id: PageId) -> Result<Arc<[u8]>> {
        self.read_page(id)
    }

    fn page_size(&self) -> u32 {
        self.page_size()
    }
}

impl<S: Storage> ReadPages for WriteBatch<'_, S> {
    fn read(&self, id: PageId) -> Result<Arc<[u8]>> {
        self.read_page(id)
    }

    fn page_size(&self) -> u32 {
        self.page_size()
    }
}

/// Longest allowed key. Keeps branch fanout healthy on every legal page
/// size; values have no limit thanks to overflow chains.
pub fn max_key_len(page_size: u32) -> usize {
    page_size as usize / 8
}

/// Values above this go to an overflow chain instead of living inline.
fn inline_max(page_size: u32) -> usize {
    page_size as usize / 4
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

pub(crate) fn get<P: ReadPages>(src: &P, root: PageId, key: &[u8]) -> Result<Option<Vec<u8>>> {
    if root == NIL_PAGE || key.len() > max_key_len(src.page_size()) {
        return Ok(None);
    }
    let mut page = root;
    loop {
        let buf = src.read(page)?;
        match Node::decode(&buf, page)? {
            Node::Branch {
                first_child,
                entries,
            } => {
                page = child_for(first_child, &entries, key);
            }
            Node::Leaf { entries } => {
                return match entries.binary_search_by(|(k, _)| k.as_slice().cmp(key)) {
                    Ok(i) => Ok(Some(read_value(src, &entries[i].1)?)),
                    Err(_) => Ok(None),
                };
            }
        }
    }
}

/// Which child of a branch covers `key`.
fn child_for(first_child: PageId, entries: &[(Vec<u8>, PageId)], key: &[u8]) -> PageId {
    let idx = entries.partition_point(|(k, _)| k.as_slice() <= key);
    if idx == 0 {
        first_child
    } else {
        entries[idx - 1].1
    }
}

pub(crate) fn read_value<P: ReadPages>(src: &P, value: &ValueRef) -> Result<Vec<u8>> {
    match value {
        ValueRef::Inline(v) => Ok(v.clone()),
        ValueRef::Overflow { head, len } => {
            let chunk = overflow_capacity(src.page_size());
            let mut out = Vec::with_capacity(*len as usize);
            let mut page = *head;
            // hard bound on chain length guards against pointer loops in a
            // corrupted file
            let max_pages = (*len as usize).div_ceil(chunk).max(1);
            for _ in 0..max_pages {
                if page == NIL_PAGE {
                    break;
                }
                let buf = src.read(page)?;
                if crate::page::page_type(&buf)? != PageType::Overflow {
                    return Err(Error::corrupted(
                        page,
                        "overflow chain hit a non-overflow page",
                    ));
                }
                let next = u64::from_le_bytes(
                    buf[PAGE_HEADER_LEN..PAGE_HEADER_LEN + 8]
                        .try_into()
                        .expect("hdr"),
                );
                let used = u16::from_le_bytes(
                    buf[PAGE_HEADER_LEN + 8..PAGE_HEADER_LEN + 10]
                        .try_into()
                        .expect("hdr"),
                ) as usize;
                if used > chunk {
                    return Err(Error::corrupted(
                        page,
                        "overflow page claims impossible length",
                    ));
                }
                out.extend_from_slice(&buf[PAGE_HEADER_LEN + 10..PAGE_HEADER_LEN + 10 + used]);
                page = next;
            }
            if out.len() as u64 != *len || page != NIL_PAGE {
                return Err(Error::corrupted(*head, "overflow chain length mismatch"));
            }
            Ok(out)
        }
    }
}

fn overflow_capacity(page_size: u32) -> usize {
    page_size as usize - PAGE_HEADER_LEN - 8 - 2
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

enum Insert {
    Done(PageId),
    Split(PageId, Vec<u8>, PageId),
}

enum Delete {
    NotFound,
    Updated(PageId),
    Emptied,
}

/// Insert or replace `key`. Returns the new root.
pub(crate) fn put<S: Storage>(
    batch: &mut WriteBatch<'_, S>,
    root: PageId,
    key: &[u8],
    value: &[u8],
) -> Result<PageId> {
    let ps = batch.page_size();
    if key.is_empty() {
        return Err(Error::InvalidArgument("keys must not be empty"));
    }
    if key.len() > max_key_len(ps) {
        return Err(Error::InvalidArgument(
            "key exceeds max_key_len for this page size",
        ));
    }

    let vref = if value.len() > inline_max(ps) {
        write_overflow(batch, value)?
    } else {
        ValueRef::Inline(value.to_vec())
    };

    if root == NIL_PAGE {
        let node = Node::Leaf {
            entries: vec![(key.to_vec(), vref)],
        };
        return write_node(batch, None, &node);
    }
    match insert_rec(batch, root, key, vref)? {
        Insert::Done(new_root) => Ok(new_root),
        Insert::Split(left, sep, right) => {
            let node = Node::Branch {
                first_child: left,
                entries: vec![(sep, right)],
            };
            write_node(batch, None, &node)
        }
    }
}

fn insert_rec<S: Storage>(
    batch: &mut WriteBatch<'_, S>,
    page: PageId,
    key: &[u8],
    value: ValueRef,
) -> Result<Insert> {
    let buf = batch.read_page(page)?;
    let mut node = Node::decode(&buf, page)?;
    match &mut node {
        Node::Leaf { entries } => {
            match entries.binary_search_by(|(k, _)| k.as_slice().cmp(key)) {
                Ok(i) => entries[i].1 = value,
                Err(i) => entries.insert(i, (key.to_vec(), value)),
            }
            finish_leaf(batch, page, node)
        }
        Node::Branch {
            first_child,
            entries,
        } => {
            let idx = entries.partition_point(|(k, _)| k.as_slice() <= key);
            let child = if idx == 0 {
                *first_child
            } else {
                entries[idx - 1].1
            };
            match insert_rec(batch, child, key, value)? {
                Insert::Done(new_child) => {
                    if idx == 0 {
                        *first_child = new_child;
                    } else {
                        entries[idx - 1].1 = new_child;
                    }
                    Ok(Insert::Done(write_node(batch, Some(page), &node)?))
                }
                Insert::Split(left, sep, right) => {
                    if idx == 0 {
                        *first_child = left;
                    } else {
                        entries[idx - 1].1 = left;
                    }
                    entries.insert(idx, (sep, right));
                    finish_branch(batch, page, node)
                }
            }
        }
    }
}

fn finish_leaf<S: Storage>(
    batch: &mut WriteBatch<'_, S>,
    old_page: PageId,
    node: Node,
) -> Result<Insert> {
    let ps = batch.page_size() as usize;
    if node.encoded_size() <= ps {
        return Ok(Insert::Done(write_node(batch, Some(old_page), &node)?));
    }
    let Node::Leaf { entries } = node else {
        unreachable!("finish_leaf on a branch")
    };

    // split by encoded size so wildly different value sizes still balance
    let total: usize = entries.iter().map(|(k, v)| leaf_cell_size(k, v)).sum();
    let mut acc = 0;
    let mut split_at = entries.len() - 1;
    for (i, (k, v)) in entries.iter().enumerate() {
        acc += leaf_cell_size(k, v);
        if acc >= total / 2 {
            split_at = (i + 1).clamp(1, entries.len() - 1);
            break;
        }
    }
    let mut left_entries = entries;
    let right_entries = left_entries.split_off(split_at);
    let sep = right_entries[0].0.clone();

    let left = write_node(
        batch,
        Some(old_page),
        &Node::Leaf {
            entries: left_entries,
        },
    )?;
    let right = write_node(
        batch,
        None,
        &Node::Leaf {
            entries: right_entries,
        },
    )?;
    Ok(Insert::Split(left, sep, right))
}

fn finish_branch<S: Storage>(
    batch: &mut WriteBatch<'_, S>,
    old_page: PageId,
    node: Node,
) -> Result<Insert> {
    let ps = batch.page_size() as usize;
    if node.encoded_size() <= ps {
        return Ok(Insert::Done(write_node(batch, Some(old_page), &node)?));
    }
    let Node::Branch {
        first_child,
        entries,
    } = node
    else {
        unreachable!("finish_branch on a leaf")
    };
    debug_assert!(
        entries.len() >= 3,
        "a splitting branch always has several entries"
    );

    let mid = (entries.len() / 2).clamp(1, entries.len() - 2);
    let mut left_entries = entries;
    let mut right_entries = left_entries.split_off(mid);
    let (sep, right_first) = right_entries.remove(0);

    let left = write_node(
        batch,
        Some(old_page),
        &Node::Branch {
            first_child,
            entries: left_entries,
        },
    )?;
    let right = write_node(
        batch,
        None,
        &Node::Branch {
            first_child: right_first,
            entries: right_entries,
        },
    )?;
    Ok(Insert::Split(left, sep, right))
}

/// Delete `key`. Returns the new root and whether the key existed.
pub(crate) fn remove<S: Storage>(
    batch: &mut WriteBatch<'_, S>,
    root: PageId,
    key: &[u8],
) -> Result<(PageId, bool)> {
    if root == NIL_PAGE || key.len() > max_key_len(batch.page_size()) {
        return Ok((root, false));
    }
    Ok(match delete_rec(batch, root, key)? {
        Delete::NotFound => (root, false),
        Delete::Updated(new_root) => (new_root, true),
        Delete::Emptied => (NIL_PAGE, true),
    })
}

fn delete_rec<S: Storage>(
    batch: &mut WriteBatch<'_, S>,
    page: PageId,
    key: &[u8],
) -> Result<Delete> {
    let buf = batch.read_page(page)?;
    let mut node = Node::decode(&buf, page)?;
    match &mut node {
        Node::Leaf { entries } => {
            let Ok(i) = entries.binary_search_by(|(k, _)| k.as_slice().cmp(key)) else {
                return Ok(Delete::NotFound);
            };
            entries.remove(i);
            if entries.is_empty() {
                Ok(Delete::Emptied)
            } else {
                Ok(Delete::Updated(write_node(batch, Some(page), &node)?))
            }
        }
        Node::Branch {
            first_child,
            entries,
        } => {
            // conceptual child index: 0 = first_child, i >= 1 = entries[i-1]
            let idx = entries.partition_point(|(k, _)| k.as_slice() <= key);
            let child = if idx == 0 {
                *first_child
            } else {
                entries[idx - 1].1
            };
            match delete_rec(batch, child, key)? {
                Delete::NotFound => Ok(Delete::NotFound),
                Delete::Updated(new_child) => {
                    if idx == 0 {
                        *first_child = new_child;
                    } else {
                        entries[idx - 1].1 = new_child;
                    }
                    merge_underfull_child(batch, first_child, entries, idx)?;
                    if entries.is_empty() {
                        // merged down to a single child: collapse the level
                        return Ok(Delete::Updated(*first_child));
                    }
                    Ok(Delete::Updated(write_node(batch, Some(page), &node)?))
                }
                Delete::Emptied => {
                    if idx == 0 {
                        if entries.is_empty() {
                            return Ok(Delete::Emptied);
                        }
                        *first_child = entries.remove(0).1;
                    } else {
                        entries.remove(idx - 1);
                    }
                    if entries.is_empty() {
                        // single-child branch: collapse the level entirely
                        return Ok(Delete::Updated(*first_child));
                    }
                    Ok(Delete::Updated(write_node(batch, Some(page), &node)?))
                }
            }
        }
    }
}

/// Serialize a node, reusing `old_page` when this batch already owns it.
/// After a delete descended into the child at conceptual index `idx`
/// (0 = first_child, i >= 1 = entries[i - 1]), merge that child with a
/// neighbor when it has become underfull and the pair fits into one page.
/// Cascades naturally: the parent gets smaller here, and the caller one
/// level up runs the same check on it.
fn merge_underfull_child<S: Storage>(
    batch: &mut WriteBatch<'_, S>,
    first_child: &mut PageId,
    entries: &mut Vec<(Vec<u8>, PageId)>,
    idx: usize,
) -> Result<()> {
    if entries.is_empty() {
        return Ok(()); // single child, nothing to merge with
    }
    let ps = batch.page_size() as usize;
    let child_page = if idx == 0 {
        *first_child
    } else {
        entries[idx - 1].1
    };
    let child_buf = batch.read_page(child_page)?;
    if Node::decode(&child_buf, child_page)?.encoded_size() >= ps / 4 {
        return Ok(()); // healthy
    }

    // prefer the right neighbor; the last child merges leftward
    let (left_idx, right_idx) = if idx < entries.len() {
        (idx, idx + 1)
    } else {
        (idx - 1, idx)
    };
    let left_page = if left_idx == 0 {
        *first_child
    } else {
        entries[left_idx - 1].1
    };
    let right_page = entries[right_idx - 1].1;
    let left_buf = batch.read_page(left_page)?;
    let right_buf = batch.read_page(right_page)?;
    let left = Node::decode(&left_buf, left_page)?;
    let right = Node::decode(&right_buf, right_page)?;

    let merged = match (left, right) {
        (Node::Leaf { entries: mut l }, Node::Leaf { entries: r }) => {
            l.extend(r);
            Node::Leaf { entries: l }
        }
        (
            Node::Branch {
                first_child: lf,
                entries: mut l,
            },
            Node::Branch {
                first_child: rf,
                entries: r,
            },
        ) => {
            // the parent separator is exactly the key the right node's
            // first child needs once it joins the left node's entry list
            let sep = entries[right_idx - 1].0.clone();
            l.push((sep, rf));
            l.extend(r);
            Node::Branch {
                first_child: lf,
                entries: l,
            }
        }
        _ => {
            return Err(Error::corrupted(
                left_page,
                "sibling nodes of different kinds",
            ))
        }
    };

    // leave headroom so a merge does not split again on the next insert
    if merged.encoded_size() > ps * 7 / 8 {
        return Ok(());
    }
    let new_left = write_node(batch, Some(left_page), &merged)?;
    if left_idx == 0 {
        *first_child = new_left;
    } else {
        entries[left_idx - 1].1 = new_left;
    }
    entries.remove(right_idx - 1);
    Ok(())
}

/// Insert every page a tree references into `out`: node pages plus
/// overflow chain pages. Garbage collection marks live pages with this;
/// `Db::stats` reuses it for diagnostics. Skipping already-present pages
/// is not just a cycle guard, it makes marking shared COW structure
/// between commits cheap, because an already-marked node implies an
/// already-marked subtree.
pub(crate) fn collect_pages<P: ReadPages>(
    src: &P,
    root: PageId,
    out: &mut HashSet<PageId>,
) -> Result<()> {
    if root == NIL_PAGE {
        return Ok(());
    }
    let mut stack = vec![root];
    while let Some(page) = stack.pop() {
        if !out.insert(page) {
            continue;
        }
        let buf = src.read(page)?;
        match Node::decode(&buf, page)? {
            Node::Leaf { entries } => {
                for (_, value) in entries {
                    if let ValueRef::Overflow { head, len } = value {
                        collect_overflow(src, head, len, out)?;
                    }
                }
            }
            Node::Branch {
                first_child,
                entries,
            } => {
                stack.push(first_child);
                stack.extend(entries.into_iter().map(|(_, child)| child));
            }
        }
    }
    Ok(())
}

fn collect_overflow<P: ReadPages>(
    src: &P,
    head: PageId,
    len: u64,
    out: &mut HashSet<PageId>,
) -> Result<()> {
    let chunk = overflow_capacity(src.page_size());
    let max_pages = (len as usize).div_ceil(chunk).max(1);
    let mut page = head;
    for _ in 0..max_pages {
        if page == NIL_PAGE || !out.insert(page) {
            break;
        }
        let buf = src.read(page)?;
        if crate::page::page_type(&buf)? != PageType::Overflow {
            return Err(Error::corrupted(
                page,
                "overflow chain hit a non-overflow page",
            ));
        }
        page = u64::from_le_bytes(
            buf[PAGE_HEADER_LEN..PAGE_HEADER_LEN + 8]
                .try_into()
                .expect("hdr"),
        );
    }
    Ok(())
}

fn write_node<S: Storage>(
    batch: &mut WriteBatch<'_, S>,
    old_page: Option<PageId>,
    node: &Node,
) -> Result<PageId> {
    let encoded = node.encode(batch.page_size());
    let id = match old_page {
        Some(id) if batch.owns(id) => id,
        _ => batch.allocate(match node {
            Node::Leaf { .. } => PageType::Leaf,
            Node::Branch { .. } => PageType::Branch,
        }),
    };
    batch.page_mut(id)?.copy_from_slice(&encoded);
    Ok(id)
}

fn write_overflow<S: Storage>(batch: &mut WriteBatch<'_, S>, value: &[u8]) -> Result<ValueRef> {
    let chunk = overflow_capacity(batch.page_size());
    let ids: Vec<PageId> = value
        .chunks(chunk)
        .map(|_| batch.allocate(PageType::Overflow))
        .collect();
    for (i, (part, &id)) in value.chunks(chunk).zip(&ids).enumerate() {
        let next = ids.get(i + 1).copied().unwrap_or(NIL_PAGE);
        let page = batch.page_mut(id)?;
        page[PAGE_HEADER_LEN..PAGE_HEADER_LEN + 8].copy_from_slice(&next.to_le_bytes());
        page[PAGE_HEADER_LEN + 8..PAGE_HEADER_LEN + 10].copy_from_slice(
            &u16::try_from(part.len())
                .expect("chunk fits u16")
                .to_le_bytes(),
        );
        page[PAGE_HEADER_LEN + 10..PAGE_HEADER_LEN + 10 + part.len()].copy_from_slice(part);
    }
    Ok(ValueRef::Overflow {
        head: ids.first().copied().unwrap_or(NIL_PAGE),
        len: value.len() as u64,
    })
}

// ---------------------------------------------------------------------------
// Range scans
// ---------------------------------------------------------------------------

/// Forward iterator over `[start, end)` in key order. `None` bounds mean
/// unbounded. Yields owned key/value pairs with overflow values resolved.
pub struct Scan<'a, P: ReadPages> {
    src: &'a P,
    /// Branches on the path with the index of the next conceptual child to
    /// descend into once the current subtree is exhausted.
    stack: Vec<(Node, usize)>,
    leaf: Vec<(Vec<u8>, ValueRef)>,
    leaf_idx: usize,
    end: Option<Vec<u8>>,
    failed: bool,
}

impl<'a, P: ReadPages> Scan<'a, P> {
    pub(crate) fn new(
        src: &'a P,
        root: PageId,
        start: Option<&[u8]>,
        end: Option<&[u8]>,
    ) -> Result<Self> {
        let mut scan = Scan {
            src,
            stack: Vec::new(),
            leaf: Vec::new(),
            leaf_idx: 0,
            end: end.map(<[u8]>::to_vec),
            failed: false,
        };
        if root != NIL_PAGE {
            scan.descend(root, start)?;
        }
        Ok(scan)
    }

    /// Walk down to the leaf that contains the first key >= start,
    /// remembering the path for later sideways moves.
    fn descend(&mut self, mut page: PageId, start: Option<&[u8]>) -> Result<()> {
        loop {
            let buf = self.src.read(page)?;
            match Node::decode(&buf, page)? {
                Node::Branch {
                    first_child,
                    entries,
                } => {
                    let idx = match start {
                        Some(key) => entries.partition_point(|(k, _)| k.as_slice() <= key),
                        None => 0,
                    };
                    let child = if idx == 0 {
                        first_child
                    } else {
                        entries[idx - 1].1
                    };
                    self.stack.push((
                        Node::Branch {
                            first_child,
                            entries,
                        },
                        idx + 1,
                    ));
                    page = child;
                }
                Node::Leaf { entries } => {
                    self.leaf_idx = match start {
                        Some(key) => entries.partition_point(|(k, _)| k.as_slice() < key),
                        None => 0,
                    };
                    self.leaf = entries;
                    return Ok(());
                }
            }
        }
    }

    /// Move to the next leaf, popping exhausted branches.
    fn advance_leaf(&mut self) -> Result<bool> {
        while let Some((node, next_idx)) = self.stack.last_mut() {
            let Node::Branch {
                first_child,
                entries,
            } = node
            else {
                unreachable!("scan stack only holds branches")
            };
            if *next_idx > entries.len() {
                self.stack.pop();
                continue;
            }
            let child = if *next_idx == 0 {
                *first_child
            } else {
                entries[*next_idx - 1].1
            };
            *next_idx += 1;
            self.descend(child, None)?;
            return Ok(true);
        }
        Ok(false)
    }
}

impl<P: ReadPages> Iterator for Scan<'_, P> {
    type Item = Result<(Vec<u8>, Vec<u8>)>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        loop {
            if self.leaf_idx >= self.leaf.len() {
                match self.advance_leaf() {
                    Ok(true) => continue,
                    Ok(false) => return None,
                    Err(e) => {
                        self.failed = true;
                        return Some(Err(e));
                    }
                }
            }
            let (key, vref) = &self.leaf[self.leaf_idx];
            if let Some(end) = &self.end {
                if key >= end {
                    return None;
                }
            }
            self.leaf_idx += 1;
            return match read_value(self.src, vref) {
                Ok(value) => Some(Ok((key.clone(), value))),
                Err(e) => {
                    self.failed = true;
                    Some(Err(e))
                }
            };
        }
    }
}
