//! What the executor promises, checked over a socket rather than claimed.

#![cfg(target_os = "linux")]

mod harness;

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
            columns: Vec::new()
        }
    );
    match client.reply(Duration::from_secs(5)) {
        ServerMessage::RowBatch { rows } => assert_eq!(rows.len(), 1),
        other => panic!("expected a row batch, got {other:?}"),
    }
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
            columns: Vec::new()
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
            columns: Vec::new()
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
