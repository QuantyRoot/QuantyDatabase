//! The tool as a user meets it: the built binary, run with arguments.
//!
//! `CARGO_BIN_EXE_quanty` is cargo's path to the binary this crate builds,
//! so these tests exercise argument handling, exit codes and what lands on
//! stdout rather than the library underneath.

mod common;

use std::process::{Command, Output, Stdio};

use common::TestDir;

fn quantydb(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_quantydb"))
        .args(args)
        .output()
        .expect("the binary runs")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

fn fixture(name: &str) -> String {
    format!(
        "{}/../quantydb-sqlite/tests/data/{name}",
        env!("CARGO_MANIFEST_DIR")
    )
}

#[test]
fn import_then_query() {
    let dir = TestDir::new();
    let target = dir.path().join("chinook.qdb");
    let target = target.to_str().unwrap();

    let imported = quantydb(&["import", &fixture("chinook.sqlite"), target]);
    assert!(imported.status.success(), "{}", stderr(&imported));
    let text = stdout(&imported);
    assert!(text.contains("15607 rows"), "{text}");
    assert!(text.contains("11 tables"), "{text}");

    let genres = quantydb(&["run", target, "get Genre { Name } limit 3"]);
    assert!(genres.status.success(), "{}", stderr(&genres));
    assert_eq!(stdout(&genres).lines().count(), 3);

    let tables = quantydb(&["tables", target]);
    assert!(tables.status.success());
    assert!(stdout(&tables).contains("Track"));
}

#[test]
fn the_sql_front_end_is_one_flag_away() {
    let dir = TestDir::new();
    let target = dir.path().join("db.qdb");
    let target = target.to_str().unwrap();
    assert!(quantydb(&["import", &fixture("records.sqlite"), target])
        .status
        .success());

    let qql = quantydb(&["run", target, "get kinds { id } limit 2"]);
    let sql = quantydb(&["run", target, "select id from kinds limit 2", "--sql"]);
    assert!(sql.status.success(), "{}", stderr(&sql));
    assert_eq!(stdout(&qql), stdout(&sql));
}

#[test]
fn a_dry_run_writes_nothing() {
    let dir = TestDir::new();
    let target = dir.path().join("nothing.qdb");

    let output = quantydb(&[
        "import",
        &fixture("records.sqlite"),
        target.to_str().unwrap(),
        "--dry-run",
    ]);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("nothing was written"));
    assert!(!target.exists(), "the dry run created a database");
}

#[test]
fn an_existing_target_is_never_overwritten() {
    let dir = TestDir::new();
    let target = dir.path().join("taken.qdb");
    let target = target.to_str().unwrap();

    assert!(quantydb(&["import", &fixture("records.sqlite"), target])
        .status
        .success());
    let before = std::fs::metadata(target).unwrap().len();

    let second = quantydb(&["import", &fixture("chinook.sqlite"), target]);
    assert!(!second.status.success(), "the second import was allowed");
    assert!(
        stderr(&second).contains("already exists"),
        "{}",
        stderr(&second)
    );
    assert_eq!(
        std::fs::metadata(target).unwrap().len(),
        before,
        "the existing database was touched"
    );
}

#[test]
fn a_database_in_wal_mode_imports_from_both_files() {
    let dir = TestDir::new();
    let target = dir.path().join("wal.qdb");
    let output = quantydb(&[
        "import",
        &fixture("wal_mode.sqlite"),
        target.to_str().unwrap(),
    ]);
    assert!(output.status.success(), "{}", stderr(&output));

    // 20 rows in t plus 200 in grown, the latter existing only in the log
    assert!(stdout(&output).contains("220 rows"), "{}", stdout(&output));
}

#[test]
fn statements_come_from_stdin_too() {
    let dir = TestDir::new();
    let target = dir.path().join("shell.qdb");
    let target = target.to_str().unwrap();
    assert!(quantydb(&["import", &fixture("records.sqlite"), target])
        .status
        .success());

    let mut child = Command::new(env!("CARGO_BIN_EXE_quantydb"))
        .args(["shell", target])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("the shell starts");
    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().unwrap();
        // a comment, a blank line, a good statement and a bad one
        writeln!(stdin, "# this is ignored").unwrap();
        writeln!(stdin).unwrap();
        writeln!(stdin, "get kinds {{ id }} limit 1").unwrap();
        writeln!(stdin, "this is not a statement").unwrap();
    }
    let output = child.wait_with_output().unwrap();

    // the good statement ran, the bad one was reported, and the session
    // carried on to the end
    assert_eq!(stdout(&output).trim(), "1");
    assert!(stderr(&output).contains("1 statement(s) failed"));
    assert!(
        !output.status.success(),
        "a failed statement sets the status"
    );
}

#[test]
fn a_closed_pipe_is_not_a_crash() {
    // `quantydb tables db | head -1` is an ordinary thing to type, and rust
    // ignores sigpipe, so this would panic without the handling in emit
    let dir = TestDir::new();
    let target = dir.path().join("pipe.qdb");
    let target = target.to_str().unwrap();
    assert!(quantydb(&["import", &fixture("chinook.sqlite"), target])
        .status
        .success());

    let mut listing = Command::new(env!("CARGO_BIN_EXE_quantydb"))
        .args(["tables", target])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let head = Command::new("head")
        .args(["-1"])
        .stdin(listing.stdout.take().unwrap())
        .output()
        .unwrap();

    assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), "Album");

    // and the writer itself ended cleanly rather than dying of a panic
    let status = listing.wait().unwrap();
    assert!(
        status.success(),
        "writing into a closed pipe ended with {status}"
    );
}

#[test]
fn wrong_arguments_explain_themselves() {
    let no_command = quantydb(&[]);
    assert_eq!(no_command.status.code(), Some(2));
    assert!(stderr(&no_command).contains("usage:"));

    let unknown = quantydb(&["frobnicate", "x"]);
    assert_eq!(unknown.status.code(), Some(2));
    assert!(stderr(&unknown).contains("unknown command frobnicate"));

    let bad_flag = quantydb(&["tables", "x.qdb", "--turbo"]);
    assert_eq!(bad_flag.status.code(), Some(2));
    assert!(stderr(&bad_flag).contains("unknown option --turbo"));

    let too_few = quantydb(&["import", "only-one.sqlite"]);
    assert_eq!(too_few.status.code(), Some(2));

    let help = quantydb(&["--help"]);
    assert_eq!(
        help.status.code(),
        Some(2),
        "help is not an error, but it is not a result either"
    );
    assert!(stderr(&help).contains("quantydb import"));
}

#[test]
fn a_missing_file_says_so_without_a_backtrace() {
    let missing = quantydb(&["run", "/nonexistent/db.qdb", "show tables"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(stderr(&missing).contains("does not exist"));
    assert!(!stderr(&missing).contains("panicked"));

    let not_sqlite = quantydb(&[
        "import",
        env!("CARGO_MANIFEST_DIR"),
        "/tmp/quantydb-cli-should-not-exist.qdb",
    ]);
    assert_eq!(not_sqlite.status.code(), Some(1));
    assert!(!stderr(&not_sqlite).contains("panicked"));
}

#[test]
fn an_empty_database_can_be_made_and_used() {
    let dir = TestDir::new();
    let target = dir.path().join("fresh.qdb");
    let path = target.to_str().unwrap();

    let made = quantydb(&["create", path]);
    assert!(made.status.success(), "{}", stderr(&made));
    assert!(target.exists());
    assert!(stdout(&made).contains("created"));

    // and it is a working database, not just a file
    let used = quantydb(&["run", path, "table t { id: int @key, v: text }"]);
    assert!(used.status.success(), "{}", stderr(&used));
    let listed = quantydb(&["tables", path]);
    assert_eq!(stdout(&listed).trim(), "t");
}

#[test]
fn create_refuses_to_touch_an_existing_database() {
    // a create that quietly reopened an existing file would be a way to
    // lose a database by typing a name twice
    let dir = TestDir::new();
    let target = dir.path().join("twice.qdb");
    let path = target.to_str().unwrap();
    assert!(quantydb(&["create", path]).status.success());

    let again = quantydb(&["create", path]);
    assert!(!again.status.success());
    assert!(stderr(&again).contains("already exists"));
}

#[test]
fn a_missing_database_is_not_created_by_accident() {
    // sqlite creates a database when you open a path that is not there,
    // which turns a typo into an empty database that answers every query
    // with nothing. run and shell refuse instead.
    let dir = TestDir::new();
    let typo = dir.path().join("typo.qdb");
    let output = quantydb(&["run", typo.to_str().unwrap(), "show tables"]);

    assert!(!output.status.success());
    assert!(stderr(&output).contains("does not exist"));
    assert!(!typo.exists(), "a database was created by a failed read");
}

// ---------------------------------------------------------------------------
// branch verbs (ADR-032)
// ---------------------------------------------------------------------------

/// A database with one table and one row, and the path to it.
fn seeded(dir: &TestDir) -> String {
    let path = dir
        .path()
        .join("branches.qdb")
        .to_str()
        .unwrap()
        .to_string();
    assert!(quantydb(&["create", &path]).status.success());
    assert!(
        quantydb(&["run", &path, "table users { id: int @key, name: text }"])
            .status
            .success()
    );
    assert!(
        quantydb(&["run", &path, "put users { id: 1, name: \"ada\" }"])
            .status
            .success()
    );
    path
}

#[test]
fn a_branch_can_be_made_switched_to_and_merged_back() {
    let dir = TestDir::new();
    let path = seeded(&dir);

    assert!(quantydb(&["branch", &path, "risky"]).status.success());

    let listed = quantydb(&["branches", &path]);
    let text = stdout(&listed);
    assert!(text.contains("* main"), "main is not marked: {text}");
    assert!(text.contains("risky"), "the branch is missing: {text}");

    assert!(quantydb(&["switch", &path, "risky"]).status.success());
    assert!(
        quantydb(&["run", &path, "put users { id: 2, name: \"grace\" }"])
            .status
            .success()
    );

    assert!(quantydb(&["switch", &path, "main"]).status.success());
    // main has not seen the write yet
    assert_eq!(
        stdout(&quantydb(&["run", &path, "get users { id }"])).trim(),
        "1"
    );

    let merged = quantydb(&["merge", &path, "risky"]);
    assert!(merged.status.success(), "{}", stderr(&merged));
    let rows = stdout(&quantydb(&["run", &path, "get users { id }"]));
    assert_eq!(rows.trim(), "1\n2", "the merge did not bring the row over");
}

#[test]
fn log_prints_one_line_per_commit_of_this_branch() {
    let dir = TestDir::new();
    let path = seeded(&dir);

    let before = stdout(&quantydb(&["log", &path])).lines().count();
    assert!(
        quantydb(&["run", &path, "put users { id: 2, name: \"grace\" }"])
            .status
            .success()
    );
    let after = stdout(&quantydb(&["log", &path]));
    assert_eq!(
        after.lines().count(),
        before + 1,
        "log did not grow: {after}"
    );
    assert!(after.contains("commit "), "not commits: {after}");
}

#[test]
fn branch_at_forks_from_an_older_commit() {
    let dir = TestDir::new();
    let path = seeded(&dir);

    assert!(quantydb(&["branch", &path, "early", "--at", "1"])
        .status
        .success());
    let listed = stdout(&quantydb(&["branches", &path]));
    assert!(listed.contains("early @1"), "not forked at 1: {listed}");

    let bad = quantydb(&["branch", &path, "nope", "--at", "zwei"]);
    assert!(!bad.status.success());
    assert!(stderr(&bad).contains("--at wants a commit id"));
}

#[test]
fn a_branch_name_meets_the_engines_rule_not_the_parsers() {
    // The whole point of building the statement instead of printing one:
    // the name never becomes text, so a bad one is refused by the thing
    // that owns the rule. Proved to catch: passing this through `run` as
    // text answers with a parse error about a stray token instead.
    let dir = TestDir::new();
    let path = seeded(&dir);

    let spaced = quantydb(&["branch", &path, "a b"]);
    assert!(!spaced.status.success());
    assert!(
        stderr(&spaced).contains("branch names are 1 to 64"),
        "not the engine's message: {}",
        stderr(&spaced)
    );

    let hostile = quantydb(&["branch", &path, "x\"; drop table users"]);
    assert!(!hostile.status.success());
    assert_eq!(
        stdout(&quantydb(&["tables", &path])).trim(),
        "users",
        "the table went away"
    );
}

#[test]
fn the_sql_flag_does_not_reach_a_statement_the_tool_wrote() {
    // Structurally true while the verb builds an AST, so the thing this
    // guards is the refactor back to text. Proved to catch: routing this
    // through run_statement with the caller's flags reads `show branches`
    // as SQL and fails.
    let dir = TestDir::new();
    let path = seeded(&dir);

    let listed = quantydb(&["branches", &path, "--sql"]);
    assert!(listed.status.success(), "{}", stderr(&listed));
    assert!(stdout(&listed).contains("main"));
}

#[test]
fn branch_flag_says_why_it_does_not_exist() {
    let dir = TestDir::new();
    let path = seeded(&dir);

    let tried = quantydb(&["run", &path, "get users { id }", "--branch", "risky"]);
    assert!(!tried.status.success());
    let message = stderr(&tried);
    assert!(
        message.contains("--branch does not exist"),
        "not named: {message}"
    );
    assert!(message.contains("switch"), "no way out offered: {message}");
}

#[test]
fn the_branch_verbs_want_their_arguments() {
    let dir = TestDir::new();
    let path = seeded(&dir);

    for args in [
        vec!["branch", path.as_str()],
        vec!["switch", path.as_str()],
        vec!["merge", path.as_str()],
        vec!["branches", path.as_str(), "extra"],
        vec!["log", path.as_str(), "extra"],
    ] {
        let out = quantydb(&args);
        assert!(!out.status.success(), "{args:?} should not succeed");
        assert!(stderr(&out).contains("usage:"), "{args:?}");
    }
}

// ---------------------------------------------------------------------------
// stats and gc
// ---------------------------------------------------------------------------

fn stat_line(text: &str, field: &str) -> u64 {
    text.lines()
        .find_map(|l| l.strip_prefix(field))
        .unwrap_or_else(|| panic!("no {field} line in {text}"))
        .trim()
        .parse()
        .expect("a number")
}

#[test]
fn stats_describes_the_file_that_is_actually_there() {
    let dir = TestDir::new();
    let path = seeded(&dir);

    let text = stdout(&quantydb(&["stats", &path]));
    let on_disk = std::fs::metadata(&path).unwrap().len();

    assert_eq!(
        stat_line(&text, "bytes"),
        on_disk,
        "stats and the file disagree: {text}"
    );
    assert_eq!(
        stat_line(&text, "pages") * stat_line(&text, "page size"),
        on_disk
    );
    assert!(stat_line(&text, "head pages") > 0, "no live pages: {text}");
}

#[test]
fn gc_frees_pages_and_stats_shows_it() {
    let dir = TestDir::new();
    let path = seeded(&dir);
    for i in 2..8 {
        assert!(quantydb(&[
            "run",
            &path,
            &format!("put users {{ id: {i}, name: \"u{i}\" }}")
        ])
        .status
        .success());
    }

    assert_eq!(
        stat_line(&stdout(&quantydb(&["stats", &path])), "free pages"),
        0
    );

    let run = quantydb(&["gc", &path, "2"]);
    assert!(run.status.success(), "{}", stderr(&run));
    assert!(stdout(&run).contains("pruned"), "{}", stdout(&run));

    let after = stdout(&quantydb(&["stats", &path]));
    assert!(
        stat_line(&after, "free pages") > 0,
        "gc reported work but freed nothing: {after}"
    );
    // the rows are still there
    assert_eq!(
        stdout(&quantydb(&["run", &path, "get users { id }"]))
            .lines()
            .count(),
        7
    );
}

#[test]
fn gc_wants_a_number_and_refuses_zero() {
    let dir = TestDir::new();
    let path = seeded(&dir);

    let words = quantydb(&["gc", &path, "viele"]);
    assert!(!words.status.success());
    assert!(stderr(&words).contains("number of commits"));

    let zero = quantydb(&["gc", &path, "0"]);
    assert!(!zero.status.success());
    assert!(stderr(&zero).contains("at least one commit"));

    let missing = quantydb(&["gc", &path]);
    assert!(!missing.status.success());
    assert!(stderr(&missing).contains("usage:"));
}

#[test]
fn show_stats_is_a_statement_too_not_only_a_verb() {
    // The verb is sugar; the capability has to be reachable through run
    // and therefore over the wire, like every other one here. The rule
    // about transactions lives with the executor, which is where a
    // session outlives a single statement.
    let dir = TestDir::new();
    let path = seeded(&dir);

    let out = quantydb(&["run", &path, "show stats"]);
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(stdout(&out).contains("page size"));
    assert_eq!(stdout(&out), stdout(&quantydb(&["stats", &path])));
}
