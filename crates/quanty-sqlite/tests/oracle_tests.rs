//! The acceptance test for the reader: every row of every table in a real
//! database, checked against what SQLite itself says is in there.
//!
//! `tests/data/chinook.oracle` holds one line per table with its row count
//! and the SHA-256 of all its rows in rowid order, produced by the real
//! SQLite library. This test renders what the reader produces in the same
//! canonical form and compares the digests. 15607 rows either match to the
//! byte or they do not.
//!
//! The rendering is deliberately physical. A column declared
//! `integer primary key` is an alias for the rowid and SQLite stores NULL
//! in its place, so the oracle renders NULL there too and the rowid is
//! carried separately. Substituting one for the other is a decision about
//! what a row means, which belongs to the importer, not here.

mod common;

use common::{hex, sha256_hex, Sha256};
use quanty_sqlite::{FileSource, Reader, SliceSource, SqliteValue};

fn data_path(name: &str) -> String {
    format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(data_path(name)).unwrap_or_else(|e| panic!("fixture {name}: {e}"))
}

/// `<table> <rows> <sha256>` lines, comments skipped.
fn oracle() -> (Vec<(String, usize, String)>, usize) {
    let text =
        std::fs::read_to_string(data_path("chinook.oracle")).expect("the oracle is checked in");
    let mut tables = Vec::new();
    let mut total = 0;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("# total rows ") {
            total = rest.trim().parse().expect("the total is a number");
            continue;
        }
        let mut parts = line.split_whitespace();
        match (parts.next(), parts.next(), parts.next()) {
            (Some(name), Some(rows), Some(digest)) => tables.push((
                name.to_string(),
                rows.parse().expect("the row count is a number"),
                digest.to_string(),
            )),
            _ => panic!("unreadable oracle line: {line}"),
        }
    }
    assert!(!tables.is_empty(), "the oracle lists no tables");
    (tables, total)
}

/// One value, rendered the way the oracle renders it.
fn render(value: &SqliteValue) -> Vec<u8> {
    match value {
        SqliteValue::Null => b"null".to_vec(),
        SqliteValue::Integer(n) => format!("i:{n}").into_bytes(),
        // the bit pattern, not a decimal rendering, so that no float
        // formatting rule of ours has to agree with python's
        SqliteValue::Real(f) => format!("f:{}", hex(&f.to_be_bytes())).into_bytes(),
        SqliteValue::Text(t) => {
            let mut out = b"t:".to_vec();
            out.extend_from_slice(t.as_bytes());
            out
        }
        SqliteValue::Blob(b) => format!("b:{}", hex(b)).into_bytes(),
    }
}

#[test]
fn sha256_matches_the_published_vectors() {
    assert_eq!(
        sha256_hex(b""),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    assert_eq!(
        sha256_hex(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
    assert_eq!(
        sha256_hex(
            b"abcdefghbcdefghicdefghijdefghijkefghijklfghijklmghijklmn\
              hijklmnoijklmnopjklmnopqklmnopqrlmnopqrsmnopqrstnopqrstu"
        ),
        "cf5b16a778af8380036ce59e7b0492370b249b11e8f07a51afac45037afee9d1"
    );
    let million_a = vec![b'a'; 1_000_000];
    assert_eq!(
        sha256_hex(&million_a),
        "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
    );

    // the streaming path has to agree with the one shot path no matter
    // where the chunk boundaries fall
    for chunk in [1usize, 7, 63, 64, 65, 1000] {
        let mut hasher = Sha256::new();
        for piece in million_a.chunks(chunk) {
            hasher.update(piece);
        }
        assert_eq!(
            hasher.finish(),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0",
            "chunks of {chunk}"
        );
    }
}

#[test]
fn every_row_of_chinook_matches_what_sqlite_reports() {
    let (expected, expected_total) = oracle();
    let bytes = fixture("chinook.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let schema = reader.schema().unwrap();

    let mut seen_total = 0;
    for (name, rows_expected, digest_expected) in &expected {
        let table = schema
            .object(name)
            .unwrap_or_else(|| panic!("{name} is not in the schema"));
        let root = table.root_page.expect("a table has a root page");

        let mut hasher = Sha256::new();
        let mut count = 0;
        let mut last_rowid = i64::MIN;
        for row in reader.table_scan(root).unwrap() {
            let row = row.unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(row.rowid > last_rowid, "{name}: rowids are not ascending");
            last_rowid = row.rowid;

            let mut line = format!("r:{}", row.rowid).into_bytes();
            for value in &row.values {
                line.push(0x1f);
                line.extend_from_slice(&render(value));
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
    assert_eq!(
        expected.len(),
        schema.user_tables().count(),
        "the oracle covers every table in the database"
    );
}

#[test]
fn reading_from_a_file_gives_the_same_rows_as_reading_from_memory() {
    let bytes = fixture("chinook.sqlite");
    let from_memory = Reader::open(SliceSource::new(&bytes)).unwrap();
    let from_file = Reader::open(FileSource::open(data_path("chinook.sqlite")).unwrap()).unwrap();

    // PlaylistTrack is the awkward one: 8715 rows, a composite primary key
    // and therefore no rowid alias at all
    let root = from_memory
        .schema()
        .unwrap()
        .object("PlaylistTrack")
        .unwrap()
        .root_page
        .unwrap();

    let memory: Vec<_> = from_memory
        .table_scan(root)
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    let file: Vec<_> = from_file
        .table_scan(root)
        .unwrap()
        .map(|r| r.unwrap())
        .collect();

    assert_eq!(memory.len(), 8715);
    assert_eq!(memory, file);
}

#[test]
fn a_table_with_a_composite_key_stores_all_of_its_columns() {
    let bytes = fixture("chinook.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let schema = reader.schema().unwrap();
    let root = schema.object("PlaylistTrack").unwrap().root_page.unwrap();

    let first = reader
        .table_scan(root)
        .unwrap()
        .next()
        .expect("the table is not empty")
        .unwrap();

    // no column is an alias for the rowid here, so both are real values
    assert_eq!(first.values.len(), 2);
    assert!(matches!(first.values[0], SqliteValue::Integer(_)));
    assert!(matches!(first.values[1], SqliteValue::Integer(_)));
}
