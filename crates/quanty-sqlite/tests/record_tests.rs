//! Cells, records and overflow chains, read out of databases written by a
//! real SQLite.
//!
//! The expected values are recomputed here from the rules in
//! tests/data/README.md, which are what the generator was told to store.
//! Nothing is compared against output this crate produced earlier, so a
//! wrong reader cannot agree with a wrong expectation.
//!
//! There is no b-tree traversal yet, so these tests walk down from a root
//! page by hand. That is on purpose for one commit: it proves the cell API
//! is enough to descend a tree before the convenience layer exists.

use quanty_sqlite::{decode_record, Cell, PageKind, Reader, SliceSource, SqliteValue};

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("fixture {path}: {e}"))
}

/// Collect every table leaf cell under `root`, in key order.
fn table_rows(reader: &Reader<SliceSource<'_>>, root: u32) -> Vec<(i64, Vec<SqliteValue>)> {
    let mut out = Vec::new();
    let page = reader.btree_page(root).expect("root page parses");
    match page.kind {
        PageKind::LeafTable => {
            for i in 0..page.cell_count() {
                match reader.cell(&page, i).expect("cell parses") {
                    Cell::TableLeaf { rowid, payload } => {
                        out.push((rowid, decode_record(&payload).expect("record decodes")))
                    }
                    other => panic!("leaf page held {other:?}"),
                }
            }
        }
        PageKind::InteriorTable => {
            let mut children = Vec::new();
            for i in 0..page.cell_count() {
                match reader.cell(&page, i).expect("cell parses") {
                    Cell::TableInterior { child, .. } => children.push(child),
                    other => panic!("interior page held {other:?}"),
                }
            }
            children.push(page.right_most.expect("interior pages have a right child"));
            for child in children {
                out.extend(table_rows(reader, child));
            }
        }
        other => panic!("page {root} is a {other:?}, not a table page"),
    }
    out
}

fn text_of(n: usize) -> String {
    (0..n)
        .map(|i| (b'a' + ((i + n) % 26) as u8) as char)
        .collect()
}

fn blob_of(n: usize) -> Vec<u8> {
    (0..n).map(|i| ((i * 7 + n) % 256) as u8).collect()
}

fn one_text(values: &[SqliteValue]) -> &str {
    match &values[1] {
        SqliteValue::Text(t) => t,
        other => panic!("expected text, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// records that fit on their page
// ---------------------------------------------------------------------------

#[test]
fn the_chinook_schema_decodes_to_its_22_objects() {
    let bytes = fixture("chinook.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let rows = table_rows(&reader, 1);
    assert_eq!(rows.len(), 22);

    // sqlite_master is (type, name, tbl_name, rootpage, sql)
    let mut tables = Vec::new();
    let mut indexes = 0;
    for (rowid, values) in &rows {
        assert_eq!(values.len(), 5, "row {rowid}");
        match (&values[0], &values[1], &values[3]) {
            (SqliteValue::Text(kind), SqliteValue::Text(name), SqliteValue::Integer(root)) => {
                assert!(*root > 0 && *root <= reader.page_count() as i64);
                match &values[4] {
                    // an index sqlite created itself for a primary key or a
                    // unique constraint has no create statement of its own
                    SqliteValue::Null => assert!(
                        name.starts_with("sqlite_autoindex_"),
                        "{name} has no sql but is not an autoindex"
                    ),
                    SqliteValue::Text(sql) => {
                        assert!(sql.to_lowercase().contains("create"), "{name}: {sql}")
                    }
                    other => panic!("{name}: sql is {other:?}"),
                }
                match kind.as_str() {
                    "table" => tables.push(name.clone()),
                    "index" => indexes += 1,
                    other => panic!("unexpected schema object kind {other}"),
                }
            }
            other => panic!("unexpected sqlite_master row shape: {other:?}"),
        }
    }
    tables.sort();
    assert_eq!(
        tables,
        vec![
            "Album",
            "Artist",
            "Customer",
            "Employee",
            "Genre",
            "Invoice",
            "InvoiceLine",
            "MediaType",
            "Playlist",
            "PlaylistTrack",
            "Track",
        ]
    );
    assert_eq!(indexes, 11);
}

#[test]
fn rowids_come_back_in_key_order() {
    let bytes = fixture("chinook.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    // Track is the big table: 3503 rows across interior and leaf pages
    let rows = table_rows(&reader, 409);
    assert_eq!(rows.len(), 3503);
    let ids: Vec<i64> = rows.iter().map(|(id, _)| *id).collect();
    assert!(ids.windows(2).all(|w| w[0] < w[1]), "rowids are ascending");
    assert_eq!(ids[0], 1);
    assert_eq!(ids[ids.len() - 1], 3503);

    // Track is (TrackId, Name, AlbumId, MediaTypeId, GenreId, Composer,
    // Milliseconds, Bytes, UnitPrice) and its first row is well known
    let (_, first) = &rows[0];
    assert_eq!(first.len(), 9);
    assert_eq!(
        first[1],
        SqliteValue::Text("For Those About To Rock (We Salute You)".into())
    );
    assert_eq!(first[8], SqliteValue::Real(0.99));
}

#[test]
fn every_serial_type_decodes_to_the_value_that_was_stored() {
    let bytes = fixture("records.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let rows = table_rows(&reader, 4);
    let by_id: Vec<SqliteValue> = rows.iter().map(|(_, v)| v[1].clone()).collect();

    assert_eq!(
        by_id,
        vec![
            SqliteValue::Null,
            SqliteValue::Integer(-128),
            SqliteValue::Integer(127),
            SqliteValue::Integer(-32768),
            SqliteValue::Integer(32767),
            SqliteValue::Integer(-8388608),
            SqliteValue::Integer(8388607),
            SqliteValue::Integer(-2147483648),
            SqliteValue::Integer(2147483647),
            SqliteValue::Integer(-140737488355328),
            SqliteValue::Integer(140737488355327),
            SqliteValue::Integer(i64::MIN),
            SqliteValue::Integer(i64::MAX),
            SqliteValue::Real(0.5),
            SqliteValue::Real(-2.25),
            SqliteValue::Real(f64::MAX),
            SqliteValue::Integer(0),
            SqliteValue::Integer(1),
            SqliteValue::Text(String::new()),
            SqliteValue::Blob(Vec::new()),
            SqliteValue::Text("grus, Zurich".into()),
            SqliteValue::Text("japanisch: konnichiwa".into()),
            SqliteValue::Blob(vec![0x00, 0x01, 0xfe, 0xff]),
        ]
    );

    // `id integer primary key` is an alias for the rowid, so the column
    // itself is stored as NULL and the value lives in the cell's rowid
    for (index, (rowid, values)) in rows.iter().enumerate() {
        assert_eq!(*rowid, index as i64 + 1);
        assert!(matches!(values[0], SqliteValue::Null));
    }
}

#[test]
fn text_outside_ascii_survives_intact() {
    let bytes = fixture("records.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let rows = table_rows(&reader, 5);
    let values: Vec<&str> = rows.iter().map(|(_, v)| one_text(v)).collect();

    assert_eq!(values[0], "\u{fc}ber");
    assert_eq!(values[1], "\u{65e5}\u{672c}\u{8a9e}");
    assert_eq!(values[2], "\u{1f600} emoji");
    // 601 bytes of utf-8 in a 512 byte page, so this one spills in the
    // middle of a two byte character
    assert_eq!(values[3], format!("a{}", "\u{e9}".repeat(300)));
    assert_eq!(values[3].len(), 601);
}

// ---------------------------------------------------------------------------
// records that do not
// ---------------------------------------------------------------------------

#[test]
fn payloads_across_the_spill_boundary_come_back_whole() {
    let bytes = fixture("records.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let rows = table_rows(&reader, 2);

    let lengths: Vec<usize> = [0usize, 1, 57, 58, 100, 400]
        .iter()
        .copied()
        .chain(468..=486)
        .chain([600, 1000, 5000, 50000])
        .collect();
    assert_eq!(rows.len(), lengths.len());

    for (row, n) in rows.iter().zip(&lengths) {
        let (rowid, values) = row;
        assert_eq!(*rowid, *n as i64, "the primary key is the rowid");
        let text = one_text(values);
        assert_eq!(text.len(), *n, "row {n} has the wrong length");
        assert_eq!(text, text_of(*n), "row {n} has the wrong content");
    }
}

#[test]
fn blobs_land_exactly_on_overflow_page_boundaries() {
    let bytes = fixture("records.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let rows = table_rows(&reader, 3);

    // 508 and 1016 are one and two full overflow pages of payload
    let lengths = [0usize, 1, 300, 474, 475, 476, 508, 509, 1016, 1017, 20000];
    assert_eq!(rows.len(), lengths.len());
    for (row, n) in rows.iter().zip(&lengths) {
        match &row.1[1] {
            SqliteValue::Blob(b) => {
                assert_eq!(b.len(), *n, "blob {n} has the wrong length");
                assert_eq!(*b, blob_of(*n), "blob {n} has the wrong content");
            }
            other => panic!("expected a blob, got {other:?}"),
        }
    }
}

#[test]
fn index_entries_spill_at_their_own_much_lower_boundary() {
    let bytes = fixture("records.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();

    // walk the index b-tree rooted at page 8 and collect its keys
    fn index_keys(reader: &Reader<SliceSource<'_>>, page_no: u32, out: &mut Vec<Vec<SqliteValue>>) {
        let page = reader.btree_page(page_no).unwrap();
        let mut children = Vec::new();
        for i in 0..page.cell_count() {
            match reader.cell(&page, i).unwrap() {
                Cell::IndexLeaf { payload } => out.push(decode_record(&payload).unwrap()),
                Cell::IndexInterior { child, payload } => {
                    children.push(child);
                    out.push(decode_record(&payload).unwrap());
                }
                other => panic!("index page held {other:?}"),
            }
        }
        if let Some(right) = page.right_most {
            children.push(right);
        }
        for child in children {
            index_keys(reader, child, out);
        }
    }

    let mut keys = Vec::new();
    index_keys(&reader, 8, &mut keys);

    // one entry per row of idx_spill, each holding the key and the rowid
    let lengths: Vec<usize> = (90..=115).chain([400, 3000]).collect();
    assert_eq!(keys.len(), lengths.len());
    for key in &keys {
        assert_eq!(key.len(), 2, "an index entry is (key, rowid)");
        let text = match &key[0] {
            SqliteValue::Text(t) => t,
            other => panic!("expected text, got {other:?}"),
        };
        let rowid = match &key[1] {
            SqliteValue::Integer(n) => *n,
            other => panic!("expected an integer rowid, got {other:?}"),
        };
        assert_eq!(text.len(), rowid as usize, "the key length is its rowid");
        assert_eq!(*text, text_of(rowid as usize));
    }
}

// ---------------------------------------------------------------------------
// cells that lie
// ---------------------------------------------------------------------------

/// Find the byte offset of the first cell on a page, so a test can corrupt
/// the payload length varint that lives there.
fn first_cell_offset(bytes: &[u8], page_size: usize, page: u32) -> usize {
    let base = (page as usize - 1) * page_size;
    let header = if page == 1 { base + 100 } else { base };
    // interior pages spend four more header bytes on their right most child
    let header_len = if matches!(bytes[header], 2 | 5) {
        12
    } else {
        8
    };
    let cell_pointer =
        u16::from_be_bytes([bytes[header + header_len], bytes[header + header_len + 1]]) as usize;
    base + cell_pointer
}

/// The first table leaf page under `root`, which is where cells with a
/// payload length worth corrupting actually live.
fn first_leaf(reader: &Reader<SliceSource<'_>>, root: u32) -> u32 {
    let page = reader.btree_page(root).unwrap();
    match page.kind {
        PageKind::LeafTable => root,
        PageKind::InteriorTable => match reader.cell(&page, 0).unwrap() {
            Cell::TableInterior { child, .. } => first_leaf(reader, child),
            other => panic!("interior page held {other:?}"),
        },
        other => panic!("{other:?} is not a table page"),
    }
}

/// Pages that are not b-tree pages. records.sqlite is vacuumed, so its
/// freelist is empty and every one of these is an overflow page.
fn overflow_pages(reader: &Reader<SliceSource<'_>>) -> Vec<u32> {
    (1..=reader.page_count())
        .filter(|p| reader.btree_page(*p).is_err())
        .collect()
}

/// An overflow page that is not the last of its chain, so that rewriting
/// its next pointer actually changes something. Page order and chain order
/// are unrelated, so the first overflow page in the file is usually the
/// tail of some chain.
fn mid_chain_overflow_page(reader: &Reader<SliceSource<'_>>) -> u32 {
    for page in overflow_pages(reader) {
        let bytes = reader.read_page(page).unwrap();
        let next = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        if next != 0 {
            return page;
        }
    }
    panic!("the fixture has no multi page overflow chain")
}

#[test]
fn a_payload_larger_than_the_file_is_refused() {
    let original = fixture("records.sqlite");
    // the reader borrows the bytes, so it lives in a block of its own and
    // only its answer escapes
    let leaf = {
        let reader = Reader::open(SliceSource::new(&original)).unwrap();
        first_leaf(&reader, 2)
    };

    // make the first cell of that leaf claim a payload of 2 mb, which a
    // 121 kb file cannot possibly back. four varint bytes, because a cell
    // sitting at the very end of the content area has no room for nine.
    let mut bytes = original;
    let at = first_cell_offset(&bytes, 512, leaf);
    bytes[at..at + 4].copy_from_slice(&[0x81, 0x80, 0x80, 0x00]);

    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let page = reader.btree_page(leaf).unwrap();
    let err = reader
        .cell(&page, 0)
        .expect_err("a payload larger than the file was accepted");
    assert!(err.to_string().contains("more than"), "message: {err}");
}

#[test]
fn a_broken_overflow_chain_is_refused_not_followed() {
    let original = fixture("records.sqlite");
    let victim = {
        let reader = Reader::open(SliceSource::new(&original)).unwrap();
        assert!(
            overflow_pages(&reader).len() > 100,
            "the fixture should be full of overflow"
        );
        mid_chain_overflow_page(&reader)
    };

    // point the first overflow page's next pointer at nothing, at a page
    // outside the file, and at itself. every one of those must surface as
    // an error from some cell, and the self reference must not hang.
    for patch in [0u32, 999_999, u32::MAX, victim] {
        let mut bytes = original.clone();
        let at = (victim as usize - 1) * 512;
        bytes[at..at + 4].copy_from_slice(&patch.to_be_bytes());

        let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
        let mut errors = 0;
        for page_no in 1..=reader.page_count() {
            if let Ok(page) = reader.btree_page(page_no) {
                for i in 0..page.cell_count() {
                    if reader.cell(&page, i).is_err() {
                        errors += 1;
                    }
                }
            }
        }
        assert!(errors > 0, "a chain pointing at {patch} was accepted");
    }
}

#[test]
fn corrupting_any_cell_never_panics() {
    let original = fixture("records.sqlite");
    let page_size = 512usize;

    // walk a window of bytes across the first cells of several pages and
    // set each to 0xff. every outcome must be an answer.
    for page in [2u32, 3, 4, 6, 8] {
        let at = first_cell_offset(&original, page_size, page);
        for offset in 0..24usize {
            let mut bytes = original.clone();
            bytes[at + offset] = 0xff;
            let reader = match Reader::open(SliceSource::new(&bytes)) {
                Ok(r) => r,
                Err(_) => continue,
            };
            if let Ok(btree) = reader.btree_page(page) {
                for i in 0..btree.cell_count() {
                    if let Ok(cell) = reader.cell(&btree, i) {
                        let payload = match &cell {
                            Cell::TableLeaf { payload, .. }
                            | Cell::IndexLeaf { payload }
                            | Cell::IndexInterior { payload, .. } => payload.clone(),
                            Cell::TableInterior { .. } => continue,
                        };
                        let _ = decode_record(&payload);
                    }
                }
            }
        }
    }
}
