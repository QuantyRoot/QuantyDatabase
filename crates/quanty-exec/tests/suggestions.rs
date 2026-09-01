//! Index suggestions: what this handle watched itself do badly.
//!
//! The acceptance question for phase 8 is not whether a suggestion looks
//! sensible but whether following it helps, so the heavy test measures a
//! workload before and after applying what was suggested.

use std::time::Instant;

use quanty_core::{Db, MemStorage};
use quanty_exec::{Output, Session};

fn lines(s: &mut Session<MemStorage>, statement: &str) -> Vec<String> {
    match s
        .execute(statement)
        .unwrap_or_else(|e| panic!("{statement}: {e}"))
    {
        Output::Lines(l) => l,
        other => panic!("expected lines, got {other:?}"),
    }
}

fn loaded(rows: usize) -> Session<MemStorage> {
    loaded_with(rows, 50)
}

fn loaded_with(rows: usize, cities: usize) -> Session<MemStorage> {
    let db = Db::in_memory().expect("open");
    let mut s = Session::new(db);
    s.execute("table users { id: int @key, city: text, bio: text }")
        .expect("table");
    s.execute("begin").expect("begin");
    for i in 0..rows {
        s.execute(&format!(
            "put users {{ id: {i}, city: \"city{}\", bio: \"alpha beta word{i}\" }}",
            i % cities
        ))
        .expect("put");
    }
    s.execute("commit").expect("commit");
    s
}

#[test]
fn a_fresh_handle_has_nothing_to_suggest() {
    let mut s = loaded(10);
    assert!(lines(&mut s, "show suggestions").is_empty());

    // and a query that already narrows teaches it nothing
    s.execute("get users { id } where id = 3").expect("by key");
    assert!(
        lines(&mut s, "show suggestions").is_empty(),
        "a key lookup was reported as a missing index"
    );
}

#[test]
fn a_scan_on_an_unindexed_column_becomes_a_suggestion() {
    let mut s = loaded(20);
    s.execute("get users { id } where city = \"city3\"")
        .expect("scan");

    let out = lines(&mut s, "show suggestions");
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].starts_with("index users.city"), "{out:?}");
    assert!(out[0].contains("1 query"), "{out:?}");
    assert!(out[0].contains("20 rows scanned"), "{out:?}");
}

#[test]
fn a_text_search_without_a_text_index_suggests_one() {
    let mut s = loaded(20);
    s.execute("get users { id } where bio match \"alpha\"")
        .expect("scan");
    let out = lines(&mut s, "show suggestions");
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].starts_with("index users.bio text"), "{out:?}");
}

#[test]
fn repeating_a_query_adds_up_rather_than_repeating_itself() {
    let mut s = loaded(20);
    for i in 0..5 {
        s.execute(&format!("get users {{ id }} where city = \"city{i}\""))
            .expect("scan");
    }
    let out = lines(&mut s, "show suggestions");
    assert_eq!(out.len(), 1, "one column, one suggestion: {out:?}");
    assert!(out[0].contains("5 queries"), "{out:?}");
    assert!(out[0].contains("100 rows scanned"), "{out:?}");
}

#[test]
fn the_one_that_scanned_the_most_comes_first() {
    // Worst by rows walked rather than by how often, because a hundred
    // scans of a small table cost less than one of a large one.
    let db = Db::in_memory().expect("open");
    let mut s = Session::new(db);
    s.execute("table small { id: int @key, tag: text }")
        .unwrap();
    s.execute("table big { id: int @key, tag: text }").unwrap();
    s.execute("begin").unwrap();
    for i in 0..5 {
        s.execute(&format!("put small {{ id: {i}, tag: \"t\" }}"))
            .unwrap();
    }
    for i in 0..200 {
        s.execute(&format!("put big {{ id: {i}, tag: \"t\" }}"))
            .unwrap();
    }
    s.execute("commit").unwrap();

    for _ in 0..10 {
        s.execute("get small { id } where tag = \"t\"").unwrap();
    }
    s.execute("get big { id } where tag = \"t\"").unwrap();

    let out = lines(&mut s, "show suggestions");
    assert_eq!(out.len(), 2);
    assert!(
        out[0].starts_with("index big.tag"),
        "not worst first: {out:?}"
    );
}

#[test]
fn applying_a_suggestion_stops_it_being_suggested() {
    let mut s = loaded(20);
    s.execute("get users { id } where city = \"city3\"")
        .unwrap();
    let statement = lines(&mut s, "show suggestions")[0]
        .split("  --")
        .next()
        .expect("a statement")
        .to_string();
    assert_eq!(statement, "index users.city");

    s.execute(&statement).expect("apply it");
    // the old count stays: it is a record of what happened, not of what
    // is still true. What changes is that the query stops adding to it.
    let before = lines(&mut s, "show suggestions");
    s.execute("get users { id } where city = \"city3\"")
        .unwrap();
    assert_eq!(
        lines(&mut s, "show suggestions"),
        before,
        "the query still counted as a scan after the index was built"
    );
}

#[test]
fn show_suggestions_is_refused_inside_a_transaction() {
    // Same rule as log, show branches and show stats: it reads what the
    // handle has done, not what this transaction is doing.
    let mut s = loaded(5);
    s.execute("begin").expect("begin");
    let err = s
        .execute("show suggestions")
        .expect_err("inside a transaction");
    assert!(err.to_string().contains("cannot run inside"), "{err}");
    s.execute("rollback").expect("rollback");
}

#[test]
#[ignore = "heavy, run with --release --ignored"]
fn following_a_suggestion_makes_the_workload_faster() {
    // The acceptance criterion, measured rather than asserted: run a
    // workload, ask what was missing, apply it, run the same workload.
    const ROWS: usize = 50_000;
    const QUERIES: usize = 200;
    // A thousand cities over fifty thousand rows, so a query answers with
    // fifty. The first version used fifty cities and answered with a
    // thousand, which measured 5x and is not what an index is for: a
    // column with fifty distinct values is a poor index, and scanning is
    // a reasonable plan for one.
    const CITIES: usize = 1000;

    let mut s = loaded_with(ROWS, CITIES);
    let workload = |s: &mut Session<MemStorage>| {
        let start = Instant::now();
        for i in 0..QUERIES {
            let out = s
                .execute(&format!(
                    "get users {{ id }} where city = \"city{}\"",
                    i % CITIES
                ))
                .expect("query");
            match out {
                Output::Rows { rows, .. } => assert_eq!(rows.len(), ROWS / CITIES),
                other => panic!("expected rows, got {other:?}"),
            }
        }
        start.elapsed()
    };

    let before = workload(&mut s);
    let suggested = lines(&mut s, "show suggestions");
    assert!(!suggested.is_empty(), "nothing was suggested");
    let statement = suggested[0]
        .split("  --")
        .next()
        .expect("a statement")
        .to_string();
    assert_eq!(statement, "index users.city");

    let built = Instant::now();
    s.execute(&statement).expect("apply the suggestion");
    let building = built.elapsed();

    let after = workload(&mut s);
    let factor = before.as_secs_f64() / after.as_secs_f64();
    println!(
        "{QUERIES} queries over {ROWS} rows: {before:.1?} before, \
         {after:.1?} after, {factor:.0}x, index built in {building:.1?}"
    );
    // Measured at 167x: 7.6s of scanning against 45ms of index lookups.
    // The bar sits far below that on purpose, since what it has to catch
    // is a suggestion that does not help, not a slow afternoon.
    assert!(
        factor > 20.0,
        "only {factor:.1}x faster after following the suggestion"
    );
}
