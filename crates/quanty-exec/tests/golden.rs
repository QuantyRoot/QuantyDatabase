//! Golden tests, sqllogictest style.
//!
//! Every file in tests/golden/ is a script. Lines starting with `> ` are
//! statements; the lines that follow, up to the next blank line, are the
//! expected output. Errors render as `error: <message>`. An expected line
//! ending in `...` is a prefix match, everything else must match exactly.
//!
//! Each file runs top to bottom against one fresh in-memory database, so
//! scripts build on their own earlier statements. The extension picks the
//! front end: .qql runs through the QQL parser, .sql through the SQL
//! parser. Same engine underneath, which is the point: the sql_* scripts
//! mirror the logical cases of their QQL counterparts and must produce
//! identical results.

use quanty_core::Db;
use quanty_exec::Session;

struct Case {
    line: usize,
    statement: String,
    expected: Vec<String>,
}

fn parse_script(source: &str) -> Vec<Case> {
    let mut cases = Vec::new();
    let mut lines = source.lines().enumerate().peekable();
    while let Some((idx, line)) = lines.next() {
        let Some(statement) = line.strip_prefix("> ") else {
            continue; // comments and blank lines between cases
        };
        let mut expected = Vec::new();
        while let Some((_, peeked)) = lines.peek() {
            if peeked.trim_end().is_empty() || peeked.starts_with("> ") {
                break;
            }
            expected.push(lines.next().unwrap().1.trim_end().to_string());
        }
        cases.push(Case {
            line: idx + 1,
            statement: statement.to_string(),
            expected,
        });
    }
    cases
}

fn line_matches(expected: &str, actual: &str) -> bool {
    match expected.strip_suffix("...") {
        Some(prefix) => actual.starts_with(prefix),
        None => expected == actual,
    }
}

fn run_file(name: &str, source: &str, sql: bool) -> (u64, Vec<String>) {
    let mut session = Session::new(Db::in_memory().expect("in-memory db"));
    let mut failures = Vec::new();
    let cases = parse_script(source);
    let count = cases.len() as u64;

    for case in cases {
        let result = if sql {
            session.execute_sql(&case.statement)
        } else {
            session.execute(&case.statement)
        };
        let rendered = match result {
            Ok(output) => output.render(),
            Err(e) => format!("error: {e}"),
        };
        let actual: Vec<&str> = if rendered.is_empty() {
            Vec::new()
        } else {
            rendered.lines().collect()
        };
        let ok = actual.len() == case.expected.len()
            && case
                .expected
                .iter()
                .zip(&actual)
                .all(|(e, a)| line_matches(e, a));
        if !ok {
            failures.push(format!(
                "{name}:{}\n  statement: {}\n  expected:  {:?}\n  actual:    {:?}",
                case.line, case.statement, case.expected, actual
            ));
        }
    }
    (count, failures)
}

#[test]
fn golden() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let mut entries: Vec<_> = std::fs::read_dir(&dir)
        .expect("golden dir")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| {
            p.extension()
                .is_some_and(|ext| ext == "qql" || ext == "sql")
        })
        .collect();
    entries.sort();
    assert!(!entries.is_empty(), "no golden files found in {dir:?}");

    let mut total = 0;
    let mut failures = Vec::new();
    for path in &entries {
        let source = std::fs::read_to_string(path).expect("read golden file");
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        let sql = path.extension().is_some_and(|ext| ext == "sql");
        let (count, mut fails) = run_file(&name, &source, sql);
        total += count;
        failures.append(&mut fails);
    }

    println!("golden: {total} cases across {} files", entries.len());
    assert!(
        failures.is_empty(),
        "{} golden case(s) failed:\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
    assert!(
        total >= 150,
        "phase 2 wants 150+ golden cases, found {total}"
    );
}
