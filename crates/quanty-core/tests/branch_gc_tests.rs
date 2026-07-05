//! Branches, fast-forward merges and garbage collection.
//!
//! The model and crash tests hammer these paths with random workloads;
//! this file pins the intended semantics down with named cases.

use quanty_core::{Db, MemStorage, PagerOptions};

fn opts() -> PagerOptions {
    PagerOptions {
        page_size: 512,
        ..PagerOptions::default()
    }
}

fn put(db: &Db<MemStorage>, key: &[u8], value: &[u8]) -> u64 {
    let mut tx = db.begin();
    tx.put(key, value).unwrap();
    tx.commit().unwrap()
}

fn get(db: &Db<MemStorage>, key: &[u8]) -> Option<Vec<u8>> {
    db.snapshot().get(key).unwrap()
}

#[test]
fn branches_write_divergent_data_and_read_it_back_independently() {
    let db = Db::create(MemStorage::new(), opts()).unwrap();
    put(&db, b"shared", b"base");
    let fork_point = db.head_commit();

    db.create_branch("experiment", None).unwrap();
    assert_eq!(db.current_branch(), "main");

    // diverge: one commit on each side
    put(&db, b"main-only", b"m");
    db.switch_branch("experiment").unwrap();
    put(&db, b"exp-only", b"e");

    // the experiment sees the fork point and its own write, not main's
    assert_eq!(get(&db, b"shared").as_deref(), Some(&b"base"[..]));
    assert_eq!(get(&db, b"exp-only").as_deref(), Some(&b"e"[..]));
    assert_eq!(get(&db, b"main-only"), None);

    db.switch_branch("main").unwrap();
    assert_eq!(get(&db, b"main-only").as_deref(), Some(&b"m"[..]));
    assert_eq!(get(&db, b"exp-only"), None);

    // both heads moved past the fork point, in different directions
    let branches = db.branches().unwrap();
    assert_eq!(branches.len(), 2);
    for (_, r) in &branches {
        assert!(r.head_id > fork_point);
    }

    // per-branch history: each log walks through the fork point
    let ids: Vec<u64> = db.log().unwrap().iter().map(|c| c.commit_id).collect();
    assert!(ids.contains(&fork_point));
}

#[test]
fn a_branch_can_start_at_an_older_commit() {
    let db = Db::create(MemStorage::new(), opts()).unwrap();
    let c1 = put(&db, b"k", b"v1");
    put(&db, b"k", b"v2");

    db.create_branch("from-past", Some(c1)).unwrap();
    db.switch_branch("from-past").unwrap();
    assert_eq!(get(&db, b"k").as_deref(), Some(&b"v1"[..]));

    // writing here forks history instead of touching main
    put(&db, b"k", b"v3");
    db.switch_branch("main").unwrap();
    assert_eq!(get(&db, b"k").as_deref(), Some(&b"v2"[..]));
}

#[test]
fn branch_name_and_existence_errors() {
    let db = Db::create(MemStorage::new(), opts()).unwrap();
    put(&db, b"k", b"v");

    assert!(db.create_branch("has space", None).is_err());
    assert!(db.create_branch("-lead", None).is_err());
    assert!(
        db.create_branch("main", None).is_err(),
        "main exists implicitly"
    );
    assert!(db.switch_branch("nope").is_err());
    assert!(db.drop_branch("nope").is_err());
    assert!(db.create_branch("b", Some(999)).is_err(), "no such commit");

    db.create_branch("b", None).unwrap();
    assert!(db.create_branch("b", None).is_err(), "duplicate");
    assert!(
        db.drop_branch("main").is_err(),
        "cannot drop the current branch"
    );
    db.drop_branch("b").unwrap();
    assert!(db.switch_branch("b").is_err(), "gone after drop");
}

#[test]
fn fast_forward_merge_moves_the_pointer_and_divergence_is_rejected() {
    let db = Db::create(MemStorage::new(), opts()).unwrap();
    put(&db, b"k", b"base");

    db.create_branch("feature", None).unwrap();
    db.switch_branch("feature").unwrap();
    put(&db, b"feature", b"work");
    let feature_head = db.head_commit();

    // main has not moved since the fork, so this is a fast-forward
    db.switch_branch("main").unwrap();
    let merged = db.merge_ff("feature").unwrap();
    assert_eq!(merged, feature_head);
    assert_eq!(db.head_commit(), feature_head);
    assert_eq!(get(&db, b"feature").as_deref(), Some(&b"work"[..]));

    // merging again is a no-op, not an error
    assert_eq!(db.merge_ff("feature").unwrap(), feature_head);

    // now diverge and watch the merge refuse politely
    db.create_branch("other", None).unwrap();
    put(&db, b"main2", b"x");
    db.switch_branch("other").unwrap();
    put(&db, b"other2", b"y");
    db.switch_branch("main").unwrap();
    let err = db.merge_ff("other").unwrap_err().to_string();
    assert!(err.contains("diverged"), "got: {err}");

    assert!(db.merge_ff("main").is_err(), "self merge");
    assert!(db.merge_ff("ghost").is_err(), "unknown branch");
}

#[test]
fn gc_prunes_old_commits_but_never_touches_retained_ones() {
    let mut db = Db::create(MemStorage::new(), opts()).unwrap();
    let mut commits = Vec::new();
    for i in 0..10u32 {
        commits.push(put(
            &db,
            format!("k{i}").as_bytes(),
            format!("v{i}").as_bytes(),
        ));
    }

    let report = db.gc(3).unwrap();
    assert_eq!(report.pruned_commits, 7);
    assert!(report.freed_pages > 0);

    // the three newest commits answer exactly as before
    for (i, &id) in commits.iter().enumerate().skip(7) {
        let snap = db.snapshot_at(id).unwrap();
        for j in 0..=i {
            let key = format!("k{j}");
            assert_eq!(
                snap.get(key.as_bytes()).unwrap().as_deref(),
                Some(format!("v{j}").as_bytes()),
            );
        }
    }

    // everything older is gone, with a helpful error
    for &id in &commits[..7] {
        match db.snapshot_at(id) {
            Ok(_) => panic!("commit {id} should have been pruned"),
            Err(e) => assert!(e.to_string().contains("garbage collected"), "got: {e}"),
        }
    }

    // gc is idempotent when nothing new fell out of retention
    let again = db.gc(3).unwrap();
    assert_eq!(again.pruned_commits, 0);

    assert!(db.gc(0).is_err(), "retain zero makes no sense");
}

#[test]
fn the_file_stops_growing_under_churn_with_gc() {
    let mut db = Db::create(MemStorage::new(), opts()).unwrap();

    // warm up: same keys rewritten over and over, gc keeps heads only
    let mut after_warmup = 0;
    for round in 0..60u32 {
        let mut tx = db.begin();
        for k in 0..20u32 {
            tx.put(
                format!("key{k}").as_bytes(),
                format!("r{round}v{k}").as_bytes(),
            )
            .unwrap();
        }
        tx.commit().unwrap();
        db.gc(1).unwrap();
        if round == 19 {
            after_warmup = db.stats().unwrap().page_count;
        }
    }
    let end = db.stats().unwrap();

    // without reuse this workload allocates fresh pages every round; with
    // the free list the file plateaus after warmup
    assert_eq!(
        end.page_count, after_warmup,
        "file kept growing: {after_warmup} -> {} pages",
        end.page_count
    );
    assert!(end.free_pages > 0, "gc left nothing to reuse");

    // and the data is still exactly the last round
    for k in 0..20u32 {
        assert_eq!(
            get(&db, format!("key{k}").as_bytes()).as_deref(),
            Some(format!("r59v{k}").as_bytes()),
        );
    }
}

#[test]
fn gc_keeps_every_branch_head_alive() {
    let mut db = Db::create(MemStorage::new(), opts()).unwrap();
    put(&db, b"base", b"b");
    db.create_branch("a", None).unwrap();
    db.create_branch("b", None).unwrap();

    for name in ["a", "b", "main"] {
        db.switch_branch(name).unwrap();
        for i in 0..5u32 {
            put(&db, format!("{name}{i}").as_bytes(), b"x");
        }
    }

    db.gc(1).unwrap();

    for name in ["a", "b", "main"] {
        db.switch_branch(name).unwrap();
        assert_eq!(
            get(&db, b"base").as_deref(),
            Some(&b"b"[..]),
            "branch {name}"
        );
        assert_eq!(
            get(&db, format!("{name}4").as_bytes()).as_deref(),
            Some(&b"x"[..])
        );
    }
}

#[test]
fn snapshot_at_time_resolves_along_the_current_branch() {
    let db = Db::create(MemStorage::new(), opts()).unwrap();
    put(&db, b"k", b"v1");
    put(&db, b"k", b"v2");

    let log = db.log().unwrap();
    let newest_ts = log[0].unix_ts_ms;
    let oldest_ts = log.last().unwrap().unix_ts_ms;

    // before all history: the empty database
    let snap = db.snapshot_at_time(0).unwrap();
    assert_eq!(snap.commit_id(), 0);
    assert_eq!(snap.get(b"k").unwrap(), None);

    // at or after the newest commit: the head
    let snap = db.snapshot_at_time(newest_ts).unwrap();
    assert_eq!(snap.get(b"k").unwrap().as_deref(), Some(&b"v2"[..]));

    // exactly at the first commit's time: at least the first commit,
    // possibly the second when both landed in the same millisecond
    let snap = db.snapshot_at_time(oldest_ts).unwrap();
    assert!(snap.get(b"k").unwrap().is_some());
}

#[test]
fn switching_persists_across_reopen() {
    let dir = std::env::temp_dir().join(format!("quanty-branch-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("reopen.qdb");
    let _ = std::fs::remove_file(&path);

    {
        let db = Db::create_file(&path).unwrap();
        let mut tx = db.begin();
        tx.put(b"k", b"main").unwrap();
        tx.commit().unwrap();
        db.create_branch("side", None).unwrap();
        db.switch_branch("side").unwrap();
        let mut tx = db.begin();
        tx.put(b"k", b"side").unwrap();
        tx.commit().unwrap();
    }

    let db = Db::open_file(&path).unwrap();
    assert_eq!(db.current_branch(), "side");
    assert_eq!(
        db.snapshot().get(b"k").unwrap().as_deref(),
        Some(&b"side"[..])
    );
    db.switch_branch("main").unwrap();
    assert_eq!(
        db.snapshot().get(b"k").unwrap().as_deref(),
        Some(&b"main"[..])
    );

    std::fs::remove_file(&path).ok();
}
