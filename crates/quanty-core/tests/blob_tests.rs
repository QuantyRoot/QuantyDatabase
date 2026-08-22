//! The blob chunk store against a real database (ADR-033).

mod common;

use common::TestDir;
use quanty_core::{hash_chunk, BlobRef, Db};

fn chunk(byte: u8, len: usize) -> Vec<u8> {
    vec![byte; len]
}

#[test]
fn the_same_bytes_stored_twice_take_one_entry() {
    let db = Db::in_memory().unwrap();
    let payload = chunk(7, 4096);

    let mut tx = db.begin();
    let first = tx.put_chunk(&payload).unwrap();
    let second = tx.put_chunk(&payload).unwrap();

    assert_eq!(first, second, "the same bytes got different addresses");
    assert_eq!(tx.get_chunk(&first).unwrap().unwrap(), payload);
    // storing it twice is one entry, so one retain leaves one reference
    tx.retain_chunk(&first).unwrap();
    assert_eq!(tx.chunk_refs(&second).unwrap(), Some(1));
}

#[test]
fn different_bytes_get_different_addresses() {
    let db = Db::in_memory().unwrap();
    let mut tx = db.begin();
    let a = tx.put_chunk(&chunk(1, 100)).unwrap();
    let b = tx.put_chunk(&chunk(2, 100)).unwrap();
    assert_ne!(a, b);
    assert_eq!(tx.get_chunk(&a).unwrap().unwrap(), chunk(1, 100));
    assert_eq!(tx.get_chunk(&b).unwrap().unwrap(), chunk(2, 100));
}

#[test]
fn a_stored_chunk_starts_at_zero_and_counts_up_and_down() {
    let db = Db::in_memory().unwrap();
    let mut tx = db.begin();
    let hash = tx.put_chunk(b"payload").unwrap();

    assert_eq!(tx.chunk_refs(&hash).unwrap(), Some(0));
    tx.retain_chunk(&hash).unwrap();
    tx.retain_chunk(&hash).unwrap();
    assert_eq!(tx.chunk_refs(&hash).unwrap(), Some(2));

    assert!(!tx.release_chunk(&hash).unwrap(), "gone one release early");
    assert_eq!(tx.chunk_refs(&hash).unwrap(), Some(1));

    assert!(tx.release_chunk(&hash).unwrap(), "the last release kept it");
    assert_eq!(tx.chunk_refs(&hash).unwrap(), None);
    assert_eq!(tx.get_chunk(&hash).unwrap(), None);
}

#[test]
fn releasing_more_often_than_retaining_is_refused_not_wrapped() {
    // A count that went negative means a descriptor was dropped twice,
    // and carrying on would delete bytes something still points at.
    let db = Db::in_memory().unwrap();
    let mut tx = db.begin();
    let hash = tx.put_chunk(b"payload").unwrap();
    tx.retain_chunk(&hash).unwrap();
    assert!(tx.release_chunk(&hash).unwrap());

    // now it is gone, so the next release has nothing to work on
    assert!(tx.release_chunk(&hash).is_err());

    let never = hash_chunk(b"never stored");
    assert!(tx.release_chunk(&never).is_err());
    assert!(tx.retain_chunk(&never).is_err());
}

#[test]
fn a_chunk_survives_a_commit_and_a_reopen() {
    let dir = TestDir::new();
    let path = dir.path().join("blobs.qdb");
    let payload = chunk(9, 70_000);
    let hash;

    {
        let db = Db::create_file(&path).unwrap();
        let mut tx = db.begin();
        hash = tx.put_chunk(&payload).unwrap();
        tx.retain_chunk(&hash).unwrap();
        tx.commit().unwrap();
    }

    let db = Db::open_file(&path).unwrap();
    let tx = db.begin();
    assert_eq!(tx.chunk_refs(&hash).unwrap(), Some(1));
    assert_eq!(
        tx.get_chunk(&hash).unwrap().unwrap(),
        payload,
        "a chunk larger than a page did not come back whole"
    );
}

#[test]
fn dedup_across_commits_costs_no_space() {
    let dir = TestDir::new();
    let path = dir.path().join("dedup.qdb");
    let payload = chunk(3, 200_000);

    let db = Db::create_file(&path).unwrap();
    let mut tx = db.begin();
    let hash = tx.put_chunk(&payload).unwrap();
    tx.retain_chunk(&hash).unwrap();
    tx.commit().unwrap();
    let after_first = db.stats().unwrap().page_count;

    // the same file arriving a second time, in its own commit
    let mut tx = db.begin();
    let again = tx.put_chunk(&payload).unwrap();
    tx.retain_chunk(&again).unwrap();
    tx.commit().unwrap();
    let after_second = db.stats().unwrap().page_count;

    assert_eq!(again, hash);
    // The payload is about 48 pages. Measured cost of the second copy is
    // 2: the count and the btree path above it. Proved to catch: storing
    // the count in the same value as the bytes makes this 52, because a
    // btree replaces a value whole and copies its overflow chain.
    let cost = after_second - after_first;
    assert!(cost <= 4, "the second copy cost {cost} pages, not a dedup");
    assert_eq!(tx_refs(&db, &hash), 2);
}

fn tx_refs(db: &Db<quanty_core::FileStorage>, hash: &quanty_core::ChunkHash) -> u64 {
    db.begin().chunk_refs(hash).unwrap().expect("the chunk")
}

#[test]
fn a_descriptor_names_its_chunks_in_order() {
    let db = Db::in_memory().unwrap();
    let mut tx = db.begin();

    let a = tx.put_chunk(b"first").unwrap();
    let b = tx.put_chunk(b"second").unwrap();
    // a blob whose middle repeats its first chunk
    let blob = BlobRef {
        len: 16,
        chunks: vec![a, b, a],
    };

    for hash in &blob.chunks {
        tx.retain_chunk(hash).unwrap();
    }
    assert_eq!(tx.chunk_refs(&a).unwrap(), Some(2));
    assert_eq!(tx.chunk_refs(&b).unwrap(), Some(1));

    let stored = blob.encode();
    assert_eq!(BlobRef::decode(&stored).unwrap(), blob);
    assert_eq!(blob.distinct_chunks(), 2);
}
