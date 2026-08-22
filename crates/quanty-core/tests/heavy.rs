//! Heavy acceptance tests, ignored by default because they want a release
//! build to finish in sensible time. CI runs them with:
//!
//!     cargo test -p quanty-core --release --test heavy -- --ignored --nocapture

use std::time::Instant;

use quanty_core::{encode_key, Db, MemStorage, PagerOptions, Value};

mod common;
use common::TestDir;

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

// ---------------------------------------------------------------------------
// phase 6: a gigabyte in and out without holding it (ADR-033)
// ---------------------------------------------------------------------------

/// A deterministic stream that does not repeat.
///
/// Repeating bytes would dedup down to one chunk and the test would pass
/// having stored a megabyte, which is the opposite of what it claims.
struct Noise {
    remaining: usize,
    state: u64,
}

impl Noise {
    fn new(len: usize) -> Self {
        Noise {
            remaining: len,
            state: 0x2545_f491_4f6c_dd1d,
        }
    }

    fn next_byte(&mut self) -> u8 {
        self.state = self
            .state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.state >> 33) as u8
    }
}

impl std::io::Read for Noise {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = buf.len().min(self.remaining);
        for slot in &mut buf[..n] {
            *slot = self.next_byte();
        }
        self.remaining -= n;
        Ok(n)
    }
}

/// Checks what comes back against a fresh stream, holding one chunk.
///
/// Collecting the blob into a Vec would need a gigabyte on the read side
/// and would prove nothing about the read side.
struct Verify {
    expect: Noise,
    seen: u64,
    scratch: Vec<u8>,
}

impl std::io::Write for Verify {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.scratch.clear();
        self.scratch.resize(buf.len(), 0);
        for slot in self.scratch.iter_mut() {
            *slot = self.expect.next_byte();
        }
        assert_eq!(buf, &self.scratch[..], "the bytes came back different");
        self.seen += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn rss_mib() -> u64 {
    let text = std::fs::read_to_string("/proc/self/statm").expect("statm");
    let pages: u64 = text
        .split_whitespace()
        .nth(1)
        .expect("resident field")
        .parse()
        .expect("a number");
    pages * 4096 / (1024 * 1024)
}

/// Watch resident memory from another thread, because the work being
/// measured never yields to let the measurement happen.
struct Watcher {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    peak: std::sync::Arc<std::sync::atomic::AtomicU64>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Watcher {
    fn start() -> Self {
        use std::sync::atomic::Ordering;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let peak = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let (s, p) = (stop.clone(), peak.clone());
        let handle = std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                p.fetch_max(rss_mib(), Ordering::Relaxed);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            p.fetch_max(rss_mib(), Ordering::Relaxed);
        });
        Watcher {
            stop,
            peak,
            handle: Some(handle),
        }
    }

    fn finish(mut self) -> u64 {
        use std::sync::atomic::Ordering;
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        self.peak.load(Ordering::Relaxed)
    }
}

#[test]
#[ignore = "heavy, run with --release --ignored"]
fn a_gigabyte_goes_in_and_out_without_being_held() {
    const GIB: usize = 1 << 30;

    let dir = TestDir::new();
    let path = dir.path().join("asset.qdb");
    let db = Db::create_file(&path).unwrap();

    let baseline = rss_mib();

    let watch = Watcher::start();
    let start = Instant::now();
    let blob = db.write_blob(Noise::new(GIB)).unwrap();
    let write_time = start.elapsed();
    let write_peak = watch.finish();

    assert_eq!(blob.len, GIB as u64);
    assert_eq!(blob.chunks.len(), GIB / quanty_core::CHUNK_SIZE);
    assert_eq!(
        blob.distinct_chunks(),
        blob.chunks.len(),
        "the source repeated, so this stored less than a gigabyte"
    );

    let watch = Watcher::start();
    let start = Instant::now();
    let mut sink = Verify {
        expect: Noise::new(GIB),
        seen: 0,
        scratch: Vec::new(),
    };
    let read = db.read_blob(&blob, &mut sink).unwrap();
    let read_time = start.elapsed();
    let read_peak = watch.finish();

    assert_eq!(read, GIB as u64);
    assert_eq!(sink.seen, GIB as u64);

    let on_disk = std::fs::metadata(&path).unwrap().len();
    println!(
        "1 GiB write {write_time:.1?} peak {write_peak} MiB, \
         read {read_time:.1?} peak {read_peak} MiB, \
         baseline {baseline} MiB, file {} MiB",
        on_disk / (1024 * 1024)
    );

    // Constant memory is the criterion: what is resident must not track
    // the size of the asset. Proved to catch: raising CHUNKS_PER_COMMIT
    // to 400 takes both peaks to about 417 MiB, and committing once at
    // the end would take them past a gigabyte.
    //
    // The headroom is deliberate and was learned the hard way. Resident
    // memory includes what the allocator keeps rather than returns, which
    // depends on the machine: the same run measures 16 MiB on one core
    // and 65 on the four core runner. A bound pinned to whichever machine
    // happened to run it first is a bound about that machine. What this
    // has to catch is memory that tracks the payload, and an order of
    // magnitude of room still catches that.
    //
    // The read peak inherits whatever the write left resident, for the
    // same reason, so it is an upper bound on the read rather than a
    // measurement of it alone.
    assert!(
        write_peak < 256,
        "writing held {write_peak} MiB, which grows with the blob"
    );
    assert!(
        read_peak < 256,
        "reading held {read_peak} MiB, which grows with the blob"
    );
}
