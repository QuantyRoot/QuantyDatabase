//! Targeted tests of the Db API: transactions, snapshots, overflow values,
//! scan bounds, commit log. The randomized heavy lifting lives in
//! btree_model.rs; these pin down the specific behaviors one by one.

mod common;

use common::TestDir;
use quantydb_core::{encode_key, Db, Error, FileStorage, MemStorage, PagerOptions, Value};

fn small_db() -> Db<MemStorage> {
    // 512 byte pages keep trees deep and splits frequent
    Db::create(
        MemStorage::new(),
        PagerOptions {
            page_size: 512,
            cache_pages: 32,
        },
    )
    .unwrap()
}

#[test]
fn empty_database_reads_empty() {
    let db = small_db();
    let snap = db.snapshot();
    assert_eq!(snap.get(b"nope").unwrap(), None);
    assert_eq!(snap.scan(None, None).unwrap().count(), 0);
    assert_eq!(db.head_commit(), 0);
}

#[test]
fn put_get_delete_within_and_across_transactions() {
    let db = small_db();
    let mut tx = db.begin();
    tx.put(b"k1", b"v1").unwrap();
    tx.put(b"k2", b"v2").unwrap();

    // own writes visible before commit
    assert_eq!(tx.get(b"k1").unwrap().as_deref(), Some(&b"v1"[..]));
    // but not to snapshots
    assert_eq!(db.snapshot().get(b"k1").unwrap(), None);

    tx.commit().unwrap();
    assert_eq!(
        db.snapshot().get(b"k1").unwrap().as_deref(),
        Some(&b"v1"[..])
    );

    let mut tx = db.begin();
    assert!(tx.delete(b"k1").unwrap());
    assert!(!tx.delete(b"k1").unwrap());
    assert!(!tx.delete(b"never-existed").unwrap());
    tx.commit().unwrap();
    assert_eq!(db.snapshot().get(b"k1").unwrap(), None);
    assert_eq!(
        db.snapshot().get(b"k2").unwrap().as_deref(),
        Some(&b"v2"[..])
    );
}

#[test]
fn overwriting_a_key_replaces_the_value() {
    let db = small_db();
    let mut tx = db.begin();
    tx.put(b"k", b"first").unwrap();
    tx.put(b"k", b"second").unwrap();
    tx.commit().unwrap();
    assert_eq!(
        db.snapshot().get(b"k").unwrap().as_deref(),
        Some(&b"second"[..])
    );
}

#[test]
fn large_values_roundtrip_through_overflow_chains() {
    let dir = common::TestDir::new();
    let path = dir.path().join("ovf.qdb");

    // spans many overflow pages, content position-dependent
    let big: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    {
        let db = Db::create(
            FileStorage::create(&path).unwrap(),
            PagerOptions {
                page_size: 512,
                cache_pages: 32,
            },
        )
        .unwrap();
        let mut tx = db.begin();
        tx.put(b"big", &big).unwrap();
        tx.put(b"small", b"tiny").unwrap();
        // readable through the tx's own scan too
        let got = tx.get(b"big").unwrap().unwrap();
        assert_eq!(got, big);
        tx.commit().unwrap();
    }
    // and after a reopen from disk
    let db = Db::open_file(&path).unwrap();
    assert_eq!(db.snapshot().get(b"big").unwrap().unwrap(), big);
    assert_eq!(
        db.snapshot().get(b"small").unwrap().as_deref(),
        Some(&b"tiny"[..])
    );
}

#[test]
fn scan_bounds_are_start_inclusive_end_exclusive() {
    let db = small_db();
    let mut tx = db.begin();
    for k in ["a", "b", "c", "d", "e"] {
        tx.put(k.as_bytes(), k.as_bytes()).unwrap();
    }
    tx.commit().unwrap();

    let snap = db.snapshot();
    let keys = |start: Option<&[u8]>, end: Option<&[u8]>| -> Vec<String> {
        snap.scan(start, end)
            .unwrap()
            .map(|r| String::from_utf8(r.unwrap().0).unwrap())
            .collect()
    };
    assert_eq!(keys(None, None), ["a", "b", "c", "d", "e"]);
    assert_eq!(keys(Some(b"b"), Some(b"d")), ["b", "c"]);
    assert_eq!(keys(Some(b"bb"), None), ["c", "d", "e"]);
    assert_eq!(keys(None, Some(b"a")), Vec::<String>::new());
    assert_eq!(keys(Some(b"x"), None), Vec::<String>::new());
}

#[test]
fn snapshots_of_old_commits_read_the_past() {
    let db = small_db();

    let mut tx = db.begin();
    tx.put(b"who", b"v1").unwrap();
    let c1 = tx.commit().unwrap();

    let mut tx = db.begin();
    tx.put(b"who", b"v2").unwrap();
    tx.put(b"extra", b"x").unwrap();
    let c2 = tx.commit().unwrap();

    let mut tx = db.begin();
    tx.delete(b"who").unwrap();
    let c3 = tx.commit().unwrap();

    assert_eq!(
        db.snapshot_at(c1).unwrap().get(b"who").unwrap().as_deref(),
        Some(&b"v1"[..])
    );
    assert_eq!(db.snapshot_at(c1).unwrap().get(b"extra").unwrap(), None);
    assert_eq!(
        db.snapshot_at(c2).unwrap().get(b"who").unwrap().as_deref(),
        Some(&b"v2"[..])
    );
    assert_eq!(db.snapshot_at(c3).unwrap().get(b"who").unwrap(), None);
    assert_eq!(
        db.snapshot_at(0).unwrap().scan(None, None).unwrap().count(),
        0
    );
    assert!(db.snapshot_at(99).is_err());
}

#[test]
fn the_log_walks_the_whole_history() {
    let db = small_db();
    for i in 0..5u8 {
        let mut tx = db.begin();
        tx.put(&[i], &[i]).unwrap();
        tx.commit().unwrap();
    }
    let log = db.log().unwrap();
    let ids: Vec<u64> = log.iter().map(|r| r.commit_id).collect();
    assert_eq!(ids, [5, 4, 3, 2, 1]);
    let parents: Vec<u64> = log.iter().map(|r| r.parent_id).collect();
    assert_eq!(parents, [4, 3, 2, 1, 0]);
}

#[test]
fn snapshots_survive_a_reopen() {
    let dir = common::TestDir::new();
    let path = dir.path().join("history.qdb");
    let (c1, c2);
    {
        let db = Db::create_file(&path).unwrap();
        let mut tx = db.begin();
        tx.put(b"k", b"old").unwrap();
        c1 = tx.commit().unwrap();
        let mut tx = db.begin();
        tx.put(b"k", b"new").unwrap();
        c2 = tx.commit().unwrap();
    }
    let db = Db::open_file(&path).unwrap();
    assert_eq!(
        db.snapshot_at(c1).unwrap().get(b"k").unwrap().as_deref(),
        Some(&b"old"[..])
    );
    assert_eq!(
        db.snapshot_at(c2).unwrap().get(b"k").unwrap().as_deref(),
        Some(&b"new"[..])
    );
}

#[test]
fn dropped_transaction_changes_nothing() {
    let db = small_db();
    let mut tx = db.begin();
    tx.put(b"ghost", b"boo").unwrap();
    drop(tx);
    assert_eq!(db.snapshot().get(b"ghost").unwrap(), None);
    assert_eq!(db.head_commit(), 0);
}

#[test]
fn oversized_and_empty_keys_are_rejected() {
    let db = small_db();
    let mut tx = db.begin();
    // max key for 512 byte pages is 64 bytes
    assert!(tx.put(&[7u8; 64], b"ok").is_ok());
    match tx.put(&[7u8; 65], b"nope") {
        Err(Error::InvalidArgument(_)) => {}
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
    match tx.put(b"", b"nope") {
        Err(Error::InvalidArgument(_)) => {}
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
    // reads and deletes of impossible keys just miss
    assert_eq!(tx.get(&[7u8; 65]).unwrap(), None);
    assert!(!tx.delete(&[7u8; 65]).unwrap());
}

#[test]
fn encoded_tuples_work_as_keys_and_sort_correctly() {
    let db = small_db();
    let mut tx = db.begin();
    for i in [-3i64, 0, 7, 100, -50] {
        let key = quantydb_core::encode_key(&[Value::Text("user".into()), Value::Int(i)]);
        tx.put(&key, &i.to_le_bytes()).unwrap();
    }
    tx.commit().unwrap();

    let snap = db.snapshot();
    let ints: Vec<i64> = snap
        .scan(None, None)
        .unwrap()
        .map(|r| i64::from_le_bytes(r.unwrap().1.try_into().unwrap()))
        .collect();
    assert_eq!(ints, [-50, -3, 0, 7, 100]);
}

#[test]
fn a_second_writer_is_turned_away_at_the_door() {
    // The lock is the first defence and the cheap one: it is taken once
    // at open rather than checked once per commit (ADR-035).
    let dir = TestDir::new();
    let path = dir.path().join("two-writers.qdb");

    let first = Db::create_file(&path).unwrap();
    match Db::open_file(&path) {
        Err(Error::AlreadyOpen) => {}
        Err(other) => panic!("wrong refusal: {other:?}"),
        Ok(_) => panic!("a second writer got in"),
    }

    // and it is a lock, not a tombstone: once the first lets go, the
    // next one may have it
    drop(first);
    let second = Db::open_file(&path).expect("the lock outlived its holder");
    drop(second);
}

#[test]
fn the_writer_lock_does_not_make_the_file_unreadable() {
    // The property Windows broke, and the reason the lock is not one call
    // to the standard library. A byte range lock is mandatory there, so
    // locking the file a database lives in stopped anybody reading that
    // database at all: a second handle answered `not a quantydb database`,
    // because its reads came back refused. The lock is a byte far past
    // the data now, which conflicts with a second writer and nothing
    // else.
    let dir = TestDir::new();
    let path = dir.path().join("readable.qdb");

    let writer = Db::create_file(&path).unwrap();
    let mut tx = writer.begin();
    tx.put(&encode_key(&[Value::Int(1)]), b"written").unwrap();
    tx.commit().unwrap();

    // Plain file reads, not through the database at all: what a lock that
    // covered the data would refuse.
    let mut raw = std::fs::File::open(&path).expect("the file cannot be opened");
    let mut head = [0u8; 64];
    std::io::Read::read_exact(&mut raw, &mut head).expect("the file cannot be read");
    assert!(head.iter().any(|&b| b != 0), "the header read back empty");

    // and the database agrees while the writer still holds it
    let reader = Db::open_file_unlocked(&path).expect("a reader was refused");
    assert_eq!(
        reader
            .snapshot()
            .get(&encode_key(&[Value::Int(1)]))
            .unwrap()
            .as_deref(),
        Some(&b"written"[..])
    );
}

#[test]
fn a_reader_does_not_need_the_writer_to_stand_aside() {
    // Many readers alongside one writer is the model, so a reader takes
    // no lock at all. A shared one would conflict with the writer's.
    let dir = TestDir::new();
    let path = dir.path().join("readers.qdb");

    let writer = Db::create_file(&path).unwrap();
    let mut tx = writer.begin();
    tx.put(&encode_key(&[Value::Int(1)]), b"written").unwrap();
    tx.commit().unwrap();

    let reader = Db::open_file_unlocked(&path).expect("a reader was refused");
    let other = Db::open_file_unlocked(&path).expect("a second reader was refused");
    for r in [&reader, &other] {
        assert_eq!(
            r.snapshot()
                .get(&encode_key(&[Value::Int(1)]))
                .unwrap()
                .as_deref(),
            Some(&b"written"[..])
        );
    }

    // and the writer still has its lock while they read
    assert!(matches!(Db::open_file(&path), Err(Error::AlreadyOpen)));
}

#[test]
fn the_commit_guard_still_catches_what_the_lock_cannot() {
    // Advisory locking is not everywhere; some network filesystems treat
    // it as a suggestion. open_file_unlocked is how that is reached on
    // purpose, and what this pins is the behaviour when the lock did not
    // help: a refusal, never a lost commit.
    //
    // Proved to catch: removing the txid_in_slot check in Pager::commit
    // makes the second commit succeed and `first` disappear.
    let dir = TestDir::new();
    let path = dir.path().join("no-lock.qdb");

    let a = Db::create_file(&path).unwrap();
    let b = Db::open_file_unlocked(&path).expect("unlocked open");

    let mut ta = a.begin();
    ta.put(&encode_key(&[Value::Int(1)]), b"first").unwrap();
    let first_txid = ta.commit().unwrap();

    let mut tb = b.begin();
    tb.put(&encode_key(&[Value::Int(2)]), b"second").unwrap();
    match tb.commit() {
        Err(Error::WriterRaced { expected, found }) => {
            assert_eq!(expected, first_txid - 1, "raced against the wrong commit");
            assert_eq!(found, first_txid);
        }
        other => panic!("the second writer was allowed through: {other:?}"),
    }

    drop(a);
    let fresh = Db::open_file(&path).unwrap();
    let snap = fresh.snapshot();
    assert_eq!(
        snap.get(&encode_key(&[Value::Int(1)])).unwrap().as_deref(),
        Some(&b"first"[..]),
        "an acknowledged commit was overwritten"
    );
    assert!(snap.get(&encode_key(&[Value::Int(2)])).unwrap().is_none());
}

#[test]
fn a_handle_that_reopens_after_the_race_works_again() {
    // The refusal is about this transaction, not about the handle: a
    // caller that reopens sees the newer state and can commit on top.
    let dir = TestDir::new();
    let path = dir.path().join("recover.qdb");

    let a = Db::create_file(&path).unwrap();
    let b = Db::open_file_unlocked(&path).unwrap();
    let mut ta = a.begin();
    ta.put(&encode_key(&[Value::Int(1)]), b"first").unwrap();
    ta.commit().unwrap();

    let mut tb = b.begin();
    tb.put(&encode_key(&[Value::Int(2)]), b"second").unwrap();
    assert!(tb.commit().is_err());

    let b = Db::open_file_unlocked(&path).unwrap();
    let mut tb = b.begin();
    tb.put(&encode_key(&[Value::Int(2)]), b"second").unwrap();
    tb.commit().expect("a reopened handle should commit");

    let snap = b.snapshot();
    assert!(snap.get(&encode_key(&[Value::Int(1)])).unwrap().is_some());
    assert!(snap.get(&encode_key(&[Value::Int(2)])).unwrap().is_some());
}

/// A statement that fails is undone by putting its pages back. Whatever the
/// batch remembers about those pages has to go back with them, or the next
/// statement starts from a page that no longer exists and writes it over
/// the one that does. This writes into the same leaves before, during and
/// after a rolled back statement, which is the only order that can tell.
#[test]
fn a_rolled_back_statement_leaves_nothing_for_the_next_one_to_build_on() {
    use std::collections::BTreeMap;

    let db = small_db();
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut tx = db.begin();
    let mut seed: u64 = 0x5eed;
    let mut next = || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for round in 0..300u64 {
        let roots = tx.roots();
        tx.savepoint();
        let mut pending = Vec::new();
        for _ in 0..(next() % 12 + 1) {
            let key = format!("k{:04}", next() % 400).into_bytes();
            let value = format!("v{round}-{}", next() % 1000).into_bytes();
            tx.put(&key, &value).unwrap();
            pending.push((key, value));
        }
        if next() % 3 == 0 {
            tx.rollback_savepoint(roots);
        } else {
            tx.release_savepoint();
            model.extend(pending);
        }
        // Written straight after, into the same neighbourhood, so a page
        // remembered from the undone statement would be built upon here.
        let key = format!("k{:04}", next() % 400).into_bytes();
        let value = format!("after{round}").into_bytes();
        tx.put(&key, &value).unwrap();
        model.insert(key, value);

        let seen: Vec<(Vec<u8>, Vec<u8>)> =
            tx.scan(None, None).unwrap().map(|r| r.unwrap()).collect();
        let expected: Vec<(Vec<u8>, Vec<u8>)> =
            model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        assert_eq!(seen, expected, "round {round}");
    }
    tx.commit().unwrap();
    let snap = db.snapshot();
    let seen: Vec<(Vec<u8>, Vec<u8>)> =
        snap.scan(None, None).unwrap().map(|r| r.unwrap()).collect();
    let expected: Vec<(Vec<u8>, Vec<u8>)> =
        model.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    assert_eq!(seen, expected, "after commit");
}

/// A leaf whose checksum is right and whose keys are out of order is the
/// one corruption a checksum cannot see: a crafted file, or a bug that
/// wrote a whole page wrongly. The pager checks a node's structure when it
/// loads the page, and readers trust that, so this proves the check is
/// there for both ways of reading: a point lookup and a scan.
#[test]
fn a_leaf_out_of_order_with_a_valid_checksum_is_an_error_on_every_read() {
    use quantydb_core::page::{lsn, seal};

    let dir = TestDir::new();
    let path = dir.path().join("crafted.qdb");
    {
        let db = Db::create(
            FileStorage::create(&path).unwrap(),
            PagerOptions {
                page_size: 512,
                cache_pages: 32,
            },
        )
        .unwrap();
        let mut tx = db.begin();
        for i in 0..40u32 {
            let key = format!("k{i:03}");
            tx.put(key.as_bytes(), b"v").unwrap();
        }
        tx.commit().unwrap();
    }

    // Find a leaf with two neighbouring keys of the same length and swap
    // them, then seal the page again so its checksum is valid.
    let mut file = std::fs::read(&path).unwrap();
    let header = quantydb_core::PAGE_HEADER_LEN;
    let mut crafted = false;
    for page in file.chunks_mut(512).skip(2) {
        if page[4] != quantydb_core::PageType::Leaf as u8 {
            continue;
        }
        let count = u16::from_le_bytes([page[6], page[7]]);
        if count < 2 {
            continue;
        }
        let k1 = u16::from_le_bytes([page[header], page[header + 1]]) as usize;
        let v1 = u32::from_le_bytes(page[header + 3 + k1..header + 7 + k1].try_into().unwrap());
        let second = header + 3 + k1 + 4 + v1 as usize;
        let k2 = u16::from_le_bytes([page[second], page[second + 1]]) as usize;
        if k1 != k2 {
            continue;
        }
        let first: Vec<u8> = page[header + 3..header + 3 + k1].to_vec();
        let other: Vec<u8> = page[second + 3..second + 3 + k2].to_vec();
        page[header + 3..header + 3 + k1].copy_from_slice(&other);
        page[second + 3..second + 3 + k2].copy_from_slice(&first);
        let txid = lsn(page);
        seal(page, txid);
        crafted = true;
        break;
    }
    assert!(crafted, "no leaf to craft; the test proved nothing");
    std::fs::write(&path, &file).unwrap();

    let db = Db::open(
        FileStorage::open(&path).unwrap(),
        PagerOptions {
            page_size: 512,
            cache_pages: 32,
        },
    )
    .unwrap();
    let snap = db.snapshot();
    let mut errors = 0;
    for i in 0..40u32 {
        if snap.get(format!("k{i:03}").as_bytes()).is_err() {
            errors += 1;
        }
    }
    assert!(errors > 0, "every point lookup succeeded on a crafted leaf");
    // A scan descends to its first leaf when it is made, so the error may
    // come from making it or from a row; either is a refusal.
    let scan_refused = match snap.scan(None, None) {
        Err(_) => true,
        Ok(mut rows) => rows.any(|r| r.is_err()),
    };
    assert!(
        scan_refused,
        "a full scan read a crafted leaf without an error"
    );
}
