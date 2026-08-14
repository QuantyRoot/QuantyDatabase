//! The schema table and the b-tree walk that reads it.
//!
//! Row counts and index names were taken from the fixtures with a real
//! SQLite, so they are an outside opinion about what the files contain.

use quanty_sqlite::{ObjectKind, Reader, SliceSource, SqliteValue};

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("fixture {path}: {e}"))
}

const CHINOOK_TABLES: [(&str, usize); 11] = [
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
];

#[test]
fn the_schema_lists_every_object_in_the_database() {
    let bytes = fixture("chinook.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let schema = reader.schema().unwrap();

    assert_eq!(schema.objects().len(), 22);
    let mut tables: Vec<&str> = schema.tables().map(|t| t.name.as_str()).collect();
    tables.sort_unstable();
    let expected: Vec<&str> = CHINOOK_TABLES.iter().map(|(n, _)| *n).collect();
    assert_eq!(tables, expected);

    // chinook has no views or triggers, and every table holds user data
    assert_eq!(schema.user_tables().count(), 11);
    assert_eq!(
        schema
            .objects()
            .iter()
            .filter(|o| o.kind == ObjectKind::Index)
            .count(),
        11
    );
}

#[test]
fn identifiers_are_matched_without_regard_to_case() {
    let bytes = fixture("chinook.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let schema = reader.schema().unwrap();

    for spelling in ["Track", "track", "TRACK", "tRaCk"] {
        let object = schema
            .object(spelling)
            .unwrap_or_else(|| panic!("{spelling} was not found"));
        assert_eq!(object.name, "Track");
        assert_eq!(object.kind, ObjectKind::Table);
    }
    assert!(schema.object("no_such_table").is_none());
}

#[test]
fn an_index_sqlite_made_itself_has_no_statement() {
    let bytes = fixture("chinook.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let schema = reader.schema().unwrap();

    let auto = schema.object("sqlite_autoindex_PlaylistTrack_1").unwrap();
    assert_eq!(auto.kind, ObjectKind::Index);
    assert_eq!(auto.table_name, "PlaylistTrack");
    assert_eq!(auto.sql, None, "an autoindex has no create statement");
    assert!(auto.is_internal());
    assert!(auto.root_page.is_some(), "but it does have a b-tree");

    // the index the schema does spell out
    let declared = schema.object("IFK_PlaylistTrackTrackId").unwrap();
    assert!(!declared.is_internal());
    assert!(declared
        .sql
        .as_deref()
        .unwrap()
        .to_lowercase()
        .contains("create index"));

    let names: Vec<&str> = schema
        .indexes_for("playlisttrack")
        .map(|o| o.name.as_str())
        .collect();
    assert_eq!(
        names,
        vec![
            "sqlite_autoindex_PlaylistTrack_1",
            "IFK_PlaylistTrackTrackId"
        ]
    );
}

#[test]
fn every_chinook_table_scans_to_the_row_count_sqlite_reports() {
    let bytes = fixture("chinook.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let schema = reader.schema().unwrap();

    let mut total = 0;
    for (name, expected) in CHINOOK_TABLES {
        let table = schema.object(name).unwrap();
        let rows: Vec<_> = reader
            .table_scan(table.root_page.unwrap())
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(rows.len(), expected, "{name} row count");

        let ids: Vec<i64> = rows.iter().map(|r| r.rowid).collect();
        assert!(
            ids.windows(2).all(|w| w[0] < w[1]),
            "{name} came back out of rowid order"
        );
        total += rows.len();
    }
    assert_eq!(total, 15607);
}

#[test]
fn a_scan_stops_at_the_first_row_a_caller_wants() {
    let bytes = fixture("chinook.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let schema = reader.schema().unwrap();
    let track = schema.object("Track").unwrap().root_page.unwrap();

    // the walk is lazy, so this must not read the whole table
    let first = reader.table_scan(track).unwrap().next().unwrap().unwrap();
    assert_eq!(first.rowid, 1);
    assert_eq!(
        first.values[1],
        SqliteValue::Text("For Those About To Rock (We Salute You)".into())
    );

    let tenth = reader.table_scan(track).unwrap().nth(9).unwrap().unwrap();
    assert_eq!(tenth.rowid, 10);
}

#[test]
fn a_rowid_alias_column_is_stored_as_null_in_the_record() {
    let bytes = fixture("records.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let schema = reader.schema().unwrap();
    let root = schema.object("kinds").unwrap().root_page.unwrap();

    for row in reader.table_scan(root).unwrap() {
        let row = row.unwrap();
        assert_eq!(
            row.values[0],
            SqliteValue::Null,
            "`id integer primary key` is the rowid, not a stored value"
        );
    }
}

#[test]
fn the_spill_table_scans_through_the_public_api() {
    let bytes = fixture("records.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let schema = reader.schema().unwrap();
    let root = schema.object("spill").unwrap().root_page.unwrap();

    let rows: Vec<_> = reader
        .table_scan(root)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(rows.len(), 29);
    for row in &rows {
        let text = match &row.values[1] {
            SqliteValue::Text(t) => t,
            other => panic!("expected text, got {other:?}"),
        };
        assert_eq!(text.len() as i64, row.rowid, "row {} length", row.rowid);
    }
    assert_eq!(rows.last().unwrap().rowid, 50000);
}

// ---------------------------------------------------------------------------
// trees that are not trees
// ---------------------------------------------------------------------------

/// Overwrite `page` with an interior table page that has no cells and whose
/// right most child is `child`, which is the smallest legal way to make a
/// tree as deep as one likes.
fn write_empty_interior(bytes: &mut [u8], page_size: usize, page: u32, child: u32) {
    let at = (page as usize - 1) * page_size;
    bytes[at..at + page_size].fill(0);
    bytes[at] = 5; // interior table page
    bytes[at + 1..at + 3].copy_from_slice(&0u16.to_be_bytes()); // no freeblocks
    bytes[at + 3..at + 5].copy_from_slice(&0u16.to_be_bytes()); // no cells
    let content_start = u16::try_from(page_size).unwrap_or(0);
    bytes[at + 5..at + 7].copy_from_slice(&content_start.to_be_bytes());
    bytes[at + 7] = 0; // no fragments
    bytes[at + 8..at + 12].copy_from_slice(&child.to_be_bytes());
}

#[test]
fn a_cycle_in_the_tree_is_refused_rather_than_walked() {
    let mut bytes = fixture("records.sqlite");
    // page 2 is a table root; point it at itself
    write_empty_interior(&mut bytes, 512, 2, 2);

    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let outcome: Result<Vec<_>, _> = reader.table_scan(2).unwrap().collect();
    let err = outcome.expect_err("a self referencing tree was walked");
    assert!(err.to_string().contains("cycle"), "message: {err}");
}

#[test]
fn a_longer_cycle_is_refused_too() {
    let mut bytes = fixture("records.sqlite");
    // 2 -> 20 -> 21 -> 22 -> 2
    write_empty_interior(&mut bytes, 512, 2, 20);
    write_empty_interior(&mut bytes, 512, 20, 21);
    write_empty_interior(&mut bytes, 512, 21, 22);
    write_empty_interior(&mut bytes, 512, 22, 2);

    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let outcome: Result<Vec<_>, _> = reader.table_scan(2).unwrap().collect();
    assert!(outcome.is_err(), "a four page cycle was walked");
}

#[test]
fn a_tree_deep_enough_to_overflow_a_recursive_walker_still_works() {
    let original = fixture("records.sqlite");
    let reader = Reader::open(SliceSource::new(&original)).unwrap();
    let schema = reader.schema().unwrap();
    // kinds is small and none of its rows spill, so the pages the chain
    // below overwrites cannot be pages this table needs
    let leaf = schema.object("kinds").unwrap().root_page.unwrap();
    let rows_before = reader.table_scan(leaf).unwrap().count();
    assert_eq!(rows_before, 23);

    let mut bytes = original.clone();
    // build 200 interior pages, each holding nothing but a pointer to the
    // next, ending at the real leaf. a walker that recurses per level dies
    // here; one with an explicit stack does not notice.
    let chain: Vec<u32> = (30..230).collect();
    for pair in chain.windows(2) {
        write_empty_interior(&mut bytes, 512, pair[0], pair[1]);
    }
    write_empty_interior(&mut bytes, 512, *chain.last().unwrap(), leaf);

    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let rows: Vec<_> = reader
        .table_scan(chain[0])
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        rows.len(),
        23,
        "the leaf at the bottom still yields its rows"
    );
}

#[test]
fn a_table_stored_in_an_index_btree_is_refused_by_name() {
    let mut bytes = fixture("records.sqlite");
    let leaf = {
        let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
        let schema = reader.schema().unwrap();
        schema.object("unicode").unwrap().root_page.unwrap()
    };
    // a without rowid table keeps its rows in an index b-tree. we cannot
    // read those yet, and the point of this test is that the refusal is
    // explicit rather than a silently empty or wrong scan.
    let at = (leaf as usize - 1) * 512;
    bytes[at] = 10; // leaf index page
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let err = reader
        .table_scan(leaf)
        .err()
        .expect("an index rooted table was scanned as a table");
    assert!(err.to_string().contains("without rowid"), "message: {err}");
}

#[test]
fn a_scan_that_fails_stays_failed() {
    let mut bytes = fixture("records.sqlite");
    write_empty_interior(&mut bytes, 512, 2, 2);
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();

    let mut scan = reader.table_scan(2).unwrap();
    assert!(scan.next().unwrap().is_err());
    assert!(
        scan.next().is_none(),
        "an iterator that has failed must not offer more rows"
    );
}

#[test]
fn a_schema_row_of_the_wrong_shape_is_refused() {
    // point the schema's root at a table whose rows are not schema rows
    let bytes = fixture("records.sqlite");
    let spill_root = {
        let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
        reader
            .schema()
            .unwrap()
            .object("spill")
            .unwrap()
            .root_page
            .unwrap()
    };
    let mut broken = bytes.clone();
    // copy the spill table's root page over page 1's b-tree area, keeping
    // the file header intact
    let source = (spill_root as usize - 1) * 512;
    broken.copy_within(source + 100..source + 512, 100);

    let reader = Reader::open(SliceSource::new(&broken)).unwrap();
    assert!(
        reader.schema().is_err(),
        "rows that are not schema rows were accepted as a schema"
    );
}

#[test]
fn rows_out_of_key_order_are_refused_rather_than_handed_on() {
    // swapping two cell pointers on a leaf page leaves every cell intact
    // and only changes the order they are visited in, which is the cheapest
    // way to produce a table b-tree that is not in key order
    let original = fixture("records.sqlite");
    let (leaf, page_size) = {
        let reader = Reader::open(SliceSource::new(&original)).unwrap();
        let root = reader
            .schema()
            .unwrap()
            .object("kinds")
            .unwrap()
            .root_page
            .unwrap();
        (root, reader.header().page_size as usize)
    };

    // the kinds table is small enough to be a single leaf page, so its root
    // is where the rows are
    let mut bytes = original.clone();
    let base = (leaf as usize - 1) * page_size;
    assert_eq!(bytes[base], 13, "the kinds table should be one leaf page");
    let first = base + 8;
    let second = base + 10;
    for offset in 0..2 {
        bytes.swap(first + offset, second + offset);
    }

    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let rows: Vec<_> = reader.table_scan(leaf).unwrap().collect();

    // the first row still reads; the second one is where the order breaks
    assert!(rows[0].is_ok());
    let err = rows
        .iter()
        .find_map(|r| r.as_ref().err())
        .expect("rows in the wrong key order were accepted");
    assert!(err.to_string().contains("key order"), "message was: {err}");
}
