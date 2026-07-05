//! Index consistency under random workloads.
//!
//! Hammers a table with random puts, sets and dels through the QQL layer,
//! then demands two things:
//!
//! 1. `verify_indexes` finds every index entry set exactly matching the
//!    rows (nothing missing, nothing stray)
//! 2. for every distinct value, the indexed query (which the planner turns
//!    into an IndexScan; asserted via explain) returns exactly the same
//!    rows as a full scan filtered in memory
//!
//! Sequence seeds print on failure. Tune with QUANTY_INDEX_SEQS.

use quanty_core::Db;
use quanty_exec::{verify_indexes, Output, Session};

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
}

fn rows(output: Output) -> Vec<Vec<quanty_core::Value>> {
    match output {
        Output::Rows(rows) => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn run_sequence(seed: u64) {
    let mut rng = Rng(seed);
    let mut session = Session::new(Db::in_memory().unwrap());
    session
        .execute("table w { id: int @key, tag: text @index, v: int @null @index, plain: int = 0 }")
        .unwrap_or_else(|e| panic!("seed {seed}: create: {e}"));

    let tags = ["a", "b", "c", "d"];
    let mut next_id = 0i64;
    let ops = 150 + rng.below(150);
    for _ in 0..ops {
        match rng.below(100) {
            // insert, sometimes with a null v
            0..=49 => {
                next_id += 1;
                let tag = tags[rng.below(4) as usize];
                let v = match rng.below(4) {
                    0 => "null".to_string(),
                    _ => (rng.below(5) as i64).to_string(),
                };
                session
                    .execute(&format!(
                        "put w {{ id: {next_id}, tag: \"{tag}\", v: {v}, plain: {} }}",
                        rng.below(100)
                    ))
                    .unwrap_or_else(|e| panic!("seed {seed}: put: {e}"));
            }
            // update an indexed column through a random access path
            50..=74 => {
                let tag = tags[rng.below(4) as usize];
                let new_tag = tags[rng.below(4) as usize];
                let new_v = match rng.below(4) {
                    0 => "null".to_string(),
                    _ => (rng.below(5) as i64).to_string(),
                };
                let filter = match rng.below(3) {
                    0 => format!("id = {}", 1 + rng.below(next_id.max(1) as u64)),
                    1 => format!("tag = \"{tag}\""),
                    _ => format!("v = {new_v}"),
                };
                session
                    .execute(&format!(
                        "set w where {filter} {{ tag = \"{new_tag}\", v = {new_v} }}"
                    ))
                    .unwrap_or_else(|e| panic!("seed {seed}: set: {e}"));
            }
            // delete through a random access path
            _ => {
                let filter = match rng.below(3) {
                    0 => format!("id = {}", 1 + rng.below(next_id.max(1) as u64)),
                    1 => format!("tag = \"{}\"", tags[rng.below(4) as usize]),
                    _ => format!("v = {}", rng.below(5)),
                };
                session
                    .execute(&format!("del w where {filter}"))
                    .unwrap_or_else(|e| panic!("seed {seed}: del: {e}"));
            }
        }
    }

    // 1. every index entry accounted for
    verify_indexes(&session).unwrap_or_else(|e| panic!("seed {seed}: {e}"));

    // 2. index scans agree with filtered full scans, and really are
    //    index scans according to the planner
    let all = rows(session.execute("get w").unwrap());
    for tag in tags {
        let plan = session
            .execute(&format!("explain get w where tag = \"{tag}\""))
            .unwrap();
        let plan = plan.render();
        assert!(
            plan.contains("IndexScan"),
            "seed {seed}: planner skipped the index:\n{plan}"
        );

        let indexed = rows(
            session
                .execute(&format!("get w where tag = \"{tag}\""))
                .unwrap(),
        );
        let filtered: Vec<_> = all
            .iter()
            .filter(|row| row[1] == quanty_core::Value::Text(tag.to_string()))
            .cloned()
            .collect();
        assert_eq!(
            indexed, filtered,
            "seed {seed}: index scan drifted for tag {tag}"
        );
    }
    for v in ["null", "0", "1", "2", "3", "4"] {
        let indexed = rows(session.execute(&format!("get w where v = {v}")).unwrap());
        let want: Vec<_> = all
            .iter()
            .filter(|row| match v {
                "null" => row[2] == quanty_core::Value::Null,
                _ => row[2] == quanty_core::Value::Int(v.parse().unwrap()),
            })
            .cloned()
            .collect();
        assert_eq!(indexed, want, "seed {seed}: index scan drifted for v = {v}");
    }
}

#[test]
fn indexes_stay_consistent_under_random_workloads() {
    let seqs: u64 = std::env::var("QUANTY_INDEX_SEQS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(60);
    for seq in 0..seqs {
        run_sequence(0xBEE5_0000_0000_0000 | seq);
    }
}

#[test]
fn verifier_actually_detects_damage() {
    // a checker that cannot fail checks nothing: damage an index entry on
    // purpose and demand a complaint
    let mut session = Session::new(Db::in_memory().unwrap());
    session
        .execute("table d { id: int @key, tag: text @index }")
        .unwrap();
    session.execute("put d { id: 1, tag: \"x\" }").unwrap();
    verify_indexes(&session).expect("clean state verifies");

    // reach under the hood: delete the row but not its index entry
    let db = session.db();
    let mut tx = db.begin();
    let keys: Vec<Vec<u8>> = tx.scan(None, None).unwrap().map(|r| r.unwrap().0).collect();
    // row keys and index keys both live in the data tree; the row is the
    // one whose stored value is non-empty
    let row_key = keys
        .iter()
        .find(|k| !tx.get(k).unwrap().unwrap().is_empty())
        .expect("row key")
        .clone();
    tx.delete(&row_key).unwrap();
    tx.commit().unwrap();

    let err = verify_indexes(&session).expect_err("stray entry must be found");
    assert!(err.to_string().contains("stray"), "got: {err}");
}
