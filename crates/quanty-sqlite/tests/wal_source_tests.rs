//! Reading a database whose log has not been checkpointed.
//!
//! The pair in tests/data is the hard case: the main file holds two pages,
//! the log holds everything else including the schema of a whole table, and
//! its last 23 frames belong to a transaction that was rolled back. What
//! sqlite itself reads from that pair is written down in the assertions
//! below, and it is what this reader has to produce.
//!
//! The other half of this file is about the refusal. Before the log could
//! be read, any database with the wal flag set was turned away, which is
//! correct for a pair like the one above and wrong for the far more common
//! case of a database that was checkpointed and closed cleanly. The flag
//! says nothing on its own; what matters is whether anything accounts for
//! the log.

use quanty_sqlite::{
    open_file, FileSource, Reader, SliceSource, SqliteError, SqliteValue, WalSource,
};

fn data_path(name: &str) -> String {
    format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"))
}

/// Every row of a table, as (rowid, first column after the rowid alias).
fn rows_of(reader: &Reader<impl quanty_sqlite::Source>, table: &str) -> Vec<(i64, String)> {
    let schema = reader.schema().unwrap();
    let object = schema
        .object(table)
        .unwrap_or_else(|| panic!("{table} is not in the schema"));
    reader
        .rows(object.root_page.unwrap())
        .unwrap()
        .map(|row| {
            let row = row.unwrap();
            let text = match &row.values[1] {
                SqliteValue::Text(t) => t.clone(),
                other => panic!("expected text, got {other:?}"),
            };
            (row.rowid.unwrap(), text)
        })
        .collect()
}

#[test]
fn the_log_supplies_what_the_main_file_is_missing() {
    let reader = open_file(data_path("wal_mode.sqlite")).unwrap();

    // the main file is two pages; the log's last commit takes the database
    // to eleven
    assert_eq!(reader.page_count(), 11);

    let t = rows_of(&reader, "t");
    assert_eq!(t.len(), 20, "t holds 20 rows");

    // the row rewritten three times comes back at its newest committed
    // version, and the row the rolled back transaction touched does not
    assert_eq!(t[0], (1, "rewritten-2".to_string()));
    assert_eq!(t[1], (2, "original-02".to_string()));
    assert!(
        !t.iter().any(|(id, _)| *id == 999),
        "a row from the rolled back transaction was returned"
    );
    assert!(
        !t.iter().any(|(_, v)| v.contains("never-committed")),
        "a value from the rolled back transaction was returned"
    );

    // grown exists only in the log, schema and all
    let grown = rows_of(&reader, "grown");
    assert_eq!(grown.len(), 200);
    assert_eq!(grown[0], (1, "wal-only-001".to_string()));
    assert_eq!(grown[199], (200, "wal-only-200".to_string()));
}

#[test]
fn without_the_log_the_same_file_is_refused() {
    // reading the main file on its own is exactly the mistake the refusal
    // exists to prevent: it parses, and it is missing a table
    let bytes = std::fs::read(data_path("wal_mode.sqlite")).unwrap();
    // Reader is not Debug (that would bound its source), so unwrap the
    // error side by hand
    let err = match Reader::open(SliceSource::new(&bytes)) {
        Ok(_) => panic!("a wal mode database was read without its log"),
        Err(e) => e,
    };

    assert!(matches!(err, SqliteError::Unsupported(_)));
    let message = err.to_string();
    assert!(message.contains("wal"), "message was: {message}");
    assert!(
        message.contains("open_file") || message.contains("checkpoint"),
        "the message should say what to do instead: {message}"
    );
}

#[test]
fn a_checkpointed_database_is_not_refused() {
    // this one has the wal flag set and no log next to it, which is what a
    // clean close leaves behind. it is complete, and turning it away would
    // reject a large share of real databases.
    let reader = open_file(data_path("wal_checkpointed.sqlite")).unwrap();
    assert!(reader.header().wal_mode, "the flag is set");
    assert_eq!(rows_of(&reader, "t").len(), 50);

    // and the plain file source reaches the same conclusion, because it
    // looked beside the file
    let reader = Reader::open(FileSource::open(data_path("wal_checkpointed.sqlite")).unwrap())
        .expect("a checkpointed database opens without ceremony");
    assert_eq!(rows_of(&reader, "t").len(), 50);
}

#[test]
fn an_absent_log_changes_nothing() {
    // a WalSource with no log is the same database as without one
    let with_none: WalSource<FileSource, FileSource> =
        WalSource::new(FileSource::open(data_path("chinook.sqlite")).unwrap(), None).unwrap();
    assert!(!with_none.has_log());

    let through_adapter = Reader::open(with_none).unwrap();
    let plain = Reader::open(FileSource::open(data_path("chinook.sqlite")).unwrap()).unwrap();
    assert_eq!(through_adapter.page_count(), plain.page_count());
    assert_eq!(
        rows_of(&through_adapter, "Album").len(),
        rows_of(&plain, "Album").len()
    );
}

#[test]
fn an_empty_log_is_the_same_as_none() {
    let empty = std::fs::read(data_path("wal_checkpointed.sqlite")).unwrap();
    let source: WalSource<SliceSource<'_>, SliceSource<'_>> =
        WalSource::new(SliceSource::new(&empty), Some(SliceSource::new(&[]))).unwrap();
    assert!(!source.has_log(), "an empty log holds no committed frames");
    assert_eq!(Reader::open(source).unwrap().page_count(), 4);
}

#[test]
fn a_log_written_for_another_page_size_is_refused() {
    let main = std::fs::read(data_path("wal_mode.sqlite")).unwrap();
    let mut log = std::fs::read(data_path("wal_mode.sqlite-wal")).unwrap();

    // claim 1024 byte pages in the log header, and reseal it so the claim
    // is the thing that gets caught rather than the checksum
    log[8..12].copy_from_slice(&1024u32.to_be_bytes());
    let (mut s0, mut s1) = (0u32, 0u32);
    for chunk in log[..24].as_chunks::<8>().0 {
        let x = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        let y = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
        s0 = s0.wrapping_add(x).wrapping_add(s1);
        s1 = s1.wrapping_add(y).wrapping_add(s0);
    }
    log[24..28].copy_from_slice(&s0.to_be_bytes());
    log[28..32].copy_from_slice(&s1.to_be_bytes());

    let result: Result<WalSource<SliceSource<'_>, SliceSource<'_>>, SqliteError> =
        WalSource::new(SliceSource::new(&main), Some(SliceSource::new(&log)));
    match result {
        Ok(_) => panic!("a log with the wrong page size was accepted"),
        Err(e) => assert!(e.to_string().contains("page"), "message was: {e}"),
    }
}

#[test]
fn a_read_spanning_two_pages_gets_each_half_from_the_right_place() {
    // the adapter serves page by page, so a range crossing a boundary must
    // come from two different files where that is what the log says
    let main = std::fs::read(data_path("wal_mode.sqlite")).unwrap();
    let log = std::fs::read(data_path("wal_mode.sqlite-wal")).unwrap();
    let source: WalSource<SliceSource<'_>, SliceSource<'_>> =
        WalSource::new(SliceSource::new(&main), Some(SliceSource::new(&log))).unwrap();

    use quanty_sqlite::Source;
    let mut across = vec![0u8; 20];
    source.read_at(512 - 10, &mut across).unwrap();

    let mut first = vec![0u8; 512];
    let mut second = vec![0u8; 512];
    source.read_at(0, &mut first).unwrap();
    source.read_at(512, &mut second).unwrap();

    assert_eq!(&across[..10], &first[502..512]);
    assert_eq!(&across[10..], &second[..10]);
}

#[test]
fn the_database_is_as_large_as_the_last_commit_says() {
    // the main file is 1024 bytes, two pages, and the log's last commit
    // says eleven. the size the reader works with is the log's.
    let main = std::fs::read(data_path("wal_mode.sqlite")).unwrap();
    let log = std::fs::read(data_path("wal_mode.sqlite-wal")).unwrap();
    assert_eq!(main.len(), 1024);

    let source: WalSource<SliceSource<'_>, SliceSource<'_>> =
        WalSource::new(SliceSource::new(&main), Some(SliceSource::new(&log))).unwrap();
    use quanty_sqlite::Source;
    assert_eq!(source.len().unwrap(), 11 * 512);
    assert!(source.has_log());
}
