//! The embedded surface, exercised the way an embedder would.

mod common;

use quanty::{Database, ErrorKind, Outcome, Value};

fn seeded() -> Database {
    let mut db = Database::in_memory().expect("open in memory");
    db.execute("table users { id: int @key, name: text @index, score: int = 0 }")
        .expect("define table");
    db
}

#[test]
fn round_trip_carries_column_names() {
    let mut db = seeded();
    db.execute("put users { id: 1, name: \"ada\", score: 7 }")
        .expect("put");

    let rows = db.query("get users { name, score }").expect("get");
    assert_eq!(rows.columns(), ["name", "score"]);
    assert_eq!(rows.len(), 1);

    let name = rows.column("name").expect("column name");
    assert_eq!(rows.rows()[0][name], Value::Text("ada".into()));
}

#[test]
fn a_committed_transaction_keeps_its_rows() {
    let mut db = seeded();
    db.transaction(|tx| {
        tx.execute("put users { id: 1, name: \"ada\" }")?;
        tx.execute("put users { id: 2, name: \"grace\" }")
    })
    .expect("transaction");

    assert!(!db.in_transaction(), "transaction left open after commit");
    assert_eq!(db.query("get users { id }").expect("get").len(), 2);
}

#[test]
fn a_failed_closure_rolls_back_and_leaves_no_transaction() {
    let mut db = seeded();
    db.execute("put users { id: 1, name: \"ada\" }")
        .expect("put");

    let outcome: Result<(), _> = db.transaction(|tx| {
        tx.execute("put users { id: 2, name: \"grace\" }")?;
        // A duplicate key, so the failure comes from the engine rather
        // than from the test deciding to give up.
        tx.execute("put users { id: 1, name: \"clash\" }")?;
        Ok(())
    });

    let err = outcome.expect_err("duplicate key should fail");
    assert_eq!(err.kind(), ErrorKind::Exec, "kind was {err}");

    assert!(!db.in_transaction(), "transaction left open after rollback");
    let rows = db.query("get users { id }").expect("get");
    assert_eq!(rows.len(), 1, "the rolled back row survived");
}

#[test]
fn an_early_return_rolls_back_too() {
    let mut db = seeded();
    let outcome: Result<(), _> = db.transaction(|tx| {
        tx.execute("put users { id: 1, name: \"ada\" }")?;
        Err(tx
            .query("get users { id } where id = 9")
            .expect("get")
            .is_empty()
            .then(|| tx.execute("no such statement").unwrap_err())
            .expect("statement should not parse"))
    });

    assert_eq!(
        outcome.expect_err("closure returned Err").kind(),
        ErrorKind::Parse
    );
    assert!(!db.in_transaction());
    assert!(db.query("get users { id }").expect("get").is_empty());
}

#[test]
fn error_kinds_name_the_layer_that_refused() {
    let mut db = seeded();

    let parse = db.execute("this is not a statement").expect_err("parse");
    assert_eq!(parse.kind(), ErrorKind::Parse, "was {parse}");

    let plan = db.execute("get nowhere { id }").expect_err("plan");
    assert_eq!(plan.kind(), ErrorKind::Plan, "was {plan}");

    let exec = db
        .execute("put users { id: \"not an int\", name: \"x\" }")
        .expect_err("exec");
    assert_eq!(exec.kind(), ErrorKind::Exec, "was {exec}");
}

#[test]
fn asking_a_non_row_statement_for_rows_says_so() {
    let mut db = seeded();
    let err = db
        .query("put users { id: 1, name: \"ada\" }")
        .expect_err("rows");
    assert_eq!(err.kind(), ErrorKind::Exec);
    assert!(
        err.to_string().contains("not rows"),
        "unhelpful message: {err}"
    );
}

#[test]
fn outcomes_are_distinguishable_without_parsing_text() {
    let mut db = seeded();
    assert!(matches!(
        db.execute("put users { id: 1, name: \"ada\" }")
            .expect("put"),
        Outcome::Affected { .. } | Outcome::Done
    ));
    assert!(matches!(
        db.execute("get users { id }").expect("get"),
        Outcome::Rows(_)
    ));
    assert!(matches!(
        db.execute("show tables").expect("show"),
        Outcome::Rows(_) | Outcome::Lines(_)
    ));
}

#[test]
fn gc_prunes_history_and_keeps_the_rows() {
    let mut db = seeded();
    for i in 0..8 {
        db.execute(&format!("put users {{ id: {i}, name: \"u{i}\" }}"))
            .expect("put");
    }
    let before = db.head();

    let report = db.gc(2).expect("gc");
    assert!(
        report.pruned_commits > 0,
        "gc pruned nothing, so this test proves nothing"
    );
    assert_eq!(db.head(), before, "gc moved the head");
    assert_eq!(db.query("get users { id }").expect("get").len(), 8);

    // Retention is a position, not a budget: asking twice prunes nothing
    // the second time.
    let again = db.gc(2).expect("second gc");
    assert_eq!(again.pruned_commits, 0);
}

#[test]
fn gc_refuses_inside_a_transaction() {
    let mut db = seeded();
    db.execute("begin").expect("begin");
    let err = db.gc(2).expect_err("gc inside a transaction");
    assert_eq!(err.kind(), ErrorKind::Exec, "was {err}");
    assert!(db.in_transaction(), "the refusal ate the transaction");
    db.execute("rollback").expect("rollback");
}

#[test]
fn a_file_database_survives_being_closed_and_opened() {
    let dir = common::TestDir::new();
    let path = dir.path().join("round-trip.qdb");

    {
        let mut db = Database::create(&path).expect("create");
        db.execute("table t { id: int @key }").expect("table");
        db.execute("put t { id: 42 }").expect("put");
    }

    let mut db = Database::open(&path).expect("open");
    let rows = db.query("get t { id }").expect("get");
    assert_eq!(rows.rows()[0][0], Value::Int(42));
    assert_eq!(db.branch(), "main");
}

#[test]
fn the_sql_front_end_is_reachable_and_agrees_with_qql() {
    let mut db = seeded();
    db.execute("put users { id: 1, name: \"ada\", score: 7 }")
        .expect("put");

    let sql = db
        .query_sql("SELECT name FROM users WHERE id = 1")
        .expect("select");
    let qql = db.query("get users { name } where id = 1").expect("get");

    assert_eq!(sql.len(), 1);
    assert_eq!(sql.rows(), qql.rows(), "the two front ends disagree");
}

#[test]
fn qualified_column_names_are_findable_by_bare_name() {
    let mut db = seeded();
    db.execute("put users { id: 1, name: \"ada\" }")
        .expect("put");
    let rows = db.query("get users { id, name }").expect("get");
    for column in rows.columns() {
        let bare = column.rsplit('.').next().expect("a name");
        assert!(rows.column(bare).is_some(), "cannot find {bare}");
    }
}
