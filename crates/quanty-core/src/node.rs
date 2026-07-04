//! B-tree node serialization.
//!
//! Nodes are decoded into plain in-memory structures, modified there, and
//! re-encoded onto fresh pages. That trades some CPU against a lot of
//! slotted-page complexity, which is the right trade while correctness is
//! being established (phase 1). See docs/FORMAT.md for the byte layout.

use std::sync::Arc;

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
    pub(crate) fn decode(buf: &Arc<[u8]>, id: PageId) -> Result<Node> {
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
    pub(crate) fn encode(&self, page_size: u32) -> Box<[u8]> {
        let mut buf = vec![0u8; page_size as usize];
        let (ptype, count) = match self {
            Node::Leaf { entries } => (PageType::Leaf, entries.len()),
            Node::Branch { entries, .. } => (PageType::Branch, entries.len()),
        };
        page::init_header(&mut buf, ptype);
        buf[6..8].copy_from_slice(
            &u16::try_from(count)
                .expect("node entry count")
                .to_le_bytes(),
        );

        let mut at = PAGE_HEADER_LEN;
        match self {
            Node::Leaf { entries } => {
                for (key, value) in entries {
                    let vflag = match value {
                        ValueRef::Inline(_) => VFLAG_INLINE,
                        ValueRef::Overflow { .. } => VFLAG_OVERFLOW,
                    };
                    at = write_key(&mut buf, at, key, vflag);
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
                }
            }
            Node::Branch {
                first_child,
                entries,
            } => {
                buf[at..at + 8].copy_from_slice(&first_child.to_le_bytes());
                at += 8;
                for (key, child) in entries {
                    at = write_key(&mut buf, at, key, 0);
                    buf[at..at + 8].copy_from_slice(&child.to_le_bytes());
                    at += 8;
                }
            }
        }
        debug_assert!(at <= page_size as usize, "node encode overran the page");
        buf.into_boxed_slice()
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

    fn arc(buf: Box<[u8]>) -> Arc<[u8]> {
        Arc::from(buf)
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
