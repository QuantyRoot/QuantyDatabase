//! Branch refs.
//!
//! Branch heads live in their own small tree whose root sits in the meta,
//! not in the versioned catalog. That separation is load bearing: a commit
//! must not version the pointers that point at it, or two branches would
//! each carry a stale copy of the other's head. Same reason git keeps
//! refs/ outside the object store.
//!
//! Keys use the ordinary tuple encoding:
//!
//! - `("head")` holds the current branch name
//! - `("branch", name)` holds a [`BranchRef`]
//!
//! A database starts with an empty refs tree, which reads as a single
//! implicit branch called "main" whose head is the newest commit. The tree
//! materializes on the first branch operation, so plain linear use never
//! pays for any of this.

use crate::encoding::{encode_key, Value};
use crate::error::{Error, Result};
use crate::page::PageId;

pub const DEFAULT_BRANCH: &str = "main";

/// One branch head as stored in the refs tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BranchRef {
    /// Newest commit on this branch.
    pub head_id: u64,
    /// Page of that commit's record; 0 when head_id is 0 (empty history).
    pub head_page: PageId,
    /// Oldest commit id still readable on this branch. 0 means the whole
    /// history down to the root is retained. Garbage collection raises
    /// this so lookups never walk into reclaimed records.
    pub floor_id: u64,
}

impl BranchRef {
    pub(crate) fn encode(&self) -> [u8; 24] {
        let mut out = [0u8; 24];
        out[0..8].copy_from_slice(&self.head_id.to_le_bytes());
        out[8..16].copy_from_slice(&self.head_page.to_le_bytes());
        out[16..24].copy_from_slice(&self.floor_id.to_le_bytes());
        out
    }

    pub(crate) fn decode(buf: &[u8]) -> Result<BranchRef> {
        if buf.len() != 24 {
            return Err(Error::corrupted(None, "branch ref has the wrong size"));
        }
        let u64_at = |off: usize| u64::from_le_bytes(buf[off..off + 8].try_into().expect("len"));
        Ok(BranchRef {
            head_id: u64_at(0),
            head_page: u64_at(8),
            floor_id: u64_at(16),
        })
    }
}

pub(crate) fn head_key() -> Vec<u8> {
    encode_key(&[Value::Text("head".into())])
}

pub(crate) fn branch_key(name: &str) -> Vec<u8> {
    encode_key(&[Value::Text("branch".into()), Value::Text(name.into())])
}

pub(crate) fn branches_prefix() -> Vec<u8> {
    encode_key(&[Value::Text("branch".into())])
}

/// Branch names travel through QQL and the refs tree; keep them boring.
pub(crate) fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric());
    if ok {
        Ok(())
    } else {
        Err(Error::InvalidArgument(
            "branch names are 1 to 64 ascii letters, digits, '_' or '-', starting alphanumeric",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_ref_roundtrips() {
        let r = BranchRef {
            head_id: 7,
            head_page: 42,
            floor_id: 3,
        };
        assert_eq!(BranchRef::decode(&r.encode()).unwrap(), r);
        assert!(BranchRef::decode(&[0u8; 10]).is_err());
    }

    #[test]
    fn name_validation() {
        for good in ["main", "feature-1", "a", "x_y", "B2"] {
            validate_name(good).unwrap();
        }
        for bad in ["", "-lead", "_lead", "has space", "ümlaut", &"x".repeat(65)] {
            assert!(validate_name(bad).is_err(), "accepted {bad:?}");
        }
    }
}
