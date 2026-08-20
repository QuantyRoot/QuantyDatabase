//! What the executor promises, checked over a socket rather than claimed.

#![cfg(target_os = "linux")]

mod harness;

use std::sync::atomic::Ordering;
use std::time::Duration;

use quanty_proto::{ErrorCode, ServerMessage};
use quanty_service::Deadlines;

use harness::Server;

fn patient() -> Deadlines {
    Deadlines {
        busy: Duration::from_secs(30),
        idle_in_txn: Duration::from_secs(30),
    }
}

/// Short on waiting for the writer, patient about the transaction itself.
///
/// The two deadlines have to be pulled apart to be tested at all: with both
/// short, whichever fires first decides the outcome and neither is being
/// measured.
fn impatient_queue() -> Deadlines {
    Deadlines {
        busy: Duration::from_millis(300),
        idle_in_txn: Duration::from_secs(30),
    }
}

/// The other way round.
fn impatient_txn() -> Deadlines {
    Deadlines {
        busy: Duration::from_secs(30),
        idle_in_txn: Duration::from_millis(300),
    }
}

fn is_error(message: &ServerMessage, expected: ErrorCode) -> bool {
    matches!(message, ServerMessage::Error { code, .. } if *code == expected.as_u16())
}

#[test]
fn a_statement_runs_and_its_answer_comes_back() {
    let server = Server::start(patient());
    let mut client = server.client();

    assert_eq!(
        client.ask("table t { id: int @key, a: int }"),
        ServerMessage::Ok
    );
    assert!(
        matches!(client.ask("put t { id: 1, a: 1 }"), ServerMessage::Count { n, .. } if n == 1),
        "the write did not report one row"
    );

    assert_eq!(
        client.ask("get t"),
        ServerMessage::RowsBegin {
            columns: vec!["id".into(), "a".into()]
        },
        "the result set arrived without its column names"
    );
    match client.reply(Duration::from_secs(5)) {
        ServerMessage::RowBatch { rows } => assert_eq!(rows.len(), 1),
        other => panic!("expected a row batch, got {other:?}"),
    }
    assert_eq!(client.reply(Duration::from_secs(5)), ServerMessage::RowsEnd);
}

/// A projection reorders and narrows the header with the values, and a
/// join qualifies it. A header that does not follow the values is worse
/// than no header at all.
#[test]
fn the_column_names_follow_the_projection_and_the_join() {
    let server = Server::start(patient());
    let mut client = server.client();
    assert_eq!(
        client.ask("table users { id: int @key, name: text }"),
        ServerMessage::Ok
    );
    assert_eq!(
        client.ask("table cities { id: int @key, name: text }"),
        ServerMessage::Ok
    );

    assert_eq!(
        client.ask("get users { name, id }"),
        ServerMessage::RowsBegin {
            columns: vec!["name".into(), "id".into()]
        },
        "the header kept table order instead of following the projection"
    );
    assert_eq!(client.reply(Duration::from_secs(5)), ServerMessage::RowsEnd);

    assert_eq!(
        client.ask("get users join cities on users.id = cities.id"),
        ServerMessage::RowsBegin {
            columns: vec![
                "users.id".into(),
                "users.name".into(),
                "cities.id".into(),
                "cities.name".into()
            ]
        },
        "a join must qualify its names, two of them are 'name'"
    );
    assert_eq!(client.reply(Duration::from_secs(5)), ServerMessage::RowsEnd);
}

#[test]
fn a_statement_that_does_not_parse_says_so_and_the_connection_lives() {
    let server = Server::start(patient());
    let mut client = server.client();

    let reply = client.ask("this is not a statement");
    assert!(is_error(&reply, ErrorCode::Parse), "got {reply:?}");
    assert_eq!(
        client.ask("table after { id: int @key }"),
        ServerMessage::Ok,
        "a failed statement must not poison the connection"
    );
}

/// One session serves every connection, so the transaction slot has to be
/// empty again for whoever comes next.
#[test]
fn the_next_connection_gets_a_clean_transaction_slot() {
    let server = Server::start(patient());
    let mut a = server.client();
    assert_eq!(a.ask("table t { id: int @key, a: int }"), ServerMessage::Ok);
    assert_eq!(a.ask("begin"), ServerMessage::Ok);
    assert!(
        matches!(a.ask("put t { id: 1, a: 1 }"), ServerMessage::Count { .. }),
        "the write inside the transaction failed"
    );
    assert_eq!(a.ask("commit"), ServerMessage::Ok);

    // Not "a transaction is already open": A's is gone and this one is B's.
    let mut b = server.client();
    assert_eq!(b.ask("begin"), ServerMessage::Ok);
    assert_eq!(b.ask("rollback"), ServerMessage::Ok);
}

/// A write inside someone else's open transaction is not there yet, and the
/// reader that asks for it waits rather than seeing half of it.
#[test]
fn an_uncommitted_write_is_not_visible_to_anyone_else() {
    let server = Server::start(patient());
    let mut a = server.client();
    assert_eq!(a.ask("table t { id: int @key, a: int }"), ServerMessage::Ok);
    assert_eq!(a.ask("begin"), ServerMessage::Ok);
    assert!(
        matches!(a.ask("put t { id: 1, a: 1 }"), ServerMessage::Count { .. }),
        "the write inside the transaction failed"
    );

    let mut b = server.client();
    b.send("get t");
    assert!(
        b.answered(Duration::from_millis(200)).is_none(),
        "the read ran while a transaction was open"
    );

    assert_eq!(a.ask("rollback"), ServerMessage::Ok);
    assert_eq!(
        b.reply(Duration::from_secs(5)),
        ServerMessage::RowsBegin {
            columns: vec!["id".into(), "a".into()]
        }
    );
    assert_eq!(
        b.reply(Duration::from_secs(5)),
        ServerMessage::RowsEnd,
        "the rolled back row was visible"
    );
}

/// A connection holding a transaction makes others wait rather than
/// blocking the worker thread that owns them.
#[test]
fn a_statement_waits_for_an_open_transaction_and_then_runs() {
    let server = Server::start(patient());
    let mut setup = server.client();
    assert_eq!(
        setup.ask("table t { id: int @key, a: int }"),
        ServerMessage::Ok
    );

    let mut holder = server.client();
    assert_eq!(holder.ask("begin"), ServerMessage::Ok);

    let mut waiting = server.client();
    waiting.send("table other { id: int @key }");
    assert!(
        waiting.answered(Duration::from_millis(200)).is_none(),
        "the statement ran while another connection held the transaction"
    );

    assert_eq!(holder.ask("commit"), ServerMessage::Ok);
    assert_eq!(
        waiting.reply(Duration::from_secs(5)),
        ServerMessage::Ok,
        "the queued statement never ran after the transaction closed"
    );
}

#[test]
fn waiting_too_long_for_the_writer_is_refused_rather_than_hung() {
    let server = Server::start(impatient_queue());
    let mut holder = server.client();
    assert_eq!(holder.ask("begin"), ServerMessage::Ok);

    let mut waiting = server.client();
    let reply = waiting.ask("table t { id: int @key, a: int }");
    assert!(
        is_error(&reply, ErrorCode::WriteQueue),
        "expected the write queue to refuse it, got {reply:?}"
    );
}

#[test]
fn a_transaction_left_open_in_silence_is_rolled_back() {
    let server = Server::start(impatient_txn());
    let mut setup = server.client();
    assert_eq!(
        setup.ask("table t { id: int @key, a: int }"),
        ServerMessage::Ok
    );

    let mut holder = server.client();
    assert_eq!(holder.ask("begin"), ServerMessage::Ok);
    assert!(
        matches!(
            holder.ask("put t { id: 1, a: 1 }"),
            ServerMessage::Count { .. }
        ),
        "the write inside the transaction failed"
    );

    std::thread::sleep(Duration::from_millis(600));

    // The rollback is reported at the next statement rather than pushed at
    // a client that is not expecting a message.
    let reply = holder.ask("commit");
    assert!(
        is_error(&reply, ErrorCode::Execution),
        "expected to be told the transaction is gone, got {reply:?}"
    );

    let mut reader = server.client();
    assert_eq!(
        reader.ask("get t"),
        ServerMessage::RowsBegin {
            columns: vec!["id".into(), "a".into()]
        }
    );
    assert_eq!(
        reader.reply(Duration::from_secs(5)),
        ServerMessage::RowsEnd,
        "the rolled back write is still there"
    );
}

/// A client that disappears mid-transaction must not hold the queue for
/// as long as the idle deadline: the close is the answer.
#[test]
fn a_connection_that_vanishes_releases_the_queue() {
    let server = Server::start(patient());
    let mut holder = server.client();
    assert_eq!(holder.ask("begin"), ServerMessage::Ok);
    holder.abandon();

    let mut next = server.client();
    assert_eq!(
        next.ask("table t { id: int @key, a: int }"),
        ServerMessage::Ok,
        "the queue stayed shut after the holder went away"
    );
}

/// The promise that makes batching allowed: statements sharing a commit
/// still fail on their own.
#[test]
fn a_bad_statement_in_a_batch_leaves_its_neighbours_alone() {
    let server = Server::start(patient());
    let mut setup = server.client();
    assert_eq!(
        setup.ask("table t { id: int @key, n: int }"),
        ServerMessage::Ok
    );

    // Sent without waiting, so they reach the executor together and share
    // a turn. The duplicate key in the middle is the one that must fail.
    let mut a = server.client();
    let mut b = server.client();
    let mut c = server.client();
    a.send("put t { id: 1, n: 1 }");
    b.send("put t { id: 1, n: 2 }, { id: 1, n: 3 }");
    c.send("put t { id: 2, n: 2 }");

    assert!(
        matches!(a.reply(Duration::from_secs(5)), ServerMessage::Count { n, .. } if n == 1),
        "the first write did not land"
    );
    let middle = b.reply(Duration::from_secs(5));
    assert!(
        is_error(&middle, ErrorCode::Execution),
        "the duplicate key should have failed, got {middle:?}"
    );
    assert!(
        matches!(c.reply(Duration::from_secs(5)), ServerMessage::Count { n, .. } if n == 1),
        "the third write was taken down with the failing one"
    );

    // Exactly the two good rows are there, and the failed statement wrote
    // nothing at all.
    let mut reader = server.client();
    assert_eq!(
        reader.ask("get t"),
        ServerMessage::RowsBegin {
            columns: vec!["id".into(), "n".into()]
        }
    );
    match reader.reply(Duration::from_secs(5)) {
        ServerMessage::RowBatch { rows } => assert_eq!(rows.len(), 2, "{rows:?}"),
        other => panic!("expected two rows, got {other:?}"),
    }
    assert_eq!(reader.reply(Duration::from_secs(5)), ServerMessage::RowsEnd);
}

/// A `begin` arriving in the same turn as ordinary statements must not be
/// swept into their transaction: it opens one of its own.
#[test]
fn a_begin_in_the_same_turn_still_takes_the_queue() {
    let server = Server::start(patient());
    let mut setup = server.client();
    assert_eq!(
        setup.ask("table t { id: int @key, n: int }"),
        ServerMessage::Ok
    );

    let mut first = server.client();
    let mut holder = server.client();
    let mut behind = server.client();
    first.send("put t { id: 1, n: 1 }");
    holder.send("begin");
    behind.send("put t { id: 2, n: 2 }");

    assert!(
        matches!(
            first.reply(Duration::from_secs(5)),
            ServerMessage::Count { .. }
        ),
        "the statement before the begin did not run"
    );
    assert_eq!(holder.reply(Duration::from_secs(5)), ServerMessage::Ok);
    assert!(
        behind.answered(Duration::from_millis(300)).is_none(),
        "the statement after the begin ran anyway"
    );

    assert_eq!(holder.ask("rollback"), ServerMessage::Ok);
    assert!(
        matches!(
            behind.reply(Duration::from_secs(5)),
            ServerMessage::Count { .. }
        ),
        "the queued statement never ran"
    );
}

/// Many statements at once must answer the same as one at a time.
#[test]
fn a_batch_answers_the_same_as_one_at_a_time() {
    let server = Server::start(patient());
    let mut setup = server.client();
    assert_eq!(
        setup.ask("table t { id: int @key, n: int }"),
        ServerMessage::Ok
    );

    let mut clients: Vec<_> = (0..24).map(|_| server.client()).collect();
    for (i, client) in clients.iter_mut().enumerate() {
        client.send(&format!("put t {{ id: {i}, n: {i} }}"));
    }
    for (i, client) in clients.iter_mut().enumerate() {
        assert!(
            matches!(client.reply(Duration::from_secs(10)), ServerMessage::Count { n, .. } if n == 1),
            "write {i} did not report one row"
        );
    }

    let mut reader = server.client();
    assert_eq!(
        reader.ask("get t"),
        ServerMessage::RowsBegin {
            columns: vec!["id".into(), "n".into()]
        }
    );
    match reader.reply(Duration::from_secs(5)) {
        ServerMessage::RowBatch { rows } => assert_eq!(rows.len(), 24),
        other => panic!("expected 24 rows, got {other:?}"),
    }
    assert_eq!(reader.reply(Duration::from_secs(5)), ServerMessage::RowsEnd);
}

/// That a batch forms at all, forced rather than hoped for.
///
/// Statements arriving on their own may well be picked up one at a time,
/// which would let every test above pass without a single shared commit
/// ever happening. Blocking them behind an open transaction removes the
/// timing from the question: when it closes, everything waiting is ready
/// in the same turn and has to share one commit.
#[test]
fn statements_released_together_share_one_commit() {
    let server = Server::start(patient());
    let mut setup = server.client();
    assert_eq!(
        setup.ask("table t { id: int @key, n: int }"),
        ServerMessage::Ok
    );

    let mut holder = server.client();
    assert_eq!(holder.ask("begin"), ServerMessage::Ok);

    let mut waiting: Vec<_> = (0..12).map(|_| server.client()).collect();
    for (i, client) in waiting.iter_mut().enumerate() {
        client.send(&format!("put t {{ id: {i}, n: {i} }}"));
    }
    // Give them time to reach the executor and pile up behind the holder.
    std::thread::sleep(Duration::from_millis(200));
    let before = server.stats().largest_batch.load(Ordering::Relaxed);

    assert_eq!(holder.ask("commit"), ServerMessage::Ok);
    for (i, client) in waiting.iter_mut().enumerate() {
        assert!(
            matches!(client.reply(Duration::from_secs(10)), ServerMessage::Count { n, .. } if n == 1),
            "write {i} did not run after the transaction closed"
        );
    }

    let stats = server.stats();
    let largest = stats.largest_batch.load(Ordering::Relaxed);
    assert!(
        largest > before && largest > 1,
        "nothing was batched: largest was {before} before and {largest} after"
    );
    assert!(
        stats.batched.load(Ordering::Relaxed) >= largest as u64,
        "the batched count does not agree with the largest batch"
    );
}
