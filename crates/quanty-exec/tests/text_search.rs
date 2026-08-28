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

#[test]
fn the_rest_of_the_condition_still_applies_to_a_ranked_search() {
    // This was wrong for one commit: the ranked path called the text
    // access directly and skipped the filter that fetch_rows applies for
    // every other access, so the extra condition was silently ignored.
    // Nothing caught it because no test asked a match anything else.
    //
    // Proved to catch: removing the residual block in the ranked arm
    // returns all five rows here.
    let mut s = with_docs(&[
        (1, "alpha"),
        (2, "alpha"),
        (3, "alpha"),
        (4, "alpha"),
        (5, "alpha"),
    ]);

    let ranked_ids = ranked(&mut s, "docs", "alpha");
    assert_eq!(ranked_ids.len(), 5, "the plain query should see all five");

    let filtered = match s
        .execute("get docs { id } where body match \"alpha\" and id > 3")
        .expect("filtered")
    {
        Output::Rows { rows, .. } => rows.len(),
        other => panic!("expected rows, got {other:?}"),
    };
    assert_eq!(filtered, 2, "the residual predicate was ignored");

    // and the same query with an explicit order, which takes the other
    // path, has to agree
    let ordered = match s
        .execute("get docs { id } where body match \"alpha\" and id > 3 order by id asc")
        .expect("ordered")
    {
        Output::Rows { rows, .. } => rows.len(),
        other => panic!("expected rows, got {other:?}"),
    };
    assert_eq!(ordered, filtered, "the two paths disagree");
}

#[test]
fn a_phrase_with_a_second_condition_filters_too() {
    let mut s = with_docs(&[
        (1, "quick brown fox"),
        (2, "quick brown dog"),
        (3, "brown quick bird"),
    ]);
    let out = match s
        .execute("get docs { id } where body phrase \"quick brown\" and id > 1")
        .expect("filtered phrase")
    {
        Output::Rows { rows, .. } => rows.len(),
        other => panic!("expected rows, got {other:?}"),
    };
    assert_eq!(out, 1, "phrase and filter did not combine");
}

// ---------------------------------------------------------------------------
// top-k: a limit that stops before the rows are read
// ---------------------------------------------------------------------------

fn limited(s: &mut Session<MemStorage>, query: &str, tail: &str) -> Vec<i64> {
    let statement = format!("get indexed {{ id }} where body match \"{query}\"{tail}");
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
fn a_limit_returns_the_same_rows_the_full_answer_starts_with() {
    // The point of pushing the limit into the access is that it must not
    // change the answer, only how much work reaching it costs.
    let docs = corpus(3000);
    let mut s = loaded(&docs);

    for query in ["w0", "w7", "w50", "w900", "w120 w7"] {
        let full = limited(&mut s, query, "");
        for n in [1usize, 3, 10, 50] {
            let short = limited(&mut s, query, &format!(" limit {n}"));
            let expected: Vec<i64> = full.iter().copied().take(n).collect();
            assert_eq!(short, expected, "query {query:?} limit {n}");
        }
    }
}

#[test]
fn a_limit_larger_than_the_answer_returns_the_answer() {
    let mut s = with_docs(&[(1, "alpha"), (2, "alpha"), (3, "beta")]);
    let all = match s
        .execute("get docs { id } where body match \"alpha\" limit 99")
        .expect("limit")
    {
        Output::Rows { rows, .. } => rows.len(),
        other => panic!("expected rows, got {other:?}"),
    };
    assert_eq!(all, 2);
}

#[test]
fn a_limit_with_a_second_condition_still_fills_up() {
    // The limit cannot travel into the access when a filter after it can
    // drop rows, or `limit 10` would answer with fewer than ten. Half the
    // documents fail the filter here, so truncating first would return
    // five.
    //
    // Proved to catch: passing get.limit down regardless of the residual
    // returns 5 instead of 10.
    let docs: Vec<(i64, &str)> = (1..=40).map(|i| (i, "alpha")).collect();
    let mut s = with_docs(&docs);

    let got = match s
        .execute("get docs { id } where body match \"alpha\" and id > 20 limit 10")
        .expect("filtered limit")
    {
        Output::Rows { rows, .. } => rows.len(),
        other => panic!("expected rows, got {other:?}"),
    };
    assert_eq!(got, 10, "the limit was applied before the filter");
}

#[test]
fn a_limit_on_a_phrase_keeps_the_best_ones() {
    let mut s = with_docs(&[
        (1, "quick brown"),
        (2, "quick brown quick brown"),
        (3, "quick brown quick brown quick brown"),
        (4, "brown quick"),
    ]);
    assert_eq!(phrase_ids(&mut s, "docs", "quick brown"), [3, 2, 1]);

    let two = match s
        .execute("get docs { id } where body phrase \"quick brown\" limit 2")
        .expect("limit")
    {
        Output::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| match r[0] {
                Value::Int(n) => n,
                _ => unreachable!(),
            })
            .collect::<Vec<i64>>(),
        other => panic!("expected rows, got {other:?}"),
    };
    assert_eq!(two, [3, 2], "the limit did not keep the best");
}

// ---------------------------------------------------------------------------
// combining with the rest of the language
// ---------------------------------------------------------------------------

fn rows_of(s: &mut Session<MemStorage>, statement: &str) -> Vec<i64> {
    match s
        .execute(statement)
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

#[test]
fn or_and_not_work_over_match_because_it_is_an_ordinary_operator() {
    // Untested until now, and working by construction rather than by
    // design: match is a binary operator, so the expression evaluator
    // handles or, and and not without search knowing about them.
    let mut s = with_docs(&[
        (1, "alpha"),
        (2, "beta"),
        (3, "alpha beta"),
        (4, "gamma"),
        (5, "alpha gamma"),
    ]);

    assert_eq!(
        rows_of(
            &mut s,
            "get docs { id } where body match \"alpha\" or body match \"beta\""
        ),
        [1, 2, 3, 5]
    );
    assert_eq!(
        rows_of(
            &mut s,
            "get docs { id } where body match \"alpha\" and not body match \"beta\""
        ),
        [1, 5]
    );
    assert_eq!(
        rows_of(&mut s, "get docs { id } where not body match \"alpha\""),
        [2, 4]
    );
    assert_eq!(
        rows_of(
            &mut s,
            "get docs { id } where body phrase \"alpha beta\" or body match \"gamma\""
        ),
        [3, 4, 5]
    );
}

#[test]
fn a_disjunction_reads_the_index_as_a_union() {
    let mut s = with_docs(&[(1, "alpha"), (2, "beta")]);
    match s
        .execute("explain get docs { id } where body match \"alpha\" or body match \"beta\"")
        .expect("explain")
    {
        Output::Lines(lines) => {
            let text = lines.join("\n");
            assert!(text.contains("text union"), "not a union: {text}");
            assert!(text.contains("[alpha] or [beta]"), "{text}");
            assert!(!text.contains("SeqScan"), "still scanning: {text}");
        }
        other => panic!("expected lines, got {other:?}"),
    }
}

#[test]
fn a_disjunction_over_two_columns_falls_back_to_a_scan() {
    // One access reads one index, so a mixture has to scan. Answering it
    // correctly and slowly beats answering it fast and wrong.
    let db = Db::in_memory().expect("open");
    let mut s = Session::new(db);
    s.execute("table docs { id: int @key, body: text @text, note: text @text }")
        .expect("table");
    s.execute("put docs { id: 1, body: \"alpha\", note: \"beta\" }")
        .expect("put");

    match s
        .execute("explain get docs { id } where body match \"alpha\" or note match \"beta\"")
        .expect("explain")
    {
        Output::Lines(lines) => {
            let text = lines.join("\n");
            assert!(text.contains("SeqScan"), "expected a scan: {text}");
        }
        other => panic!("expected lines, got {other:?}"),
    }
    assert_eq!(
        rows_of(
            &mut s,
            "get docs { id } where body match \"alpha\" or note match \"beta\""
        ),
        [1],
        "the scan answered wrongly"
    );
}

#[test]
fn a_union_agrees_with_the_scan() {
    let docs = corpus(2000);
    let mut s = loaded(&docs);
    for (a, b) in [
        ("w0", "w1"),
        ("w900", "w4000"),
        ("w900", "nothinglikethis"),
        ("nothinglikethis", "alsonothing"),
        ("w5 w12", "w3"),
    ] {
        let fast = rows_of(
            &mut s,
            &format!("get indexed {{ id }} where body match \"{a}\" or body match \"{b}\""),
        );
        let slow = rows_of(
            &mut s,
            &format!("get plain {{ id }} where body match \"{a}\" or body match \"{b}\""),
        );
        assert_eq!(fast, slow, "union of {a:?} and {b:?} disagreed");
    }
}

#[test]
fn a_union_ranks_a_document_holding_both_above_one_holding_either() {
    // Scoring sums over the query terms a document actually holds, which
    // is the classic answer to what `or` should rank higher.
    let mut s = with_docs(&[(1, "alpha filler"), (2, "alpha beta"), (3, "beta filler")]);
    let statement = "get docs { id } where body match \"alpha\" or body match \"beta\"";
    match s.execute(statement).expect("union") {
        Output::Rows { rows, .. } => {
            let ids: Vec<i64> = rows
                .into_iter()
                .map(|r| match r[0] {
                    Value::Int(n) => n,
                    _ => unreachable!(),
                })
                .collect();
            assert_eq!(ids[0], 2, "the document with both should lead: {ids:?}");
            assert_eq!(ids.len(), 3);
        }
        other => panic!("expected rows, got {other:?}"),
    }
}

#[test]
fn a_document_answering_both_sides_is_returned_once() {
    let mut s = with_docs(&[(1, "alpha beta"), (2, "alpha")]);
    assert_eq!(
        rows_of(
            &mut s,
            "get docs { id } where body match \"alpha\" or body match \"beta\""
        ),
        [1, 2],
        "a document was counted twice or lost"
    );
}

#[test]
fn a_union_mixing_a_phrase_and_a_match_still_agrees_with_the_scan() {
    let mut s = with_docs(&[
        (1, "quick brown fox"),
        (2, "brown quick fox"),
        (3, "gamma"),
        (4, "quick gamma brown"),
    ]);
    let q = "get docs { id } where body phrase \"quick brown\" or body match \"gamma\"";
    assert_eq!(rows_of(&mut s, q), [1, 3, 4]);
}

#[test]
fn an_empty_query_in_a_disjunction_falls_back_to_a_scan() {
    // `match ""` matches everything, so the union does too and the index
    // has nothing to narrow with.
    let mut s = with_docs(&[(1, "alpha"), (2, "beta")]);
    match s
        .execute("explain get docs { id } where body match \"alpha\" or body match \"  \"")
        .expect("explain")
    {
        Output::Lines(lines) => {
            let text = lines.join("\n");
            assert!(text.contains("SeqScan"), "expected a scan: {text}");
        }
        other => panic!("expected lines, got {other:?}"),
    }
    assert_eq!(
        rows_of(
            &mut s,
            "get docs { id } where body match \"alpha\" or body match \"  \""
        ),
        [1, 2]
    );
}

#[test]
fn a_null_document_matches_nothing_and_is_not_matched_by_not_either() {
    // A null is not a document, so it holds no words. `not match` on it
    // is the interesting half: it says true, because the row does not
    // contain the word.
    let db = Db::in_memory().expect("open");
    let mut s = Session::new(db);
    s.execute("table docs { id: int @key, body: text @text @null }")
        .expect("table");
    s.execute("put docs { id: 1, body: null }").expect("put");
    s.execute("put docs { id: 2, body: \"alpha\" }")
        .expect("put");

    assert_eq!(
        rows_of(&mut s, "get docs { id } where body match \"alpha\""),
        [2]
    );
    assert_eq!(
        rows_of(&mut s, "get docs { id } where not body match \"alpha\""),
        [1]
    );
}

// ---------------------------------------------------------------------------
// prefix terms
// ---------------------------------------------------------------------------

#[test]
fn a_trailing_star_matches_every_word_that_starts_with_it() {
    let mut s = with_docs(&[
        (1, "quick"),
        (2, "quickly"),
        (3, "quicksand and quicker"),
        (4, "quiet"),
        (5, "unquick"),
    ]);
    assert_eq!(
        rows_of(&mut s, "get docs { id } where body match \"quick*\""),
        [1, 2, 3]
    );
    assert_eq!(
        rows_of(&mut s, "get docs { id } where body match \"quick\""),
        [1]
    );
    assert_eq!(
        rows_of(&mut s, "get docs { id } where body match \"qui*\""),
        [1, 2, 3, 4]
    );
}

#[test]
fn a_prefix_agrees_with_the_scan() {
    let docs = corpus(2000);
    let mut s = loaded(&docs);
    for query in ["w1*", "w10*", "w4*", "w999*", "zzz*", "w1* w2*", "w1* w20"] {
        let fast = rows_of(
            &mut s,
            &format!("get indexed {{ id }} where body match \"{query}\""),
        );
        let slow = rows_of(
            &mut s,
            &format!("get plain {{ id }} where body match \"{query}\""),
        );
        assert_eq!(fast, slow, "prefix {query:?} disagreed");
    }
}

#[test]
fn a_prefix_takes_the_index_and_the_plan_shows_the_star() {
    let mut s = with_docs(&[(1, "quick")]);
    match s
        .execute("explain get docs { id } where body match \"quick*\"")
        .expect("explain")
    {
        Output::Lines(lines) => {
            let text = lines.join("\n");
            assert!(text.contains("text match"), "not the index: {text}");
            assert!(text.contains("quick*"), "the star is invisible: {text}");
        }
        other => panic!("expected lines, got {other:?}"),
    }
}

#[test]
fn a_document_reached_by_two_words_of_one_prefix_is_returned_once() {
    // quicksand and quicker both answer `quick*` in document 3, and one
    // document is one answer. Its frequency is the sum, so it outranks a
    // document reached by one word.
    let mut s = with_docs(&[(1, "quicksand"), (2, "quicksand quicker quickly")]);
    let ids = match s
        .execute("get docs { id } where body match \"quick*\"")
        .expect("prefix")
    {
        Output::Rows { rows, .. } => rows
            .into_iter()
            .map(|r| match r[0] {
                Value::Int(n) => n,
                _ => unreachable!(),
            })
            .collect::<Vec<i64>>(),
        other => panic!("expected rows, got {other:?}"),
    };
    assert_eq!(ids, [2, 1], "counted twice, lost, or ranked the wrong way");
}

#[test]
fn a_star_only_counts_at_the_end_of_a_word() {
    // Anywhere else it is a separator like any other punctuation, which
    // is what the tokenizer has always done with it. Document 3 is what
    // makes this test say anything: reading `qu*ick` as `qu` and `ick*`
    // would match it, because icky starts with ick, and reading the star
    // as a separator does not.
    //
    // Proved to catch: marking a chunk as a prefix when it merely
    // contains a star returns 2 and 3 here instead of 2.
    let mut s = with_docs(&[(1, "quick brown"), (2, "qu ick"), (3, "qu icky")]);
    assert_eq!(
        rows_of(&mut s, "get docs { id } where body match \"qu*ick\""),
        [2]
    );
    assert_eq!(
        rows_of(&mut s, "get docs { id } where body match \"quick*\""),
        [1]
    );
}

#[test]
fn a_star_alone_asks_for_nothing_and_so_matches_everything() {
    let mut s = with_docs(&[(1, "alpha"), (2, "beta")]);
    assert_eq!(
        rows_of(&mut s, "get docs { id } where body match \"*\""),
        [1, 2]
    );
}

#[test]
fn a_phrase_refuses_a_prefix_rather_than_ignoring_it() {
    // Treating the star as punctuation would answer `phrase "quick
    // brown*"` with the exact phrase, which is a wrong answer nobody
    // asked for. Both paths refuse, so the index and the scan agree about
    // the refusal too.
    let mut s = with_docs(&[(1, "quick brown fox")]);
    let err = s
        .execute("get docs { id } where body phrase \"quick brown*\"")
        .expect_err("a phrase with a star");
    assert!(err.to_string().contains("no prefix terms"), "{err}");

    let db = Db::in_memory().expect("open");
    let mut plain = Session::new(db);
    plain
        .execute("table docs { id: int @key, body: text }")
        .expect("table");
    plain
        .execute("put docs { id: 1, body: \"quick brown fox\" }")
        .expect("put");
    let err = plain
        .execute("get docs { id } where body phrase \"quick brown*\"")
        .expect_err("the scan refuses too");
    assert!(err.to_string().contains("no prefix terms"), "{err}");
}

#[test]
fn a_prefix_works_inside_a_union_too() {
    let mut s = with_docs(&[(1, "quicksand"), (2, "gamma"), (3, "quiet"), (4, "delta")]);
    assert_eq!(
        rows_of(
            &mut s,
            "get docs { id } where body match \"quick*\" or body match \"gamma\""
        ),
        [1, 2]
    );
}
