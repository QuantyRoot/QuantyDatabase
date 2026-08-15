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
//!
//! ## Reads are timed on their own
//!
//! The read workloads run against a database loaded beforehand, outside the
//! measurement, so what they report is reading rather than the loading that
//! used to dominate them. The `startup` row is the floor for all of them:
//! open the database, run nothing, exit. Subtract it mentally before
//! reading anything into a small difference.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

const ROWS: usize = 5_000;
const BATCH: usize = 100;
const LOOKUPS: usize = 5_000;
const SCANS: usize = 20;
/// Runs per workload; the median is reported, so one unlucky run does not
/// decide anything.
const REPEATS: usize = 3;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Timed from an empty database: what writing costs.
    Write(Mode),
    /// Timed against a database prepared beforehand and not timed, so the
    /// reading is what shows rather than the loading that preceded it.
    Read,
}

impl Phase {
    fn label(self) -> &'static str {
        match self {
            Phase::Write(mode) => mode.label(),
            Phase::Read => "read",
        }
    }
}

struct Workload {
    name: &'static str,
    phase: Phase,
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

/// The same for bulk, where one fsync covers the whole workload and the
/// engines' own work is what shows. We are further behind here, so the
/// ceiling is looser, and lowering it is the point of the next round.
const BULK_CEILING: f64 = 25.0;

/// And for reads, against a database that was loaded beforehand.
const READ_CEILING: f64 = 25.0;

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

    // one prepared database per engine, loaded once and not timed, so the
    // read numbers are reading rather than loading
    let prepared = [
        (Engine::Quanty, prepare(&dir, &quanty, Engine::Quanty)),
        (Engine::Sqlite, prepare(&dir, &sqlite, Engine::Sqlite)),
    ];
    let prepared_for = |engine: Engine| -> PathBuf {
        prepared
            .iter()
            .find(|(e, _)| {
                matches!(
                    (e, engine),
                    (Engine::Quanty, Engine::Quanty) | (Engine::Sqlite, Engine::Sqlite)
                )
            })
            .map(|(_, path)| path.clone())
            .expect("both engines were prepared")
    };

    let mut rows = Vec::new();
    for workload in &workloads {
        let ours = time_engine(
            &dir,
            &quanty,
            workload,
            Engine::Quanty,
            &prepared_for(Engine::Quanty),
        );
        let theirs = time_engine(
            &dir,
            &sqlite,
            workload,
            Engine::Sqlite,
            &prepared_for(Engine::Sqlite),
        );
        rows.push((
            workload.phase,
            workload.name,
            workload.what.clone(),
            ours,
            theirs,
        ));
    }

    print_table(&rows);

    if check {
        let mut over = Vec::new();
        for (phase, name, _, ours, theirs) in &rows {
            let ratio = ours.as_secs_f64() / theirs.as_secs_f64();
            // both modes gate now. bulk was exempt while an open
            // transaction replayed its whole buffer per statement, which
            // was quadratic; ADR-021 replaced that with a suspended write
            // batch and the ceiling below is what the fix bought.
            let ceiling = match phase {
                Phase::Write(Mode::Durable) => DURABLE_CEILING,
                Phase::Write(Mode::Bulk) => BULK_CEILING,
                Phase::Read => READ_CEILING,
            };
            if ratio > ceiling {
                over.push(format!("{name}: {ratio:.1}x"));
            }
        }
        if !over.is_empty() {
            eprintln!("\nover the ceiling against sqlite: {}", over.join(", "));
            std::process::exit(1);
        }
        println!(
            "\nall workloads within their ceiling: {DURABLE_CEILING}x durable, \
             {BULK_CEILING}x bulk"
        );
    }
}

#[derive(Clone, Copy)]
enum Engine {
    Quanty,
    Sqlite,
}

/// Load a database for the read workloads, once, outside any measurement.
fn prepare(dir: &Path, binary: &Path, engine: Engine) -> PathBuf {
    let (db, script, args) = match engine {
        Engine::Quanty => {
            let db = dir.join("prepared.qdb");
            let script = dir.join("prepare.qql");
            std::fs::write(&script, quanty_insert(Mode::Bulk)).expect("writing the script");
            let made = Command::new(binary)
                .args(["create", &path(&db)])
                .stdout(std::process::Stdio::null())
                .status()
                .expect("creating the database");
            assert!(made.success());
            (db.clone(), script, vec!["shell".to_string(), path(&db)])
        }
        Engine::Sqlite => {
            let db = dir.join("prepared.sqlite");
            let script = dir.join("prepare.sql");
            std::fs::write(&script, sqlite_insert(Mode::Bulk)).expect("writing the script");
            (db.clone(), script, vec![path(&db)])
        }
    };
    let input = std::fs::File::open(&script).expect("opening the script");
    let status = Command::new(binary)
        .args(&args)
        .stdin(input)
        .stdout(std::process::Stdio::null())
        .status()
        .expect("preparing the database");
    assert!(status.success(), "could not prepare {}", db.display());
    db
}

/// Run one workload `REPEATS` times against one engine and take the median.
fn time_engine(
    dir: &Path,
    binary: &Path,
    workload: &Workload,
    engine: Engine,
    prepared: &Path,
) -> Duration {
    let mode = match workload.phase {
        Phase::Write(mode) => mode,
        Phase::Read => Mode::Bulk,
    };
    let mut times = Vec::with_capacity(REPEATS);
    for run in 0..REPEATS {
        let name = format!("{}-{}-{run}", workload.name, workload.phase.label());
        let (db, script, args) = match engine {
            Engine::Quanty => {
                let db = match workload.phase {
                    Phase::Read => prepared.to_path_buf(),
                    Phase::Write(_) => dir.join(format!("{name}.qdb")),
                };
                let script = dir.join(format!("{name}.qql"));
                std::fs::write(&script, (workload.quanty)(mode)).expect("writing the script");
                (db.clone(), script, vec!["shell".to_string(), path(&db)])
            }
            Engine::Sqlite => {
                let db = match workload.phase {
                    Phase::Read => prepared.to_path_buf(),
                    Phase::Write(_) => dir.join(format!("{name}.sqlite")),
                };
                let script = dir.join(format!("{name}.sql"));
                std::fs::write(&script, (workload.sqlite)(mode)).expect("writing the script");
                (db.clone(), script, vec![path(&db)])
            }
        };
        if let Phase::Write(_) = workload.phase {
            let _ = std::fs::remove_file(&db);
        }

        let input = std::fs::File::open(&script).expect("opening the script");
        let started = Instant::now();
        // sqlite creates its file on open and we do not, so for the write
        // workloads the create step is inside the timed region for us and
        // inside sqlite's own run for it. neither engine gets it for free.
        if let (Engine::Quanty, Phase::Write(_)) = (engine, workload.phase) {
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
            name: "startup",
            phase: Phase::Read,
            what: "open a database and do nothing".to_string(),
            quanty: |_| String::new(),
            sqlite: |_| String::new(),
        },
        Workload {
            name: "insert",
            phase: Phase::Write(Mode::Durable),
            what: format!("{ROWS} rows in batches of {BATCH}, one commit each"),
            quanty: quanty_insert,
            sqlite: sqlite_insert,
        },
        Workload {
            name: "insert",
            phase: Phase::Write(Mode::Bulk),
            what: format!("{ROWS} rows in batches of {BATCH}, one transaction"),
            quanty: quanty_insert,
            sqlite: sqlite_insert,
        },
        Workload {
            name: "lookup",
            phase: Phase::Read,
            what: format!("{LOOKUPS} lookups by key"),
            quanty: quanty_lookup,
            sqlite: sqlite_lookup,
        },
        Workload {
            name: "scan",
            phase: Phase::Read,
            what: format!("{SCANS} full scans of {ROWS} rows"),
            quanty: quanty_scan,
            sqlite: sqlite_scan,
        },
        Workload {
            name: "indexed",
            phase: Phase::Read,
            what: format!("{LOOKUPS} lookups through a secondary index"),
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

/// The read scripts run against a database prepared beforehand, so they
/// hold nothing but the reads.
fn quanty_lookup(_mode: Mode) -> String {
    let mut s = String::new();
    for key in lookup_keys() {
        let _ = writeln!(s, "get bench {{ name, score }} where id = {key}");
    }
    s
}

fn sqlite_lookup(_mode: Mode) -> String {
    let mut s = String::new();
    for key in lookup_keys() {
        let _ = writeln!(s, "select name, score from bench where id = {key};");
    }
    s
}

fn quanty_scan(_mode: Mode) -> String {
    let mut s = String::new();
    for _ in 0..SCANS {
        s.push_str("get bench { id, name, score }\n");
    }
    s
}

fn sqlite_scan(_mode: Mode) -> String {
    let mut s = String::new();
    for _ in 0..SCANS {
        s.push_str("select id, name, score from bench;\n");
    }
    s
}

fn quanty_indexed(_mode: Mode) -> String {
    let mut s = String::new();
    for key in lookup_keys() {
        let _ = writeln!(
            s,
            "get bench {{ id, score }} where name = \"{}\"",
            name_of(key)
        );
    }
    s
}

fn sqlite_indexed(_mode: Mode) -> String {
    let mut s = String::new();
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

fn print_table(rows: &[(Phase, &str, String, Duration, Duration)]) {
    println!(
        "{:<9} {:<9} {:<38} {:>10} {:>10} {:>9}",
        "phase", "workload", "what", "quanty", "sqlite", "ratio"
    );
    println!("{}", "-".repeat(89));
    for (phase, name, what, ours, theirs) in rows {
        let ratio = ours.as_secs_f64() / theirs.as_secs_f64();
        println!(
            "{:<9} {:<9} {:<38} {:>10} {:>10} {:>8.2}x",
            phase.label(),
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
    for (phase, name, _, ours, theirs) in rows {
        println!(
            "RESULT {phase} {name} quanty_ms={:.1} sqlite_ms={:.1} ratio={:.3}",
            ours.as_secs_f64() * 1000.0,
            theirs.as_secs_f64() * 1000.0,
            ours.as_secs_f64() / theirs.as_secs_f64(),
            phase = phase.label()
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
