//! Timing QuantyDB against SQLite, on the same machine, in the same run.
//!
//! ## What this measures, and why it is built this way
//!
//! Both engines get the same workload, expressed in their own language,
//! fed to their own command line tool, and the whole process is timed. No
//! part of this harness can favour us: it does not touch our internals, it
//! does not skip our parser, and it does not run SQLite through a wrapper
//! that would slow it down. Two binaries, one script each, a stopwatch.
//!
//! ## The number that matters is the ratio
//!
//! Absolute timings from a shared CI runner are close to worthless: the
//! same job can be twice as slow an hour later with nothing changed. What
//! survives that noise is the ratio between two engines measured minutes
//! apart on the same machine, which is why every workload here runs both
//! and reports how many times slower or faster we are. A regression shows
//! up as the ratio moving, not as the milliseconds moving.
//!
//! ## Durability is stated, not assumed
//!
//! This is where benchmarks lie most often, usually without meaning to. Two
//! settings are run for both engines:
//!
//! - `durable`: every statement commits and reaches the disk. SQLite's
//!   default (`synchronous = full`, rollback journal) against ours, which
//!   fsyncs every commit. This is the honest default-versus-default number
//!   and it is dominated by fsync on both sides.
//! - `bulk`: the whole workload inside one explicit transaction, which is
//!   how anybody actually loads data, and where the engines' own work shows
//!   through instead of the disk's.
//!
//! Anything else about the configuration is left alone. Turning
//! `synchronous` off for SQLite would produce a flattering number for us
//! and describe a database nobody should run.
//!
//! ## What is deliberately not here
//!
//! No claim that the workloads are representative of any application. They
//! are four narrow shapes: bulk insert, point lookup by key, full scan, and
//! lookup through a secondary index. They are the shapes where a storage
//! engine's basic choices show, and they are the ones we can express in
//! both languages, since our dialect has no aggregates yet.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const ROWS: usize = 5_000;
const BATCH: usize = 100;
const LOOKUPS: usize = 500;
const SCANS: usize = 5;
/// Runs per workload; the median is reported, so one unlucky run does not
/// decide anything.
const REPEATS: usize = 3;

struct Workload {
    name: &'static str,
    /// What it does, for the table the report prints. Built from the
    /// constants above rather than written out, because a label that says
    /// 50000 while the run does 5000 is how a benchmark starts lying
    /// without anybody deciding to.
    what: String,
    quanty: fn(Mode) -> String,
    sqlite: fn(Mode) -> String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Every statement is its own transaction and reaches the disk.
    Durable,
    /// One transaction around the whole workload.
    Bulk,
}

impl Mode {
    fn label(self) -> &'static str {
        match self {
            Mode::Durable => "durable",
            Mode::Bulk => "bulk",
        }
    }
}

/// The most times slower than sqlite a durable workload may be before this
/// exits non-zero under `--check`.
///
/// It is deliberately generous. The point of the gate is to catch a change
/// that makes something ten times worse overnight, not to hold a number
/// steady on a shared runner where the same job varies by a factor of two
/// on its own.
const DURABLE_CEILING: f64 = 15.0;

fn main() {
    let check = std::env::args().any(|a| a == "--check");
    let quanty = binary("quanty");
    let sqlite = which("sqlite3").unwrap_or_else(|| {
        eprintln!("sqlite3 is not on PATH, and this harness is pointless without it");
        std::process::exit(2);
    });

    let dir = scratch_dir();
    println!("quanty:  {}", quanty.display());
    println!("sqlite3: {}", version(&sqlite));
    println!("scratch: {}\n", dir.display());

    let workloads = workloads();
    let mut rows = Vec::new();

    for mode in [Mode::Durable, Mode::Bulk] {
        for workload in &workloads {
            let ours = time_engine(&dir, &quanty, workload, mode, Engine::Quanty);
            let theirs = time_engine(&dir, &sqlite, workload, mode, Engine::Sqlite);
            rows.push((mode, workload.name, workload.what.clone(), ours, theirs));
        }
    }

    print_table(&rows);

    if check {
        let mut over = Vec::new();
        for (mode, name, _, ours, theirs) in &rows {
            let ratio = ours.as_secs_f64() / theirs.as_secs_f64();
            // only the durable numbers gate. bulk is measured and printed,
            // and it is currently far past any ceiling for a known reason
            // that is written down in the roadmap: a statement inside an
            // open transaction replays every statement before it, which is
            // quadratic. gating on a defect we have already named would
            // just mean a permanently red job.
            if *mode == Mode::Durable && ratio > DURABLE_CEILING {
                over.push(format!("{name}: {ratio:.1}x"));
            }
        }
        if !over.is_empty() {
            eprintln!(
                "\nover the {DURABLE_CEILING}x ceiling against sqlite: {}",
                over.join(", ")
            );
            std::process::exit(1);
        }
        println!("\nall durable workloads are within {DURABLE_CEILING}x of sqlite");
    }
}

#[derive(Clone, Copy)]
enum Engine {
    Quanty,
    Sqlite,
}

/// Run one workload `REPEATS` times against one engine and take the median.
fn time_engine(
    dir: &Path,
    binary: &Path,
    workload: &Workload,
    mode: Mode,
    engine: Engine,
) -> Duration {
    let mut times = Vec::with_capacity(REPEATS);
    for run in 0..REPEATS {
        let name = format!("{}-{}-{run}", workload.name, mode.label());
        let (db, script, args) = match engine {
            Engine::Quanty => {
                let db = dir.join(format!("{name}.qdb"));
                let script = dir.join(format!("{name}.qql"));
                std::fs::write(&script, (workload.quanty)(mode)).expect("writing the script");
                (db.clone(), script, vec!["shell".to_string(), path(&db)])
            }
            // sqlite creates its file on open and we do not, so the create
            // step is inside the timed region below for us and inside
            // sqlite's own run for it. neither engine gets it for free.
            Engine::Sqlite => {
                let db = dir.join(format!("{name}.sqlite"));
                let script = dir.join(format!("{name}.sql"));
                std::fs::write(&script, (workload.sqlite)(mode)).expect("writing the script");
                (db.clone(), script, vec![path(&db)])
            }
        };
        let _ = std::fs::remove_file(&db);

        let input = std::fs::File::open(&script).expect("opening the script");
        let started = Instant::now();
        if let Engine::Quanty = engine {
            let made = Command::new(binary)
                .args(["create", &path(&db)])
                .stdout(std::process::Stdio::null())
                .status()
                .expect("creating the database");
            assert!(made.success(), "could not create {}", db.display());
        }
        let status = Command::new(binary)
            .args(&args)
            .stdin(input)
            .stdout(std::process::Stdio::null())
            .status()
            .expect("running the engine");
        let elapsed = started.elapsed();
        assert!(status.success(), "{} failed on {name}", binary.display());
        times.push(elapsed);
    }
    times.sort();
    times[times.len() / 2]
}

// ---------------------------------------------------------------------------
// the workloads, in both languages
// ---------------------------------------------------------------------------

fn workloads() -> Vec<Workload> {
    vec![
        Workload {
            name: "insert",
            what: format!("{ROWS} rows in batches of {BATCH}"),
            quanty: quanty_insert,
            sqlite: sqlite_insert,
        },
        Workload {
            name: "lookup",
            what: format!("that, then {LOOKUPS} lookups by key"),
            quanty: quanty_lookup,
            sqlite: sqlite_lookup,
        },
        Workload {
            name: "scan",
            what: format!("that, then {SCANS} full scans"),
            quanty: quanty_scan,
            sqlite: sqlite_scan,
        },
        Workload {
            name: "indexed",
            what: format!("that, then {LOOKUPS} lookups by index"),
            quanty: quanty_indexed,
            sqlite: sqlite_indexed,
        },
    ]
}

/// The value of row `i`, the same on both sides so neither engine is asked
/// to store something the other is not.
fn name_of(i: usize) -> String {
    format!("name-{i:06}")
}

fn score_of(i: usize) -> usize {
    (i * 7919) % 100_000
}

/// Keys the lookups ask for, spread across the table rather than clustered.
fn lookup_keys() -> impl Iterator<Item = usize> {
    (0..LOOKUPS).map(|n| (n * 6151) % ROWS)
}

fn quanty_insert(mode: Mode) -> String {
    let mut s = String::new();
    s.push_str("table bench { id: int @key, name: text @index, score: int }\n");
    if mode == Mode::Bulk {
        s.push_str("begin\n");
    }
    for chunk in (0..ROWS).collect::<Vec<_>>().chunks(BATCH) {
        s.push_str("put bench ");
        for (n, i) in chunk.iter().enumerate() {
            if n > 0 {
                s.push_str(", ");
            }
            let _ = write!(
                s,
                "{{ id: {i}, name: \"{}\", score: {} }}",
                name_of(*i),
                score_of(*i)
            );
        }
        s.push('\n');
    }
    if mode == Mode::Bulk {
        s.push_str("commit\n");
    }
    s
}

fn sqlite_insert(mode: Mode) -> String {
    let mut s = String::new();
    s.push_str("create table bench (id integer primary key, name text not null, score integer not null);\n");
    s.push_str("create index bench_name on bench (name);\n");
    if mode == Mode::Bulk {
        s.push_str("begin;\n");
    }
    for chunk in (0..ROWS).collect::<Vec<_>>().chunks(BATCH) {
        s.push_str("insert into bench (id, name, score) values ");
        for (n, i) in chunk.iter().enumerate() {
            if n > 0 {
                s.push_str(", ");
            }
            let _ = write!(s, "({i}, '{}', {})", name_of(*i), score_of(*i));
        }
        s.push_str(";\n");
    }
    if mode == Mode::Bulk {
        s.push_str("commit;\n");
    }
    s
}

/// The read workloads load the table first, so what they time is the load
/// plus the reads. The insert row above is what separates the two.
fn quanty_lookup(mode: Mode) -> String {
    let mut s = quanty_insert(mode);
    for key in lookup_keys() {
        let _ = writeln!(s, "get bench {{ name, score }} where id = {key}");
    }
    s
}

fn sqlite_lookup(mode: Mode) -> String {
    let mut s = sqlite_insert(mode);
    for key in lookup_keys() {
        let _ = writeln!(s, "select name, score from bench where id = {key};");
    }
    s
}

fn quanty_scan(mode: Mode) -> String {
    let mut s = quanty_insert(mode);
    for _ in 0..SCANS {
        s.push_str("get bench { id, name, score }\n");
    }
    s
}

fn sqlite_scan(mode: Mode) -> String {
    let mut s = sqlite_insert(mode);
    for _ in 0..SCANS {
        s.push_str("select id, name, score from bench;\n");
    }
    s
}

fn quanty_indexed(mode: Mode) -> String {
    let mut s = quanty_insert(mode);
    for key in lookup_keys() {
        let _ = writeln!(
            s,
            "get bench {{ id, score }} where name = \"{}\"",
            name_of(key)
        );
    }
    s
}

fn sqlite_indexed(mode: Mode) -> String {
    let mut s = sqlite_insert(mode);
    for key in lookup_keys() {
        let _ = writeln!(
            s,
            "select id, score from bench where name = '{}';",
            name_of(key)
        );
    }
    s
}

// ---------------------------------------------------------------------------
// reporting
// ---------------------------------------------------------------------------

fn print_table(rows: &[(Mode, &str, String, Duration, Duration)]) {
    println!(
        "{:<9} {:<9} {:<38} {:>10} {:>10} {:>9}",
        "mode", "workload", "what", "quanty", "sqlite", "ratio"
    );
    println!("{}", "-".repeat(89));
    for (mode, name, what, ours, theirs) in rows {
        let ratio = ours.as_secs_f64() / theirs.as_secs_f64();
        println!(
            "{:<9} {:<9} {:<38} {:>10} {:>10} {:>8.2}x",
            mode.label(),
            name,
            what,
            millis(*ours),
            millis(*theirs),
            ratio
        );
    }
    println!(
        "\nratio is quanty divided by sqlite: below 1.00 is faster than sqlite, \
         above 1.00 is slower.\nmedian of {REPEATS} runs each, both engines driven \
         through their own command line tool."
    );

    // a machine readable line per result, so a CI job can keep them
    println!();
    for (mode, name, _, ours, theirs) in rows {
        println!(
            "RESULT {mode} {name} quanty_ms={:.1} sqlite_ms={:.1} ratio={:.3}",
            ours.as_secs_f64() * 1000.0,
            theirs.as_secs_f64() * 1000.0,
            ours.as_secs_f64() / theirs.as_secs_f64(),
            mode = mode.label()
        );
    }
}

fn millis(d: Duration) -> String {
    format!("{:.1} ms", d.as_secs_f64() * 1000.0)
}

// ---------------------------------------------------------------------------
// finding things
// ---------------------------------------------------------------------------

/// The quanty binary that was built alongside this one.
fn binary(name: &str) -> PathBuf {
    let mut path = std::env::current_exe().expect("this binary has a path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    let candidate = path.join(name);
    if candidate.exists() {
        return candidate;
    }
    which(name).unwrap_or_else(|| {
        eprintln!("cannot find the {name} binary; build the workspace first");
        std::process::exit(2);
    })
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var("PATH").ok()?;
    for dir in path.split(':') {
        let candidate = Path::new(dir).join(name);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn version(sqlite: &Path) -> String {
    let out = Command::new(sqlite)
        .arg("--version")
        .output()
        .expect("sqlite3 --version");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn scratch_dir() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the clock is after 1970")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("quanty-bench-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("creating the scratch directory");
    dir
}

fn path(p: &Path) -> String {
    p.to_str().expect("a utf-8 path").to_string()
}
