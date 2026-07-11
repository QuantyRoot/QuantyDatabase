//! Joins against a brute force reference, on randomized workloads.
//!
//! Two claims, checked on every iteration with a fresh random dataset:
//!
//! 1. Strategy equivalence. The same rows go into three structurally
//!    different right tables: one where the join column is the primary key
//!    (key probe), one where it carries a secondary index (index probe)
//!    and one where it is neither (nested loop). The same logical query
//!    must return the same multiset of rows on all three. `explain` is
//!    asserted per shape, so this really compares three strategies and
//!    not three spellings of one.
//!
//! 2. Absolute correctness. Every result must equal a triple-checked
//!    reference join written as the obvious double loop right here, with
//!    the engine's null rules (null = null holds, null against a value
//!    does not, unmatched left rows pad with nulls on a left join).
//!
//! Multisets, not sequences: join output order is an implementation
//! detail unless `order by` says otherwise, so rows are compared sorted.
//! Budget via QUANTY_JOIN_ITERS (default 150).

use quanty_core::{Db, Value};
use quanty_exec::{Output, Session};
use quanty_ql::ast::JoinKind;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    /// Small int or null, the domain that makes collisions and misses
    /// both likely.
    fn small(&mut self, nulls: bool) -> Value {
        if nulls && self.below(4) == 0 {
            Value::Null
        } else {
            Value::Int(self.below(6) as i64)
        }
    }

    fn tag(&mut self) -> Value {
        match self.below(4) {
            0 => Value::Null,
            1 => Value::Text("x".into()),
            2 => Value::Text("y".into()),
            _ => Value::Text("z".into()),
        }
    }
}

/// The engine's `=`: null is a plain value.
fn ref_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Null, _) | (_, Value::Null) => false,
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Text(x), Value::Text(y)) => x == y,
        _ => unreachable!("the generator only compares like with like"),
    }
}

/// The reference: the obvious loop. `right_width` is passed rather than
/// read off `right`, so left padding is correct even when `right` is empty
/// (an empty table still has a known column count).
fn reference_join(
    left: &[Vec<Value>],
    right: &[Vec<Value>],
    right_width: usize,
    kind: JoinKind,
    on: impl Fn(&[Value], &[Value]) -> bool,
) -> Vec<Vec<Value>> {
    let mut out = Vec::new();
    for l in left {
        let mut matched = false;
        for r in right {
            if on(l, r) {
                let mut row = l.clone();
                row.extend(r.iter().cloned());
                out.push(row);
                matched = true;
            }
        }
        if kind == JoinKind::Left && !matched {
            let mut row = l.clone();
            row.resize(row.len() + right_width, Value::Null);
            out.push(row);
        }
    }
    out
}

fn rows_of(output: Output) -> Vec<Vec<Value>> {
    match output {
        Output::Rows(rows) => rows,
        other => panic!("expected rows, got {:?}", other.render()),
    }
}

/// Render and sort, for multiset comparison with readable failures.
fn canon(mut rows: Vec<Vec<Value>>) -> Vec<String> {
    let mut lines: Vec<String> = rows
        .drain(..)
        .map(|r| Output::Rows(vec![r]).render())
        .collect();
    lines.sort();
    lines
}

fn exec(session: &mut Session<impl quanty_core::Storage>, q: &str) -> Output {
    session
        .execute(q)
        .unwrap_or_else(|e| panic!("query failed: {q}\n{e}"))
}

#[test]
fn joins_match_the_brute_force_reference() {
    let iters: u64 = std::env::var("QUANTY_JOIN_ITERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(150);
    let seed = std::env::var("QUANTY_JOIN_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64
                | 1
        });
    let mut rng = Rng(seed);

    for iter in 0..iters {
        let mut session = Session::new(Db::in_memory().expect("in-memory db"));

        // left table: plain, sequential key
        exec(
            &mut session,
            "table l { id: int @key, a: int @null, t: text @null }",
        );
        // three right shapes carrying identical logical rows:
        // r_key joins on its primary key, r_idx on an indexed column,
        // r_seq on a bare column
        exec(&mut session, "table r_key { j: int @key, tag: text @null }");
        exec(
            &mut session,
            "table r_idx { id: int @key, j: int @null @index, tag: text @null }",
        );
        exec(
            &mut session,
            "table r_seq { id: int @key, j: int @null, tag: text @null }",
        );

        let n_left = 1 + rng.below(8);
        let mut left_rows = Vec::new();
        for id in 0..n_left {
            let a = rng.small(true);
            let t = rng.tag();
            exec(
                &mut session,
                &format!("put l {{ id: {id}, a: {}, t: {} }}", lit(&a), lit(&t)),
            );
            left_rows.push(vec![Value::Int(id as i64), a, t]);
        }

        // r_key's key cannot be null and must be unique, so its rows are a
        // dedup of the non-null j values; the other shapes carry all rows
        let n_right = rng.below(8);
        let mut right_rows = Vec::new();
        for id in 0..n_right {
            let j = rng.small(true);
            let tag = rng.tag();
            exec(
                &mut session,
                &format!(
                    "put r_idx {{ id: {id}, j: {}, tag: {} }}",
                    lit(&j),
                    lit(&tag)
                ),
            );
            exec(
                &mut session,
                &format!(
                    "put r_seq {{ id: {id}, j: {}, tag: {} }}",
                    lit(&j),
                    lit(&tag)
                ),
            );
            right_rows.push(vec![Value::Int(id as i64), j.clone(), tag.clone()]);
        }
        let mut key_rows = Vec::new();
        for row in &right_rows {
            if let Value::Int(j) = &row[1] {
                if key_rows.iter().all(|r: &Vec<Value>| r[0] != row[1]) {
                    exec(
                        &mut session,
                        &format!("put r_key {{ j: {j}, tag: {} }}", lit(&row[2])),
                    );
                    key_rows.push(vec![row[1].clone(), row[2].clone()]);
                }
            }
        }

        let kind = if rng.below(2) == 0 {
            JoinKind::Inner
        } else {
            JoinKind::Left
        };
        let kw = match kind {
            JoinKind::Inner => "join",
            JoinKind::Left => "left join",
        };
        // sometimes a residual conjunct that the probe cannot consume
        let residual = rng.below(2) == 0;
        let on_tail = if residual { " and l.t = {R}.tag" } else { "" };

        for (right_name, right_data, right_width, want_strategy) in [
            ("r_key", &key_rows, 2usize, "KeyProbe r_key"),
            ("r_idx", &right_rows, 3usize, "IndexProbe r_idx"),
            ("r_seq", &right_rows, 3usize, "SeqScan r_seq"),
        ] {
            let on = format!("l.a = {right_name}.j{}", on_tail.replace("{R}", right_name));
            let q = format!("get l {kw} {right_name} on {on}");

            // the plan must actually use the strategy under test
            let explain = exec(&mut session, &format!("explain {q}")).render();
            assert!(
                explain.contains(want_strategy),
                "iter {iter} seed {seed}: wrong strategy\nquery: {q}\nplan:\n{explain}"
            );

            // in every right shape the join column is second to last and
            // tag is last: r_key is (j, tag), r_idx and r_seq are (id, j, tag)
            let j_at = right_width - 2;
            let tag_at = right_width - 1;
            let got = canon(rows_of(exec(&mut session, &q)));
            let expected = canon(reference_join(
                &left_rows,
                right_data,
                right_width,
                kind,
                |l, r| {
                    let base = ref_eq(&l[1], &r[j_at]);
                    if residual {
                        base && ref_eq(&l[2], &r[tag_at])
                    } else {
                        base
                    }
                },
            ));
            assert_eq!(
                got, expected,
                "iter {iter} seed {seed}: results differ from the reference\nquery: {q}"
            );
        }

        // a chained three-table join every few iterations, nested loop on
        // the second hop, against a triple loop reference
        if iter % 4 == 0 {
            let q =
                format!("get l {kw} r_idx on l.a = r_idx.j {kw} r_seq on r_idx.tag = r_seq.tag");
            // r_idx and r_seq are both width 3
            let first = reference_join(&left_rows, &right_rows, 3, kind, |l, r| {
                ref_eq(&l[1], &r[1])
            });
            let expected = canon(reference_join(&first, &right_rows, 3, kind, |l, r| {
                ref_eq(&l[5], &r[2])
            }));
            let got = canon(rows_of(exec(&mut session, &q)));
            assert_eq!(
                got, expected,
                "iter {iter} seed {seed}: three-table join differs\nquery: {q}"
            );
        }
    }
    println!("join model: {iters} iterations (seed {seed})");
}

fn lit(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Int(i) => i.to_string(),
        Value::Text(t) => format!("\"{t}\""),
        _ => unreachable!("the generator only makes ints, texts and nulls"),
    }
}
