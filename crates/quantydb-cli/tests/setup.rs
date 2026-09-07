//! Getting started, and getting rid of it.
//!
//! The assertion worth having here is the one about what is *not* touched.
//! A wizard that writes a database and an uninstall that removes it is a
//! data loss bug with a friendly interface, and a copy of the tool in a
//! downloads folder must not be able to take out the service that runs the
//! installed one.

mod common;

use std::env::consts::EXE_SUFFIX;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use common::TestDir;

/// A directory with its own copy of the tool, so uninstall has something
/// to remove that is not the one the test runner is using.
fn own_copy(dir: &TestDir) -> PathBuf {
    let binary = dir.path().join(format!("quantydb{EXE_SUFFIX}"));
    fs::copy(env!("CARGO_BIN_EXE_quantydb"), &binary).expect("copy the tool");
    binary
}

#[test]
fn setup_writes_a_database_a_token_and_says_how_to_start() {
    let dir = TestDir::new();
    let database = dir.path().join("d.qdb");
    let tokens = dir.path().join("d.tokens");

    let out = Command::new(env!("CARGO_BIN_EXE_quantydb"))
        .arg("setup")
        .arg(&database)
        .arg("--tokens")
        .arg(&tokens)
        .args(["--listen", "127.0.0.1:7878", "--no-service", "--yes"])
        .output()
        .expect("the tool runs");
    let said = String::from_utf8_lossy(&out.stdout).to_string();

    assert!(out.status.success(), "setup failed: {said}");
    assert!(database.exists(), "no database: {said}");
    assert!(tokens.exists(), "no token file: {said}");

    let file = fs::read_to_string(&tokens).expect("read the token file");
    assert_eq!(file.lines().count(), 1, "expected one token line: {file:?}");
    let mut words = file.split_whitespace();
    let hash = words.next().expect("a hash");
    assert_eq!(hash.len(), 64, "not a sha256 hex: {hash}");
    assert_eq!(words.next(), Some("first"), "the label is missing");

    assert!(
        said.contains("quantydb serve"),
        "did not say how to start it: {said}"
    );
    assert!(
        said.contains("quantydb connect"),
        "did not say how to connect: {said}"
    );

    // The token is printed once and stored nowhere: the file holds a hash.
    let printed = said
        .lines()
        .map(str::trim)
        .find(|l| l.len() == 64 && l.chars().all(|c| c.is_ascii_hexdigit()))
        .expect("the token was not printed");
    assert!(
        !file.contains(printed),
        "the token itself was written to the token file"
    );
}

/// A second run adds a token rather than replacing the file, and keeps the
/// database that is already there.
#[test]
fn setup_twice_adds_a_token_and_keeps_the_database() {
    let dir = TestDir::new();
    let database = dir.path().join("d.qdb");
    let tokens = dir.path().join("d.tokens");

    let run = || {
        Command::new(env!("CARGO_BIN_EXE_quantydb"))
            .arg("setup")
            .arg(&database)
            .arg("--tokens")
            .arg(&tokens)
            .args(["--listen", "127.0.0.1:7878", "--no-service", "--yes"])
            .output()
            .expect("the tool runs")
    };

    assert!(run().status.success());
    let first = fs::read_to_string(&tokens).expect("read");
    let stamp = fs::metadata(&database).expect("stat").len();

    let second = run();
    assert!(second.status.success(), "the second run failed");
    let now = fs::read_to_string(&tokens).expect("read");

    assert_eq!(now.lines().count(), 2, "the second token was not added");
    assert!(now.starts_with(&first), "the first token was overwritten");
    assert_eq!(
        fs::metadata(&database).expect("stat").len(),
        stamp,
        "the database was rewritten"
    );
    assert!(
        String::from_utf8_lossy(&second.stdout).contains("using the database"),
        "did not say it was reusing the database"
    );
}

#[cfg(unix)]
#[test]
fn the_token_file_is_not_readable_by_others() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TestDir::new();
    let tokens = dir.path().join("d.tokens");
    let out = Command::new(env!("CARGO_BIN_EXE_quantydb"))
        .arg("setup")
        .arg(dir.path().join("d.qdb"))
        .arg("--tokens")
        .arg(&tokens)
        .args(["--listen", "127.0.0.1:7878", "--no-service", "--yes"])
        .output()
        .expect("the tool runs");
    assert!(out.status.success());

    let mode = fs::metadata(&tokens).expect("stat").permissions().mode();
    assert_eq!(mode & 0o077, 0, "others can read the token file: {mode:o}");
}

#[test]
fn an_address_that_is_not_loopback_is_said_out_loud() {
    let dir = TestDir::new();
    let out = Command::new(env!("CARGO_BIN_EXE_quantydb"))
        .arg("setup")
        .arg(dir.path().join("d.qdb"))
        .arg("--tokens")
        .arg(dir.path().join("d.tokens"))
        .args(["--listen", "0.0.0.0:7878", "--no-service", "--yes"])
        .output()
        .expect("the tool runs");
    let said = String::from_utf8_lossy(&out.stdout).to_string();

    assert!(out.status.success());
    assert!(
        said.contains("not encrypted"),
        "a public address got no warning: {said}"
    );
}

#[test]
fn uninstall_removes_the_binary_and_leaves_the_data() {
    let dir = TestDir::new();
    let binary = own_copy(&dir);
    let database = dir.path().join("d.qdb");
    let tokens = dir.path().join("d.tokens");

    let made = Command::new(&binary)
        .arg("setup")
        .arg(&database)
        .arg("--tokens")
        .arg(&tokens)
        .args(["--listen", "127.0.0.1:7878", "--no-service", "--yes"])
        .output()
        .expect("the tool runs");
    assert!(made.status.success());

    let out = Command::new(&binary)
        .args(["uninstall", "--yes"])
        .output()
        .expect("the tool runs");
    let said = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(out.status.success(), "uninstall failed: {said}");

    assert!(database.exists(), "it took the database: {said}");
    assert!(tokens.exists(), "it took the token file: {said}");
    if cfg!(unix) {
        assert!(!binary.exists(), "the binary is still there: {said}");
    }
}

/// The one that matters. This copy is not the installed one, so whatever a
/// real service unit on this machine points at, it is not this, and
/// uninstall has to leave it alone.
#[test]
fn uninstall_does_not_touch_a_service_that_runs_another_binary() {
    let dir = TestDir::new();
    let binary = own_copy(&dir);

    let out = Command::new(&binary)
        .args(["uninstall", "--yes"])
        .output()
        .expect("the tool runs");
    let said = String::from_utf8_lossy(&out.stdout).to_string();

    assert!(out.status.success(), "uninstall failed: {said}");
    for line in said.lines() {
        if line.trim_start().starts_with("removed /etc/") {
            panic!("a copy in a temporary directory removed a system file: {line}");
        }
    }
}
