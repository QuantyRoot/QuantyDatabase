//! Crash harness for explicit transactions.
//!
//! The phase 4 acceptance bar: SIGKILL a process while a transaction is
//! open, over and over, and never once reopen into a database that holds
//! half a transaction.
//!
//! Same shape as the core crash harness: the parent re-executes this test
//! binary as a child (gated by an env var). The child hammers transactions
//! into a database file, each one writing a fixed group of rows, and
//! prints every commit it completes. The parent waits a random few
//! milliseconds, SIGKILLs the child mid-transaction, reopens the file and
//! demands two things:
//!
//! 1. durability: every transaction the child acknowledged is fully there
//! 2. atomicity: every transaction group is all rows or no rows, never a
//!    partial group. A kill lands inside an open transaction almost every
//!    time, and that transaction must leave no trace whatsoever.
//!
//! Iteration count comes from QUANTY_TXN_CRASH_ITERS (default 100, CI
//! runs 1000).

mod common;

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use quanty_core::{Db, Value};
use quanty_exec::{Output, Session};

const ENV_CHILD: &str = "QUANTY_TXN_CRASH_CHILD";
const ENV_DB: &str = "QUANTY_TXN_CRASH_DB";
const ENV_ITERS: &str = "QUANTY_TXN_CRASH_ITERS";

/// Rows per transaction. Enough that a kill has a real chance of landing
/// between the first and last write of a group.
const GROUP: i64 = 5;

/// Transaction `k` owns row ids `k * GROUP .. k * GROUP + GROUP`, each
/// carrying `n = k`. Deterministic, so the parent can verify without
/// talking to the child.
fn ids_of(k: i64) -> std::ops::Range<i64> {
    (k * GROUP)..(k * GROUP + GROUP)
}

/// Child mode: write transactions until killed.
///
/// A #[test] only so it lives in the same binary as the parent; it exits
/// immediately unless the parent set the env var.
#[test]
fn txn_crash_child_entry() {
    if std::env::var(ENV_CHILD).is_err() {
        return;
    }
    let path = std::env::var(ENV_DB).expect("child needs QUANTY_TXN_CRASH_DB");
    let db = Db::create_file(&path).expect("child create db");
    let mut session = Session::new(db);
    session
        .execute("table t { id: int @key, n: int }")
        .expect("child create table");

    let mut out = std::io::stdout();
    writeln!(out, "READY").expect("write ready");
    out.flush().expect("flush ready");

    for k in 1i64.. {
        session.execute("begin").expect("begin");
        for id in ids_of(k) {
            session
                .execute(&format!("put t {{ id: {id}, n: {k} }}"))
                .expect("put in txn");
        }
        session.execute("commit").expect("commit");
        // only acknowledged after commit returned, so the parent may
        // demand every acknowledged transaction back, in full
        writeln!(out, "COMMIT {k}").expect("write commit");
        out.flush().expect("flush commit");
    }
}

#[test]
fn kill_inside_an_open_transaction_leaves_no_trace() {
    if std::env::var(ENV_CHILD).is_ok() {
        return; // this process is a child
    }
    let iters: u64 = std::env::var(ENV_ITERS)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);

    let exe = std::env::current_exe().expect("current exe");
    let mut rng = 0x5DEE_CE66_D1CE_B00Du64
        ^ SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock before the unix epoch")
            .as_nanos() as u64;
    let mut kills_mid_run = 0u64;
    let started = Instant::now();

    for iter in 0..iters {
        let dir = common::TestDir::new();
        let db_path = dir.path().join("txn_crash.qdb");

        let mut child = Command::new(&exe)
            .arg("txn_crash_child_entry")
            .arg("--exact")
            .arg("--nocapture")
            .env(ENV_CHILD, "1")
            .env(ENV_DB, &db_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn child");

        let last_committed = Arc::new(AtomicU64::new(0));
        let ready = Arc::new(AtomicU64::new(0));
        let reader_last = Arc::clone(&last_committed);
        let reader_ready = Arc::clone(&ready);
        let stdout = child.stdout.take().expect("child stdout");
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if line.contains("READY") {
                    reader_ready.store(1, Ordering::SeqCst);
                } else if let Some(pos) = line.find("COMMIT ") {
                    if let Ok(k) = line[pos + 7..].trim().parse::<u64>() {
                        reader_last.store(k, Ordering::SeqCst);
                    }
                }
            }
        });

        let deadline = Instant::now() + Duration::from_secs(10);
        while ready.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_micros(200));
        }
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        // a spread wide enough to land both between transactions and, far
        // more often, right in the middle of one
        std::thread::sleep(Duration::from_micros(rng % 25_000));

        child.kill().expect("SIGKILL child");
        child.wait().expect("reap child");
        reader.join().expect("reader thread");
        let acked = last_committed.load(Ordering::SeqCst);
        if acked > 0 {
            kills_mid_run += 1;
        }

        verify(&db_path, acked, iter);
    }

    println!(
        "txn crash harness: {iters} kills, {kills_mid_run} with acknowledged transactions, {:.1?} total",
        started.elapsed()
    );
}

fn verify(path: &std::path::Path, acked: u64, iter: u64) {
    let db = Db::open_file(path)
        .unwrap_or_else(|e| panic!("iter {iter}: recovery failed: {e} ({path:?})"));
    let mut session = Session::new(db);

    let rows = match session.execute("get t") {
        Ok(Output::Rows(rows)) => rows,
        Ok(other) => panic!("iter {iter}: expected rows, got {:?}", other.render()),
        // the table itself is committed before the first transaction, so a
        // kill can land before it exists; nothing to verify then
        Err(_) if acked == 0 => return,
        Err(e) => panic!("iter {iter}: read failed: {e} ({path:?})"),
    };

    // group the surviving rows by the transaction that wrote them
    let mut groups: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    for row in &rows {
        let (Value::Int(id), Value::Int(n)) = (&row[0], &row[1]) else {
            panic!("iter {iter}: unexpected row shape {row:?} ({path:?})");
        };
        let k = id / GROUP;
        assert_eq!(
            *n, k,
            "iter {iter}: row {id} carries n={n} but belongs to transaction {k} ({path:?})"
        );
        groups.entry(k).or_default().push(*id);
    }

    // atomicity: every group is whole. a torn transaction would show up
    // here as a group with fewer than GROUP rows
    for (k, ids) in &groups {
        assert_eq!(
            ids.len() as i64,
            GROUP,
            "iter {iter}: transaction {k} is torn: {} of {GROUP} rows survived ({path:?})",
            ids.len()
        );
        let mut want: Vec<i64> = ids_of(*k).collect();
        let mut got = ids.clone();
        want.sort_unstable();
        got.sort_unstable();
        assert_eq!(
            got, want,
            "iter {iter}: transaction {k} has the wrong rows ({path:?})"
        );
    }

    // durability: everything the child acknowledged is there. a
    // transaction that committed after the last line we managed to read is
    // allowed to be present too, which is why this is a floor, not an
    // equality
    for k in 1..=acked as i64 {
        assert!(
            groups.contains_key(&k),
            "iter {iter}: durability violation: acknowledged transaction {k} is missing ({path:?})"
        );
    }
}
