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

// ---------------------------------------------------------------------------
// streaming
// ---------------------------------------------------------------------------

/// A source that hands over a few bytes at a time, as a socket would.
struct Trickle<'a> {
    left: &'a [u8],
    at_most: usize,
}

impl std::io::Read for Trickle<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = buf.len().min(self.at_most).min(self.left.len());
        buf[..n].copy_from_slice(&self.left[..n]);
        self.left = &self.left[n..];
        Ok(n)
    }
}

fn pattern(len: usize) -> Vec<u8> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as u8
        })
        .collect()
}

fn round_trip(db: &Db<quanty_core::MemStorage>, data: &[u8]) -> BlobRef {
    let blob = db.write_blob(data).unwrap();
    let mut out = Vec::new();
    assert_eq!(db.read_blob(&blob, &mut out).unwrap(), data.len() as u64);
    assert_eq!(out, data, "the bytes came back different");
    blob
}

#[test]
fn a_blob_smaller_than_a_chunk_is_one_chunk() {
    let db = Db::in_memory().unwrap();
    let data = pattern(100);
    let blob = round_trip(&db, &data);
    assert_eq!(blob.chunks.len(), 1);
    assert_eq!(blob.len, 100);
}

#[test]
fn an_empty_source_is_a_blob_of_nothing() {
    let db = Db::in_memory().unwrap();
    let before = db.stats().unwrap().page_count;
    let blob = db.write_blob(&[][..]).unwrap();
    assert_eq!(blob.len, 0);
    assert!(blob.chunks.is_empty());
    assert_eq!(
        db.stats().unwrap().page_count,
        before,
        "an empty blob burned a commit"
    );

    let mut out = Vec::new();
    assert_eq!(db.read_blob(&blob, &mut out).unwrap(), 0);
    assert!(out.is_empty());
}

#[test]
fn a_blob_crossing_a_commit_boundary_round_trips() {
    // One chunk past CHUNKS_PER_COMMIT, plus a partial one, so the write
    // commits twice and ends mid chunk.
    let db = Db::in_memory().unwrap();
    let chunks = quanty_core::CHUNKS_PER_COMMIT + 1;
    let len = chunks * quanty_core::CHUNK_SIZE + 1234;
    let data = pattern(len);

    let blob = round_trip(&db, &data);
    assert_eq!(blob.chunks.len(), chunks + 1);
    assert_eq!(blob.len, len as u64);
    assert_eq!(blob.distinct_chunks(), chunks + 1, "the pattern repeated");
}

#[test]
fn short_reads_do_not_move_where_chunks_end() {
    // This is the whole reason fill() loops. Proved to catch: returning
    // after one read() call instead of filling the buffer gives a
    // different chunk list for the same bytes.
    let db = Db::in_memory().unwrap();
    let data = pattern(2 * quanty_core::CHUNK_SIZE + 500);

    let whole = db.write_blob(&data[..]).unwrap();
    let trickled = db
        .write_blob(Trickle {
            left: &data,
            at_most: 7,
        })
        .unwrap();

    assert_eq!(
        whole.chunks, trickled.chunks,
        "how the bytes arrived changed where the chunks end"
    );
    assert_eq!(whole.len, trickled.len);
}

#[test]
fn a_blob_naming_a_chunk_that_is_not_there_is_refused() {
    let db = Db::in_memory().unwrap();
    let data = pattern(4096);
    let mut blob = round_trip(&db, &data);
    blob.chunks.push(hash_chunk(b"never stored"));

    let mut out = Vec::new();
    assert!(
        db.read_blob(&blob, &mut out).is_err(),
        "a missing chunk was read as if it were there"
    );
}

#[test]
fn a_length_that_does_not_match_the_chunks_is_refused() {
    let db = Db::in_memory().unwrap();
    let data = pattern(4096);
    let mut blob = round_trip(&db, &data);
    blob.len += 1;

    let mut out = Vec::new();
    assert!(db.read_blob(&blob, &mut out).is_err());
}

// ---------------------------------------------------------------------------
// the checker (ADR-033, ADR-034)
// ---------------------------------------------------------------------------

#[test]
fn a_fresh_store_checks_out_empty() {
    let db = Db::in_memory().unwrap();
    let report = db.check_blobs().unwrap();
    assert!(report.is_sound());
    assert_eq!(report.sound, 0);
    assert_eq!(report.unclaimed, 0);
}

#[test]
fn the_checker_counts_what_is_claimed_and_what_is_not() {
    let db = Db::in_memory().unwrap();
    let claimed = pattern(5000);
    let dropped = pattern(9000);

    let mut tx = db.begin();
    let a = tx.put_chunk(&claimed).unwrap();
    tx.retain_chunk(&a).unwrap();
    let b = tx.put_chunk(&dropped).unwrap();
    tx.commit().unwrap();

    let report = db.check_blobs().unwrap();
    assert!(report.is_sound(), "broken: {:?}", report.broken.len());
    assert_eq!(report.sound, 1);
    assert_eq!(report.bytes, 5000);
    assert_eq!(report.unclaimed, 1, "the chunk nobody claimed went unseen");
    assert_eq!(report.unclaimed_bytes, 9000);
    assert_ne!(a, b);
}

#[test]
fn a_written_blob_is_all_unclaimed_until_something_names_it() {
    // This is ADR-033's open edge, stated as a measurement rather than a
    // worry: between write_blob and whatever stores the descriptor, every
    // chunk is legitimately unclaimed, so a sweep of unclaimed chunks in
    // that window would delete live data.
    let db = Db::in_memory().unwrap();
    let data = pattern(3 * quanty_core::CHUNK_SIZE);
    let blob = db.write_blob(&data[..]).unwrap();

    let mid = db.check_blobs().unwrap();
    assert!(mid.is_sound());
    assert_eq!(mid.sound, 0);
    assert_eq!(mid.unclaimed, 3);

    let mut tx = db.begin();
    for hash in &blob.chunks {
        tx.retain_chunk(hash).unwrap();
    }
    tx.commit().unwrap();

    let after = db.check_blobs().unwrap();
    assert!(after.is_sound());
    assert_eq!(after.sound, 3);
    assert_eq!(after.unclaimed, 0);
}

#[test]
fn releasing_the_last_reference_leaves_the_store_sound() {
    let db = Db::in_memory().unwrap();
    let data = pattern(2 * quanty_core::CHUNK_SIZE);
    let blob = db.write_blob(&data[..]).unwrap();

    let mut tx = db.begin();
    for hash in &blob.chunks {
        tx.retain_chunk(hash).unwrap();
    }
    tx.commit().unwrap();

    let mut tx = db.begin();
    for hash in &blob.chunks {
        assert!(
            tx.release_chunk(hash).unwrap(),
            "a chunk outlived its last reference"
        );
    }
    tx.commit().unwrap();

    let report = db.check_blobs().unwrap();
    assert!(report.is_sound(), "half a chunk was left behind");
    assert_eq!(report.sound, 0);
    assert_eq!(report.unclaimed, 0, "the bytes stayed without their count");
}

#[test]
fn the_checker_finds_a_chunk_that_is_only_half_there() {
    // Nothing in this crate can produce this state, so the test makes it
    // by hand. Without it the broken arm of the checker is never reached
    // and the checker only ever says what we already believed.
    use quanty_core::{encode_key, Value};

    let db = Db::in_memory().unwrap();
    let data = pattern(5000);
    let mut tx = db.begin();
    let hash = tx.put_chunk(&data).unwrap();
    tx.retain_chunk(&hash).unwrap();
    tx.commit().unwrap();
    assert!(db.check_blobs().unwrap().is_sound());

    // take the count away and leave the bytes
    let refs_key = encode_key(&[Value::Text("blobrefs".into()), Value::Bytes(hash.to_vec())]);
    let mut tx = db.begin();
    assert!(tx.catalog_delete(&refs_key).unwrap(), "wrong key");
    tx.commit().unwrap();

    let report = db.check_blobs().unwrap();
    assert!(!report.is_sound(), "a half chunk passed as sound");
    assert_eq!(report.broken, vec![hash]);
    assert_eq!(report.sound, 0);

    // and the other way round: bytes gone, count left
    let bytes_key = encode_key(&[Value::Text("blob".into()), Value::Bytes(hash.to_vec())]);
    let mut tx = db.begin();
    tx.catalog_put(&refs_key, &1u64.to_le_bytes()).unwrap();
    assert!(tx.catalog_delete(&bytes_key).unwrap());
    tx.commit().unwrap();

    let report = db.check_blobs().unwrap();
    assert!(!report.is_sound(), "a count without bytes passed as sound");
    assert_eq!(report.broken, vec![hash]);
}
