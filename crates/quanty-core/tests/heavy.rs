//! Heavy acceptance tests, ignored by default because they want a release
//! build to finish in sensible time. CI runs them with:
//!
//!     cargo test -p quanty-core --release --test heavy -- --ignored --nocapture

use std::time::Instant;

use quanty_core::{encode_key, Db, MemStorage, PagerOptions, Value};

#[test]
#[ignore = "heavy, run with --release --ignored"]
fn one_million_keys_bulk_load_and_full_scan() {
    const TOTAL: i64 = 1_000_000;
    const PER_COMMIT: i64 = 50_000;

    let db = Db::create(
        MemStorage::new(),
        PagerOptions {
            page_size: 4096,
            cache_pages: 4096,
        },
    )
    .unwrap();

    let started = Instant::now();
    let mut inserted = 0i64;
    while inserted < TOTAL {
        let mut tx = db.begin();
        for i in inserted..inserted + PER_COMMIT {
            let key = encode_key(&[Value::Int(i)]);
            tx.put(&key, &(i * 3).to_le_bytes()).unwrap();
        }
        tx.commit().unwrap();
        inserted += PER_COMMIT;
    }
    let load_time = started.elapsed();

    // full scan: exactly TOTAL entries, in order, values intact
    let started = Instant::now();
    let snap = db.snapshot();
    let mut expected = 0i64;
    for item in snap.scan(None, None).unwrap() {
        let (key, value) = item.unwrap();
        assert_eq!(key, encode_key(&[Value::Int(expected)]));
        assert_eq!(i64::from_le_bytes(value.try_into().unwrap()), expected * 3);
        expected += 1;
    }
    assert_eq!(expected, TOTAL);
    let scan_time = started.elapsed();

    // spot point reads across the range
    for i in [0i64, 1, 499_999, 999_999] {
        let got = snap.get(&encode_key(&[Value::Int(i)])).unwrap().unwrap();
        assert_eq!(i64::from_le_bytes(got.try_into().unwrap()), i * 3);
    }

    println!("bulk load of {TOTAL} keys: {load_time:.1?}, full scan: {scan_time:.1?}");
}
