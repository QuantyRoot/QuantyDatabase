//! The claims `quanty about` makes, checked against the repository.
//!
//! A tool that advertises its own numbers will drift from them, and the
//! drift is silent because nothing looks. So the numbers are floors rather
//! than snapshots: the counts here may fall behind reality and never
//! exceed it, which means nobody has to touch them on an ordinary commit
//! and nobody can quietly overclaim on a loud one.
//!
//! The dependency count is the exception and is checked exactly. It is the
//! one claim the project makes that is worth nothing if it is approximate.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The repository root, from this crate's manifest.
fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("the workspace root is two levels up")
        .to_path_buf()
}

fn about() -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_quanty"))
        .arg("about")
        .output()
        .expect("the binary runs");
    assert!(out.status.success(), "about failed");
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Every package in the lock file, by name.
fn locked_packages() -> Vec<String> {
    let text = fs::read_to_string(root().join("Cargo.lock")).expect("read Cargo.lock");
    text.lines()
        .filter_map(|l| l.strip_prefix("name = \""))
        .filter_map(|l| l.strip_suffix('"'))
        .map(|s| s.to_string())
        .collect()
}

fn count_in_tree(matches: &dyn Fn(&Path) -> bool, needle: Option<&str>) -> usize {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                walk(&path, out);
            } else {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    walk(&root().join("crates"), &mut files);
    files
        .iter()
        .filter(|p| matches(p))
        .map(|p| match needle {
            None => 1,
            Some(needle) => fs::read_to_string(p)
                .map(|t| t.matches(needle).count())
                .unwrap_or(0),
        })
        .sum()
}

/// The claim the whole project rests on, checked exactly.
#[test]
fn the_lock_file_holds_nothing_but_this_workspace() {
    let packages = locked_packages();
    let foreign: Vec<&String> = packages
        .iter()
        .filter(|n| !n.starts_with("quanty"))
        .collect();
    assert_eq!(
        foreign.len(),
        quanty_cli_claims::FOREIGN_DEPENDENCIES,
        "the lock file grew a dependency: {foreign:?}"
    );
    assert!(
        !packages.is_empty(),
        "the lock file could not be read, so this test proved nothing"
    );
    assert!(about().contains("dependencies   0"));
}

#[test]
fn about_never_claims_more_crates_than_exist() {
    let crates = fs::read_dir(root().join("crates"))
        .expect("read crates")
        .flatten()
        .filter(|e| e.path().is_dir())
        .count();
    assert!(
        crates >= quanty_cli_claims::CRATES,
        "about claims {} crates, {crates} exist",
        quanty_cli_claims::CRATES
    );
    assert!(about().contains(&format!("crates         {}", quanty_cli_claims::CRATES)));
}

#[test]
fn about_never_claims_more_tests_than_exist() {
    let tests = count_in_tree(
        &|p| p.extension().is_some_and(|e| e == "rs"),
        Some("#[test]"),
    );
    assert!(
        tests >= quanty_cli_claims::TESTS,
        "about claims {} test functions, {tests} exist",
        quanty_cli_claims::TESTS
    );
}

#[test]
fn about_never_claims_more_decisions_than_exist() {
    let text = fs::read_to_string(root().join("docs/DECISIONS.md")).expect("read DECISIONS.md");
    let decisions = text.lines().filter(|l| l.starts_with("## ADR-")).count();
    assert!(
        decisions >= quanty_cli_claims::DECISIONS,
        "about claims {} decisions, {decisions} exist",
        quanty_cli_claims::DECISIONS
    );
}

/// The numbers `about` prints, mirrored here because an integration test
/// cannot reach into a binary crate.
mod quanty_cli_claims {
    pub const CRATES: usize = 14;
    pub const TESTS: usize = 500;
    pub const DECISIONS: usize = 36;
    pub const FOREIGN_DEPENDENCIES: usize = 0;
}

/// Every internal dependency names a version, not just a path.
///
/// Cargo does not need one: a path is enough to build. crates.io refuses
/// the package outright, and it refuses it at publish time, which is
/// after some of the other crates have already gone up.
///
/// A version that disagrees with the workspace is a different problem and
/// not this one. Cargo catches that itself, and harder: while the major
/// version is zero, `^0.3.0` does not match `0.4.0`, so the build stops
/// before any test runs. This checks the case cargo is happy with.
#[test]
fn every_internal_dependency_names_a_version() {
    let manifest = fs::read_to_string(root().join("Cargo.toml")).expect("read Cargo.toml");

    let entries: Vec<&str> = manifest
        .lines()
        .skip_while(|l| l.trim() != "[workspace.dependencies]")
        .skip(1)
        .take_while(|l| !l.trim_start().starts_with('['))
        .filter(|l| l.contains("path = "))
        .collect();

    assert!(
        !entries.is_empty(),
        "no internal dependencies found, so this test proved nothing"
    );
    let naked: Vec<&&str> = entries.iter().filter(|l| !l.contains("version")).collect();
    assert!(
        naked.is_empty(),
        "these build fine and cannot be published: {naked:?}"
    );
}

/// Every crate that goes to crates.io needs a description there.
#[test]
fn every_crate_says_what_it_is() {
    let mut missing = Vec::new();
    for entry in fs::read_dir(root().join("crates")).expect("read crates") {
        let path = entry.expect("entry").path().join("Cargo.toml");
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        if !text
            .lines()
            .any(|l| l.trim_start().starts_with("description"))
        {
            missing.push(path.display().to_string());
        }
    }
    assert!(
        missing.is_empty(),
        "crates.io refuses a package with no description: {missing:?}"
    );
}
