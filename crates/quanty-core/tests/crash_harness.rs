//! Crash harness.
//!
//! The acceptance bar for phase 0: kill -9 a process mid-write, thousands
//! of times, and never once reopen into a corrupted or half-committed
//! database.
//!
//! How it works: the parent test re-executes this same test binary as a
//! child (gated by an env var). The child hammers commits into a database
//! file and reports every commit on stdout. The parent waits a random few
//! milliseconds, SIGKILLs the child, reopens the file and verifies:
//!
//! 1. the database opens (a valid meta survived)
//! 2. the recovered txid is at least the last commit the child reported,
//!    so nothing acknowledged was lost
//! 3. every page below the recovered page count has a valid checksum and
//!    exactly the content the workload deterministically wrote for it,
//!    so nothing half-written is reachable
//!
//! Two workloads share this file: the raw pager workload (phase 0) and
//! the transactional B-tree workload (phase 1). The tree parent replays
//! the deterministic workload up to the recovered commit and demands that
//! the reopened tree contains exactly that state: committed data survives,
//! uncommitted data fully disappears.
//!
//! Iteration count comes from QUANTY_CRASH_ITERS (default 100, CI runs
//! 1000). Failure detail lands in the panic message with the temp path so
//! a failing file can be inspected.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use quanty_core::{Db, FileStorage, PageType, Pager, PagerOptions, PAGE_HEADER_LEN};

const ENV_CHILD: &str = "QUANTY_CRASH_CHILD";
const ENV_TREE_CHILD: &str = "QUANTY_CRASH_TREE_CHILD";
const ENV_DB: &str = "QUANTY_CRASH_DB";
const ENV_ITERS: &str = "QUANTY_CRASH_ITERS";
const PAGE_SIZE: u32 = 512;

/// Deterministic page content, shared by the child (writing) and the
/// parent (verifying). Everything but the 16 byte header is derived from
/// the page id alone.
fn expected_body_byte(page_id: u64, index: usize) -> u8 {
    let mut x = page_id
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(index as u64)
        .wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 31;
    x as u8
}

fn fill_page(page_id: u64, buf: &mut [u8]) {
    for (i, b) in buf.iter_mut().enumerate().skip(PAGE_HEADER_LEN) {
        *b = expected_body_byte(page_id, i);
    }
}

/// Child mode: commit forever until killed.
///
/// This is a #[test] only so it lives in the same binary as the parent; it
/// exits immediately unless the parent set the env var.
#[test]
fn crash_child_entry() {
    if std::env::var(ENV_CHILD).is_err() {
        return;
    }
    let path = std::env::var(ENV_DB).expect("child needs QUANTY_CRASH_DB");
    let pager = Pager::create(
        FileStorage::create(&path).expect("child create file"),
        PagerOptions {
            page_size: PAGE_SIZE,
            cache_pages: 64,
        },
    )
    .expect("child create pager");

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut rng = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos() as u64
        | 1;

    writeln!(out, "READY").unwrap();
    out.flush().unwrap();

    loop {
        let mut batch = pager.begin();
        // 1..=4 pages per commit, keeps commit timing varied
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        let pages = 1 + (rng % 4);
        let mut last = 0;
        for _ in 0..pages {
            let id = batch.allocate(PageType::Leaf);
            fill_page(id, batch.page_mut(id).unwrap());
            last = id;
        }
        batch.set_data_root(last);
        let txid = batch.commit().expect("child commit");
        // Only report AFTER commit returned: this line is the durability
        // promise the parent holds us to.
        writeln!(out, "COMMIT {txid}").unwrap();
        out.flush().unwrap();
    }
}

#[test]
fn crash_harness() {
    // don't run the harness inside the child process
    if std::env::var(ENV_CHILD).is_ok() {
        return;
    }
    let iters: u64 = std::env::var(ENV_ITERS)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);

    let exe = std::env::current_exe().expect("current exe");
    let mut rng = 0x1234_5678_9ABC_DEF0u64
        ^ SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
    let mut kills_mid_run = 0u64;
    let started = Instant::now();

    for iter in 0..iters {
        let dir = common::TestDir::new();
        let db_path = dir.path().join("crash.qdb");

        let mut child = Command::new(&exe)
            .arg("crash_child_entry")
            .arg("--exact")
            .arg("--nocapture")
            .env(ENV_CHILD, "1")
            .env(ENV_DB, &db_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn child");

        // collect the child's committed txids off a background thread so a
        // full pipe can never stall the child at an unrepresentative point
        let last_committed = Arc::new(AtomicU64::new(0));
        let reader_last = Arc::clone(&last_committed);
        let stdout = child.stdout.take().expect("child stdout");
        let reader = std::thread::spawn(move || {
            let mut saw_ready = false;
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                // libtest prints "test crash_child_entry ... " without a
                // trailing newline, so the first workload line arrives with
                // that prefix glued on. Match by substring, not equality.
                if line.contains("READY") {
                    saw_ready = true;
                } else if let Some(pos) = line.find("COMMIT ") {
                    if let Ok(txid) = line[pos + 7..].trim().parse::<u64>() {
                        reader_last.store(txid, Ordering::SeqCst);
                    }
                }
            }
            saw_ready
        });

        // wait for READY (first bytes on the pipe), then kill at a random
        // point inside the commit storm
        let ready_deadline = Instant::now() + Duration::from_secs(10);
        while last_committed.load(Ordering::SeqCst) == 0 && Instant::now() < ready_deadline {
            std::thread::sleep(Duration::from_micros(200));
        }
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        std::thread::sleep(Duration::from_micros(rng % 25_000));

        child.kill().expect("SIGKILL child");
        child.wait().expect("reap child");
        let saw_ready = reader.join().expect("reader thread");
        assert!(saw_ready, "iter {iter}: child never became ready");
        if last_committed.load(Ordering::SeqCst) > 0 {
            kills_mid_run += 1;
        }

        verify_database(&db_path, last_committed.load(Ordering::SeqCst), iter);
    }

    println!(
        "crash harness: {iters} kills, {kills_mid_run} with acknowledged commits, {:.1?} total",
        started.elapsed()
    );
}

fn verify_database(path: &std::path::Path, last_acked_txid: u64, iter: u64) {
    // 1. it must open
    let pager = Pager::open(
        FileStorage::open(path)
            .unwrap_or_else(|e| panic!("iter {iter}: open file: {e} ({path:?})")),
        PagerOptions::default(),
    )
    .unwrap_or_else(|e| panic!("iter {iter}: recovery failed: {e} ({path:?})"));

    let meta = pager.committed_meta();

    // 2. nothing acknowledged may be lost
    assert!(
        meta.txid >= last_acked_txid,
        "iter {iter}: durability violation: recovered txid {} < acknowledged {} ({path:?})",
        meta.txid,
        last_acked_txid,
    );

    // 3. every reachable page must be intact and hold exactly the
    //    deterministic content the workload wrote for it
    for id in 2..meta.page_count {
        let page = pager.read_page(id).unwrap_or_else(|e| {
            panic!("iter {iter}: page {id} unreadable after crash: {e} ({path:?})")
        });
        for (i, b) in page.iter().enumerate().skip(PAGE_HEADER_LEN) {
            let want = expected_body_byte(id, i);
            assert!(
                *b == want,
                "iter {iter}: page {id} byte {i}: got {b:#04x}, want {want:#04x} ({path:?})",
            );
        }
    }

    // the root must point at a real page when anything was committed
    if meta.txid > 0 {
        assert!(
            meta.data_root >= 2 && meta.data_root < meta.page_count,
            "iter {iter}: data root {} outside file ({path:?})",
            meta.data_root,
        );
    }
}

// ---------------------------------------------------------------------------
// Transactional B-tree workload
// ---------------------------------------------------------------------------

/// Deterministic workload, commit by commit. Commit c inserts three keys
/// derived from c; every third commit deletes one key of commit c-1. Both
/// the child (executing) and the parent (replaying for verification) use
/// exactly this function.
type Puts = Vec<(Vec<u8>, Vec<u8>)>;

/// The child's progress counter lives inside the tree itself, so replay
/// stays anchored to applied operations rather than to txids. That matters
/// once garbage collection runs in the kill window: a gc commit consumes a
/// txid without carrying workload, and a replay keyed on txids would drift.
const OP_COUNTER_KEY: &[u8] = b"\x00op-counter";

fn tree_ops_for_commit(c: u64) -> (Puts, Option<Vec<u8>>) {
    let key = |c: u64, j: u64| format!("k/{:08}/{j}", c).into_bytes();
    let mut puts = Vec::new();
    for j in 0..3 {
        let mut seed = c
            .wrapping_mul(31)
            .wrapping_add(j)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15);
        // one in ten values is overflow-sized to drag chains into the blast radius
        let len = if seed % 10 == 0 {
            1400
        } else {
            (seed % 40) as usize + 1
        };
        let mut value = Vec::with_capacity(len);
        for _ in 0..len {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            value.push(seed as u8);
        }
        puts.push((key(c, j), value));
    }
    let delete = (c % 3 == 0 && c > 1).then(|| key(c - 1, 0));
    (puts, delete)
}

#[test]
fn crash_tree_child_entry() {
    if std::env::var(ENV_TREE_CHILD).is_err() {
        return;
    }
    let path = std::env::var(ENV_DB).expect("child needs QUANTY_CRASH_DB");
    let db = Db::create(
        FileStorage::create(&path).expect("child create file"),
        PagerOptions {
            page_size: PAGE_SIZE,
            cache_pages: 64,
        },
    )
    .expect("child create db");

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "READY").unwrap();
    out.flush().unwrap();

    let mut db = db;
    loop {
        let mut tx = db.begin();
        let n = match tx.get(OP_COUNTER_KEY).expect("child read counter") {
            Some(v) => u64::from_le_bytes(v.as_slice().try_into().expect("counter width")) + 1,
            None => 1,
        };
        let (puts, delete) = tree_ops_for_commit(n);
        for (k, v) in &puts {
            tx.put(k, v).expect("child put");
        }
        if let Some(k) = &delete {
            tx.delete(k).expect("child delete");
        }
        tx.put(OP_COUNTER_KEY, &n.to_le_bytes())
            .expect("child put counter");
        tx.commit().expect("child commit");
        writeln!(out, "COMMIT {n}").unwrap();
        out.flush().unwrap();

        // garbage collection runs inside the kill window on purpose: a
        // SIGKILL mid-gc must never cost a retained commit
        if n % 6 == 0 {
            db.gc(3).expect("child gc");
        }
    }
}

#[test]
fn crash_harness_tree() {
    if std::env::var(ENV_CHILD).is_ok() || std::env::var(ENV_TREE_CHILD).is_ok() {
        return;
    }
    let iters: u64 = std::env::var(ENV_ITERS)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);

    let exe = std::env::current_exe().expect("current exe");
    let mut rng = 0xFEED_FACE_0BAD_F00Du64
        ^ SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
    let mut kills_mid_run = 0u64;
    let started = Instant::now();

    for iter in 0..iters {
        let dir = common::TestDir::new();
        let db_path = dir.path().join("crash_tree.qdb");

        let mut child = Command::new(&exe)
            .arg("crash_tree_child_entry")
            .arg("--exact")
            .arg("--nocapture")
            .env(ENV_TREE_CHILD, "1")
            .env(ENV_DB, &db_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn child");

        let last_committed = Arc::new(AtomicU64::new(0));
        let reader_last = Arc::clone(&last_committed);
        let stdout = child.stdout.take().expect("child stdout");
        let reader = std::thread::spawn(move || {
            let mut saw_ready = false;
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if line.contains("READY") {
                    saw_ready = true;
                } else if let Some(pos) = line.find("COMMIT ") {
                    if let Ok(txid) = line[pos + 7..].trim().parse::<u64>() {
                        reader_last.store(txid, Ordering::SeqCst);
                    }
                }
            }
            saw_ready
        });

        let ready_deadline = Instant::now() + Duration::from_secs(10);
        while last_committed.load(Ordering::SeqCst) == 0 && Instant::now() < ready_deadline {
            std::thread::sleep(Duration::from_micros(200));
        }
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        std::thread::sleep(Duration::from_micros(rng % 25_000));

        child.kill().expect("SIGKILL child");
        child.wait().expect("reap child");
        let saw_ready = reader.join().expect("reader thread");
        assert!(saw_ready, "iter {iter}: tree child never became ready");
        if last_committed.load(Ordering::SeqCst) > 0 {
            kills_mid_run += 1;
        }

        verify_tree_database(&db_path, last_committed.load(Ordering::SeqCst), iter);
    }

    println!(
        "tree crash harness: {iters} kills, {kills_mid_run} with acknowledged commits, {:.1?} total",
        started.elapsed()
    );
}

fn verify_tree_database(path: &std::path::Path, last_acked: u64, iter: u64) {
    let db = Db::open(
        FileStorage::open(path).unwrap_or_else(|e| panic!("iter {iter}: open: {e} ({path:?})")),
        PagerOptions::default(),
    )
    .unwrap_or_else(|e| panic!("iter {iter}: recovery failed: {e} ({path:?})"));

    let snap = db.snapshot();
    let recovered = match snap
        .get(OP_COUNTER_KEY)
        .unwrap_or_else(|e| panic!("iter {iter}: counter read: {e} ({path:?})"))
    {
        Some(v) => u64::from_le_bytes(v.as_slice().try_into().expect("counter width")),
        None => 0,
    };
    assert!(
        recovered >= last_acked,
        "iter {iter}: durability violation: recovered op {recovered} < acknowledged {last_acked} ({path:?})",
    );

    // replay the deterministic workload up to the recovered operation
    let mut model = std::collections::BTreeMap::new();
    for c in 1..=recovered {
        let (puts, delete) = tree_ops_for_commit(c);
        for (k, v) in puts {
            model.insert(k, v);
        }
        if let Some(k) = delete {
            model.remove(&k);
        }
    }
    if recovered > 0 {
        model.insert(OP_COUNTER_KEY.to_vec(), recovered.to_le_bytes().to_vec());
    }

    // the reopened tree must contain exactly that state: nothing missing
    // (committed data survives), nothing extra (uncommitted work vanished)
    let got: Vec<_> = snap
        .scan(None, None)
        .unwrap_or_else(|e| panic!("iter {iter}: scan: {e} ({path:?})"))
        .map(|r| r.unwrap_or_else(|e| panic!("iter {iter}: scan item: {e} ({path:?})")))
        .collect();
    let want: Vec<_> = model.into_iter().collect();
    assert!(
        got == want,
        "iter {iter}: recovered state differs from replay at commit {recovered} ({path:?}): got {} entries, want {}",
        got.len(),
        want.len(),
    );
}
