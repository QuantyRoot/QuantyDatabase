//! Phase 7 acceptance: `match` against the index has to agree with
//! `match` against a scan, and beat it by a wide margin (ADR-036).
//!
//! The comparison is honest because both sides run the same predicate.
//! `match` is a binary operator, so a column without `@text` evaluates it
//! row by row; that is the brute force, not a second implementation
//! written to agree.

use std::time::Instant;

use quanty_core::{Db, MemStorage, Value};
use quanty_exec::{Output, Session};

/// Documents drawn from a Zipf-shaped vocabulary, because that is what
/// text looks like: a handful of words in most documents and a long tail
/// of words in almost none.
///
/// A flat vocabulary was tried first and is the wrong instrument. With
/// twenty-four words every term sits in a third of the corpus, every
/// query matches tens of thousands of rows, and no index can beat a scan
/// at producing tens of thousands of rows. That measures the corpus, not
/// the index.
fn corpus(n: usize) -> Vec<String> {
    const VOCAB: usize = 5000;
    const WORDS_PER_DOC: usize = 15;
    let cumulative: Vec<f64> = {
        let weights: Vec<f64> = (1..=VOCAB).map(|rank| 1.0 / rank as f64).collect();
        let total: f64 = weights.iter().sum();
        weights
            .iter()
            .scan(0.0, |acc, w| {
                *acc += w;
                Some(*acc / total)
            })
            .collect()
    };
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 11) as f64 / (1u64 << 53) as f64
    };
    (0..n)
        .map(|_| {
            (0..WORDS_PER_DOC)
                .map(|_| {
                    let u = next();
                    let rank = cumulative.partition_point(|c| *c < u).min(VOCAB - 1);
                    format!("w{rank}")
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

/// Two tables with the same rows, one indexed and one not.
fn loaded(docs: &[String]) -> Session<MemStorage> {
    let db = Db::in_memory().expect("open");
    let mut s = Session::new(db);
    s.execute("table indexed { id: int @key, body: text @text }")
        .expect("indexed");
    s.execute("table plain { id: int @key, body: text }")
        .expect("plain");

    s.execute("begin").expect("begin");
    for (i, doc) in docs.iter().enumerate() {
        s.execute(&format!("put indexed {{ id: {i}, body: \"{doc}\" }}"))
            .expect("put indexed");
        s.execute(&format!("put plain {{ id: {i}, body: \"{doc}\" }}"))
            .expect("put plain");
    }
    s.execute("commit").expect("commit");
    s
}

fn ids(s: &mut Session<MemStorage>, table: &str, query: &str) -> Vec<i64> {
    let statement = format!("get {table} {{ id }} where body match \"{query}\"");
    match s
        .execute(&statement)
        .unwrap_or_else(|e| panic!("{statement}: {e}"))
    {
        Output::Rows { rows, .. } => {
            let mut out: Vec<i64> = rows
                .into_iter()
                .map(|r| match r[0] {
                    Value::Int(n) => n,
                    ref other => panic!("unexpected id {other:?}"),
                })
                .collect();
            out.sort_unstable();
            out
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

/// What people actually search for: words specific enough to be worth
/// typing. Nobody searches a corpus for its most common word.
const QUERIES: [&str; 8] = [
    "w900",
    "w4000",
    "w2000 w1500",
    "w333 w1200",
    "w750",
    "nothinglikethis",
    "w900 nothinglikethis",
    "w1100 w2300 w3100",
];

/// Queries at the other end, kept to show what the index cannot do.
const COMMON: [&str; 3] = ["w0", "w1 w2", "w7"];

#[test]
fn the_index_agrees_with_the_scan() {
    let docs = corpus(2000);
    let mut s = loaded(&docs);

    for query in QUERIES {
        let fast = ids(&mut s, "indexed", query);
        let slow = ids(&mut s, "plain", query);
        assert_eq!(fast, slow, "query {query:?} disagreed");
    }
}

#[test]
fn the_plan_says_which_path_it_took() {
    // Without this the speed test could be measuring two scans.
    let docs = corpus(50);
    let mut s = loaded(&docs);

    let explain = |s: &mut Session<MemStorage>, table: &str| -> String {
        match s
            .execute(&format!(
                "explain get {table} {{ id }} where body match \"alpha beta\""
            ))
            .expect("explain")
        {
            Output::Lines(l) => l.join("\n"),
            other => panic!("expected lines, got {other:?}"),
        }
    };

    let indexed = explain(&mut s, "indexed");
    assert!(
        indexed.contains("text match") && indexed.contains("alpha"),
        "indexed table did not take the text path: {indexed}"
    );

    let plain = explain(&mut s, "plain");
    assert!(
        !plain.contains("text match"),
        "plain table took a text path it has no index for: {plain}"
    );
}

#[test]
fn a_query_with_no_words_matches_everything_on_both_sides() {
    let docs = corpus(200);
    let mut s = loaded(&docs);
    assert_eq!(
        ids(&mut s, "indexed", "   ...  "),
        ids(&mut s, "plain", "   ...  ")
    );
    assert_eq!(ids(&mut s, "indexed", "   ...  ").len(), 200);
}

#[test]
fn match_is_on_words_not_substrings() {
    let db = Db::in_memory().unwrap();
    let mut s = Session::new(db);
    s.execute("table t { id: int @key, body: text @text }")
        .unwrap();
    s.execute("put t { id: 1, body: \"category theory\" }")
        .unwrap();

    assert!(ids(&mut s, "t", "cat").is_empty(), "substring matched");
    assert_eq!(ids(&mut s, "t", "category"), [1]);
    assert_eq!(ids(&mut s, "t", "CATEGORY"), [1], "case should not matter");
}

#[test]
#[ignore = "heavy, run with --release --ignored"]
fn a_hundred_thousand_documents_and_a_hundredfold() {
    let docs = corpus(100_000);
    let mut s = loaded(&docs);

    // warm both paths so neither pays for a cold tree
    for query in QUERIES.iter().chain(COMMON.iter()) {
        let _ = ids(&mut s, "indexed", query);
        let _ = ids(&mut s, "plain", query);
    }

    let mut indexed_total = 0.0;
    let mut scan_total = 0.0;
    println!("100k documents, 15 words each, Zipf over 5000 terms");
    for query in QUERIES.iter().chain(COMMON.iter()) {
        let start = Instant::now();
        let fast = ids(&mut s, "indexed", query);
        let with_index = start.elapsed();

        let start = Instant::now();
        let slow = ids(&mut s, "plain", query);
        let with_scan = start.elapsed();

        assert_eq!(fast, slow, "query {query:?} disagreed at 100k");

        let factor = with_scan.as_secs_f64() / with_index.as_secs_f64();
        println!(
            "  {query:24} {:6} hits  index {with_index:>9.1?}  scan {with_scan:>9.1?}  {factor:.0}x",
            fast.len()
        );
        if QUERIES.contains(query) {
            indexed_total += with_index.as_secs_f64();
            scan_total += with_scan.as_secs_f64();
        }
    }

    let factor = scan_total / indexed_total;
    println!("  search mix: {factor:.0}x");
    assert!(
        factor > 100.0,
        "only {factor:.1}x faster than the scan over the search mix"
    );
}
