//! B-tree node serialization.
//!
//! Nodes are decoded into plain in-memory structures, modified there, and
//! re-encoded onto fresh pages. That trades some CPU against a lot of
//! slotted-page complexity, which is the right trade while correctness is
//! being established (phase 1). See docs/FORMAT.md for the byte layout.
//!
//! The one exception is a new cell in a leaf the write batch already
//! owns, which goes straight into the page through [`write_leaf_cell`],
//! the same function the encoder uses (ADR-048).

use crate::error::{Error, Result};
use crate::page::{self, PageId, PageType, PAGE_HEADER_LEN};

/// Where a leaf entry's value lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ValueRef {
    Inline(Vec<u8>),
    /// Head page of an overflow chain plus the total value length.
    Overflow {
        head: PageId,
        len: u64,
    },
}

const VFLAG_INLINE: u8 = 0;
const VFLAG_OVERFLOW: u8 = 1;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Node {
    Leaf {
        entries: Vec<(Vec<u8>, ValueRef)>,
    },
    Branch {
        /// Child for keys below entries[0].key.
        first_child: PageId,
        /// entries[i].0 is the lowest key reachable through entries[i].1.
        entries: Vec<(Vec<u8>, PageId)>,
    },
}

impl Node {
    pub(crate) fn decode(buf: &[u8], id: PageId) -> Result<Node> {
        let bad = |what: &str| Error::corrupted(id, format!("node decode: {what}"));
        let count = u16::from_le_bytes(buf[6..8].try_into().expect("hdr")) as usize;
        let body = &buf[PAGE_HEADER_LEN..];
        match page::page_type(buf)? {
            PageType::Leaf => {
                let mut entries = Vec::with_capacity(count);
                let mut cur = body;
                for _ in 0..count {
                    let (key, vflag, rest) = read_key(cur).ok_or_else(|| bad("truncated cell"))?;
                    cur = rest;
                    let value = match vflag {
                        VFLAG_INLINE => {
                            let (len, rest) = read_u32(cur).ok_or_else(|| bad("truncated vlen"))?;
                            if rest.len() < len as usize {
                                return Err(bad("truncated value"));
                            }
                            let (val, rest) = rest.split_at(len as usize);
                            cur = rest;
                            ValueRef::Inline(val.to_vec())
                        }
                        VFLAG_OVERFLOW => {
                            let (head, rest) = read_u64(cur).ok_or_else(|| bad("truncated ovf"))?;
                            let (len, rest) = read_u64(rest).ok_or_else(|| bad("truncated ovf"))?;
                            cur = rest;
                            ValueRef::Overflow { head, len }
                        }
                        _ => return Err(bad("bad value flag")),
                    };
                    entries.push((key, value));
                }
                if !entries.windows(2).all(|w| w[0].0 < w[1].0) {
                    return Err(bad("leaf keys not strictly sorted"));
                }
                Ok(Node::Leaf { entries })
            }
            PageType::Branch => {
                let (first_child, mut cur) = read_u64(body).ok_or_else(|| bad("truncated"))?;
                let mut entries = Vec::with_capacity(count);
                for _ in 0..count {
                    let (key, child_hi, rest) =
                        read_key(cur).ok_or_else(|| bad("truncated cell"))?;
                    // branch cells reuse the flag byte as the top of nothing:
                    // it must be zero in format v1
                    if child_hi != 0 {
                        return Err(bad("bad branch flag"));
                    }
                    let (child, rest) = read_u64(rest).ok_or_else(|| bad("truncated child"))?;
                    cur = rest;
                    entries.push((key, child));
                }
                if !entries.windows(2).all(|w| w[0].0 < w[1].0) {
                    return Err(bad("branch keys not strictly sorted"));
                }
                Ok(Node::Branch {
                    first_child,
                    entries,
                })
            }
            other => Err(bad(&format!("expected a btree node, found {other:?}"))),
        }
    }

    /// Encode into a fresh page buffer of `page_size` bytes. The caller has
    /// already checked [`Node::encoded_size`] against the page, so an
    /// overrun here is a logic bug, not an input error.
    /// Encode into a fresh buffer. Only the round-trip tests want this;
    /// the insert path writes into the page directly.
    #[cfg(test)]
    pub(crate) fn encode(&self, page_size: u32) -> Box<[u8]> {
        let mut buf = vec![0u8; page_size as usize];
        self.encode_into(&mut buf);
        buf.into_boxed_slice()
    }

    /// Write straight into a page rather than into a buffer that is then
    /// copied over one.
    ///
    /// The tail past the last cell is zeroed explicitly. Decoding stops at
    /// the entry count and would never read it, but the same logical node
    /// has to produce the same bytes: the codec fuzzer proves the encoding
    /// is canonical, and leaving whatever the page held before would make
    /// that quietly untrue.
    /// Which child of a branch page covers `key`, read straight from the
    /// bytes.
    ///
    /// Decoding a branch to follow one pointer allocates a `Vec` for every
    /// key on the page and throws them all away again, at every level, on
    /// every descent. This walks the cells in place instead. The scan is
    /// linear where the decoded version binary searched, which is the
    /// trade: a hundred and fifty short memcmps against a hundred and
    /// fifty allocations.
    ///
    /// Same rule as the decoded path: the child of the last key that is
    /// not greater than `key`, or the first child if there is none.
    pub(crate) fn branch_child(buf: &[u8], id: PageId, key: &[u8]) -> Result<PageId> {
        let bad = |what: &str| Error::corrupted(id, format!("branch scan: {what}"));
        let count = u16::from_le_bytes(buf[6..8].try_into().expect("hdr")) as usize;
        let body = &buf[PAGE_HEADER_LEN..];
        let (first_child, mut cur) = read_u64(body).ok_or_else(|| bad("truncated"))?;

        let mut child = first_child;
        for _ in 0..count {
            let (cell_key, flag, rest) = read_key_ref(cur).ok_or_else(|| bad("truncated cell"))?;
            if flag != 0 {
                return Err(bad("bad branch flag"));
            }
            let (next, rest) = read_u64(rest).ok_or_else(|| bad("truncated child"))?;
            cur = rest;
            if cell_key > key {
                break;
            }
            child = next;
        }
        Ok(child)
    }

    /// Returns the number of bytes the encoding uses, header included.
    pub(crate) fn encode_into(&self, buf: &mut [u8]) -> usize {
        let page_size = buf.len();
        let (ptype, count) = match self {
            Node::Leaf { entries } => (PageType::Leaf, entries.len()),
            Node::Branch { entries, .. } => (PageType::Branch, entries.len()),
        };
        page::init_header(buf, ptype);
        buf[6..8].copy_from_slice(
            &u16::try_from(count)
                .expect("node entry count")
                .to_le_bytes(),
        );

        let mut at = PAGE_HEADER_LEN;
        match self {
            Node::Leaf { entries } => {
                for (key, value) in entries {
                    at = write_leaf_cell(buf, at, key, value);
                }
            }
            Node::Branch {
                first_child,
                entries,
            } => {
                buf[at..at + 8].copy_from_slice(&first_child.to_le_bytes());
                at += 8;
                for (key, child) in entries {
                    at = write_key(buf, at, key, 0);
                    buf[at..at + 8].copy_from_slice(&child.to_le_bytes());
                    at += 8;
                }
            }
        }
        debug_assert!(at <= page_size, "node encode overran the page");
        buf[at..].fill(0);
        at
    }

    /// Exact number of bytes [`Node::encode`] will use.
    pub(crate) fn encoded_size(&self) -> usize {
        PAGE_HEADER_LEN
            + match self {
                Node::Leaf { entries } => entries.iter().map(|(k, v)| leaf_cell_size(k, v)).sum(),
                Node::Branch { entries, .. } => {
                    8 + entries
                        .iter()
                        .map(|(k, _)| branch_cell_size(k))
                        .sum::<usize>()
                }
            }
    }
}

/// Where a leaf cell's value is, read in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CellValue {
    /// Byte range of the value within the page.
    Inline(usize, usize),
    Overflow {
        head: PageId,
        len: u64,
    },
}

/// One leaf cell at byte offset `at`: the key's byte range, where the
/// value is, and the offset of the next cell. Exactly the checks `decode`
/// makes for a cell, without copying anything.
pub(crate) fn leaf_cell_at(
    buf: &[u8],
    at: usize,
    id: PageId,
) -> Result<((usize, usize), CellValue, usize)> {
    let bad = |what: &str| Error::corrupted(id, format!("node decode: {what}"));
    let (key, vflag, rest) = read_key_ref(&buf[at..]).ok_or_else(|| bad("truncated cell"))?;
    let key_start = at + 3;
    let key_end = key_start + key.len();
    match vflag {
        VFLAG_INLINE => {
            let (len, rest) = read_u32(rest).ok_or_else(|| bad("truncated vlen"))?;
            if rest.len() < len as usize {
                return Err(bad("truncated value"));
            }
            let start = key_end + 4;
            let end = start + len as usize;
            Ok(((key_start, key_end), CellValue::Inline(start, end), end))
        }
        VFLAG_OVERFLOW => {
            let (head, rest) = read_u64(rest).ok_or_else(|| bad("truncated ovf"))?;
            let (len, _) = read_u64(rest).ok_or_else(|| bad("truncated ovf"))?;
            Ok((
                (key_start, key_end),
                CellValue::Overflow { head, len },
                key_end + 16,
            ))
        }
        _ => Err(bad("bad value flag")),
    }
}

/// The value stored under `key` in a leaf page, read in place.
///
/// Every cell is still read and the keys checked for order, exactly as
/// `decode` would, so a damaged leaf is an error here too rather than an
/// answer; what is gone is building a vector with every key on the page
/// in it to find one of them.
pub(crate) fn find_in_leaf(buf: &[u8], id: PageId, key: &[u8]) -> Result<Option<CellValue>> {
    let count = u16::from_le_bytes(buf[6..8].try_into().expect("hdr")) as usize;
    let mut at = PAGE_HEADER_LEN;
    let mut prev: Option<(usize, usize)> = None;
    let mut found = None;
    for _ in 0..count {
        let ((ks, ke), value, next) = leaf_cell_at(buf, at, id)?;
        if let Some((s, e)) = prev {
            if buf[s..e] >= buf[ks..ke] {
                return Err(Error::corrupted(
                    id,
                    "node decode: leaf keys not strictly sorted",
                ));
            }
        }
        if found.is_none() && &buf[ks..ke] == key {
            found = Some(value);
        }
        prev = Some((ks, ke));
        at = next;
    }
    Ok(found)
}

/// A branch page checked the way `decode` would, without building it:
/// every cell readable, flags zero, keys strictly increasing. Returns the
/// first child and the number of entries.
pub(crate) fn check_branch(buf: &[u8], id: PageId) -> Result<(PageId, usize)> {
    let bad = |what: &str| Error::corrupted(id, format!("node decode: {what}"));
    let count = u16::from_le_bytes(buf[6..8].try_into().expect("hdr")) as usize;
    let body = &buf[PAGE_HEADER_LEN..];
    let (first_child, mut cur) = read_u64(body).ok_or_else(|| bad("truncated"))?;
    let mut prev: Option<&[u8]> = None;
    for _ in 0..count {
        let (key, flag, rest) = read_key_ref(cur).ok_or_else(|| bad("truncated cell"))?;
        if flag != 0 {
            return Err(bad("bad branch flag"));
        }
        let (_, rest) = read_u64(rest).ok_or_else(|| bad("truncated child"))?;
        if prev.is_some_and(|p| p >= key) {
            return Err(bad("branch keys not strictly sorted"));
        }
        prev = Some(key);
        cur = rest;
    }
    Ok((first_child, count))
}

/// How many entries of an already checked branch have a key not greater
/// than `key`, which is the decoded path's `partition_point`.
pub(crate) fn branch_rank(buf: &[u8], key: &[u8]) -> usize {
    let count = u16::from_le_bytes(buf[6..8].try_into().expect("hdr")) as usize;
    let mut cur = &buf[PAGE_HEADER_LEN + 8..];
    let mut rank = 0;
    for _ in 0..count {
        let (cell_key, _, rest) = read_key_ref(cur).expect("checked branch");
        if cell_key > key {
            break;
        }
        rank += 1;
        cur = &rest[8..];
    }
    rank
}

/// Child `idx` of an already checked branch: 0 is the first child, and
/// `i` is the child of entry `i - 1`.
pub(crate) fn branch_child_at(buf: &[u8], idx: usize) -> PageId {
    let body = &buf[PAGE_HEADER_LEN..];
    let (first_child, mut cur) = read_u64(body).expect("checked branch");
    if idx == 0 {
        return first_child;
    }
    for _ in 0..idx - 1 {
        let (_, _, rest) = read_key_ref(cur).expect("checked branch");
        cur = &rest[8..];
    }
    let (_, _, rest) = read_key_ref(cur).expect("checked branch");
    read_u64(rest).expect("checked branch").0
}

/// Check a leaf page the way `decode` would, without building it: every
/// cell readable and keys strictly increasing. Returns the cell count.
pub(crate) fn check_leaf(buf: &[u8], id: PageId) -> Result<usize> {
    let count = u16::from_le_bytes(buf[6..8].try_into().expect("hdr")) as usize;
    let mut at = PAGE_HEADER_LEN;
    let mut prev: Option<(usize, usize)> = None;
    for _ in 0..count {
        let (key, _, next) = leaf_cell_at(buf, at, id)?;
        if let Some((s, e)) = prev {
            if buf[s..e] >= buf[key.0..key.1] {
                return Err(Error::corrupted(
                    id,
                    "node decode: leaf keys not strictly sorted",
                ));
            }
        }
        prev = Some(key);
        at = next;
    }
    Ok(count)
}

/// Write one leaf cell at `at` and return where the next one starts. The
/// encoder and the in-place insert both write cells through this, so the
/// two cannot disagree about the layout.
pub(crate) fn write_leaf_cell(buf: &mut [u8], at: usize, key: &[u8], value: &ValueRef) -> usize {
    let vflag = match value {
        ValueRef::Inline(_) => VFLAG_INLINE,
        ValueRef::Overflow { .. } => VFLAG_OVERFLOW,
    };
    let mut at = write_key(buf, at, key, vflag);
    match value {
        ValueRef::Inline(v) => {
            let len = u32::try_from(v.len()).expect("inline value length");
            buf[at..at + 4].copy_from_slice(&len.to_le_bytes());
            at += 4;
            buf[at..at + v.len()].copy_from_slice(v);
            at += v.len();
        }
        ValueRef::Overflow { head, len } => {
            buf[at..at + 8].copy_from_slice(&head.to_le_bytes());
            buf[at + 8..at + 16].copy_from_slice(&len.to_le_bytes());
            at += 16;
        }
    }
    at
}

pub(crate) fn leaf_cell_size(key: &[u8], value: &ValueRef) -> usize {
    2 + 1
        + key.len()
        + match value {
            ValueRef::Inline(v) => 4 + v.len(),
            ValueRef::Overflow { .. } => 16,
        }
}

pub(crate) fn branch_cell_size(key: &[u8]) -> usize {
    2 + 1 + key.len() + 8
}

/// A cell's key without copying it.
fn read_key_ref(buf: &[u8]) -> Option<(&[u8], u8, &[u8])> {
    if buf.len() < 3 {
        return None;
    }
    let klen = u16::from_le_bytes(buf[..2].try_into().expect("len")) as usize;
    let flag = buf[2];
    let rest = &buf[3..];
    if rest.len() < klen {
        return None;
    }
    let (key, rest) = rest.split_at(klen);
    Some((key, flag, rest))
}

fn read_key(buf: &[u8]) -> Option<(Vec<u8>, u8, &[u8])> {
    if buf.len() < 3 {
        return None;
    }
    let klen = u16::from_le_bytes(buf[..2].try_into().expect("len")) as usize;
    let flag = buf[2];
    let rest = &buf[3..];
    if rest.len() < klen {
        return None;
    }
    let (key, rest) = rest.split_at(klen);
    Some((key.to_vec(), flag, rest))
}

fn write_key(buf: &mut [u8], at: usize, key: &[u8], flag: u8) -> usize {
    let klen = u16::try_from(key.len()).expect("key length fits u16");
    buf[at..at + 2].copy_from_slice(&klen.to_le_bytes());
    buf[at + 2] = flag;
    buf[at + 3..at + 3 + key.len()].copy_from_slice(key);
    at + 3 + key.len()
}

fn read_u32(buf: &[u8]) -> Option<(u32, &[u8])> {
    if buf.len() < 4 {
        return None;
    }
    Some((
        u32::from_le_bytes(buf[..4].try_into().expect("len")),
        &buf[4..],
    ))
}

fn read_u64(buf: &[u8]) -> Option<(u64, &[u8])> {
    if buf.len() < 8 {
        return None;
    }
    Some((
        u64::from_le_bytes(buf[..8].try_into().expect("len")),
        &buf[8..],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decoding takes a plain slice now, so this just names the step the
    /// tests used to spell out.
    fn arc(buf: Box<[u8]>) -> Box<[u8]> {
        buf
    }

    #[test]
    fn leaf_roundtrips() {
        let node = Node::Leaf {
            entries: vec![
                (b"a".to_vec(), ValueRef::Inline(b"hello".to_vec())),
                (
                    b"b".to_vec(),
                    ValueRef::Overflow {
                        head: 42,
                        len: 100_000,
                    },
                ),
                (b"c\x00d".to_vec(), ValueRef::Inline(vec![])),
            ],
        };
        assert!(node.encoded_size() <= 512);
        let buf = arc(node.encode(512));
        assert_eq!(Node::decode(&buf, 5).unwrap(), node);
    }

    #[test]
    fn branch_roundtrips() {
        let node = Node::Branch {
            first_child: 7,
            entries: vec![(b"m".to_vec(), 8), (b"t".to_vec(), 9)],
        };
        let buf = arc(node.encode(512));
        assert_eq!(Node::decode(&buf, 5).unwrap(), node);
    }

    #[test]
    fn encoded_size_is_exact() {
        let node = Node::Leaf {
            entries: vec![
                (b"key".to_vec(), ValueRef::Inline(b"value".to_vec())),
                // len chosen so its most significant (= last LE) byte is
                // non-zero, otherwise the rposition trick below undershoots
                (
                    b"key2".to_vec(),
                    ValueRef::Overflow {
                        head: 3,
                        len: 0xAB00_0000_0000_0009,
                    },
                ),
            ],
        };
        // encode into a page and find the last non-zero byte; it must sit
        // exactly at encoded_size
        let buf = node.encode(512);
        let used = buf.iter().rposition(|&b| b != 0).unwrap() + 1;
        assert_eq!(used, node.encoded_size());
    }

    #[test]
    fn unsorted_leaf_is_rejected() {
        let node = Node::Leaf {
            entries: vec![
                (b"b".to_vec(), ValueRef::Inline(vec![1])),
                (b"a".to_vec(), ValueRef::Inline(vec![2])),
            ],
        };
        let buf = arc(node.encode(512));
        assert!(Node::decode(&buf, 5).is_err());
    }
}

#[cfg(test)]
mod in_place {
    //! The readers that work on page bytes answer exactly what decoding
    //! and searching the node would, and refuse exactly the pages decoding
    //! refuses. They replaced `decode` on every read path, so this is the
    //! claim the read side now rests on.

    use super::*;

    fn rng(seed: &mut u64) -> u64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *seed
    }

    fn page(node: &Node) -> Vec<u8> {
        let mut buf = vec![0u8; 4096];
        node.encode_into(&mut buf);
        buf
    }

    fn keys(seed: &mut u64, n: usize) -> Vec<Vec<u8>> {
        let mut ks: Vec<Vec<u8>> = (0..n)
            .map(|_| {
                let len = (rng(seed) % 12) as usize + 1;
                (0..len).map(|_| (rng(seed) % 4) as u8).collect()
            })
            .collect();
        ks.sort();
        ks.dedup();
        ks
    }

    #[test]
    fn a_leaf_answers_as_its_decoded_node_does() {
        let mut seed = 0x1eaf;
        for _ in 0..300 {
            let n = (rng(&mut seed) % 60) as usize;
            let entries: Vec<(Vec<u8>, ValueRef)> = keys(&mut seed, n)
                .into_iter()
                .map(|k| {
                    let v = if rng(&mut seed).is_multiple_of(5) {
                        ValueRef::Overflow {
                            head: rng(&mut seed) % 1000,
                            len: rng(&mut seed) % 100_000,
                        }
                    } else {
                        ValueRef::Inline(k.iter().rev().copied().collect())
                    };
                    (k, v)
                })
                .collect();
            let buf = page(&Node::Leaf {
                entries: entries.clone(),
            });
            assert_eq!(check_leaf(&buf, 3).unwrap(), entries.len());
            let probes = keys(&mut seed, 20);
            for probe in probes.iter().chain(entries.iter().map(|(k, _)| k)) {
                let want = entries.iter().find(|(k, _)| k == probe).map(|(_, v)| v);
                let got = find_in_leaf(&buf, 3, probe).unwrap();
                match (want, got) {
                    (None, None) => {}
                    (Some(ValueRef::Inline(v)), Some(CellValue::Inline(s, e))) => {
                        assert_eq!(&buf[s..e], &v[..])
                    }
                    (
                        Some(ValueRef::Overflow { head, len }),
                        Some(CellValue::Overflow { head: h, len: l }),
                    ) => {
                        assert_eq!((*head, *len), (h, l))
                    }
                    (w, g) => panic!("decoded {w:?}, read in place {g:?}"),
                }
            }
        }
    }

    #[test]
    fn a_branch_routes_as_its_decoded_node_does() {
        let mut seed = 0xb4a7;
        for _ in 0..300 {
            let n = (rng(&mut seed) % 60) as usize;
            let entries: Vec<(Vec<u8>, PageId)> = keys(&mut seed, n)
                .into_iter()
                .map(|k| (k, rng(&mut seed) % 10_000))
                .collect();
            let first_child = rng(&mut seed) % 10_000;
            let buf = page(&Node::Branch {
                first_child,
                entries: entries.clone(),
            });
            assert_eq!(check_branch(&buf, 3).unwrap(), (first_child, entries.len()));
            for i in 0..=entries.len() {
                let want = if i == 0 {
                    first_child
                } else {
                    entries[i - 1].1
                };
                assert_eq!(branch_child_at(&buf, i), want);
            }
            for probe in keys(&mut seed, 20)
                .iter()
                .chain(entries.iter().map(|(k, _)| k))
            {
                let want = entries.partition_point(|(k, _)| k <= probe);
                assert_eq!(branch_rank(&buf, probe), want);
                let child = if want == 0 {
                    first_child
                } else {
                    entries[want - 1].1
                };
                assert_eq!(Node::branch_child(&buf, 3, probe).unwrap(), child);
            }
        }
    }

    #[test]
    fn what_decode_refuses_the_readers_refuse() {
        let leaf = page(&Node::Leaf {
            entries: vec![
                (b"a".to_vec(), ValueRef::Inline(b"1".to_vec())),
                (b"b".to_vec(), ValueRef::Inline(b"2".to_vec())),
            ],
        });
        // Keys out of order: swap the two one byte keys in place.
        let mut unsorted = leaf.clone();
        let a = unsorted.iter().position(|&b| b == b'a').unwrap();
        let b = unsorted.iter().position(|&b| b == b'b').unwrap();
        unsorted.swap(a, b);
        // A value flag that means nothing.
        let mut flagged = leaf.clone();
        flagged[PAGE_HEADER_LEN + 2] = 9;
        // More cells claimed than the page holds.
        let mut overcounted = leaf.clone();
        overcounted[6..8].copy_from_slice(&500u16.to_le_bytes());
        for bad in [&unsorted, &flagged, &overcounted] {
            assert!(Node::decode(bad, 3).is_err());
            assert!(check_leaf(bad, 3).is_err());
            assert!(find_in_leaf(bad, 3, b"zz").is_err());
        }

        let branch = page(&Node::Branch {
            first_child: 7,
            entries: vec![(b"a".to_vec(), 8), (b"b".to_vec(), 9)],
        });
        let mut unsorted = branch.clone();
        let a = unsorted.iter().position(|&b| b == b'a').unwrap();
        let b = unsorted.iter().position(|&b| b == b'b').unwrap();
        unsorted.swap(a, b);
        let mut flagged = branch.clone();
        flagged[PAGE_HEADER_LEN + 8 + 2] = 1;
        let mut overcounted = branch.clone();
        overcounted[6..8].copy_from_slice(&500u16.to_le_bytes());
        for bad in [&unsorted, &flagged, &overcounted] {
            assert!(Node::decode(bad, 3).is_err());
            assert!(check_branch(bad, 3).is_err());
        }
    }
}
