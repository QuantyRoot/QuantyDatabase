//! Replacing the tool with a copy of itself, and refusing to.
//!
//! The interesting half is the refusing. A self-updater that installs
//! first and checks later turns a truncated download into a machine with
//! no working tool on it, so every one of these asserts the same second
//! thing: after the failure, the binary that was there still runs.
//!
//! The copy under test is the one that runs, so this exercises the real
//! path -- a program replacing itself -- rather than a special case with a
//! target argument that only tests would ever pass.

mod common;

use std::env::consts::EXE_SUFFIX;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::TestDir;

/// A directory holding a copy of the tool, and a candidate to install.
fn staged(dir: &TestDir) -> (PathBuf, PathBuf) {
    let installed = dir.path().join(format!("quantydb{EXE_SUFFIX}"));
    let candidate = dir.path().join(format!("candidate{EXE_SUFFIX}"));
    fs::copy(env!("CARGO_BIN_EXE_quantydb"), &installed).expect("copy the tool");
    fs::copy(env!("CARGO_BIN_EXE_quantydb"), &candidate).expect("copy the candidate");
    (installed, candidate)
}

/// Whether the binary at `path` still works.
fn still_runs(path: &Path) -> bool {
    Command::new(path)
        .arg("about")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn a_good_binary_is_installed_and_the_old_one_kept() {
    let dir = TestDir::new();
    let (installed, candidate) = staged(&dir);

    let out = Command::new(&installed)
        .args(["update", "--file"])
        .arg(&candidate)
        .arg("--yes")
        .output()
        .expect("the tool runs");
    let said = String::from_utf8_lossy(&out.stdout).to_string();

    assert!(out.status.success(), "update failed: {said}");
    assert!(still_runs(&installed), "the installed tool stopped working");
    assert!(
        dir.path()
            .join(format!("quantydb.old{EXE_SUFFIX}"))
            .exists(),
        "the old binary was not kept: {said}"
    );
    assert!(
        !dir.path()
            .join(format!("quantydb.new{EXE_SUFFIX}"))
            .exists(),
        "a staged file was left behind: {said}"
    );
    assert!(said.contains("sha "), "the checksum was not shown: {said}");
}

#[test]
fn half_a_download_is_refused_and_nothing_is_touched() {
    let dir = TestDir::new();
    let (installed, candidate) = staged(&dir);

    let whole = fs::read(&candidate).expect("read the candidate");
    fs::write(&candidate, &whole[..whole.len() / 3]).expect("truncate it");

    let out = Command::new(&installed)
        .args(["update", "--file"])
        .arg(&candidate)
        .arg("--yes")
        .output()
        .expect("the tool runs");

    assert!(!out.status.success(), "a truncated binary was accepted");
    assert!(still_runs(&installed), "the installed tool stopped working");
    assert!(
        !dir.path()
            .join(format!("quantydb.new{EXE_SUFFIX}"))
            .exists(),
        "a staged file was left behind"
    );
}

#[test]
fn something_that_is_not_the_tool_is_refused() {
    let dir = TestDir::new();
    let (installed, candidate) = staged(&dir);
    fs::write(&candidate, b"<html>404 not found</html>\n").expect("write a page");

    let out = Command::new(&installed)
        .args(["update", "--file"])
        .arg(&candidate)
        .arg("--yes")
        .output()
        .expect("the tool runs");

    assert!(!out.status.success(), "an html page was accepted");
    assert!(still_runs(&installed), "the installed tool stopped working");
}

#[test]
fn a_checksum_that_does_not_match_stops_it() {
    let dir = TestDir::new();
    let (installed, candidate) = staged(&dir);

    let out = Command::new(&installed)
        .args(["update", "--file"])
        .arg(&candidate)
        .args(["--sha256", "00", "--yes"])
        .output()
        .expect("the tool runs");
    let said = String::from_utf8_lossy(&out.stderr).to_string();

    assert!(!out.status.success(), "a wrong checksum was accepted");
    assert!(
        said.contains("checksum does not match"),
        "did not say why: {said}"
    );
    assert!(still_runs(&installed), "the installed tool stopped working");
    assert!(
        !dir.path()
            .join(format!("quantydb.old{EXE_SUFFIX}"))
            .exists(),
        "it moved the old binary aside before checking"
    );
}

#[test]
fn an_empty_file_is_refused_before_anything_is_written() {
    let dir = TestDir::new();
    let (installed, candidate) = staged(&dir);
    fs::write(&candidate, b"").expect("empty it");

    let out = Command::new(&installed)
        .args(["update", "--file"])
        .arg(&candidate)
        .arg("--yes")
        .output()
        .expect("the tool runs");

    assert!(!out.status.success(), "an empty file was accepted");
    assert!(still_runs(&installed), "the installed tool stopped working");
}

/// One byte short, which is the sharpest version of the question.
///
/// It passes only if the length the headers describe is the whole file
/// and not merely something smaller than it, so it is really a test of
/// the header reading for whichever executable format this platform
/// uses: ELF here, Mach-O and PE on the other two runners.
#[test]
fn one_byte_short_is_still_short() {
    let dir = TestDir::new();
    let (installed, candidate) = staged(&dir);

    let whole = fs::read(&candidate).expect("read the candidate");
    fs::write(&candidate, &whole[..whole.len() - 1]).expect("shorten it");

    let out = Command::new(&installed)
        .args(["update", "--file"])
        .arg(&candidate)
        .arg("--yes")
        .output()
        .expect("the tool runs");
    let said = String::from_utf8_lossy(&out.stderr).to_string();

    assert!(
        !out.status.success(),
        "a binary one byte short was accepted"
    );
    assert!(said.contains("incomplete"), "did not say why: {said}");
    assert!(still_runs(&installed));
}

/// And the other direction: something longer than the headers describe is
/// not damaged, it is signed, padded or concatenated. Do not refuse it.
#[test]
fn a_longer_file_than_the_headers_describe_is_fine() {
    let dir = TestDir::new();
    let (installed, candidate) = staged(&dir);

    let mut whole = fs::read(&candidate).expect("read the candidate");
    whole.extend_from_slice(&[0u8; 128]);
    fs::write(&candidate, &whole).expect("pad it");

    let out = Command::new(&installed)
        .args(["update", "--file"])
        .arg(&candidate)
        .arg("--yes")
        .output()
        .expect("the tool runs");

    assert!(
        out.status.success(),
        "a padded binary was refused: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(still_runs(&installed));
}

#[test]
fn without_a_file_it_says_what_it_is_waiting_for() {
    let dir = TestDir::new();
    let (installed, _) = staged(&dir);

    let out = Command::new(&installed)
        .arg("update")
        .output()
        .expect("the tool runs");
    let said = String::from_utf8_lossy(&out.stderr).to_string();

    assert!(!out.status.success(), "update with no file should refuse");
    assert!(
        said.contains("--file"),
        "did not point at the way in: {said}"
    );
    assert!(still_runs(&installed));
}
