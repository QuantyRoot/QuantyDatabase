//! Reading the header and the b-tree page headers of a real database, and
//! refusing files that lie about themselves.
//!
//! The expectations for chinook were taken from the file itself with an
//! independent tool, not from this reader, so a bug here cannot agree with
//! them by construction.

use quanty_sqlite::{FileSource, PageKind, Reader, SliceSource, SqliteError, TextEncoding};

fn chinook_path() -> String {
    format!("{}/tests/data/chinook.sqlite", env!("CARGO_MANIFEST_DIR"))
}

fn chinook_bytes() -> Vec<u8> {
    std::fs::read(chinook_path()).expect("the chinook fixture is checked into the repo")
}

fn open_bytes(bytes: &[u8]) -> Result<Reader<SliceSource<'_>>, SqliteError> {
    Reader::open(SliceSource::new(bytes))
}

/// `Reader` is deliberately not `Debug` (it would have to bound `S`), so
/// the tests unwrap the error side by hand.
fn open_err(bytes: &[u8]) -> SqliteError {
    match open_bytes(bytes) {
        Ok(_) => panic!("expected an error, but the file was accepted"),
        Err(e) => e,
    }
}

#[test]
fn reads_the_chinook_header() {
    let reader = Reader::open(FileSource::open(chinook_path()).unwrap()).unwrap();
    let header = reader.header();

    assert_eq!(header.page_size, 1024);
    assert_eq!(header.reserved_space, 0);
    assert_eq!(header.usable_size(), 1024);
    assert_eq!(header.text_encoding, TextEncoding::Utf8);
    assert_eq!(header.schema_format, 4);
    assert_eq!(header.first_freelist_trunk, 8);
    assert_eq!(header.freelist_page_count, 199);
    assert_eq!(header.sqlite_version_number, 3036000);
    assert_eq!(header.header_page_count, Some(1042));
    assert_eq!(reader.page_count(), 1042);
}

#[test]
fn the_file_and_slice_backends_agree() {
    let bytes = chinook_bytes();
    let from_file = Reader::open(FileSource::open(chinook_path()).unwrap()).unwrap();
    let from_slice = open_bytes(&bytes).unwrap();

    assert_eq!(from_file.header(), from_slice.header());
    assert_eq!(from_file.page_count(), from_slice.page_count());
    for page in [1u32, 2, 19, 409, 1042] {
        assert_eq!(
            from_file.read_page(page).unwrap(),
            from_slice.read_page(page).unwrap(),
            "page {page} differs between backends"
        );
    }
}

#[test]
fn page_one_is_the_schema_root() {
    let bytes = chinook_bytes();
    let reader = open_bytes(&bytes).unwrap();
    let page = reader.btree_page(1).unwrap();

    // sqlite_master is a table b-tree rooted at page 1. chinook's schema is
    // 22 objects (11 tables and 11 indexes) and the create statements are
    // long enough that they do not fit one 1024 byte page, so the root is an
    // interior page with eight leaves under it.
    assert_eq!(page.kind, PageKind::InteriorTable);
    assert_eq!(page.cell_count(), 7);
    assert_eq!(page.right_most, Some(419));
    assert!(page.cell(7).is_err(), "cell 7 does not exist");

    // an interior table cell starts with its child page number, so the
    // children can be walked without decoding records yet
    let mut children = Vec::new();
    for i in 0..page.cell_count() {
        let cell = page.cell(i).unwrap();
        children.push(u32::from_be_bytes([cell[0], cell[1], cell[2], cell[3]]));
    }
    children.push(page.right_most.unwrap());
    assert_eq!(children, vec![387, 391, 394, 397, 401, 408, 412, 419]);

    let entries: usize = children
        .iter()
        .map(|c| {
            let leaf = reader.btree_page(*c).unwrap();
            assert_eq!(leaf.kind, PageKind::LeafTable);
            leaf.cell_count()
        })
        .sum();
    assert_eq!(entries, 22, "11 tables plus 11 indexes");
}

#[test]
fn every_page_of_chinook_parses_or_is_not_a_btree_page() {
    let bytes = chinook_bytes();
    let reader = open_bytes(&bytes).unwrap();

    // freelist and overflow pages are not b-tree pages, so a rejection is
    // the correct answer for some of these. what matters is that all 1042
    // return an answer rather than panicking, and that the b-tree pages
    // that do parse are self consistent.
    let mut parsed = 0;
    for number in 1..=reader.page_count() {
        if let Ok(page) = reader.btree_page(number) {
            parsed += 1;
            for i in 0..page.cell_count() {
                page.cell(i).expect("a validated cell pointer stays valid");
            }
            assert_eq!(page.right_most.is_some(), page.kind.is_interior());
        }
    }
    assert!(
        parsed > 800,
        "only {parsed} of {} pages parsed as b-tree pages",
        reader.page_count()
    );
}

#[test]
fn interior_pages_carry_a_right_most_child() {
    let bytes = chinook_bytes();
    let reader = open_bytes(&bytes).unwrap();

    // Track is the largest table and does not fit a single page, so its
    // root at page 409 has to be an interior page
    let root = reader.btree_page(409).unwrap();
    assert_eq!(root.kind, PageKind::InteriorTable);
    let child = root.right_most.expect("interior pages have a right child");
    assert!(child >= 1 && child <= reader.page_count());
}

// ---------------------------------------------------------------------------
// files that lie
// ---------------------------------------------------------------------------

#[test]
fn empty_and_short_files_are_not_sqlite() {
    assert!(matches!(open_err(&[]), SqliteError::NotSqlite(_)));
    assert!(matches!(open_err(&[0u8; 99]), SqliteError::NotSqlite(_)));
    let almost = b"SQLite format 3\0";
    assert!(matches!(open_err(almost), SqliteError::NotSqlite(_)));
}

#[test]
fn a_wrong_magic_string_is_not_sqlite() {
    let mut bytes = chinook_bytes();
    bytes[3] = b'x';
    assert!(matches!(open_err(&bytes), SqliteError::NotSqlite(_)));
}

#[test]
fn impossible_page_sizes_are_refused() {
    // 0, 3 and 1000 are respectively zero, not a power of two, and not a
    // power of two either; 1 is legal and means 65536
    for (hi, lo, why) in [
        (0u8, 0u8, "zero"),
        (0, 3, "not a power of two"),
        (3, 232, "1000"),
    ] {
        let mut bytes = chinook_bytes();
        bytes[16] = hi;
        bytes[17] = lo;
        assert!(
            matches!(open_err(&bytes), SqliteError::Malformed { .. }),
            "page size {why} was accepted"
        );
    }
}

#[test]
fn wal_mode_is_refused_rather_than_read_stale() {
    let mut bytes = chinook_bytes();
    bytes[18] = 2;
    bytes[19] = 2;
    let err = open_err(&bytes);
    assert!(matches!(err, SqliteError::Unsupported(_)));
    assert!(err.to_string().contains("wal"), "message was: {err}");
}

#[test]
fn utf16_is_refused_rather_than_guessed() {
    for encoding in [2u8, 3] {
        let mut bytes = chinook_bytes();
        bytes[59] = encoding;
        assert!(matches!(open_err(&bytes), SqliteError::Unsupported(_)));
    }
}

#[test]
fn a_bad_encoding_number_is_malformed() {
    let mut bytes = chinook_bytes();
    bytes[59] = 7;
    assert!(matches!(open_err(&bytes), SqliteError::Malformed { .. }));
}

#[test]
fn reserved_space_that_leaves_too_little_room_is_refused() {
    let mut bytes = chinook_bytes();
    bytes[20] = 200; // 1024 - 200 = 824, still fine
    assert!(open_bytes(&bytes).is_ok());
    bytes[20] = 250; // 774, fine
    assert!(open_bytes(&bytes).is_ok());
    bytes[20] = 255; // 769, still above the 480 floor
    assert!(open_bytes(&bytes).is_ok());

    // the floor only bites on small pages, so shrink the page size too
    let mut small = chinook_bytes();
    small[16] = 2; // page size 512
    small[17] = 0;
    small[20] = 200; // 512 - 200 = 312, below the 480 the format requires
    assert!(matches!(open_err(&small), SqliteError::Malformed { .. }));
}

#[test]
fn wrong_payload_fractions_are_malformed() {
    for at in [21usize, 22, 23] {
        let mut bytes = chinook_bytes();
        bytes[at] = 99;
        assert!(
            matches!(open_err(&bytes), SqliteError::Malformed { .. }),
            "byte {at} was accepted"
        );
    }
}

#[test]
fn a_truncated_file_is_caught_at_open() {
    let mut bytes = chinook_bytes();
    bytes.truncate(bytes.len() / 2);
    let err = open_err(&bytes);
    assert!(matches!(err, SqliteError::Malformed { .. }));
    assert!(err.to_string().contains("1042"), "message was: {err}");
}

#[test]
fn a_stale_header_page_count_falls_back_to_the_file() {
    let mut bytes = chinook_bytes();
    // make version-valid-for disagree with the change counter, which is how
    // sqlite marks the in-header page count as not current
    bytes[95] = bytes[95].wrapping_add(1);
    let reader = open_bytes(&bytes).unwrap();
    assert_eq!(reader.header().header_page_count, None);
    assert_eq!(reader.page_count(), 1042, "derived from the file length");
}

#[test]
fn pages_outside_the_file_are_refused() {
    let bytes = chinook_bytes();
    let reader = open_bytes(&bytes).unwrap();
    assert!(reader.read_page(0).is_err());
    assert!(reader.read_page(1043).is_err());
    assert!(reader.read_page(u32::MAX).is_err());
    assert!(reader.btree_page(0).is_err());
}

#[test]
fn a_corrupt_page_header_never_panics() {
    let bytes = chinook_bytes();
    let good = open_bytes(&bytes).unwrap();
    let page_one_kind = good.btree_page(1).unwrap().kind;
    assert_eq!(page_one_kind, PageKind::InteriorTable);

    // walk one byte at a time through page 1's b-tree header and its first
    // cell pointers, setting each to 0xff. every result must be an answer,
    // and any page that still parses must still be self consistent.
    for at in 100..140usize {
        let mut broken = bytes.clone();
        broken[at] = 0xff;
        match open_bytes(&broken) {
            Err(_) => continue,
            Ok(reader) => {
                if let Ok(page) = reader.btree_page(1) {
                    for i in 0..page.cell_count() {
                        page.cell(i).expect("validated pointers stay in range");
                    }
                }
            }
        }
    }
}

#[test]
fn arbitrary_bytes_behind_a_valid_magic_never_panic() {
    // a deterministic pseudo random body under a correct magic string: the
    // shape an attacker would send. this is a smoke test, the real fuzzing
    // of this path comes with its own harness.
    let mut state = 0x2545_f491_4f6c_dd1du64;
    for round in 0..200 {
        let mut bytes = vec![0u8; 4096];
        bytes[..16].copy_from_slice(b"SQLite format 3\0");
        for b in bytes[16..].iter_mut() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *b = (state >> 24) as u8;
        }
        if let Ok(reader) = open_bytes(&bytes) {
            for number in 1..=reader.page_count().min(8) {
                if let Ok(page) = reader.btree_page(number) {
                    for i in 0..page.cell_count() {
                        page.cell(i).expect("validated pointers stay in range");
                    }
                }
            }
        }
        let _ = round;
    }
}
