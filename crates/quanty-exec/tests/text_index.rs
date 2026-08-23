//! The `@text` index: postings kept in step with the rows (ADR-036).
//!
//! Every case ends at `verify_indexes`, which rebuilds the expected
//! postings from the rows and compares keys and values. That is the same
//! tool phase 2 used for secondary indexes, which is the point: search
//! did not get a consistency story of its own.

use quanty_core::{Db, MemStorage, Value};
use quanty_exec::{verify_indexes, Output, Session};

fn session() -> Session<MemStorage> {
    let db = Db::in_memory().expect("open");
    let mut s = Session::new(db);
    s.execute("table docs { id: int @key, body: text @text }")
        .expect("define");
    s
}

fn put(s: &mut Session<MemStorage>, id: i64, body: &str) {
    s.execute(&format!("put docs {{ id: {id}, body: \"{body}\" }}"))
        .unwrap_or_else(|e| panic!("put {id}: {e}"));
}

fn ok(s: &Session<MemStorage>) {
    verify_indexes(s).unwrap_or_else(|e| panic!("{e}"));
}

/// Every entry under the text index, as key and value.
fn entries(s: &Session<MemStorage>, index_id: i64) -> Vec<(Vec<Value>, Vec<u8>)> {
    let db = s.db();
    let tx = db.begin();
    let prefix = quanty_core::encode_key(&[Value::Int(index_id)]);
    let mut end = prefix.clone();
    *end.last_mut().unwrap() += 1;
    let mut out = Vec::new();
    for item in tx.scan(Some(&prefix), Some(&end)).expect("scan") {
        let (key, value) = item.expect("entry");
        out.push((quanty_core::decode_key(&key).expect("key"), value));
    }
    out
}

/// The text index gets the id after the table's, since ids are handed out
/// in declaration order and this table has one text column.
const TEXT_ID: i64 = 2;

#[test]
fn a_document_becomes_postings_a_length_and_a_count() {
    let mut s = session();
    put(&mut s, 1, "the quick brown fox");
    ok(&s);

    let all = entries(&s, TEXT_ID);
    let terms: Vec<String> = all
        .iter()
        .filter_map(|(k, _)| match &k[1] {
            Value::Text(t) => Some(t.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(terms, ["brown", "fox", "quick", "the"], "postings wrong");

    // the integer namespace holds the length and the corpus counters,
    // and sorts ahead of every term
    let ints: Vec<i64> = all
        .iter()
        .filter_map(|(k, _)| match k[1] {
            Value::Int(n) => Some(n),
            _ => None,
        })
        .collect();
    assert_eq!(
        ints,
        [0, 1],
        "length and corpus entries missing or out of order"
    );
}

#[test]
fn a_repeated_word_keeps_one_posting_with_every_position() {
    let mut s = session();
    put(&mut s, 1, "one two one three one");
    ok(&s);

    let the_one = entries(&s, TEXT_ID)
        .into_iter()
        .find(|(k, _)| matches!(&k[1], Value::Text(t) if t == "one"))
        .expect("posting for 'one'");
    assert_eq!(
        quanty_exec::decode_positions(&the_one.1),
        Some(vec![0, 2, 4]),
        "term frequency and positions disagree"
    );
}

#[test]
fn deleting_a_row_takes_its_postings_with_it() {
    let mut s = session();
    put(&mut s, 1, "alpha beta");
    put(&mut s, 2, "beta gamma");
    ok(&s);

    s.execute("del docs where id = 1").expect("del");
    ok(&s);

    let terms: Vec<String> = entries(&s, TEXT_ID)
        .into_iter()
        .filter_map(|(k, _)| match &k[1] {
            Value::Text(t) => Some(t.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(terms, ["beta", "gamma"], "alpha outlived its row");
}

#[test]
fn overwriting_replaces_the_postings_rather_than_adding_to_them() {
    let mut s = session();
    put(&mut s, 1, "before words");
    ok(&s);

    s.execute("set docs where id = 1 { body = \"after text\" }")
        .expect("set");
    ok(&s);

    let terms: Vec<String> = entries(&s, TEXT_ID)
        .into_iter()
        .filter_map(|(k, _)| match &k[1] {
            Value::Text(t) => Some(t.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(terms, ["after", "text"]);
}

#[test]
fn the_corpus_counters_track_the_documents() {
    let mut s = session();
    let corpus = |s: &Session<MemStorage>| -> Option<(u64, u64)> {
        entries(s, TEXT_ID)
            .into_iter()
            .find(|(k, _)| matches!(k[1], Value::Int(1)))
            .map(|(_, v)| {
                (
                    u64::from_le_bytes(v[..8].try_into().unwrap()),
                    u64::from_le_bytes(v[8..].try_into().unwrap()),
                )
            })
    };

    assert_eq!(corpus(&s), None, "an empty index should hold no counters");

    put(&mut s, 1, "one two three");
    put(&mut s, 2, "four five");
    ok(&s);
    assert_eq!(corpus(&s), Some((2, 5)), "docs and total length");

    s.execute("del docs where id = 2").expect("del");
    ok(&s);
    assert_eq!(corpus(&s), Some((1, 3)));

    s.execute("del docs where id = 1").expect("del");
    ok(&s);
    assert_eq!(corpus(&s), None, "the last document left counters behind");
}

#[test]
fn a_null_document_is_not_a_document() {
    let mut s = Session::new(Db::in_memory().unwrap());
    s.execute("table docs { id: int @key, body: text @text @null }")
        .expect("define");
    s.execute("put docs { id: 1, body: null }").expect("put");
    s.execute("put docs { id: 2, body: \"has words\" }")
        .expect("put");
    ok(&s);

    let ints: Vec<i64> = entries(&s, TEXT_ID)
        .into_iter()
        .filter_map(|(k, _)| match k[1] {
            Value::Int(n) => Some(n),
            _ => None,
        })
        .collect();
    // one length entry and one corpus entry: the null row contributed
    // neither, so the average is over documents that exist
    assert_eq!(ints, [0, 1]);
}

#[test]
fn dropping_the_table_takes_the_whole_index() {
    let mut s = session();
    put(&mut s, 1, "some words here");
    ok(&s);

    s.execute("drop table docs").expect("drop");
    ok(&s);
    assert!(
        entries(&s, TEXT_ID).is_empty(),
        "the index outlived its table"
    );
}

#[test]
fn text_is_refused_on_a_column_that_is_not_text() {
    let mut s = Session::new(Db::in_memory().unwrap());
    let err = s
        .execute("table t { id: int @key @text }")
        .expect_err("@text on an int");
    assert!(err.to_string().contains("@text"), "{err}");
}

#[test]
fn a_random_workload_leaves_the_index_verifiable() {
    // The same shape phase 2 used for secondary indexes: put, overwrite
    // and delete in a jumble, then rebuild the expected postings from the
    // rows and compare.
    let words = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta"];
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = move || {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 33) as usize
    };

    let mut s = session();
    for step in 0..300 {
        let id = (next() % 40) as i64;
        match next() % 3 {
            0 => {
                let body: Vec<&str> = (0..1 + next() % 5).map(|_| words[next() % 6]).collect();
                let _ = s.execute(&format!(
                    "put docs {{ id: {id}, body: \"{}\" }}",
                    body.join(" ")
                ));
            }
            1 => {
                let body: Vec<&str> = (0..1 + next() % 4).map(|_| words[next() % 6]).collect();
                let _ = s.execute(&format!(
                    "set docs where id = {id} {{ body = \"{}\" }}",
                    body.join(" ")
                ));
            }
            _ => {
                let _ = s.execute(&format!("del docs where id = {id}"));
            }
        }
        if step % 25 == 0 {
            verify_indexes(&s).unwrap_or_else(|e| panic!("step {step}: {e}"));
        }
    }
    verify_indexes(&s).unwrap_or_else(|e| panic!("final: {e}"));

    let out = s.execute("get docs { id }").expect("get");
    assert!(matches!(out, Output::Rows { .. } | Output::Lines(_)));
}
