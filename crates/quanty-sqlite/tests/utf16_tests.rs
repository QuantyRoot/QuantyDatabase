//! Databases that do not store their text as utf-8.
//!
//! SQLite writes text in whichever of the three encodings the header names,
//! and `pragma encoding` fixes that at creation time. The two fixtures here
//! hold the same eight rows in utf-16 little endian and utf-16 big endian,
//! so the reader has to produce identical strings from files whose bytes
//! differ in every text value.
//!
//! Text comes back as a Rust string either way. The encoding is a property
//! of the bytes on disk, and nothing above the reader should have to know
//! about it.

use quanty_sqlite::{Reader, SliceSource, SqliteError, SqliteValue, TextEncoding};

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("fixture {path}: {e}"))
}

/// The values the generator stored, in rowid order.
fn expected() -> Vec<String> {
    vec![
        "ascii".to_string(),
        "\u{fc}ber \u{e4}nderung".to_string(),
        "\u{65e5}\u{672c}\u{8a9e}".to_string(),
        "\u{1f600} emoji".to_string(),
        // a flag built from tag characters, all of them astral
        "\u{1f3f4}\u{e0067}\u{e0062}\u{e0073}\u{e0063}\u{e0074}\u{e007f}".to_string(),
        String::new(),
        "a".repeat(2000),
        "\u{1f600}".repeat(400),
    ]
}

fn read_values(bytes: &[u8]) -> Vec<String> {
    let reader = Reader::open(SliceSource::new(bytes)).unwrap();
    let root = reader
        .schema()
        .unwrap()
        .object("t")
        .unwrap()
        .root_page
        .unwrap();
    reader
        .rows(root)
        .unwrap()
        .map(|row| match &row.unwrap().values[1] {
            SqliteValue::Text(t) => t.clone(),
            other => panic!("expected text, got {other:?}"),
        })
        .collect()
}

#[test]
fn the_header_names_the_encoding() {
    // the bytes have to outlive the reader that borrows them
    let bytes = fixture("utf16le.sqlite");
    let le = Reader::open(SliceSource::new(&bytes)).unwrap();
    assert_eq!(le.header().text_encoding, TextEncoding::Utf16Le);

    let bytes = fixture("utf16be.sqlite");
    let be = Reader::open(SliceSource::new(&bytes)).unwrap();
    assert_eq!(be.header().text_encoding, TextEncoding::Utf16Be);
}

#[test]
fn both_byte_orders_produce_the_same_strings() {
    let le = read_values(&fixture("utf16le.sqlite"));
    let be = read_values(&fixture("utf16be.sqlite"));

    assert_eq!(le, expected(), "little endian");
    assert_eq!(be, expected(), "big endian");
    assert_eq!(le, be);
}

#[test]
fn the_two_files_really_do_differ_on_disk() {
    // otherwise the test above would pass for the wrong reason
    let le = fixture("utf16le.sqlite");
    let be = fixture("utf16be.sqlite");
    assert_eq!(le.len(), be.len());
    assert_ne!(le, be, "the fixtures should differ byte for byte");
}

#[test]
fn characters_outside_the_basic_plane_survive() {
    // an emoji is one code point and two utf-16 code units, so a reader
    // that treats code units as characters loses half of it
    for name in ["utf16le.sqlite", "utf16be.sqlite"] {
        let values = read_values(&fixture(name));
        assert_eq!(values[3], "\u{1f600} emoji", "{name}");
        assert_eq!(values[3].chars().count(), 7, "{name}: seven characters");
        assert_eq!(values[4].chars().count(), 7, "{name}: a tag sequence flag");
    }
}

#[test]
fn a_long_value_spills_and_comes_back_whole() {
    for name in ["utf16le.sqlite", "utf16be.sqlite"] {
        let values = read_values(&fixture(name));
        // 2000 characters of utf-16 is 4000 bytes, far past a 512 byte page
        assert_eq!(values[6].len(), 2000, "{name}");
        assert!(values[6].chars().all(|c| c == 'a'), "{name}");

        // 400 emoji is 1600 bytes of surrogate pairs, and the spill lands
        // in the middle of them
        assert_eq!(values[7].chars().count(), 400, "{name}");
        assert!(values[7].chars().all(|c| c == '\u{1f600}'), "{name}");
    }
}

#[test]
fn an_unpaired_surrogate_is_refused_rather_than_replaced() {
    // find the record holding "ascii" and turn its first code unit into a
    // lone high surrogate. lossy decoding would hand back a replacement
    // character and carry on, which is how almost-right text gets into a
    // database and stays there.
    let mut bytes = fixture("utf16le.sqlite");
    let needle: Vec<u8> = "ascii"
        .encode_utf16()
        .flat_map(|u| u.to_le_bytes())
        .collect();
    let at = bytes
        .windows(needle.len())
        .position(|window| window == needle)
        .expect("the stored string is in the file");
    bytes[at] = 0x00;
    bytes[at + 1] = 0xd8; // 0xd800, a high surrogate with nothing after it

    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let root = reader
        .schema()
        .unwrap()
        .object("t")
        .unwrap()
        .root_page
        .unwrap();
    let first = reader.rows(root).unwrap().next().unwrap();
    match first {
        Ok(row) => panic!("an unpaired surrogate was accepted as {:?}", row.values),
        Err(e) => assert!(e.to_string().contains("surrogate"), "message was: {e}"),
    }
}

#[test]
fn a_text_value_of_odd_length_is_refused() {
    // utf-16 comes in pairs of bytes, so a text value with an odd number of
    // them is a file contradicting itself. serial type 15 is a text value
    // of one byte.
    let payload = [2u8, 15, b'x'];
    let err = quanty_sqlite::decode_record(&payload, TextEncoding::Utf16Le)
        .expect_err("an odd length utf-16 value was accepted");
    assert!(matches!(err, SqliteError::Malformed { .. }));
    assert!(err.to_string().contains("code unit"), "message was: {err}");

    // and the same bytes are perfectly good utf-8
    let value = quanty_sqlite::decode_record(&payload, TextEncoding::Utf8).unwrap();
    assert_eq!(value, vec![SqliteValue::Text("x".to_string())]);
}

#[test]
fn the_reader_supplies_the_encoding_itself() {
    // the free function can be handed the wrong encoding; the method on the
    // reader cannot, because it reads it off the header in hand
    let bytes = fixture("utf16be.sqlite");
    let reader = Reader::open(SliceSource::new(&bytes)).unwrap();
    let root = reader
        .schema()
        .unwrap()
        .object("t")
        .unwrap()
        .root_page
        .unwrap();

    // the long values push this table past one page, so walk down to a
    // leaf instead of assuming the root is one
    let mut page = reader.btree_page(root).unwrap();
    while page.kind.is_interior() {
        page = match reader.cell(&page, 0).unwrap() {
            quanty_sqlite::Cell::TableInterior { child, .. } => reader.btree_page(child).unwrap(),
            other => panic!("unexpected cell {other:?}"),
        };
    }
    let payload = match reader.cell(&page, 0).unwrap() {
        quanty_sqlite::Cell::TableLeaf { payload, .. } => payload,
        other => panic!("unexpected cell {other:?}"),
    };

    assert_eq!(
        reader.decode_record(&payload).unwrap(),
        quanty_sqlite::decode_record(&payload, TextEncoding::Utf16Be).unwrap()
    );
    // decoded as utf-16 the wrong way round, the same bytes are either an
    // error or a different string, never the same one
    let wrong = quanty_sqlite::decode_record(&payload, TextEncoding::Utf16Le);
    if let Ok(values) = wrong {
        assert_ne!(values, reader.decode_record(&payload).unwrap());
    }
}
