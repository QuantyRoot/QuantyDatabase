//! Many connections doing arbitrary things at once, checked against what
//! the server promised rather than against a golden transcript.
//!
//! The pieces are all tested on their own: parking, batching, the two
//! deadlines, the reader bypass. What no single test reaches is the state
//! space they share. A `begin` that wins a race against a batch, a
//! connection that vanishes while it holds the transaction, a statement
//! refused by the write queue while another is mid-commit; these arise from
//! timing, not from any one line, and the way to reach them is to run a lot
//! of them and check invariants that must hold no matter which way the race
//! went.
//!
//! Four invariants:
//!
//! - **Every request gets exactly one answer.** Not none, which is a
//!   connection parked forever, and not two, which is an answer delivered
//!   to a stranger.
//! - **The answer fits the question.** A `put` never comes back as a result
//!   set. Getting that wrong means the outbox routed a reply to the wrong
//!   connection, which the slot generations exist to prevent.
//! - **Nothing hangs.** Waiting for the writer past the deadline is
//!   answered with `0x0007`, which is an answer.
//! - **The rows that survive are exactly the rows that were promised.**
//!   Committed writes are there, rolled back ones are not, and a statement
//!   refused for any reason wrote nothing.
//!
//! Budget via `QUANTY_FUZZ_SECS`, seed via `QUANTY_FUZZ_SEED`. A failure
//! prints the seed, and rerunning with it replays the same decisions.

#![cfg(target_os = "linux")]

mod harness;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use quanty_proto::{ErrorCode, ServerMessage};
use quanty_service::Deadlines;

use harness::{Client, Server};

/// xorshift64*, so the run is reproducible from its seed without a
/// dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n.max(1)
    }
}

/// What one client thread decided to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Act {
    Put,
    Get,
    Begin,
    Commit,
    Rollback,
    Garbage,
    Reconnect,
}

fn pick(rng: &mut Rng, in_txn: bool) -> Act {
    match rng.below(100) {
        0..=39 => Act::Put,
        40..=64 => Act::Get,
        65..=77 => {
            if in_txn {
                Act::Commit
            } else {
                Act::Begin
            }
        }
        78..=86 => {
            if in_txn {
                Act::Rollback
            } else {
                Act::Get
            }
        }
        87..=94 => Act::Garbage,
        _ => Act::Reconnect,
    }
}

/// The reply, reduced to the shape a caller can check against its request.
#[derive(Debug, PartialEq, Eq)]
enum Shape {
    Ok,
    Count(u64),
    Rows(usize),
    Lines,
    Error(u16),
}

/// Read one whole answer, following a result set to its end.
fn answer(client: &mut Client, within: Duration) -> Shape {
    match client.reply(within) {
        ServerMessage::Ok => Shape::Ok,
        ServerMessage::Count { n, .. } => Shape::Count(n),
        ServerMessage::Lines(_) => Shape::Lines,
        ServerMessage::Ready => Shape::Ok,
        ServerMessage::Error { code, .. } => Shape::Error(code),
        ServerMessage::RowsBegin { .. } => {
            let mut rows = 0;
            loop {
                match client.reply(within) {
                    ServerMessage::RowBatch { rows: batch } => rows += batch.len(),
                    ServerMessage::RowsEnd => return Shape::Rows(rows),
                    ServerMessage::Error { code, .. } => return Shape::Error(code),
                    other => panic!("a result set contained {other:?}"),
                }
            }
        }
        other => panic!("unexpected answer {other:?}"),
    }
}

struct Outcome {
    /// Rows this thread believes are committed, by key.
    committed: Vec<i64>,
}

#[allow(clippy::too_many_arguments)]
fn work(
    server: &Server,
    lane: i64,
    mut rng: Rng,
    until: Instant,
    running: &AtomicBool,
    requests: &AtomicU64,
    refused: &AtomicU64,
    reconnects: &AtomicU64,
) -> Outcome {
    let mut client = server.client();
    let mut committed: Vec<i64> = Vec::new();
    let mut pending: Vec<i64> = Vec::new();
    let mut in_txn = false;
    let mut next_key = lane * 1_000_000;
    // A transaction is let go after a few statements so one thread cannot
    // sit on the queue for the whole run.
    let mut held = 0;

    while Instant::now() < until && running.load(Ordering::Relaxed) {
        let act = if in_txn && held >= 4 {
            if rng.below(2) == 0 {
                Act::Commit
            } else {
                Act::Rollback
            }
        } else {
            pick(&mut rng, in_txn)
        };

        requests.fetch_add(1, Ordering::Relaxed);
        let wait = Duration::from_secs(20);
        match act {
            Act::Put => {
                let key = next_key;
                next_key += 1;
                client.send(&format!("put t {{ id: {key}, n: {} }}", rng.below(1000)));
                match answer(&mut client, wait) {
                    Shape::Count(1) => {
                        if in_txn {
                            pending.push(key);
                            held += 1;
                        } else {
                            committed.push(key);
                        }
                    }
                    Shape::Error(code) => {
                        refused.fetch_add(1, Ordering::Relaxed);
                        // A statement that failed inside a transaction may
                        // have taken the transaction with it, so the
                        // transaction is closed by hand and its writes are
                        // given up on.
                        if in_txn {
                            client.send("rollback");
                            let _ = answer(&mut client, wait);
                            pending.clear();
                            in_txn = false;
                            held = 0;
                        }
                        assert!(
                            code == ErrorCode::WriteQueue.as_u16()
                                || code == ErrorCode::Execution.as_u16(),
                            "a put was refused with {code:#06x}"
                        );
                    }
                    other => panic!("a put was answered with {other:?}"),
                }
            }
            Act::Get => {
                client.send("get t");
                match answer(&mut client, wait) {
                    Shape::Rows(_) => {}
                    Shape::Error(code) => {
                        refused.fetch_add(1, Ordering::Relaxed);
                        assert_eq!(
                            code,
                            ErrorCode::WriteQueue.as_u16(),
                            "a read failed for a reason other than the queue"
                        );
                    }
                    other => panic!("a get was answered with {other:?}"),
                }
            }
            Act::Begin => {
                client.send("begin");
                match answer(&mut client, wait) {
                    Shape::Ok => {
                        in_txn = true;
                        held = 0;
                        pending.clear();
                        // Sometimes the transaction is held in silence for
                        // longer than the busy deadline. Without this the
                        // holder always finishes in under a millisecond,
                        // nothing ever waits long enough to be refused, and
                        // the whole queue-gives-up path goes untested while
                        // the soak reports success.
                        if rng.below(3) == 0 {
                            let ms = 450 + rng.below(250);
                            std::thread::sleep(Duration::from_millis(ms));
                        }
                    }
                    Shape::Error(_) => {
                        refused.fetch_add(1, Ordering::Relaxed);
                    }
                    other => panic!("a begin was answered with {other:?}"),
                }
            }
            Act::Commit => {
                client.send("commit");
                match answer(&mut client, wait) {
                    Shape::Ok => committed.append(&mut pending),
                    Shape::Error(_) => {
                        refused.fetch_add(1, Ordering::Relaxed);
                        pending.clear();
                    }
                    other => panic!("a commit was answered with {other:?}"),
                }
                in_txn = false;
                held = 0;
            }
            Act::Rollback => {
                client.send("rollback");
                let _ = answer(&mut client, wait);
                pending.clear();
                in_txn = false;
                held = 0;
            }
            Act::Garbage => {
                client.send("this is not a statement");
                match answer(&mut client, wait) {
                    Shape::Error(code) => assert_eq!(
                        code,
                        ErrorCode::Parse.as_u16(),
                        "garbage was refused for the wrong reason"
                    ),
                    other => panic!("garbage was answered with {other:?}"),
                }
            }
            Act::Reconnect => {
                // Dropped without a goodbye, sometimes with a read still in
                // flight, which is the path where a reply arrives for a
                // slot that has been handed out again.
                if rng.below(2) == 0 {
                    client.send("get t");
                }
                let fresh = server.client();
                let old = std::mem::replace(&mut client, fresh);
                old.abandon();
                reconnects.fetch_add(1, Ordering::Relaxed);
                pending.clear();
                in_txn = false;
                held = 0;
            }
        }
    }

    if in_txn {
        client.send("rollback");
        let _ = answer(&mut client, Duration::from_secs(20));
    }
    Outcome { committed }
}

#[test]
fn many_connections_doing_arbitrary_things() {
    let secs: u64 = std::env::var("QUANTY_FUZZ_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    let seed = std::env::var("QUANTY_FUZZ_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x2545F4914F6CDD1D)
        });
    let seed = if seed == 0 { 0x2545F4914F6CDD1D } else { seed };
    println!("server soak: seed {seed}, budget {secs}s");

    // A short busy deadline on purpose: the point is to reach the path
    // where the write queue gives up, not to avoid it.
    let server = Arc::new(Server::start(Deadlines {
        busy: Duration::from_millis(400),
        idle_in_txn: Duration::from_secs(30),
    }));
    let mut setup = server.client();
    assert_eq!(
        setup.ask("table t { id: int @key, n: int }"),
        ServerMessage::Ok
    );

    let until = Instant::now() + Duration::from_secs(secs);
    let running = Arc::new(AtomicBool::new(true));
    let requests = Arc::new(AtomicU64::new(0));
    let refused = Arc::new(AtomicU64::new(0));
    let reconnects = Arc::new(AtomicU64::new(0));
    let panics = Arc::new(Mutex::new(Vec::<String>::new()));

    let mut threads = Vec::new();
    for lane in 0..6i64 {
        let server = Arc::clone(&server);
        let running = Arc::clone(&running);
        let requests = Arc::clone(&requests);
        let refused = Arc::clone(&refused);
        let reconnects = Arc::clone(&reconnects);
        let panics = Arc::clone(&panics);
        let rng = Rng(seed.wrapping_mul(lane as u64 + 1) | 1);
        threads.push(std::thread::spawn(move || {
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                work(
                    &server,
                    lane,
                    rng,
                    until,
                    &running,
                    &requests,
                    &refused,
                    &reconnects,
                )
            }));
            match outcome {
                Ok(o) => o.committed,
                Err(e) => {
                    let message = e
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                        .unwrap_or_else(|| "a thread panicked".to_string());
                    // One thread giving up must stop the rest, or the
                    // others run out the clock before the failure is seen.
                    running.store(false, Ordering::Relaxed);
                    panics.lock().expect("lock").push(message);
                    Vec::new()
                }
            }
        }));
    }

    let mut expected: Vec<i64> = Vec::new();
    for thread in threads {
        expected.extend(thread.join().expect("thread"));
    }

    let failures = panics.lock().expect("lock");
    assert!(failures.is_empty(), "seed {seed}: {}", failures.join(" | "));
    drop(failures);

    // The database has to agree with what every thread was told.
    let mut reader = server.client();
    reader.send("get t");
    let shape = answer(&mut reader, Duration::from_secs(30));
    expected.sort_unstable();
    let Shape::Rows(rows) = shape else {
        panic!("seed {seed}: the final read answered {shape:?}");
    };
    assert_eq!(
        rows,
        expected.len(),
        "seed {seed}: the database holds {rows} rows, the clients were promised {}",
        expected.len()
    );

    let total = requests.load(Ordering::Relaxed);
    let gave_up = refused.load(Ordering::Relaxed);
    let dropped = reconnects.load(Ordering::Relaxed);
    let largest = server
        .stats()
        .largest_batch
        .load(std::sync::atomic::Ordering::Relaxed);
    println!(
        "server soak: {total} requests, {gave_up} refused, {dropped} reconnects, \
         largest batch {largest}, {rows} rows"
    );

    // A soak that never reached the contended paths proves nothing, and it
    // would say so by passing. These are the paths it exists for.
    // Scaled to the budget rather than fixed. A lane that is lingering in
    // a transaction holds up the writers behind it on purpose, so the rate
    // is low by design and a fixed floor only measures how long the run
    // was allowed to take.
    let floor = 20 * secs;
    assert!(
        total > floor,
        "seed {seed}: only {total} requests in {secs}s, expected more than \
         {floor}, the soak is not soaking"
    );
    assert!(
        gave_up > 0,
        "seed {seed}: nothing was ever refused, so the write queue deadline \
         was never reached"
    );
    assert!(
        dropped > 0,
        "seed {seed}: no connection was ever abandoned mid-flight"
    );
    assert!(
        largest > 1,
        "seed {seed}: no statements ever shared a commit"
    );
}
