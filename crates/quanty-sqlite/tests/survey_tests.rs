//! Lining records up with columns, and reporting what each column holds.
//!
//! Every expectation here comes from what the generator was told to store,
//! or from SQLite's own reading of the file, never from an earlier run of
//! this code.

use quanty_sqlite::{
    Affinity, MappedCell, Reader, RowLayout, SliceSource, SqliteValue, StorageClass,
};

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("fixture {path}: {e}"))
}

// ---------------------------------------------------------------------------
// lining a record up with the declaration
// ---------------------------------------------------------------------------

#[test]
fn a_virtual_column_takes_no_slot_and_shifts_nothing_after_it() {
    let bytes = fixture("shapes.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let schema = reader.schema().unwrap();
    let object = schema.object("generated").unwrap();
    let def = object.table_def().unwrap();
    let layout = RowLayout::new(&def);

    // declared: id, a, v (virtual), s (stored), z. the record holds four of
    // those five, so a naive zip would put s's value into v and z's into s.
    assert_eq!(def.columns.len(), 5);
    assert_eq!(layout.stored_columns(), 4);

    let rows: Vec<_> = reader
        .rows(object.root_page.unwrap())
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(rows.len(), 3);

    for (index, row) in rows.iter().enumerate() {
        let a = (index as i64 + 1) * 10;
        assert_eq!(layout.cell(row, 0), MappedCell::Rowid(index as i64 + 1));
        assert_eq!(
            layout.cell(row, 1),
            MappedCell::Value(&SqliteValue::Integer(a))
        );
        assert_eq!(layout.cell(row, 2), MappedCell::Virtual);
        assert_eq!(
            layout.cell(row, 3),
            MappedCell::Value(&SqliteValue::Integer(a * 3)),
            "the stored generated column keeps its own slot"
        );
        assert_eq!(
            layout.cell(row, 4),
            MappedCell::Value(&SqliteValue::Text(format!("z{}", index + 1))),
            "the column after the virtual one must not be shifted"
        );
    }
}

#[test]
fn a_record_that_ends_early_reports_missing_rather_than_wrong() {
    let bytes = fixture("shapes.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let schema = reader.schema().unwrap();
    let object = schema.object("grown").unwrap();
    let def = object.table_def().unwrap();
    let layout = RowLayout::new(&def);

    // two columns were added after two rows already existed, and sqlite did
    // not rewrite them
    assert_eq!(def.columns.len(), 4);
    assert_eq!(def.columns[2].default.as_deref(), Some("'fallback'"));
    assert_eq!(def.columns[3].default, None);

    let rows: Vec<_> = reader
        .rows(object.root_page.unwrap())
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(rows.len(), 3);

    for row in &rows[..2] {
        assert_eq!(row.values.len(), 2, "the old rows are short");
        assert_eq!(layout.cell(row, 2), MappedCell::Missing);
        assert_eq!(layout.cell(row, 3), MappedCell::Missing);
    }
    assert_eq!(
        layout.cell(&rows[2], 2),
        MappedCell::Value(&SqliteValue::Text("echt".into()))
    );
    assert_eq!(
        layout.cell(&rows[2], 3),
        MappedCell::Value(&SqliteValue::Integer(42))
    );
}

#[test]
fn a_rowid_alias_reads_as_the_rowid_and_not_as_the_stored_null() {
    let bytes = fixture("rowid_alias.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let schema = reader.schema().unwrap();

    for (table, alias) in [("a_col_pk", true), ("b_col_pk_desc", false)] {
        let object = schema.object(table).unwrap();
        let def = object.table_def().unwrap();
        let layout = RowLayout::new(&def);
        let row = reader
            .rows(object.root_page.unwrap())
            .unwrap()
            .next()
            .unwrap()
            .unwrap();

        if alias {
            assert_eq!(layout.cell(&row, 0), MappedCell::Rowid(7), "{table}");
            assert_eq!(row.values[0], SqliteValue::Null, "{table}: stored as null");
        } else {
            assert_eq!(
                layout.cell(&row, 0),
                MappedCell::Value(&SqliteValue::Integer(7)),
                "{table}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// what a column holds
// ---------------------------------------------------------------------------

#[test]
fn affinity_decides_whether_a_stored_integer_is_a_float() {
    let bytes = fixture("shapes.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let schema = reader.schema().unwrap();
    let survey = reader
        .survey_table(schema.object("affinity").unwrap())
        .unwrap();
    assert_eq!(survey.rows, 4);

    let column = |name: &str| {
        survey
            .columns
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no column {name}"))
    };

    // r holds 1.0, 2.5, 3.0, 4.0. three of those are physically integers,
    // and every one of them is a float, which is what sqlite reports too.
    let r = column("r");
    assert_eq!(r.affinity, Affinity::Real);
    assert_eq!(r.reals, 4);
    assert_eq!(r.integers, 0);
    assert_eq!(r.classes(), vec![StorageClass::Real]);

    // n has numeric affinity, where the conversion is permanent: the whole
    // values really are integers now, so the column really is mixed
    let n = column("n");
    assert_eq!(n.affinity, Affinity::Numeric);
    assert_eq!((n.integers, n.reals), (3, 1));
    assert_eq!(n.classes(), vec![StorageClass::Integer, StorageClass::Real]);

    let i = column("i");
    assert_eq!(i.affinity, Affinity::Integer);
    assert_eq!(i.integers, 4);

    // u was declared without a type, so nothing was ever converted and the
    // text stayed text
    let u = column("u");
    assert_eq!(u.affinity, Affinity::Blob);
    assert_eq!((u.reals, u.texts), (3, 1));
    assert_eq!(u.classes(), vec![StorageClass::Real, StorageClass::Text]);
}

#[test]
fn the_survey_counts_missing_values_separately_from_nulls() {
    let bytes = fixture("shapes.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let schema = reader.schema().unwrap();
    let survey = reader
        .survey_table(schema.object("grown").unwrap())
        .unwrap();

    let b = survey.columns.iter().find(|c| c.name == "b").unwrap();
    assert_eq!(b.missing, 2, "two rows predate the column");
    assert_eq!(b.texts, 1);
    assert_eq!(b.nulls, 0);
    assert!(
        b.has_nulls(),
        "a missing value still means the column can be empty"
    );

    let c = survey.columns.iter().find(|c| c.name == "c").unwrap();
    assert_eq!(c.missing, 2);
    assert_eq!(c.integers, 1);
}

#[test]
fn a_virtual_column_surveys_as_holding_nothing() {
    let bytes = fixture("shapes.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let schema = reader.schema().unwrap();
    let survey = reader
        .survey_table(schema.object("generated").unwrap())
        .unwrap();

    let v = survey.columns.iter().find(|c| c.name == "v").unwrap();
    assert!(v.is_virtual);
    assert_eq!(v.classes(), vec![], "the file holds nothing for it");
    assert_eq!(v.missing, 0, "absent is not the same as missing");

    let s = survey.columns.iter().find(|c| c.name == "s").unwrap();
    assert!(!s.is_virtual);
    assert_eq!(s.integers, 3);
}

#[test]
fn a_primary_key_holding_null_is_visible_in_the_survey() {
    let bytes = fixture("shapes.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let schema = reader.schema().unwrap();
    let survey = reader
        .survey_table(schema.object("nullable_pk").unwrap())
        .unwrap();

    // a text primary key is not an alias for the rowid, and sqlite still
    // lets it hold null, which our key columns cannot
    assert_eq!(survey.primary_key, vec!["k".to_string()]);
    let k = survey.columns.iter().find(|c| c.name == "k").unwrap();
    assert_eq!(k.nulls, 2);
    assert_eq!(k.texts, 1);
    assert!(k.has_nulls(), "so this key cannot be ours as declared");
}

#[test]
fn a_table_without_a_primary_key_still_has_rowids_to_fall_back_on() {
    let bytes = fixture("shapes.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let schema = reader.schema().unwrap();
    let survey = reader.survey_table(schema.object("nopk").unwrap()).unwrap();

    assert!(survey.primary_key.is_empty());
    assert_eq!(survey.rows, 2);
    assert_eq!(survey.largest_rowid, 2);
}

#[test]
fn surveying_chinook_agrees_with_what_sqlite_reports() {
    let bytes = fixture("chinook.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let schema = reader.schema().unwrap();

    let track = reader
        .survey_table(schema.object("Track").unwrap())
        .unwrap();
    assert_eq!(track.rows, 3503);
    assert_eq!(track.primary_key, vec!["TrackId".to_string()]);

    let column = |survey: &quanty_sqlite::TableSurvey, name: &str| {
        survey
            .columns
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no column {name}"))
            .clone()
    };

    // the key is the rowid, so every row has one and none is null
    let id = column(&track, "TrackId");
    assert!(id.is_rowid_alias);
    assert_eq!(id.integers, 3503);
    assert_eq!(id.largest_integer, 3503);

    // NVARCHAR is text affinity and every value is text
    let name = column(&track, "Name");
    assert_eq!(name.affinity, Affinity::Text);
    assert_eq!(name.texts, 3503);

    // Composer is nullable in the data
    let composer = column(&track, "Composer");
    assert!(composer.has_nulls());
    assert_eq!(composer.nulls + composer.texts, 3503);

    // UnitPrice is NUMERIC(10,2) and every price has a fraction, so nothing
    // was converted to an integer and the column is not mixed
    let price = column(&track, "UnitPrice");
    assert_eq!(price.affinity, Affinity::Numeric);
    assert_eq!(price.classes(), vec![StorageClass::Real]);

    // the whole database surveys without error
    let mut total = 0;
    for table in schema.user_tables() {
        total += reader.survey_table(table).unwrap().rows;
    }
    assert_eq!(total, 15607);
}

#[test]
fn a_datetime_column_holds_the_text_it_was_given() {
    let bytes = fixture("chinook.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let schema = reader.schema().unwrap();
    let survey = reader
        .survey_table(schema.object("Employee").unwrap())
        .unwrap();

    let birth = survey
        .columns
        .iter()
        .find(|c| c.name == "BirthDate")
        .unwrap();
    // declared DATETIME, which is numeric affinity, and holding text in
    // every row: the declaration alone would have built a number column
    assert_eq!(birth.declared_type.as_deref(), Some("DATETIME"));
    assert_eq!(birth.affinity, Affinity::Numeric);
    assert_eq!(birth.classes(), vec![StorageClass::Text]);
}
