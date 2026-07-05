//! Targeted tests of the Db API: transactions, snapshots, overflow values,
//! scan bounds, commit log. The randomized heavy lifting lives in
//! btree_model.rs; these pin down the specific behaviors one by one.

mod common;

use quanty_core::{Db, Error, FileStorage, MemStorage, PagerOptions, Value};

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
        let key = quanty_core::encode_key(&[Value::Text("user".into()), Value::Int(i)]);
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
