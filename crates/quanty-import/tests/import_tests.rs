//! Importing a real database, end to end.
//!
//! The reader is verified against sqlite's own digests elsewhere. What is
//! checked here is the other half: that what the reader produces arrives in
//! a QuantyDB database intact, through the plan the first pass decided, and
//! that reading it back gives the same values.

mod common;

use common::TestDir;
use quanty_core::{Db, Value};
use quanty_exec::Session;
use quanty_import::{execute, plan, Options};
use quanty_sqlite::{FileSource, Reader, SliceSource};

fn data_path(name: &str) -> String {
    format!(
        "{}/../quanty-sqlite/tests/data/{name}",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// Import a fixture into a fresh database and hand back the session.
fn import(
    fixture: &str,
    dir: &TestDir,
) -> (Session<quanty_core::FileStorage>, quanty_import::Report) {
    let reader = Reader::open(FileSource::open(data_path(fixture)).unwrap()).unwrap();
    let plan = plan(&reader, &Options::default()).unwrap();
    let db = Db::create_file(dir.path().join("imported.qdb")).unwrap();
    let mut session = Session::new(db);
    let report = execute(&reader, &plan, &mut session).expect("the import runs");
    (session, report)
}

/// One column of every row of a table, in whatever order the database
/// returns them.
fn column(
    session: &mut Session<quanty_core::FileStorage>,
    table: &str,
    column: &str,
) -> Vec<String> {
    let output = session
        .execute(&format!("get {table} {{ {column} }}"))
        .unwrap_or_else(|e| panic!("reading {table}.{column}: {e}"));
    // the renderer prints one row per line and no header
    output
        .render()
        .lines()
        .map(|line| line.trim().to_string())
        .collect()
}

#[test]
fn chinook_arrives_with_every_row() {
    let dir = TestDir::new();
    let (mut session, report) = import("chinook.sqlite", &dir);

    assert_eq!(report.rows(), 15607, "every row of the source");
    let mut tables: Vec<(&str, u64)> = report
        .tables
        .iter()
        .map(|t| (t.name.as_str(), t.rows))
        .collect();
    tables.sort();
    assert_eq!(
        tables,
        vec![
            ("Album", 347),
            ("Artist", 275),
            ("Customer", 59),
            ("Employee", 8),
            ("Genre", 25),
            ("Invoice", 412),
            ("InvoiceLine", 2240),
            ("MediaType", 5),
            ("Playlist", 18),
            ("PlaylistTrack", 8715),
            ("Track", 3503),
        ]
    );

    // and the database itself agrees, read back through the query language
    let names = column(&mut session, "Genre", "Name");
    assert_eq!(names.len(), 25);
    assert!(names.iter().any(|n| n.contains("Rock")));
}

#[test]
fn values_survive_the_trip() {
    let dir = TestDir::new();
    let (mut session, _) = import("chinook.sqlite", &dir);

    // a known row, all the way through: read it back by its key
    let output = session
        .execute("get Track { Name, Milliseconds, UnitPrice } where TrackId = 1")
        .unwrap();
    let text = output.render();
    assert!(
        text.contains("For Those About To Rock (We Salute You)"),
        "{text}"
    );
    assert!(text.contains("343719"), "{text}");
    assert!(text.contains("0.99"), "{text}");
}

#[test]
fn a_rowid_alias_becomes_the_key_and_keeps_its_value() {
    let dir = TestDir::new();
    let (mut session, _) = import("chinook.sqlite", &dir);

    // AlbumId is `integer primary key`, stored as NULL in the record with
    // the value in the cell's rowid. it has to arrive as the real number.
    let ids = column(&mut session, "Album", "AlbumId");
    assert_eq!(ids.len(), 347);
    assert!(ids.iter().any(|id| id == "1"), "the first album is there");
    assert!(ids.iter().any(|id| id == "347"), "and the last one");
    assert!(
        !ids.iter().any(|id| id == "null" || id.is_empty()),
        "no key came through as null"
    );
}

#[test]
fn a_table_without_a_rowid_imports_the_same_way() {
    let dir = TestDir::new();
    let (mut session, report) = import("without_rowid.sqlite", &dir);

    let kv = report
        .tables
        .iter()
        .find(|t| t.source_name == "kv")
        .expect("kv was imported");
    assert_eq!(kv.rows, 500);

    let keys = column(&mut session, &kv.name, "k");
    assert_eq!(keys.len(), 500);
    assert!(keys.iter().any(|k| k == "key-0000"));
    assert!(keys.iter().any(|k| k == "key-0499"));
}

#[test]
fn a_database_that_needs_its_log_imports_from_both_files() {
    let dir = TestDir::new();
    let reader = quanty_sqlite::open_file(data_path("wal_mode.sqlite")).unwrap();
    let plan = plan(&reader, &Options::default()).unwrap();
    let db = Db::create_file(dir.path().join("wal.qdb")).unwrap();
    let mut session = Session::new(db);
    let report = execute(&reader, &plan, &mut session).unwrap();

    // grown exists only in the log, and the rolled back rows are not there
    let grown = report
        .tables
        .iter()
        .find(|t| t.source_name == "grown")
        .expect("the table from the log was imported");
    assert_eq!(grown.rows, 200);

    let values = column(&mut session, &grown.name, "v");
    assert!(values.iter().any(|v| v.contains("wal-only-001")));
    assert!(
        !values.iter().any(|v| v.contains("never-committed")),
        "a rolled back row was imported"
    );
}

#[test]
fn text_in_another_encoding_arrives_as_the_same_text() {
    let dir = TestDir::new();
    let (mut session, report) = import("utf16be.sqlite", &dir);
    assert_eq!(report.rows(), 8);

    let values = column(&mut session, &report.tables[0].name, "v");
    assert!(values.iter().any(|v| v.contains("\u{1f600}")), "{values:?}");
    assert!(values
        .iter()
        .any(|v| v.contains("\u{65e5}\u{672c}\u{8a9e}")));
}

#[test]
fn the_import_writes_nothing_a_second_time() {
    // running the same import twice into the same database must fail on the
    // table already existing rather than quietly doubling the rows
    let dir = TestDir::new();
    let reader = Reader::open(FileSource::open(data_path("chinook.sqlite")).unwrap()).unwrap();
    let plan = plan(&reader, &Options::default()).unwrap();
    let db = Db::create_file(dir.path().join("twice.qdb")).unwrap();
    let mut session = Session::new(db);

    execute(&reader, &plan, &mut session).unwrap();
    let second = execute(&reader, &plan, &mut session);
    assert!(second.is_err(), "the second import was allowed");
}

#[test]
fn a_source_that_changed_between_the_passes_is_caught() {
    // the plan counts rows; if the file no longer holds that many, the two
    // passes disagree and the import stops rather than writing a table that
    // does not match its own plan
    let bytes = std::fs::read(data_path("records.sqlite")).unwrap();
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let mut plan = plan(&reader, &Options::default()).unwrap();
    plan.tables[0].rows += 1;

    let dir = TestDir::new();
    let db = Db::create_file(dir.path().join("changed.qdb")).unwrap();
    let mut session = Session::new(db);
    let err = execute(&reader, &plan, &mut session)
        .expect_err("a plan that disagrees with the file was accepted");
    assert!(err.to_string().contains("counted"), "message was: {err}");
}

/// Every value of every row, read back out of the imported database.
fn imported_rows(
    session: &mut Session<quanty_core::FileStorage>,
    table: &str,
    columns: &[String],
) -> Vec<Vec<Value>> {
    let list = columns.join(", ");
    match session
        .execute(&format!("get {table} {{ {list} }}"))
        .unwrap_or_else(|e| panic!("reading {table}: {e}"))
    {
        quanty_exec::Output::Rows(rows) => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

/// The same rows as the source holds them, seen through the reader.
///
/// The reader is verified against sqlite's own digests elsewhere, row for
/// row over all 15607 of them. So comparing the imported database against
/// the reader closes the chain: source to reader is checked against sqlite,
/// reader to database is checked here, and nothing in between is taken on
/// trust.
fn source_rows(
    reader: &Reader<FileSource>,
    plan: &quanty_import::ImportPlan,
    table: &quanty_import::TablePlan,
) -> Vec<Vec<Value>> {
    let schema = reader.schema().unwrap();
    let object = schema.object(&table.source_name).unwrap();
    let def = object.table_def().unwrap();
    let layout = quanty_sqlite::RowLayout::new(&def);
    let _ = plan;

    let mut out = Vec::new();
    for row in reader.rows(table.root_page).unwrap() {
        let row = row.unwrap();
        let mut values = Vec::with_capacity(table.columns.len());
        for column in &table.columns {
            let value = match column.source {
                quanty_import::ValueSource::Rowid => Value::Int(row.rowid.unwrap()),
                quanty_import::ValueSource::Declared(index) => match layout.cell(&row, index) {
                    quanty_sqlite::MappedCell::Rowid(id) => Value::Int(id),
                    quanty_sqlite::MappedCell::Missing => {
                        column.default.clone().unwrap_or(Value::Null)
                    }
                    quanty_sqlite::MappedCell::Virtual => panic!("a virtual column was planned"),
                    quanty_sqlite::MappedCell::Value(value) => convert(value, column.ty),
                },
            };
            values.push(value);
        }
        out.push(values);
    }
    out
}

/// The conversion the importer documents, written out a second time so the
/// test does not simply agree with the code it is checking.
fn convert(value: &quanty_sqlite::SqliteValue, ty: quanty_ql::ast::TypeName) -> Value {
    use quanty_ql::ast::TypeName;
    use quanty_sqlite::SqliteValue as S;
    match (value, ty) {
        (S::Null, _) => Value::Null,
        (S::Integer(n), TypeName::Int) => Value::Int(*n),
        (S::Integer(n), TypeName::Float) => Value::Float(*n as f64),
        (S::Integer(n), TypeName::Text) => Value::Text(n.to_string()),
        (S::Integer(n), TypeName::Bytes) => Value::Bytes(n.to_string().into_bytes()),
        (S::Real(f), TypeName::Float) => Value::Float(*f),
        (S::Real(f), TypeName::Text) => Value::Text(format!("{f}")),
        (S::Real(f), TypeName::Bytes) => Value::Bytes(format!("{f}").into_bytes()),
        (S::Text(t), TypeName::Text) => Value::Text(t.clone()),
        (S::Text(t), TypeName::Bytes) => Value::Bytes(t.clone().into_bytes()),
        (S::Blob(b), TypeName::Bytes) => Value::Bytes(b.clone()),
        (value, ty) => panic!("no conversion from {value:?} to {ty:?}"),
    }
}

#[test]
fn every_imported_row_matches_the_source() {
    let dir = TestDir::new();
    let reader = Reader::open(FileSource::open(data_path("chinook.sqlite")).unwrap()).unwrap();
    let plan = plan(&reader, &Options::default()).unwrap();
    let db = Db::create_file(dir.path().join("verify.qdb")).unwrap();
    let mut session = Session::new(db);
    execute(&reader, &plan, &mut session).unwrap();

    let mut checked = 0u64;
    for table in &plan.tables {
        let columns: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();
        let mut expected = source_rows(&reader, &plan, table);
        let mut actual = imported_rows(&mut session, &table.name, &columns);

        assert_eq!(
            actual.len(),
            expected.len(),
            "{}: row counts differ",
            table.name
        );

        // a composite key orders rows differently from a rowid, so both
        // sides are sorted before comparing rather than assuming an order
        let key = |row: &Vec<Value>| format!("{row:?}");
        expected.sort_by_key(key);
        actual.sort_by_key(key);

        for (index, (got, want)) in actual.iter().zip(&expected).enumerate() {
            assert_eq!(
                got, want,
                "{}: row {index} differs\n  imported: {got:?}\n  source:   {want:?}",
                table.name
            );
            checked += 1;
        }
    }
    assert_eq!(checked, 15607, "every row of chinook was compared");
}
