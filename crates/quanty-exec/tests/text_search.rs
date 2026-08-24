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

// ---------------------------------------------------------------------------
// ranking (ADR-036)
// ---------------------------------------------------------------------------

/// Ids in the order the engine returned them, unsorted.
fn ranked(s: &mut Session<MemStorage>, table: &str, query: &str) -> Vec<i64> {
    let statement = format!("get {table} {{ id }} where body match \"{query}\"");
    match s
        .execute(&statement)
        .unwrap_or_else(|e| panic!("{statement}: {e}"))
    {
        Output::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| match r[0] {
                Value::Int(n) => n,
                ref other => panic!("unexpected id {other:?}"),
            })
            .collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}

fn with_docs(docs: &[(i64, &str)]) -> Session<MemStorage> {
    let db = Db::in_memory().expect("open");
    let mut s = Session::new(db);
    s.execute("table docs { id: int @key, body: text @text }")
        .expect("table");
    for (id, body) in docs {
        s.execute(&format!("put docs {{ id: {id}, body: \"{body}\" }}"))
            .expect("put");
    }
    s
}

#[test]
fn a_term_that_occurs_more_often_ranks_higher() {
    let mut s = with_docs(&[
        (1, "quick"),
        (2, "quick quick quick quick"),
        (3, "quick quick"),
    ]);
    // all three documents are the same length in terms of nothing else,
    // so only term frequency separates them
    assert_eq!(ranked(&mut s, "docs", "quick"), [2, 3, 1]);
}

#[test]
fn a_shorter_document_ranks_higher_at_the_same_frequency() {
    // BM25 normalises by length: one hit in five words is worth more than
    // one hit in fifty.
    let long = format!("quick {}", vec!["filler"; 50].join(" "));
    let mut s = with_docs(&[(1, &long), (2, "quick and short")]);
    assert_eq!(ranked(&mut s, "docs", "quick"), [2, 1]);
}

#[test]
fn a_rare_term_counts_for_more_than_a_common_one() {
    // Both documents hold both terms, four words each, so frequency and
    // length cancel out and only the rarity of the terms separates them.
    // The one whose occurrences are of the rare word has to win.
    //
    // Proved to catch: fixing idf at 1.0 makes these two tie and the
    // order fall back to the key, which is 1 before 2.
    let mut s = with_docs(&[
        (1, "rare common common common"),
        (2, "rare rare rare common"),
        (3, "common filler filler filler"),
        (4, "common filler filler filler"),
        (5, "common filler filler filler"),
        (6, "common filler filler filler"),
    ]);
    assert_eq!(
        ranked(&mut s, "docs", "common rare"),
        [2, 1],
        "the rarer term did not count for more"
    );
}

#[test]
fn an_explicit_order_wins_over_the_ranking() {
    let mut s = with_docs(&[(1, "quick"), (2, "quick quick quick"), (3, "quick quick")]);
    assert_eq!(ranked(&mut s, "docs", "quick"), [2, 3, 1], "not ranked");

    let out = s
        .execute("get docs { id } where body match \"quick\" order by id asc")
        .expect("ordered");
    match out {
        Output::Rows { rows, .. } => {
            let ids: Vec<i64> = rows
                .into_iter()
                .map(|r| match r[0] {
                    Value::Int(n) => n,
                    _ => unreachable!(),
                })
                .collect();
            assert_eq!(ids, [1, 2, 3], "the explicit order was overruled");
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn ties_come_out_in_key_order_and_stay_there() {
    // What this pins is the observable property: equal scores come back
    // in primary key order, and the same query gives the same answer
    // twice. The explicit tie break in the sort is what keeps that true
    // if the seed list ever stops arriving in key order; taking it out
    // does not fail this test today, and the comment says so rather than
    // claiming a proof it does not have.
    let mut s = with_docs(&[(7, "same words"), (3, "same words"), (5, "same words")]);
    let first = ranked(&mut s, "docs", "same words");
    for _ in 0..5 {
        assert_eq!(ranked(&mut s, "docs", "same words"), first);
    }
    assert_eq!(first, [3, 5, 7], "ties should fall back to the key");
}

#[test]
fn ranking_does_not_change_which_documents_come_back() {
    // The set is what the scan agrees with; the order is extra.
    let docs = corpus(500);
    let mut s = loaded(&docs);
    for query in QUERIES {
        let mut fast = ranked(&mut s, "indexed", query);
        let slow = ids(&mut s, "plain", query);
        fast.sort_unstable();
        assert_eq!(fast, slow, "query {query:?} changed under ranking");
    }
}

// ---------------------------------------------------------------------------
// phrase search: what the positions were stored for
// ---------------------------------------------------------------------------

fn phrase_ids(s: &mut Session<MemStorage>, table: &str, query: &str) -> Vec<i64> {
    let statement = format!("get {table} {{ id }} where body phrase \"{query}\"");
    match s
        .execute(&statement)
        .unwrap_or_else(|e| panic!("{statement}: {e}"))
    {
        Output::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| match r[0] {
                Value::Int(n) => n,
                ref other => panic!("unexpected id {other:?}"),
            })
            .collect(),
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn a_phrase_wants_the_words_in_order_and_adjacent() {
    let mut s = with_docs(&[
        (1, "the quick brown fox"),
        (2, "the brown quick fox"),
        (3, "quick and then brown"),
        (4, "nothing here"),
        (5, "a quick brown thing"),
    ]);

    let mut hits = phrase_ids(&mut s, "docs", "quick brown");
    hits.sort_unstable();
    assert_eq!(hits, [1, 5], "order or adjacency was ignored");

    // the same words as a match are laxer, and that is the point
    let mut loose = ranked(&mut s, "docs", "quick brown");
    loose.sort_unstable();
    assert_eq!(loose, [1, 2, 3, 5]);
}

#[test]
fn the_index_and_the_scan_agree_about_phrases() {
    // The whole reason `phrase` is a plain binary operator: a column
    // without @text evaluates it row by row, so the brute force is the
    // same predicate rather than a second implementation.
    let docs = corpus(2000);
    let mut s = loaded(&docs);

    for query in [
        "w0 w1",
        "w1 w0",
        "w0 w0",
        "w5 w12 w3",
        "w900 w0",
        "nothinglikethis w0",
        "w0",
        "   ...   ",
    ] {
        let mut fast = phrase_ids(&mut s, "indexed", query);
        let mut slow = phrase_ids(&mut s, "plain", query);
        fast.sort_unstable();
        slow.sort_unstable();
        assert_eq!(fast, slow, "phrase {query:?} disagreed");
    }
}

#[test]
fn a_phrase_takes_the_index_and_says_so() {
    let mut s = with_docs(&[(1, "quick brown fox")]);
    match s
        .execute("explain get docs { id } where body phrase \"quick brown\"")
        .expect("explain")
    {
        Output::Lines(lines) => {
            let text = lines.join("\n");
            assert!(text.contains("text phrase"), "not the phrase path: {text}");
            assert!(text.contains("quick"), "{text}");
        }
        other => panic!("expected lines, got {other:?}"),
    }
}

#[test]
fn a_repeated_phrase_ranks_above_a_single_one() {
    // A phrase is scored as a term of its own, so two occurrences beat
    // one. Summing the words would rank these the other way round, since
    // the loser here holds more of each word apart.
    let mut s = with_docs(&[
        (1, "quick brown and quick and brown and quick and brown"),
        (2, "quick brown quick brown"),
    ]);
    assert_eq!(
        phrase_ids(&mut s, "docs", "quick brown"),
        [2, 1],
        "the document with the phrase twice should lead"
    );
}

#[test]
fn a_one_word_phrase_is_the_same_as_a_match() {
    let mut s = with_docs(&[(1, "alpha beta"), (2, "beta"), (3, "alpha alpha")]);
    let mut phrase = phrase_ids(&mut s, "docs", "alpha");
    let mut plain = ranked(&mut s, "docs", "alpha");
    phrase.sort_unstable();
    plain.sort_unstable();
    assert_eq!(phrase, plain);
}

#[test]
fn a_phrase_that_overruns_the_document_finds_nothing() {
    let mut s = with_docs(&[(1, "quick"), (2, "quick brown")]);
    assert_eq!(
        phrase_ids(&mut s, "docs", "quick brown fox"),
        Vec::<i64>::new()
    );
    assert_eq!(phrase_ids(&mut s, "docs", "quick brown"), [2]);
}
