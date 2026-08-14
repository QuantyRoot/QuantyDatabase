//! Tables that have no rowid.
//!
//! Such a table is not stored in a table b-tree at all: it *is* an index
//! b-tree keyed by its primary key, every entry holds a whole row, and the
//! record puts the key columns first in key order and the rest in declared
//! order. Nothing in the bytes says so, which is why reading one needs the
//! create statement parsed first.
//!
//! The oracle in tests/data/without_rowid.oracle renders rows in declared
//! order, so it only matches a reader that has undone that permutation.

mod common;

use common::Sha256;
use quanty_sqlite::{
    Cell, FileSource, MappedCell, Reader, RowLayout, Rows, SliceSource, SqliteValue,
};

fn data_path(name: &str) -> String {
    format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(data_path(name)).unwrap_or_else(|e| panic!("fixture {name}: {e}"))
}

fn oracle() -> (Vec<(String, usize, String)>, usize) {
    let text = std::fs::read_to_string(data_path("without_rowid.oracle"))
        .expect("the oracle is checked in");
    let mut tables = Vec::new();
    let mut total = 0;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# total rows ") {
            total = rest.trim().parse().expect("the total is a number");
            continue;
        }
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next(), parts.next()) {
            (Some(name), Some(rows), Some(digest)) => {
                tables.push((name.to_string(), rows.parse().unwrap(), digest.to_string()))
            }
            _ => panic!("unreadable oracle line: {line}"),
        }
    }
    (tables, total)
}

fn render(value: &SqliteValue) -> Vec<u8> {
    match value {
        SqliteValue::Null => b"null".to_vec(),
        SqliteValue::Integer(n) => format!("i:{n}").into_bytes(),
        SqliteValue::Real(f) => {
            let mut out = String::from("f:");
            for b in f.to_be_bytes() {
                out.push_str(&format!("{b:02x}"));
            }
            out.into_bytes()
        }
        SqliteValue::Text(t) => {
            let mut out = b"t:".to_vec();
            out.extend_from_slice(t.as_bytes());
            out
        }
        SqliteValue::Blob(b) => {
            let mut out = String::from("b:");
            for byte in b {
                out.push_str(&format!("{byte:02x}"));
            }
            out.into_bytes()
        }
    }
}

#[test]
fn every_row_matches_what_sqlite_reports() {
    let (expected, expected_total) = oracle();
    let bytes = fixture("without_rowid.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let schema = reader.schema().unwrap();

    let mut seen_total = 0;
    for (name, rows_expected, digest_expected) in &expected {
        let object = schema
            .object(name)
            .unwrap_or_else(|| panic!("{name} is not in the schema"));
        let def = object.table_def().unwrap();
        assert!(def.without_rowid, "{name} should be a without rowid table");
        let layout = RowLayout::new(&def);
        let root = object.root_page.unwrap();

        let mut hasher = Sha256::new();
        let mut count = 0;
        for row in reader.rows(root).unwrap() {
            let row = row.unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(row.rowid, None, "{name}: these rows have no rowid");

            let mut line = Vec::new();
            for index in 0..def.columns.len() {
                if index > 0 {
                    line.push(0x1f);
                }
                match layout.cell(&row, index) {
                    MappedCell::Value(value) => line.extend_from_slice(&render(value)),
                    MappedCell::Missing => line.extend_from_slice(b"missing"),
                    other => panic!("{name}: unexpected cell {other:?}"),
                }
            }
            line.push(b'\n');
            hasher.update(&line);
            count += 1;
        }

        assert_eq!(count, *rows_expected, "{name}: wrong number of rows");
        assert_eq!(
            hasher.finish(),
            *digest_expected,
            "{name}: {count} rows read, but their contents differ from sqlite's"
        );
        seen_total += count;
    }
    assert_eq!(seen_total, expected_total);
}

#[test]
fn the_record_puts_the_key_columns_first() {
    // `two` is declared a, b, c, d and keyed (c, a), so the record holds
    // c, a, b, d. reading it raw proves the permutation is real rather than
    // something the layout invents.
    let bytes = fixture("without_rowid.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let object = reader.schema().unwrap().object("two").unwrap().clone();
    let root = object.root_page.unwrap();

    let raw = reader
        .index_scan(root)
        .unwrap()
        .next()
        .expect("the table is not empty")
        .unwrap();
    assert_eq!(
        raw,
        vec![
            SqliteValue::Text("c0".into()),
            SqliteValue::Text("a0".into()),
            SqliteValue::Integer(0),
            SqliteValue::Text("d0".into()),
        ],
        "stored order is the key columns first"
    );

    // and the layout hands them back in declared order
    let def = object.table_def().unwrap();
    let layout = RowLayout::new(&def);
    let row = reader.rows(root).unwrap().next().unwrap().unwrap();
    let declared: Vec<String> = (0..def.columns.len())
        .map(|i| match layout.cell(&row, i) {
            MappedCell::Value(SqliteValue::Text(t)) => t.clone(),
            MappedCell::Value(SqliteValue::Integer(n)) => n.to_string(),
            other => panic!("unexpected cell {other:?}"),
        })
        .collect();
    assert_eq!(declared, vec!["a0", "0", "c0", "d0"]);
}

#[test]
fn an_integer_key_is_not_a_rowid_alias_here() {
    // in a rowid table `k integer primary key` aliases the rowid and is
    // stored as NULL. in a without rowid table there is no rowid to alias,
    // so the value is really in the record.
    let bytes = fixture("without_rowid.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let object = reader.schema().unwrap().object("int_pk").unwrap().clone();
    let def = object.table_def().unwrap();
    assert!(
        def.rowid_alias().is_none(),
        "a without rowid table has none"
    );

    let first = reader
        .index_scan(object.root_page.unwrap())
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    assert_eq!(first[0], SqliteValue::Integer(1));
}

#[test]
fn a_descending_key_comes_back_in_descending_order() {
    let bytes = fixture("without_rowid.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let root = reader
        .schema()
        .unwrap()
        .object("desc_key")
        .unwrap()
        .root_page
        .unwrap();

    let keys: Vec<i64> = reader
        .index_scan(root)
        .unwrap()
        .map(|entry| match entry.unwrap()[0] {
            SqliteValue::Integer(n) => n,
            ref other => panic!("unexpected key {other:?}"),
        })
        .collect();
    // the tree itself is ordered descending; a scan reports the order that
    // is there rather than one it prefers
    assert_eq!(keys, vec![5, 4, 3, 2, 1, 0]);
}

#[test]
fn a_column_added_later_is_missing_from_the_older_rows() {
    let bytes = fixture("without_rowid.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let object = reader.schema().unwrap().object("grown_wr").unwrap().clone();
    let def = object.table_def().unwrap();
    let layout = RowLayout::new(&def);
    let extra = def.column_index("extra").unwrap();

    let mut missing = 0;
    let mut present = 0;
    for row in reader.rows(object.root_page.unwrap()).unwrap() {
        match layout.cell(&row.unwrap(), extra) {
            MappedCell::Missing => missing += 1,
            MappedCell::Value(_) => present += 1,
            other => panic!("unexpected cell {other:?}"),
        }
    }
    assert_eq!((missing, present), (2, 2), "two rows predate the column");
}

#[test]
fn a_large_table_walks_through_its_interior_pages() {
    let bytes = fixture("without_rowid.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let root = reader
        .schema()
        .unwrap()
        .object("kv")
        .unwrap()
        .root_page
        .unwrap();

    // the root is an interior index page, so this exercises the in-order
    // walk rather than a single leaf
    assert!(reader.btree_page(root).unwrap().kind.is_interior());

    let keys: Vec<String> = reader
        .index_scan(root)
        .unwrap()
        .map(|entry| match &entry.unwrap()[0] {
            SqliteValue::Text(t) => t.clone(),
            other => panic!("unexpected key {other:?}"),
        })
        .collect();
    assert_eq!(keys.len(), 500);
    assert_eq!(keys[0], "key-0000");
    assert_eq!(keys[499], "key-0499");
    assert!(
        keys.windows(2).all(|w| w[0] < w[1]),
        "an index walk is in key order"
    );
}

#[test]
fn rows_picks_the_walk_from_the_page_rather_than_the_sql() {
    // a rowid table and a without rowid table, read through the same call
    let chinook = fixture("chinook.sqlite");
    let reader =
        Reader::open(FileSource::open(data_path("without_rowid.sqlite")).unwrap()).unwrap();
    let keyed = reader
        .rows(
            reader
                .schema()
                .unwrap()
                .object("kv")
                .unwrap()
                .root_page
                .unwrap(),
        )
        .unwrap();
    assert!(matches!(keyed, Rows::Keyed(_)));

    let reader = Reader::open(SliceSource::new(&chinook)).unwrap();
    let rowid = reader
        .rows(
            reader
                .schema()
                .unwrap()
                .object("Album")
                .unwrap()
                .root_page
                .unwrap(),
        )
        .unwrap();
    assert!(matches!(rowid, Rows::Rowid(_)));
}

#[test]
fn a_table_scan_of_an_index_tree_is_refused_by_name() {
    let bytes = fixture("without_rowid.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let root = reader
        .schema()
        .unwrap()
        .object("kv")
        .unwrap()
        .root_page
        .unwrap();

    let err = reader
        .table_scan(root)
        .err()
        .expect("a table scan of an index b-tree was accepted");
    assert!(
        err.to_string().contains("index_scan") || err.to_string().contains("rows"),
        "the message should point at the right call: {err}"
    );
    // and the cell type it holds is an index cell, not a table one
    let page = reader.btree_page(root).unwrap();
    assert!(matches!(
        reader.cell(&page, 0).unwrap(),
        Cell::IndexInterior { .. } | Cell::IndexLeaf { .. }
    ));
}
