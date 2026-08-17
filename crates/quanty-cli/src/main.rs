//! The `quanty` command line tool.
//!
//! Four things, which is what "minimal" means here: import a SQLite file,
//! run a statement, run a session of them, and say what a database holds.
//!
//! The argument parsing is written out rather than pulled in. A clap sized
//! dependency under a command line tool would be the largest thing in the
//! workspace by some margin, and it would have to build on the MSRV
//! toolchain for the next several years (ADR-008, ADR-013). What follows is
//! forty lines and does exactly what these four commands need.
//!
//! Everything the tool prints about an import goes to stdout; anything that
//! went wrong goes to stderr and sets the exit status, so it composes with
//! a shell the way a tool should.

use std::io::{BufRead, Write};
use std::path::Path;
use std::process::ExitCode;

use quanty_core::{Db, FileStorage};
use quanty_exec::{Output, Session};
use quanty_import::{execute, plan, Options};

const USAGE: &str = "\
quanty, a database that remembers

usage:
  quanty create <database.qdb>
  quanty import <source.sqlite> <target.qdb> [--dry-run] [--strict]
  quanty run <database.qdb> <statement> [--sql]
  quanty shell <database.qdb> [--sql]
  quanty serve <database.qdb> [--listen <addr>] [--workers <n>]
  quanty tables <database.qdb>

  create   make an empty database
  import   read a sqlite file and write it into a new quanty database
             --dry-run  print what would happen and write nothing
             --strict   refuse anything lossy instead of reporting it
  run      execute one statement and print the result
  shell    read statements from stdin, one per line
  tables   list the tables in a database

  --sql    read the statement in sql rather than qql

serve    --listen   address to bind, default 127.0.0.1:7878
         --workers  event loop threads, default one per core
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Usage(message)) => {
            eprintln!("{message}\n\n{USAGE}");
            ExitCode::from(2)
        }
        Err(Failure::Failed(message)) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
        Err(Failure::PipeClosed) => ExitCode::SUCCESS,
    }
}

enum Failure {
    /// The command line itself was wrong, so the usage is worth printing.
    Usage(String),
    /// The command was understood and did not work.
    Failed(String),
    /// Whoever was reading our output stopped, as `head` does. Nothing
    /// went wrong and there is nobody left to tell.
    PipeClosed,
}

fn usage(message: impl Into<String>) -> Failure {
    Failure::Usage(message.into())
}

fn failed(message: impl Into<String>) -> Failure {
    Failure::Failed(message.into())
}

/// Write a line to stdout.
///
/// Rust ignores SIGPIPE, so a `println!` into a pipe nobody is reading any
/// more panics with a backtrace. `quanty tables db | head -1` is an
/// ordinary thing to type, and it must end quietly rather than looking like
/// a crash, so every line this tool prints goes through here.
fn emit(text: &str) -> Result<(), Failure> {
    let mut out = std::io::stdout().lock();
    match writeln!(out, "{text}") {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Err(Failure::PipeClosed),
        Err(e) => Err(failed(format!("writing to stdout: {e}"))),
    }
}

/// Flags pulled out of the arguments, leaving the positional ones behind.
struct Flags {
    dry_run: bool,
    strict: bool,
    sql: bool,
    listen: Option<String>,
    workers: Option<usize>,
}

fn split_flags(args: &[String]) -> Result<(Vec<&str>, Flags), Failure> {
    let mut positional = Vec::new();
    let mut flags = Flags {
        dry_run: false,
        strict: false,
        sql: false,
        listen: None,
        workers: None,
    };
    let mut expect: Option<&str> = None;
    for arg in args {
        if let Some(name) = expect.take() {
            match name {
                "--listen" => flags.listen = Some(arg.clone()),
                "--workers" => {
                    let n = arg
                        .parse::<usize>()
                        .map_err(|_| usage(format!("--workers wants a number, got {arg}")))?;
                    if n == 0 {
                        return Err(usage("--workers must be at least 1"));
                    }
                    flags.workers = Some(n);
                }
                _ => unreachable!(),
            }
            continue;
        }
        match arg.as_str() {
            "--listen" | "--workers" => {
                expect = Some(match arg.as_str() {
                    "--listen" => "--listen",
                    _ => "--workers",
                })
            }
            "--dry-run" => flags.dry_run = true,
            "--strict" => flags.strict = true,
            "--sql" => flags.sql = true,
            "--help" | "-h" => return Err(usage("")),
            other if other.starts_with("--") => {
                return Err(usage(format!("unknown option {other}")))
            }
            other => positional.push(other),
        }
    }
    Ok((positional, flags))
}

fn run(args: &[String]) -> Result<(), Failure> {
    let (positional, flags) = split_flags(args)?;
    let Some((command, rest)) = positional.split_first() else {
        return Err(usage("no command given"));
    };

    match *command {
        "create" => match rest {
            [database] => create(Path::new(database)),
            _ => Err(usage("create takes a database")),
        },
        "import" => match rest {
            [source, target] => import(Path::new(source), Path::new(target), &flags),
            _ => Err(usage("import takes a source and a target")),
        },
        "run" => match rest {
            [database, statement] => run_statement(Path::new(database), statement, &flags),
            _ => Err(usage("run takes a database and a statement")),
        },
        "shell" => match rest {
            [database] => shell(Path::new(database), &flags),
            _ => Err(usage("shell takes a database")),
        },
        "serve" => match rest {
            [database] => serve(Path::new(database), &flags),
            _ => Err(usage("serve takes a database")),
        },
        "tables" => match rest {
            [database] => run_statement(
                Path::new(database),
                "show tables",
                &Flags {
                    dry_run: false,
                    strict: false,
                    sql: false,
                    listen: None,
                    workers: None,
                },
            ),
            _ => Err(usage("tables takes a database")),
        },
        "help" => Err(usage("")),
        other => Err(usage(format!("unknown command {other}"))),
    }
}

// ---------------------------------------------------------------------------
// create
// ---------------------------------------------------------------------------

/// Make an empty database.
///
/// This exists as its own command rather than as something `run` and
/// `shell` do when the file is missing. Creating a database by mistyping a
/// path is a mistake that looks like success: the tool answers, the queries
/// return nothing, and the real database is somewhere else with the data
/// still in it.
fn create(database: &Path) -> Result<(), Failure> {
    if database.exists() {
        return Err(failed(format!("{} already exists", database.display())));
    }
    Db::create_file(database).map_err(|e| failed(format!("{}: {e}", database.display())))?;
    emit(&format!("created {}", database.display()))
}

// ---------------------------------------------------------------------------
// import
// ---------------------------------------------------------------------------

fn import(source: &Path, target: &Path, flags: &Flags) -> Result<(), Failure> {
    let reader = quanty_sqlite::open_file(source)
        .map_err(|e| failed(format!("{}: {e}", source.display())))?;
    let plan = plan(
        &reader,
        &Options {
            strict: flags.strict,
        },
    )
    .map_err(|e| failed(format!("{}: {e}", source.display())))?;

    // the plan renders itself, so this tool and the dry run report print
    // the same thing rather than two descriptions that drift apart
    emit(plan.report().trim_end())?;
    emit(&format!(
        "\n{} tables, {} rows",
        plan.tables.len(),
        plan.rows()
    ))?;

    if !plan.is_runnable() {
        return Err(failed(format!(
            "{} problem(s), nothing was written",
            plan.problems.len()
        )));
    }
    if flags.dry_run {
        return emit("\ndry run, nothing was written");
    }

    // a target that already exists is not overwritten. an import that
    // silently replaced a database would be the one mistake in this tool
    // that cannot be undone.
    if target.exists() {
        return Err(failed(format!(
            "{} already exists; delete it or pick another name",
            target.display()
        )));
    }

    let db = Db::create_file(target).map_err(|e| failed(format!("{}: {e}", target.display())))?;
    let mut session = Session::new(db);
    let report = execute(&reader, &plan, &mut session).map_err(|e| {
        failed(format!(
            "{e}\n\n{} is incomplete and should be deleted",
            target.display()
        ))
    })?;

    emit(&format!(
        "\nimported {} rows into {} tables in {}",
        report.rows(),
        report.tables.len(),
        target.display()
    ))
}

// ---------------------------------------------------------------------------
// running statements
// ---------------------------------------------------------------------------

fn open(database: &Path) -> Result<Session<FileStorage>, Failure> {
    if !database.exists() {
        return Err(failed(format!("{} does not exist", database.display())));
    }
    let db = Db::open_file(database).map_err(|e| failed(format!("{}: {e}", database.display())))?;
    Ok(Session::new(db))
}

fn run_statement(database: &Path, statement: &str, flags: &Flags) -> Result<(), Failure> {
    let mut session = open(database)?;
    let output = execute_one(&mut session, statement, flags.sql).map_err(failed)?;
    print_output(&output)
}

fn execute_one(
    session: &mut Session<FileStorage>,
    statement: &str,
    sql: bool,
) -> Result<Output, String> {
    let result = if sql {
        session.execute_sql(statement)
    } else {
        session.execute(statement)
    };
    result.map_err(|e| e.to_string())
}

fn print_output(output: &Output) -> Result<(), Failure> {
    let text = output.render();
    if text.is_empty() {
        return Ok(());
    }
    emit(&text)
}

/// Read statements from stdin, one per line.
///
/// Errors do not end the session: a typo in one statement is not a reason
/// to throw away the ones after it, which is the whole point of a shell.
fn shell(database: &Path, flags: &Flags) -> Result<(), Failure> {
    let mut session = open(database)?;
    let stdin = std::io::stdin();
    let interactive = is_terminal();

    if interactive {
        emit(&format!(
            "{} -- one statement per line, ctrl-d to leave",
            database.display()
        ))?;
    }
    let mut failures = 0u32;
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| failed(format!("reading stdin: {e}")))?;
        let statement = line.trim();
        if statement.is_empty() || statement.starts_with('#') {
            continue;
        }
        match execute_one(&mut session, statement, flags.sql) {
            Ok(output) => print_output(&output)?,
            Err(message) => {
                failures += 1;
                eprintln!("{message}");
            }
        }
        if interactive {
            let _ = std::io::stdout().flush();
        }
    }
    if failures > 0 {
        return Err(failed(format!("{failures} statement(s) failed")));
    }
    Ok(())
}

/// Whether stdin is a terminal, so the shell knows whether to greet anyone.
///
/// `IsTerminal` landed in 1.70 and the MSRV is 1.75, so this is the
/// standard library's answer rather than a guess.
fn is_terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
}

#[cfg(target_os = "linux")]
fn serve(database: &Path, flags: &Flags) -> Result<(), Failure> {
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use quanty_server::{Idle, Worker};

    if !database.exists() {
        return Err(failed(format!("no database at {}", database.display())));
    }
    open(database)?;

    let addr = flags.listen.as_deref().unwrap_or("127.0.0.1:7878");
    let workers = match flags.workers {
        Some(n) => n,
        None => thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
    };

    let listener = TcpListener::bind(addr).map_err(|e| failed(format!("binding {addr}: {e}")))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| failed(format!("{addr}: {e}")))?;
    let bound = listener
        .local_addr()
        .map_err(|e| failed(format!("{addr}: {e}")))?;
    let listener = Arc::new(listener);

    let running = Arc::new(AtomicBool::new(true));
    let accepted = Arc::new(AtomicUsize::new(0));
    let live: Arc<Vec<AtomicUsize>> = Arc::new((0..workers).map(|_| AtomicUsize::new(0)).collect());

    let mut handles = Vec::with_capacity(workers);
    for id in 0..workers {
        let mut worker = Worker::new(listener.clone(), running.clone())
            .map_err(|e| failed(format!("worker {id}: {e}")))?;
        let running = running.clone();
        let accepted = accepted.clone();
        let live = live.clone();
        handles.push(thread::spawn(move || {
            let mut idle = Idle;
            while running.load(Ordering::Relaxed) {
                match worker.turn(200, &mut idle) {
                    Ok(turn) => {
                        if turn.accepted > 0 {
                            accepted.fetch_add(turn.accepted, Ordering::Relaxed);
                        }
                        live[id].store(worker.len(), Ordering::Relaxed);
                    }
                    Err(e) => {
                        eprintln!("worker {id}: {e}");
                        break;
                    }
                }
            }
            worker.shutdown();
        }));
    }

    emit(&format!("listening on {bound}, {workers} workers"))?;
    emit("connections are accepted and held; statements are not served yet")?;

    loop {
        thread::sleep(Duration::from_secs(5));
        let held: usize = live.iter().map(|c| c.load(Ordering::Relaxed)).sum();
        let spread: Vec<usize> = live.iter().map(|c| c.load(Ordering::Relaxed)).collect();
        emit(&format!(
            "held={held} accepted={} spread={spread:?}",
            accepted.load(Ordering::Relaxed)
        ))?;
        if handles.iter().all(|h| h.is_finished()) {
            break;
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn serve(_database: &Path, _flags: &Flags) -> Result<(), Failure> {
    Err(failed("serve needs epoll and is linux only for now"))
}
