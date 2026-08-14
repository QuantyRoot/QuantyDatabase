//! Parsing `create table` statements.
//!
//! Three sources of truth here, in descending order of trustworthiness:
//! the statements SQLite itself wrote into the two real fixtures, a fixture
//! built specifically from the rowid alias cases, and hand written
//! statements for the shapes no fixture happens to contain.

use quanty_sqlite::{parse_create_table, Reader, SliceSource, SqliteValue};

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("fixture {path}: {e}"))
}

#[test]
fn every_create_statement_in_the_fixtures_parses() {
    for name in ["chinook.sqlite", "records.sqlite", "rowid_alias.sqlite"] {
        let bytes = fixture(name);
        let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
        let schema = reader.schema().unwrap();
        let mut parsed = 0;
        for table in schema.tables() {
            let def = table
                .table_def()
                .unwrap_or_else(|e| panic!("{name}, table {}: {e}", table.name));
            assert_eq!(
                def.name.to_lowercase(),
                table.name.to_lowercase(),
                "{name}: the parsed name differs from the schema's"
            );
            assert!(!def.columns.is_empty(), "{}: no columns", table.name);
            parsed += 1;
        }
        assert!(parsed > 0, "{name} has no tables");
    }
}

#[test]
fn the_chinook_tables_come_out_with_their_columns_in_order() {
    let bytes = fixture("chinook.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let schema = reader.schema().unwrap();

    // chinook writes bracket quoted names, types with arguments, a table
    // level named primary key constraint and foreign keys with actions
    let album = schema.object("Album").unwrap().table_def().unwrap();
    let names: Vec<&str> = album.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["AlbumId", "Title", "ArtistId"]);
    assert_eq!(
        album.columns[1].declared_type.as_deref(),
        Some("NVARCHAR(160)")
    );
    assert!(album.columns[0].not_null);
    assert_eq!(album.primary_key.len(), 1);
    assert_eq!(album.primary_key[0].name, "AlbumId");
    assert!(!album.without_rowid);

    // AlbumId is declared INTEGER and is the whole primary key, so it is an
    // alias for the rowid even though the key is a table constraint
    assert_eq!(
        album.rowid_alias().map(|c| c.name.as_str()),
        Some("AlbumId")
    );

    // a composite key has no alias, and both of its columns are stored
    let playlist_track = schema.object("PlaylistTrack").unwrap().table_def().unwrap();
    assert_eq!(playlist_track.primary_key.len(), 2);
    assert_eq!(playlist_track.primary_key[0].name, "PlaylistId");
    assert_eq!(playlist_track.primary_key[1].name, "TrackId");
    assert!(playlist_track.rowid_alias().is_none());

    // a nullable column, for contrast
    let artist = schema.object("Artist").unwrap().table_def().unwrap();
    assert!(!artist.columns[1].not_null);
    assert_eq!(
        artist.columns[1].declared_type.as_deref(),
        Some("NVARCHAR(120)")
    );
}

#[test]
fn the_rowid_alias_rule_matches_what_sqlite_actually_stored() {
    // the fixture holds one table per case, each with a single row where
    // the key is 7. if the column is an alias, sqlite stored NULL in the
    // record and put 7 in the rowid; if it is not, the record holds 7 and
    // the rowid is 1. so the file itself says which is which, and the
    // parser has to agree with it.
    let bytes = fixture("rowid_alias.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let schema = reader.schema().unwrap();

    let expected = [
        ("a_col_pk", true),
        ("b_col_pk_desc", false),
        ("c_tbl_pk", true),
        ("d_tbl_pk_desc", true),
        ("e_int_not_integer", false),
        ("f_autoinc", true),
        ("g_mixed_case", true),
    ];

    for (name, is_alias) in expected {
        let object = schema.object(name).unwrap();
        let def = object.table_def().unwrap();
        assert_eq!(
            def.rowid_alias().is_some(),
            is_alias,
            "{name}: the parser disagrees about the rowid alias"
        );

        let row = reader
            .table_scan(object.root_page.unwrap())
            .unwrap()
            .next()
            .expect("one row")
            .unwrap();
        if is_alias {
            assert_eq!(row.rowid, 7, "{name}: the key should be the rowid");
            assert_eq!(row.values[0], SqliteValue::Null, "{name}: stored as null");
        } else {
            assert_eq!(row.rowid, 1, "{name}: the rowid is separate from the key");
            assert_eq!(row.values[0], SqliteValue::Integer(7), "{name}: stored");
        }
    }
}

// ---------------------------------------------------------------------------
// shapes the fixtures do not contain
// ---------------------------------------------------------------------------

#[test]
fn all_four_ways_of_quoting_a_name_are_understood() {
    let def =
        parse_create_table(r#"create table "od d" ([a b] int, `c,d` text, "e""f" int, plain int)"#)
            .unwrap();
    assert_eq!(def.name, "od d");
    let names: Vec<&str> = def.columns.iter().map(|c| c.name.as_str()).collect();
    // a comma inside a quoted name must not split the column list, and a
    // doubled quote is one quote
    assert_eq!(names, vec!["a b", "c,d", "e\"f", "plain"]);
}

#[test]
fn a_type_name_can_be_several_words_or_missing_entirely() {
    let def = parse_create_table(
        "create table t (a double precision, b unsigned big int, c varchar(10), d, \
         e decimal(10, 5))",
    )
    .unwrap();
    assert_eq!(
        def.columns[0].declared_type.as_deref(),
        Some("double precision")
    );
    assert_eq!(
        def.columns[1].declared_type.as_deref(),
        Some("unsigned big int")
    );
    assert_eq!(def.columns[2].declared_type.as_deref(), Some("varchar(10)"));
    assert_eq!(def.columns[3].declared_type, None);
    assert_eq!(
        def.columns[4].declared_type.as_deref(),
        Some("decimal(10, 5)")
    );
}

#[test]
fn a_declared_type_comes_back_exactly_as_it_was_written() {
    // the spacing is the statement's, not ours: the type text is taken from
    // the original span rather than reassembled from tokens
    let def = parse_create_table(
        "create table t (a varchar ( 10 ), b  DOUBLE   PRECISION, c [odd type] not null)",
    )
    .unwrap();
    assert_eq!(
        def.columns[0].declared_type.as_deref(),
        Some("varchar ( 10 )")
    );
    assert_eq!(
        def.columns[1].declared_type.as_deref(),
        Some("DOUBLE   PRECISION")
    );
    assert_eq!(def.columns[2].declared_type.as_deref(), Some("[odd type]"));
    assert!(def.columns[2].not_null);
}

#[test]
fn constraints_that_say_nothing_about_a_row_are_stepped_over() {
    let def = parse_create_table(
        "create table t (
             a integer primary key autoincrement,
             b text not null on conflict replace collate nocase,
             c int references other (id) on delete cascade deferrable initially deferred,
             d int check (d > 0 and d < 10),
             e int unique on conflict ignore,
             constraint ck check (a <> b),
             unique (b, c),
             foreign key (d) references other (id) on update set null
         )",
    )
    .unwrap();
    let names: Vec<&str> = def.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["a", "b", "c", "d", "e"]);
    assert!(def.columns[1].not_null);
    assert_eq!(def.primary_key.len(), 1);
    assert_eq!(def.rowid_alias().map(|c| c.name.as_str()), Some("a"));
}

#[test]
fn a_check_constraint_holding_a_comma_or_a_paren_does_not_split_the_list() {
    let def =
        parse_create_table("create table t (a text check (substr(a, 1, 2) <> ')'), b int, c int)")
            .unwrap();
    let names: Vec<&str> = def.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["a", "b", "c"]);
}

#[test]
fn defaults_are_kept_as_written() {
    let def = parse_create_table(
        "create table t (a int default 42, b int default -1, c text default 'hi', \
         d text default current_timestamp, e int default (1 + 2), f blob default x'00ff')",
    )
    .unwrap();
    let defaults: Vec<Option<&str>> = def.columns.iter().map(|c| c.default.as_deref()).collect();
    assert_eq!(
        defaults,
        vec![
            Some("42"),
            Some("-1"),
            Some("'hi'"),
            Some("current_timestamp"),
            Some("(1 + 2)"),
            Some("x'00ff'"),
        ]
    );
}

#[test]
fn comments_anywhere_are_ignored() {
    let def = parse_create_table(
        "create table -- the name follows
         t /* inline */ (
             a int, -- the key
             b /* between */ text
         ) -- trailing",
    )
    .unwrap();
    assert_eq!(def.name, "t");
    assert_eq!(def.columns.len(), 2);
}

#[test]
fn without_rowid_and_strict_are_recognised_in_any_order() {
    for suffix in [
        "without rowid",
        "strict",
        "without rowid, strict",
        "strict, without rowid",
    ] {
        let sql = format!("create table t (a int, b text, primary key (a)) {suffix}");
        let def = parse_create_table(&sql).unwrap();
        assert_eq!(def.without_rowid, suffix.contains("without"), "{suffix}");
        assert_eq!(def.strict, suffix.contains("strict"), "{suffix}");
        if def.without_rowid {
            // there is no rowid to be an alias for
            assert!(def.rowid_alias().is_none(), "{suffix}");
        }
    }
}

#[test]
fn generated_columns_are_marked() {
    let def = parse_create_table(
        "create table t (a int, b int generated always as (a * 2) stored, c int as (a + 1))",
    )
    .unwrap();
    assert!(!def.columns[0].generated);
    assert!(def.columns[1].generated);
    assert!(def.columns[2].generated);
}

#[test]
fn a_table_defined_by_a_select_is_refused_by_name() {
    let err = parse_create_table("create table t as select 1 as a, 2 as b").unwrap_err();
    let message = err.to_string();
    assert!(message.contains("select"), "message was: {message}");
    assert!(message.contains('t'), "message was: {message}");
}

#[test]
fn statements_this_parser_cannot_read_say_so_instead_of_guessing() {
    for sql in [
        "",
        "create index i on t (a)",
        "create table t",
        "create table t (",
        "create table t (a int",
        "create table t ()",
        "create table t (a int, primary key (nosuchcolumn))",
        "create table t (a int primary key, b int primary key)",
    ] {
        let result = parse_create_table(sql);
        assert!(result.is_err(), "accepted {sql:?}");
    }
}

#[test]
fn a_schema_qualified_name_keeps_the_table_part() {
    let def = parse_create_table("create table main.\"t\" (a int)").unwrap();
    assert_eq!(def.name, "t");
}

#[test]
fn if_not_exists_and_temp_are_accepted() {
    for prefix in [
        "create table if not exists",
        "create temp table",
        "create temporary table if not exists",
    ] {
        let def = parse_create_table(&format!("{prefix} t (a int)")).unwrap();
        assert_eq!(def.name, "t", "{prefix}");
    }
}
