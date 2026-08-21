//! What a commit costs, and therefore what group commit could buy.
//!
//! ADR-024 says group commit falls out of the write queue: drain what is
//! waiting, apply it in one batch, fsync once. ADR-016 says do not build it
//! before a measurement says what it is worth. This is that measurement.
//!
//! **Batching k statements into one transaction is the same arithmetic as
//! group commit with queue depth k.** Both turn k commits into one, so the
//! curve over k is the ceiling group commit could reach, minus whatever the
//! per-statement savepoints it would need cost on top.
//!
//! Two numbers are needed, not one. If a commit costs about what an fsync
//! costs, then amortizing the fsync is the whole win. If a commit costs far
//! more, the fsync is not the bottleneck and group commit would be work
//! spent on the wrong half.
//!
//! Run it where the answer matters. On a container with an overlay
//! filesystem `fsync` may not reach a disk at all, and then the numbers
//! here describe the container and not the machine.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use quanty_core::{encode_key, Db, Value};
use quanty_exec::Session;

fn main() {
    let dir = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/quanty-commit-cost".to_string());
    let dir = PathBuf::from(dir);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create the working directory");

    // `bulk` runs only the statement path, so a profile of it is not
    // diluted by fsync probes and savepoint timing.
    if std::env::args().any(|a| a == "bulk") {
        let elapsed = time_puts(&dir, 50_000, 50_000);
        println!(
            "bulk 50000 rows in one transaction   {:>8.1} ms   {:>7.2} us/row",
            elapsed.as_secs_f64() * 1000.0,
            micros(elapsed / 50_000),
        );
        return;
    }

    let fsync = time_fsync(&dir, 200);
    println!(
        "fsync           {:>8.1} us  mean of 200 on {}",
        micros(fsync),
        dir.display()
    );
    println!();
    println!("batch   statements    per statement   commits/s   statements/s");

    for batch in [1usize, 2, 8, 64, 512] {
        let statements = if batch == 1 { 2_000 } else { 8_000 };
        let elapsed = time_puts(&dir, batch, statements);
        let each = elapsed / statements as u32;
        println!(
            "{batch:>5}   {statements:>10}   {:>11.1} us   {:>9.0}   {:>12.0}",
            micros(each),
            (statements / batch) as f64 / elapsed.as_secs_f64(),
            statements as f64 / elapsed.as_secs_f64(),
        );
    }

    // The bulk case: everything in one transaction, so exactly one commit
    // and one fsync. Whatever this costs is not durability, it is the
    // per-insert work in the b-tree, and it is where sqlite is ahead.
    println!();
    for rows in [1_000usize, 5_000, 20_000] {
        let elapsed = time_puts(&dir, rows, rows);
        println!(
            "bulk {rows:>6} rows in one transaction   {:>8.1} ms   {:>7.2} us/row",
            elapsed.as_secs_f64() * 1000.0,
            micros(elapsed / rows as u32),
        );
    }

    // Group commit cannot let one bad statement undo the nine good ones
    // batched with it, so each needs a savepoint around it. If that costs
    // as much as the commit it replaces, the whole idea is a wash.
    println!();
    let (plain, guarded) = time_savepoints(&dir, 20_000);
    println!(
        "20000 writes in one transaction   plain {:>7.2} us each   with a \
         savepoint each {:>7.2} us",
        micros(plain),
        micros(guarded)
    );
}

fn micros(d: Duration) -> f64 {
    d.as_secs_f64() * 1e6
}

/// What one durable write costs the filesystem underneath us.
fn time_fsync(dir: &Path, rounds: usize) -> Duration {
    let path = dir.join("fsync-probe");
    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&path)
        .expect("open the probe file");
    let page = [0u8; 4096];

    // One round first, so the cost of creating the file is not counted.
    file.write_all(&page).expect("write");
    file.sync_all().expect("sync");

    let start = Instant::now();
    for _ in 0..rounds {
        file.write_all(&page).expect("write");
        file.sync_all().expect("sync");
    }
    let elapsed = start.elapsed();
    drop(file);
    let _ = std::fs::remove_file(&path);
    elapsed / rounds as u32
}

/// What wrapping every write in a savepoint costs inside one transaction.
///
/// Below the statement layer on purpose: this is the machinery group
/// commit needs, not the statements it would batch.
fn time_savepoints(dir: &Path, rounds: usize) -> (Duration, Duration) {
    let mut out = [Duration::ZERO; 2];
    for (slot, guarded) in [false, true].into_iter().enumerate() {
        let path = dir.join(format!("savepoint-{guarded}.qdb"));
        let _ = std::fs::remove_file(&path);
        let db = Db::create_file(&path).expect("create the database");

        let mut tx = db.begin();
        let start = Instant::now();
        for i in 0..rounds {
            if guarded {
                tx.savepoint();
            }
            let key = encode_key(&[Value::Int(i as i64)]);
            tx.put(&key, b"value").expect("put");
            if guarded {
                tx.release_savepoint();
            }
        }
        out[slot] = start.elapsed() / rounds as u32;
        tx.commit().expect("commit");
        drop(db);
        let _ = std::fs::remove_file(&path);
    }
    (out[0], out[1])
}

/// Time `statements` writes with `batch` of them per commit.
fn time_puts(dir: &Path, batch: usize, statements: usize) -> Duration {
    let path = dir.join(format!("batch-{batch}.qdb"));
    let _ = std::fs::remove_file(&path);
    let db = Db::create_file(&path).expect("create the database");
    let mut session = Session::new(db);
    session
        .execute("table t { id: int @key, n: int }")
        .expect("create the table");

    // The statements are built up front: this measures the engine, not
    // `format!`.
    let sources: Vec<String> = (0..statements)
        .map(|i| format!("put t {{ id: {i}, n: {i} }}"))
        .collect();

    let start = Instant::now();
    for chunk in sources.chunks(batch) {
        if batch > 1 {
            session.execute("begin").expect("begin");
        }
        for source in chunk {
            session.execute(source).expect("put");
        }
        if batch > 1 {
            session.execute("commit").expect("commit");
        }
    }
    let elapsed = start.elapsed();

    drop(session);
    let size = File::open(&path)
        .and_then(|f| f.metadata())
        .map(|m| m.len())
        .unwrap_or(0);
    eprintln!("  (batch {batch}: {} KiB on disk)", size / 1024);
    let _ = std::fs::remove_file(&path);
    elapsed
}
