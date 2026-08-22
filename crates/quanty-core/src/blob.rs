//! The blob store: content addressed chunks, counted.
//!
//! Chunks live in the catalog tree under `("blob", hash)`, so they are
//! versioned per commit like everything else and their payloads take the
//! overflow chain the B-tree already has. Nothing here needs a new root or
//! a format change (ADR-033).
//!
//! Reachability is not enough to collect them: a chunk nobody references
//! is still an entry in a live tree. So each carries a count, and the
//! entry goes away when it reaches zero.

use crate::encoding::{decode_key, encode_key, Value};
use crate::error::{Error, Result};
use crate::sha256::sha256;

/// Bytes per chunk. Fixed rather than content defined, which dedups
/// identical files and not shifted ones (ADR-033).
pub const CHUNK_SIZE: usize = 1 << 20;

/// Chunks per commit while streaming a blob in.
///
/// A write batch holds every dirty page until it commits, so this is the
/// knob that keeps a gigabyte from becoming a gigabyte of memory. Higher
/// means fewer fsyncs and more resident bytes (ADR-033).
pub const CHUNKS_PER_COMMIT: usize = 8;

/// The address of a chunk: the SHA-256 of its bytes.
pub type ChunkHash = [u8; 32];

/// The catalog key a chunk's bytes live at. Written once and never
/// rewritten, so its overflow chain is stable.
pub(crate) fn chunk_key(hash: &ChunkHash) -> Vec<u8> {
    encode_key(&[Value::Text("blob".into()), Value::Bytes(hash.to_vec())])
}

/// The key a chunk's reference count lives at, away from the bytes.
///
/// A B-tree replaces a value whole, so a count stored alongside the
/// payload would copy the payload's whole overflow chain every time the
/// count moved. That was measured: retaining a 200 kB chunk a second time
/// cost 52 pages, which is the chunk again (ADR-033).
pub(crate) fn refs_key(hash: &ChunkHash) -> Vec<u8> {
    encode_key(&[Value::Text("blobrefs".into()), Value::Bytes(hash.to_vec())])
}

/// The address of these bytes.
pub fn hash_chunk(bytes: &[u8]) -> ChunkHash {
    sha256(bytes)
}

/// Every chunk's bytes, for a walk over the store.
pub(crate) fn chunks_prefix() -> Vec<u8> {
    encode_key(&[Value::Text("blob".into())])
}

/// Every chunk's count.
pub(crate) fn refs_prefix() -> Vec<u8> {
    encode_key(&[Value::Text("blobrefs".into())])
}

/// The hash a chunk key names, or None if the key is not one of ours.
pub(crate) fn hash_from_key(key: &[u8]) -> Result<ChunkHash> {
    let parts = decode_key(key)?;
    let bad = || Error::corrupted(None, "a blob key that is not a blob key");
    match parts.get(1) {
        Some(Value::Bytes(raw)) if raw.len() == 32 => {
            let mut hash = [0u8; 32];
            hash.copy_from_slice(raw);
            Ok(hash)
        }
        _ => Err(bad()),
    }
}

/// What a walk over the chunk store found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlobCheck {
    /// Chunks with a count and bytes, as it should be.
    pub sound: u64,
    /// Bytes of every sound chunk, as stored.
    pub bytes: u64,
    /// Chunks nothing has claimed yet. Either an upload in flight or one
    /// that died; the store cannot tell which, which ADR-033 explains.
    pub unclaimed: u64,
    /// Bytes held by those.
    pub unclaimed_bytes: u64,
    /// Chunks whose bytes are there but whose count is not, or the other
    /// way round. Always zero on a database this code wrote.
    pub broken: Vec<ChunkHash>,
}

impl BlobCheck {
    /// Whether the store is internally consistent.
    pub fn is_sound(&self) -> bool {
        self.broken.is_empty()
    }
}

pub(crate) fn encode_refs(refs: u64) -> Vec<u8> {
    refs.to_le_bytes().to_vec()
}

pub(crate) fn decode_refs(stored: &[u8]) -> Result<u64> {
    let bytes: [u8; 8] = stored
        .try_into()
        .map_err(|_| Error::corrupted(None, "blob reference count is not eight bytes"))?;
    Ok(u64::from_le_bytes(bytes))
}

/// What a row keeps instead of the bytes: the size, and the chunks in
/// order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobRef {
    /// Total bytes of the blob, which the chunk list also implies but
    /// which a reader wants without walking it.
    pub len: u64,
    /// The chunks, in order. Repeats are allowed and are the point: a
    /// file of a million zeroes is one chunk named many times.
    pub chunks: Vec<ChunkHash>,
}

impl BlobRef {
    /// How many distinct chunks this blob holds.
    pub fn distinct_chunks(&self) -> usize {
        let mut seen: Vec<&ChunkHash> = self.chunks.iter().collect();
        seen.sort_unstable();
        seen.dedup();
        seen.len()
    }

    /// The form a row keeps.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(12 + self.chunks.len() * 32);
        out.extend_from_slice(&self.len.to_le_bytes());
        out.extend_from_slice(&(self.chunks.len() as u32).to_le_bytes());
        for hash in &self.chunks {
            out.extend_from_slice(hash);
        }
        out
    }

    /// Read one back, refusing anything malformed.
    pub fn decode(bytes: &[u8]) -> Result<BlobRef> {
        let bad = || Error::corrupted(None, "malformed blob descriptor");
        let len = u64::from_le_bytes(
            bytes
                .get(..8)
                .and_then(|s| s.try_into().ok())
                .ok_or_else(bad)?,
        );
        let count = u32::from_le_bytes(
            bytes
                .get(8..12)
                .and_then(|s| s.try_into().ok())
                .ok_or_else(bad)?,
        ) as usize;
        let rest = bytes.get(12..).ok_or_else(bad)?;
        if rest.len() != count * 32 {
            return Err(bad());
        }
        let mut chunks = Vec::with_capacity(count);
        for slot in rest.chunks_exact(32) {
            let mut hash = [0u8; 32];
            hash.copy_from_slice(slot);
            chunks.push(hash);
        }
        Ok(BlobRef { len, chunks })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_descriptor_survives_a_round_trip() {
        let blob = BlobRef {
            len: 3_000_000,
            chunks: vec![hash_chunk(b"a"), hash_chunk(b"b"), hash_chunk(b"a")],
        };
        assert_eq!(BlobRef::decode(&blob.encode()).unwrap(), blob);
        assert_eq!(blob.distinct_chunks(), 2);
    }

    #[test]
    fn a_truncated_descriptor_is_refused_rather_than_guessed() {
        let blob = BlobRef {
            len: 10,
            chunks: vec![hash_chunk(b"a")],
        };
        let full = blob.encode();
        for cut in 0..full.len() {
            assert!(
                BlobRef::decode(&full[..cut]).is_err(),
                "accepted {cut} of {} bytes",
                full.len()
            );
        }
    }

    #[test]
    fn a_count_is_eight_bytes_and_nothing_else() {
        assert_eq!(decode_refs(&encode_refs(3)).unwrap(), 3);
        assert!(decode_refs(&[0u8; 4]).is_err());
        assert!(decode_refs(&[0u8; 9]).is_err());
    }

    #[test]
    fn the_bytes_and_the_count_live_at_different_keys() {
        let hash = hash_chunk(b"x");
        assert_ne!(chunk_key(&hash), refs_key(&hash));
    }

    #[test]
    fn the_same_bytes_land_on_the_same_key() {
        let a = hash_chunk(b"hello");
        let b = hash_chunk(b"hello");
        assert_eq!(chunk_key(&a), chunk_key(&b));
        assert_ne!(chunk_key(&a), chunk_key(&hash_chunk(b"hellp")));
    }
}
