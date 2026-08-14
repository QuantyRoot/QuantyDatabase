//! What a SQLite database is decided to become, before anything is
//! written.
//!
//! The fixtures are the same real files the reader is tested against, so
//! these are decisions about databases somebody else wrote, not about ones
//! shaped to suit us.

use quanty_import::{plan, ImportPlan, Note, Options, Problem, ValueSource};
use quanty_ql::ast::TypeName;
use quanty_sqlite::{Reader, SliceSource};

fn fixture(name: &str) -> Vec<u8> {
    let path = format!(
        "{}/../quanty-sqlite/tests/data/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read(&path).unwrap_or_else(|e| panic!("fixture {path}: {e}"))
}

fn plan_for(name: &str, options: &Options) -> ImportPlan {
    let bytes = fixture(name);
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    plan(&reader, options).unwrap()
}

fn table<'a>(plan: &'a ImportPlan, name: &str) -> &'a quanty_import::TablePlan {
    plan.tables
        .iter()
        .find(|t| t.source_name.eq_ignore_ascii_case(name))
        .unwrap_or_else(|| panic!("no plan for table {name}"))
}

fn column<'a>(table: &'a quanty_import::TablePlan, name: &str) -> &'a quanty_import::ColumnPlan {
    table
        .columns
        .iter()
        .find(|c| c.source_name.eq_ignore_ascii_case(name))
        .unwrap_or_else(|| panic!("no plan for column {name}"))
}

// ---------------------------------------------------------------------------
// a real database
// ---------------------------------------------------------------------------

#[test]
fn chinook_plans_without_a_single_problem() {
    let plan = plan_for("chinook.sqlite", &Options::default());
    assert!(
        plan.is_runnable(),
        "chinook should import as it is: {:?}",
        plan.problems
    );
    assert_eq!(plan.tables.len(), 11);
    assert_eq!(plan.rows(), 15607);

    // no table name needed changing
    assert!(!plan.notes.iter().any(|n| matches!(n, Note::Renamed { .. })));
}

#[test]
fn the_declared_type_and_the_data_together_decide_the_column() {
    let plan = plan_for("chinook.sqlite", &Options::default());
    let track = table(&plan, "Track");

    // an integer primary key is the key, and it comes from the rowid
    let id = column(track, "TrackId");
    assert_eq!(id.ty, TypeName::Int);
    assert!(id.key);
    assert!(!id.nullable);
    assert_eq!(id.source, ValueSource::Declared(0));

    // NVARCHAR is text and holds text
    assert_eq!(column(track, "Name").ty, TypeName::Text);
    // Composer is text with nulls in it
    assert!(column(track, "Composer").nullable);
    assert!(!column(track, "Name").nullable);

    // NUMERIC(10,2) holding reals becomes float, not int: the declaration
    // alone would have said either
    assert_eq!(column(track, "UnitPrice").ty, TypeName::Float);

    // DATETIME is numeric affinity and holds text, so it stays text. the
    // declaration alone would have built a number column here.
    let employee = table(&plan, "Employee");
    assert_eq!(column(employee, "BirthDate").ty, TypeName::Text);
}

#[test]
fn a_composite_primary_key_becomes_a_composite_key() {
    let plan = plan_for("chinook.sqlite", &Options::default());
    let playlist_track = table(&plan, "PlaylistTrack");

    let keys: Vec<&str> = playlist_track
        .key_columns()
        .map(|c| c.name.as_str())
        .collect();
    assert_eq!(keys, vec!["PlaylistId", "TrackId"]);
    assert!(playlist_track.columns.iter().all(|c| !c.nullable));
    // no rowid column had to be invented for it
    assert!(playlist_track
        .columns
        .iter()
        .all(|c| c.source != ValueSource::Rowid));
}

#[test]
fn single_column_indexes_carry_over() {
    let plan = plan_for("chinook.sqlite", &Options::default());
    // chinook indexes the foreign keys, one column each
    let track = table(&plan, "Track");
    assert!(
        column(track, "AlbumId").indexed,
        "IFK_TrackAlbumId covers one column"
    );
    // a key column does not need a second index on top of it
    assert!(!column(track, "TrackId").indexed);
}

// ---------------------------------------------------------------------------
// the shapes that need a decision
// ---------------------------------------------------------------------------

#[test]
fn a_table_without_a_usable_key_gets_its_rowid() {
    let plan = plan_for("shapes.sqlite", &Options::default());

    let nopk = table(&plan, "nopk");
    let key: Vec<&quanty_import::ColumnPlan> = nopk.key_columns().collect();
    assert_eq!(key.len(), 1);
    assert_eq!(key[0].source, ValueSource::Rowid);
    assert_eq!(key[0].ty, TypeName::Int);
    assert!(!key[0].nullable);
    // the columns it did have are still there, after the added one
    assert_eq!(nopk.columns.len(), 3);

    // a primary key holding null cannot be ours, so the rowid steps in and
    // the declared key stays as ordinary data
    let nullable_pk = table(&plan, "nullable_pk");
    let key: Vec<&quanty_import::ColumnPlan> = nullable_pk.key_columns().collect();
    assert_eq!(key.len(), 1);
    assert_eq!(key[0].source, ValueSource::Rowid);
    let k = column(nullable_pk, "k");
    assert!(!k.key);
    assert!(k.nullable, "it really does hold null");

    assert!(plan.notes.iter().any(|n| matches!(
        n,
        Note::AddedRowidKey { table, reason, .. }
            if table == "nullable_pk" && reason.contains("null")
    )));
}

#[test]
fn a_virtual_column_is_skipped_and_a_stored_one_is_not() {
    let plan = plan_for("shapes.sqlite", &Options::default());
    let generated = table(&plan, "generated");

    let names: Vec<&str> = generated.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["id", "a", "s", "z"],
        "v holds nothing in the file"
    );

    // and the columns that remain still point at the right declared
    // positions, which is what keeps s's value out of z
    assert_eq!(column(generated, "s").source, ValueSource::Declared(3));
    assert_eq!(column(generated, "z").source, ValueSource::Declared(4));

    assert!(plan.notes.iter().any(|n| matches!(
        n,
        Note::Skipped { what, reason } if what.ends_with(".v") && reason.contains("virtual")
    )));
}

#[test]
fn a_default_fills_the_rows_that_predate_its_column() {
    let plan = plan_for("shapes.sqlite", &Options::default());
    let grown = table(&plan, "grown");

    // b was added with a default, so the two older rows are not nulls
    let b = column(grown, "b");
    assert_eq!(b.ty, TypeName::Text);
    assert_eq!(b.default, Some(quanty_core::Value::Text("fallback".into())));
    assert!(!b.nullable, "the default covers the rows that predate it");

    // c was added without one, so those rows really are empty
    let c = column(grown, "c");
    assert!(c.nullable);
    assert_eq!(c.default, None);
}

#[test]
fn mixed_columns_widen_and_say_so() {
    let plan = plan_for("shapes.sqlite", &Options::default());
    let affinity = table(&plan, "affinity");

    // real affinity: physically part integer, logically all float, and not
    // a mixture at all
    assert_eq!(column(affinity, "r").ty, TypeName::Float);
    assert!(!plan.notes.iter().any(|n| matches!(
        n, Note::Widened { column, .. } if column.ends_with(".r")
    )));

    // numeric affinity: the conversion was permanent, so this one really is
    // mixed, and float holds both
    assert_eq!(column(affinity, "n").ty, TypeName::Float);
    assert!(plan.notes.iter().any(|n| matches!(
        n, Note::Widened { column, to, .. } if column.ends_with(".n") && *to == TypeName::Float
    )));

    // no declared type: reals and a text together, so text it is
    assert_eq!(column(affinity, "u").ty, TypeName::Text);
    assert!(plan.notes.iter().any(|n| matches!(
        n, Note::Widened { column, to, .. } if column.ends_with(".u") && *to == TypeName::Text
    )));

    assert!(plan.is_runnable(), "widening does not stop an import");
}

#[test]
fn strict_turns_every_widening_into_a_refusal() {
    let lenient = plan_for("shapes.sqlite", &Options::default());
    let strict = plan_for("shapes.sqlite", &Options { strict: true });

    assert!(lenient.is_runnable());
    assert!(
        !strict.is_runnable(),
        "strict should refuse the mixed columns"
    );

    // the same columns, and only those
    let widened: Vec<String> = lenient
        .notes
        .iter()
        .filter_map(|n| match n {
            Note::Widened { column, .. } => Some(column.clone()),
            _ => None,
        })
        .collect();
    let refused: Vec<String> = strict
        .problems
        .iter()
        .filter_map(|p| match p {
            Problem::WouldWiden { column, .. } => Some(column.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(widened, refused);
    assert!(!widened.is_empty());

    // and the plan is otherwise the same, so --strict is a refusal and not
    // a different import
    assert_eq!(lenient.tables.len(), strict.tables.len());
}

#[test]
fn chinook_is_unchanged_by_strict_because_it_needs_no_judgement() {
    let strict = plan_for("chinook.sqlite", &Options { strict: true });
    assert!(
        strict.is_runnable(),
        "chinook needs no widening: {:?}",
        strict.problems
    );
}

#[test]
fn sqlites_own_tables_are_left_where_they_are() {
    let plan = plan_for("rowid_alias.sqlite", &Options::default());
    assert!(
        plan.tables
            .iter()
            .all(|t| !t.source_name.starts_with("sqlite_")),
        "sqlite_sequence is not the user's data"
    );
    assert!(plan.notes.iter().any(|n| matches!(
        n, Note::Skipped { what, .. } if what == "sqlite_sequence"
    )));
}

#[test]
fn the_report_names_every_table_column_and_decision() {
    let plan = plan_for("shapes.sqlite", &Options::default());
    let report = plan.report();

    for table in &plan.tables {
        assert!(report.contains(&table.name), "{} is missing", table.name);
        for column in &table.columns {
            assert!(
                report.contains(&column.name),
                "{}.{} is missing",
                table.name,
                column.name
            );
        }
    }
    assert!(report.contains("@key"));
    assert!(report.contains("notes:"));
    // the report says what a widening costs rather than only that it happened
    assert!(report.contains("comparisons follow the new type"));
}

#[test]
fn a_strict_report_says_nothing_was_written() {
    let strict = plan_for("shapes.sqlite", &Options { strict: true });
    let report = strict.report();
    assert!(report.contains("problems, nothing was written"));
    assert!(report.contains("--strict"));
}

#[test]
fn nullability_matches_column_for_column_what_sqlite_reports() {
    // every chinook column holding at least one null, taken from sqlite
    // with `select count(*) where c is null`, not from this crate
    let expected = [
        ("Customer", "Company"),
        ("Customer", "State"),
        ("Customer", "PostalCode"),
        ("Customer", "Phone"),
        ("Customer", "Fax"),
        ("Employee", "ReportsTo"),
        ("Invoice", "BillingState"),
        ("Invoice", "BillingPostalCode"),
        ("Track", "Composer"),
    ];

    let plan = plan_for("chinook.sqlite", &Options::default());
    let mut nullable: Vec<(String, String)> = Vec::new();
    for table in &plan.tables {
        for column in &table.columns {
            if column.nullable {
                nullable.push((table.source_name.clone(), column.source_name.clone()));
            }
        }
    }
    nullable.sort();

    let mut want: Vec<(String, String)> = expected
        .iter()
        .map(|(t, c)| (t.to_string(), c.to_string()))
        .collect();
    want.sort();

    assert_eq!(
        nullable, want,
        "a column marked non-null that holds null fails on the first insert, \
         and one marked null that never is loses a guarantee"
    );
}

#[test]
fn every_rule_the_source_enforces_and_we_cannot_is_named() {
    // chinook declares eleven foreign keys, counted with sqlite's own
    // `pragma foreign_key_list`: Album 1, Customer 1, Employee 1, Invoice 1,
    // InvoiceLine 2, PlaylistTrack 2, Track 3. after an import nothing
    // enforces any of them, and a developer who is not told that believes
    // their data is still guarded.
    let plan = plan_for("chinook.sqlite", &Options::default());
    let carried: Vec<&Note> = plan
        .notes
        .iter()
        .filter(|n| matches!(n, Note::ConstraintNotCarried { .. }))
        .collect();

    let foreign_keys = carried
        .iter()
        .filter(|n| matches!(n, Note::ConstraintNotCarried { what, .. } if what.contains("foreign key")))
        .count();
    assert_eq!(foreign_keys, 11, "one note per foreign key in the source");

    // the note says what it costs, not only that something was lost
    let text = plan.report();
    assert!(text.contains("nothing will stop a row pointing at something that is not there"));
}

#[test]
fn a_lost_constraint_is_a_note_and_never_a_refusal() {
    // --strict refuses judgement calls, where another answer was available.
    // a foreign key has no other answer: our language cannot hold one, so
    // refusing would leave the developer without an import and without the
    // foreign key either.
    let strict = plan_for("chinook.sqlite", &Options { strict: true });
    assert!(strict.is_runnable());
    assert!(strict
        .notes
        .iter()
        .any(|n| matches!(n, Note::ConstraintNotCarried { .. })));
    assert!(!strict
        .problems
        .iter()
        .any(|p| format!("{p:?}").contains("foreign")));
}
