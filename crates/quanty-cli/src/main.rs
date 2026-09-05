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
use quanty_ql::ast::Statement;

const USAGE: &str = "\
quanty, a database that remembers

usage:
  quanty create <database.qdb>
  quanty import <source.sqlite> <target.qdb> [--dry-run] [--strict]
  quanty run <database.qdb> <statement> [--sql]
  quanty shell <database.qdb> [--sql]
  quanty serve <database.qdb> [--listen <addr>] [--workers <n>]
                              [--tokens <file>]
  quanty tables <database.qdb>
  quanty branch <database.qdb> <name> [--at <commit>]
  quanty branches <database.qdb>
  quanty switch <database.qdb> <branch>
  quanty merge <database.qdb> <branch>
  quanty log <database.qdb>
  quanty stats <database.qdb>
  quanty gc <database.qdb> <keep> | blobs
  quanty token <label>
  quanty connect <addr> [statement] [--token <t>] [--sql]
  quanty about

  create   make an empty database
  import   read a sqlite file and write it into a new quanty database
             --dry-run  print what would happen and write nothing
             --strict   refuse anything lossy instead of reporting it
  run      execute one statement and print the result
  shell    read statements from stdin, one per line
  tables   list the tables in a database
  branch   fork the current branch under a new name
             --at       fork from this commit instead of the head
  branches list them, marking the one you are on
  switch   move to another branch
  merge    fast forward the current branch onto another one
  log      print the commits of the current branch
  stats    page counts for the file as it stands
  gc       drop history, keeping <keep> commits per branch
             blobs    drop chunks no row names any more
  token    mint one and print it, with the line that accepts it
  connect  talk to a running server; with a statement it runs that one,
             without it reads statements from stdin, as shell does
  about    what this is, who made it, and what it does not depend on

  --sql    read the statement in sql rather than qql

  deleting a branch is `quanty run <db> \"drop branch <name>\"`

connect  --token    the token to show, if the server requires one

serve    --listen   address to bind, default 127.0.0.1:7878
         --workers  event loop threads, default one per core
         --tokens   file of accepted token hashes; without it the server
                    requires no authentication and belongs on loopback
";

mod client;

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
        Err(Failure::Refused) => ExitCode::FAILURE,
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
    /// A server refused a statement and has already said why, so there is
    /// nothing to add beyond the exit code.
    Refused,
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
#[derive(Default)]
struct Flags {
    dry_run: bool,
    strict: bool,
    sql: bool,
    at: Option<u64>,
    listen: Option<String>,
    workers: Option<usize>,
    tokens: Option<String>,
    token: Option<String>,
    elchi: bool,
}

fn split_flags(args: &[String]) -> Result<(Vec<&str>, Flags), Failure> {
    let mut positional = Vec::new();
    let mut flags = Flags::default();
    let mut expect: Option<&str> = None;
    for arg in args {
        if let Some(name) = expect.take() {
            match name {
                "--listen" => flags.listen = Some(arg.clone()),
                "--tokens" => flags.tokens = Some(arg.clone()),
                "--token" => flags.token = Some(arg.clone()),
                "--workers" => {
                    let n = arg
                        .parse::<usize>()
                        .map_err(|_| usage(format!("--workers wants a number, got {arg}")))?;
                    if n == 0 {
                        return Err(usage("--workers must be at least 1"));
                    }
                    flags.workers = Some(n);
                }
                "--at" => {
                    flags.at = Some(
                        arg.parse::<u64>()
                            .map_err(|_| usage(format!("--at wants a commit id, got {arg}")))?,
                    );
                }
                _ => unreachable!(),
            }
            continue;
        }
        match arg.as_str() {
            "--listen" | "--workers" | "--tokens" | "--token" | "--at" => {
                expect = Some(match arg.as_str() {
                    "--listen" => "--listen",
                    "--tokens" => "--tokens",
                    "--token" => "--token",
                    "--at" => "--at",
                    _ => "--workers",
                })
            }
            // Named rather than left to fall into 'unknown option', because
            // the README sketched it for years and ADR-032 says why not.
            "--branch" => {
                return Err(usage(
                    "--branch does not exist: a write always lands on the current \
                     branch, so running elsewhere means switching there and back, \
                     which is three commits and a window where a kill leaves the \
                     database on a branch nobody chose. Use switch instead.",
                ))
            }
            "--elchi" => flags.elchi = true,
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
        if flags.elchi {
            return emit("<3");
        }
        return Err(usage("no command given"));
    };

    if flags.elchi {
        return emit("<3");
    }

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
        "token" => match rest {
            [label] => token(label),
            _ => Err(usage("token takes a label")),
        },
        "about" => match rest {
            [] => about(),
            _ => Err(usage("about takes nothing")),
        },
        "connect" => match rest {
            [addr] => client::connect(addr, None, flags.token.as_deref(), flags.sql),
            [addr, statement] => {
                client::connect(addr, Some(statement), flags.token.as_deref(), flags.sql)
            }
            _ => Err(usage("connect takes an address and an optional statement")),
        },
        "tables" => match rest {
            [database] => run_ours(database, &Statement::ShowTables),
            _ => Err(usage("tables takes a database")),
        },
        "branch" => match rest {
            [database, name] => run_ours(
                database,
                &Statement::Branch {
                    name: (*name).to_string(),
                    at: flags.at,
                },
            ),
            _ => Err(usage("branch takes a database and a name")),
        },
        "branches" => match rest {
            [database] => run_ours(database, &Statement::ShowBranches),
            _ => Err(usage("branches takes a database")),
        },
        "switch" => match rest {
            [database, name] => run_ours(
                database,
                &Statement::Switch {
                    name: (*name).to_string(),
                },
            ),
            _ => Err(usage("switch takes a database and a branch")),
        },
        "merge" => match rest {
            [database, name] => run_ours(
                database,
                &Statement::Merge {
                    name: (*name).to_string(),
                },
            ),
            _ => Err(usage("merge takes a database and a branch")),
        },
        "log" => match rest {
            [database] => run_ours(database, &Statement::Log),
            _ => Err(usage("log takes a database")),
        },
        "stats" => match rest {
            [database] => run_ours(database, &Statement::ShowStats),
            _ => Err(usage("stats takes a database")),
        },
        "gc" => match rest {
            [database, "blobs"] => run_ours(database, &Statement::GcBlobs),
            [database, keep] => {
                let keep = keep
                    .parse::<u64>()
                    .map_err(|_| usage(format!("gc wants a number of commits, got {keep}")))?;
                if keep == 0 {
                    return Err(usage("gc must keep at least one commit per branch"));
                }
                run_ours(database, &Statement::Gc { keep })
            }
            _ => Err(usage("gc takes a database and how many commits to keep")),
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
    open_for(database, true)
}

/// Open the database, taking the writer lock only if we are going to
/// write.
///
/// A `get` or a `show` changes nothing, and holding an exclusive lock to
/// answer one would mean a served database could not be read from the
/// shell. Many readers alongside one writer is the model (ADR-035).
fn open_for(database: &Path, writing: bool) -> Result<Session<FileStorage>, Failure> {
    if !database.exists() {
        return Err(failed(format!("{} does not exist", database.display())));
    }
    let opened = if writing {
        Db::open_file(database)
    } else {
        Db::open_file_unlocked(database)
    };
    let db = opened.map_err(|e| failed(format!("{}: {e}", database.display())))?;
    Ok(Session::new(db))
}

/// Run a statement this tool built rather than one the user typed.
///
/// It goes in as an AST, so no name is ever glued into text and a bad one
/// meets the engine's own message instead of the parser's. The user's
/// flags deliberately do not apply: reading `branch x` as SQL could only
/// ever be a mistake (ADR-032).
fn run_ours(database: &str, statement: &Statement) -> Result<(), Failure> {
    let mut session = open_for(Path::new(database), statement.writes())?;
    let output = session
        .execute_ast(statement)
        .map_err(|e| failed(e.to_string()))?;
    print_output(&output)
}

fn run_statement(database: &Path, statement: &str, flags: &Flags) -> Result<(), Failure> {
    // Parse before opening, so a read does not ask for the writer lock.
    // Something that will not parse cannot write either, so it opens
    // unlocked and fails with the parser's message; asking for the lock
    // first would answer a typo with a complaint about locks.
    let writing = if flags.sql {
        quanty_ql::parse_sql(statement).is_ok_and(|s| s.writes())
    } else {
        quanty_ql::parse(statement).is_ok_and(|s| s.writes())
    };
    let mut session = open_for(database, writing)?;
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

/// Counts that `about` prints, and that a test holds to reality.
///
/// Each is a floor rather than a snapshot: the test checks that the real
/// number is at least this, so the tool can fall behind but can never
/// overclaim, and nobody has to update it on every commit.
pub const CRATES: usize = 14;
/// At least this many test functions exist.
pub const TESTS: usize = 500;
/// At least this many decision records exist.
pub const DECISIONS: usize = 36;
/// Exactly this many packages that are not this workspace.
pub const FOREIGN_DEPENDENCIES: usize = 0;

/// What this is, who made it, and what it does not depend on.
fn about() -> Result<(), Failure> {
    emit(&format!("quanty {}", env!("CARGO_PKG_VERSION")))?;
    emit("one database that reshapes itself into whatever you need")?;
    emit("")?;
    emit(&format!(
        "  dependencies   {FOREIGN_DEPENDENCIES}, and that is the whole list"
    ))?;
    emit("  people         1")?;
    emit("  funding        none")?;
    emit(&format!(
        "  crates         {CRATES}, all of them in this repository"
    ))?;
    emit(&format!(
        "  tests          {TESTS}+ functions, run on every push"
    ))?;
    emit(&format!(
        "  decisions      {DECISIONS}+ written down, with their costs"
    ))?;
    emit("")?;
    emit("The checksum, the locks, the epoll layer, sha256 and the wire")?;
    emit("protocol are written out here rather than pulled in. Every one of")?;
    emit("those choices is argued in docs/DECISIONS.md, cost included.")?;
    emit("")?;
    emit("  source   https://github.com/QuantyRoot/QuantyDatabase")?;
    emit("  licence  MIT")?;
    emit("  history  HUNDRED.md, for the bugs and the ideas that lost")
}

/// Print a new token and the line that makes a server accept it.
///
/// The token is printed once and stored nowhere: this is the only moment
/// it exists in one place, which is the property that makes the file worth
/// keeping only hashes.
fn token(label: &str) -> Result<(), Failure> {
    if label.split_whitespace().count() != 1 {
        return Err(usage("a label is one word, it goes on the line as-is"));
    }
    let (token, line) = quanty_auth::mint(label)
        .map_err(|e| failed(format!("could not read /dev/urandom: {e}")))?;
    emit(&format!("token {token}"))?;
    emit(&format!("line  {line}"))?;
    emit("")?;
    emit("give the token to its owner and append the line to --tokens.")?;
    emit("the token is not stored anywhere; losing it means minting another.")
}

#[cfg(target_os = "linux")]
fn serve(database: &Path, flags: &Flags) -> Result<(), Failure> {
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    use quanty_auth::Tokens;
    use quanty_server::Worker;
    use quanty_service::{Deadlines, Executor};

    if !database.exists() {
        return Err(failed(format!("no database at {}", database.display())));
    }
    let session = open(database)?;

    // No token file means no authentication, which is a real configuration
    // and the reason the default address is loopback (ADR-026).
    let tokens = match &flags.tokens {
        Some(path) => {
            Some(Tokens::load(path).map_err(|e| failed(format!("token file {path}: {e}")))?)
        }
        None => None,
    };

    let addr = flags.listen.as_deref().unwrap_or("127.0.0.1:7878");
    let workers = match flags.workers {
        Some(n) => n,
        None => thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1),
    };

    let probe = TcpListener::bind(addr).map_err(|e| failed(format!("binding {addr}: {e}")))?;
    let bound = probe
        .local_addr()
        .map_err(|e| failed(format!("{addr}: {e}")))?;
    drop(probe);

    let running = Arc::new(AtomicBool::new(true));
    let accepted = Arc::new(AtomicUsize::new(0));
    let live: Arc<Vec<AtomicUsize>> = Arc::new((0..workers).map(|_| AtomicUsize::new(0)).collect());

    match &tokens {
        Some(t) => {
            emit(&format!(
                "requiring a token, {} in force from {}",
                t.len(),
                t.path().display()
            ))?;
            if t.permissive() {
                emit("note: the token file is readable by others; chmod 600 it")?;
            }
        }
        None => emit("no authentication required; keep this on loopback")?,
    }

    // One thread owns the session; every worker submits to it. It is
    // created before the workers and dropped after them, so no handle
    // outlives the executor it points at.
    let executor = Executor::spawn(session, Deadlines::default(), tokens);

    let mut handles = Vec::with_capacity(workers);
    for id in 0..workers {
        let own = quanty_server::bind_reuseport(bound)
            .map_err(|e| failed(format!("worker {id} binding {bound}: {e}")))?;
        own.set_nonblocking(true)
            .map_err(|e| failed(format!("worker {id}: {e}")))?;
        let mut worker = Worker::owning(own, running.clone())
            .map_err(|e| failed(format!("worker {id}: {e}")))?;
        let running = running.clone();
        let accepted = accepted.clone();
        let live = live.clone();
        let dispatch = executor.handle();
        handles.push(thread::spawn(move || {
            while running.load(Ordering::Relaxed) {
                match worker.turn(200, &dispatch) {
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
            worker.shutdown(&dispatch);
        }));
    }

    emit(&format!("listening on {bound}, {workers} workers"))?;
    emit(&format!("serving {}", database.display()))?;

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
