//! Property-style model testing: the B-tree must behave exactly like
//! `std::collections::BTreeMap` under arbitrary operation sequences,
//! including commits, reopens from disk and historical snapshots.
//!
//! Hand rolled driver instead of a property testing crate to keep dev
//! dependencies light; every failure prints the sequence seed, so any run
//! reproduces exactly. Tune with QUANTY_MODEL_SEQS (sequences per test) and
//! QUANTY_MODEL_OPS (operations per sequence).

mod common;

use std::collections::BTreeMap;
use std::ops::Bound;

use quanty_core::{Db, FileStorage, MemStorage, PagerOptions, Storage};

type Model = BTreeMap<Vec<u8>, Vec<u8>>;

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

fn env_or(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// Keys come from a small structured space so collisions, overwrites and
/// shared prefixes happen constantly.
fn random_key(rng: &mut Rng) -> Vec<u8> {
    let prefix = [&b"a"[..], b"ab", b"b", b"\x00", b"\xff", b"user:", b""][rng.below(7) as usize];
    let mut key = prefix.to_vec();
    let extra = rng.below(6) as usize;
    for _ in 0..extra {
        key.push((rng.below(4) * 85) as u8); // 0x00, 0x55, 0xAA, 0xFF
    }
    key.push(rng.below(40) as u8 + 1);
    key
}

fn random_value(rng: &mut Rng, page_size: u32) -> Vec<u8> {
    let len = match rng.below(100) {
        0..=79 => rng.below(24) as usize,
        80..=94 => rng.below(200) as usize,
        // big enough for multi-page overflow chains
        _ => (page_size as usize * 2) + rng.below(page_size as u64) as usize,
    };
    let seed = rng.next();
    (0..len).map(|i| (seed as usize + i) as u8).collect()
}

fn model_range(
    model: &Model,
    start: &Option<Vec<u8>>,
    end: &Option<Vec<u8>>,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let lo = start
        .as_ref()
        .map_or(Bound::Unbounded, |s| Bound::Included(s.clone()));
    let hi = end
        .as_ref()
        .map_or(Bound::Unbounded, |e| Bound::Excluded(e.clone()));
    model
        .range((lo, hi))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// One randomized sequence against one database. `reopen` re-creates the Db
/// handle from persistent storage, or is skipped for in-memory runs.
fn run_sequence<S: Storage, F: FnMut() -> Db<S>>(
    seed: u64,
    ops: u64,
    mut reopen: Option<F>,
    db: Db<S>,
) {
    let mut rng = Rng(seed);
    let mut model: Model = BTreeMap::new();
    let mut history: Vec<(u64, Model)> = Vec::new();
    let mut db = db;
    let page_size = 512;

    let mut done = 0u64;
    while done < ops {
        // one transaction of 1..=16 operations
        let tx_ops = 1 + rng.below(16).min(ops - done);
        let mut tx = db.begin();
        for _ in 0..tx_ops {
            done += 1;
            match rng.below(100) {
                // put
                0..=54 => {
                    let key = random_key(&mut rng);
                    let value = random_value(&mut rng, page_size);
                    tx.put(&key, &value)
                        .unwrap_or_else(|e| panic!("seed {seed}: put: {e}"));
                    model.insert(key, value);
                }
                // delete, biased toward keys that exist
                55..=79 => {
                    let key = if !model.is_empty() && rng.below(2) == 0 {
                        let idx = rng.below(model.len() as u64) as usize;
                        model.keys().nth(idx).unwrap().clone()
                    } else {
                        random_key(&mut rng)
                    };
                    let existed = tx
                        .delete(&key)
                        .unwrap_or_else(|e| panic!("seed {seed}: delete: {e}"));
                    assert_eq!(
                        existed,
                        model.remove(&key).is_some(),
                        "seed {seed}: delete disagreement on {key:02x?}",
                    );
                }
                // point read
                80..=91 => {
                    let key = if !model.is_empty() && rng.below(2) == 0 {
                        let idx = rng.below(model.len() as u64) as usize;
                        model.keys().nth(idx).unwrap().clone()
                    } else {
                        random_key(&mut rng)
                    };
                    let got = tx
                        .get(&key)
                        .unwrap_or_else(|e| panic!("seed {seed}: get: {e}"));
                    assert_eq!(got.as_ref(), model.get(&key), "seed {seed}: get {key:02x?}");
                }
                // range scan inside the open transaction
                _ => {
                    let mut a = random_key(&mut rng);
                    let mut b = random_key(&mut rng);
                    if a > b {
                        std::mem::swap(&mut a, &mut b);
                    }
                    let (start, end) = match rng.below(4) {
                        0 => (None, None),
                        1 => (Some(a), None),
                        2 => (None, Some(b)),
                        _ => (Some(a), Some(b)),
                    };
                    let got: Vec<_> = tx
                        .scan(start.as_deref(), end.as_deref())
                        .unwrap_or_else(|e| panic!("seed {seed}: scan: {e}"))
                        .map(|r| r.unwrap_or_else(|e| panic!("seed {seed}: scan item: {e}")))
                        .collect();
                    assert_eq!(got, model_range(&model, &start, &end), "seed {seed}: scan");
                }
            }
        }
        let commit_id = tx
            .commit()
            .unwrap_or_else(|e| panic!("seed {seed}: commit: {e}"));

        // occasionally remember this exact state to check via snapshot_at
        if rng.below(10) == 0 && history.len() < 8 {
            history.push((commit_id, model.clone()));
        }

        // occasionally collect garbage; the head must be untouched (the
        // post-commit comparison below runs right after) and history
        // checks at the end must see retention respected exactly
        if rng.below(25) == 0 {
            let keep = 1 + rng.below(4) as usize;
            db.gc(keep)
                .unwrap_or_else(|e| panic!("seed {seed}: gc: {e}"));
        }

        // occasionally drop everything and reopen from disk
        if let Some(reopen) = reopen.as_mut() {
            if rng.below(8) == 0 {
                db = reopen();
            }
        }

        // full state comparison against the committed snapshot
        let snap = db.snapshot();
        let got: Vec<_> = snap
            .scan(None, None)
            .unwrap_or_else(|e| panic!("seed {seed}: post-commit scan: {e}"))
            .map(|r| r.unwrap_or_else(|e| panic!("seed {seed}: post-commit item: {e}")))
            .collect();
        assert_eq!(
            got,
            model_range(&model, &None, &None),
            "seed {seed}: state after commit"
        );
    }

    // retained snapshots must read exactly the state they were taken at;
    // snapshots that fell out of retention must say so, never lie
    let floor = db.log().unwrap().last().map(|c| c.commit_id).unwrap_or(0);
    for (commit_id, old_model) in &history {
        if *commit_id < floor {
            assert!(
                db.snapshot_at(*commit_id).is_err(),
                "seed {seed}: snapshot_at({commit_id}) below floor {floor} did not error",
            );
            continue;
        }
        let snap = db
            .snapshot_at(*commit_id)
            .unwrap_or_else(|e| panic!("seed {seed}: snapshot_at({commit_id}): {e}"));
        let got: Vec<_> = snap
            .scan(None, None)
            .unwrap()
            .map(|r| r.unwrap_or_else(|e| panic!("seed {seed}: history item: {e}")))
            .collect();
        assert_eq!(
            got,
            model_range(old_model, &None, &None),
            "seed {seed}: snapshot_at({commit_id}) drifted from history",
        );
    }
}

#[test]
fn model_in_memory() {
    let seqs = env_or("QUANTY_MODEL_SEQS", 200);
    let ops = env_or("QUANTY_MODEL_OPS", 300);
    for seq in 0..seqs {
        let seed = 0xC0FF_EE00_0000_0000 | seq;
        let db = Db::create(
            MemStorage::new(),
            PagerOptions {
                page_size: 512,
                cache_pages: 32,
            },
        )
        .unwrap();
        run_sequence::<MemStorage, fn() -> Db<MemStorage>>(seed, ops, None, db);
    }
}

#[test]
fn model_on_disk_with_reopens() {
    let seqs = env_or("QUANTY_MODEL_SEQS", 200).div_ceil(4);
    let ops = env_or("QUANTY_MODEL_OPS", 300);
    for seq in 0..seqs {
        let seed = 0xD15C_0000_0000_0000 | seq;
        let dir = common::TestDir::new();
        let path = dir.path().join("model.qdb");
        let db = Db::create(
            FileStorage::create(&path).unwrap(),
            PagerOptions {
                page_size: 512,
                cache_pages: 32,
            },
        )
        .unwrap();
        let reopen_path = path.clone();
        let reopen = move || {
            Db::open(
                FileStorage::open(&reopen_path).unwrap(),
                PagerOptions {
                    page_size: 512,
                    cache_pages: 32,
                },
            )
            .unwrap()
        };
        run_sequence(seed, ops, Some(reopen), db);
    }
}
