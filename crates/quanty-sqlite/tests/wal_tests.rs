//! Reading a write-ahead log.
//!
//! The fixture pair in tests/data was produced by a real SQLite and never
//! checkpointed, so the main file holds 2 pages and everything else, the
//! `grown` table included, exists only in the log. Its 38 frames end in an
//! open transaction that spilled out of the page cache and was rolled back,
//! which is the case that separates a reader from a plausible one: those
//! frames are well formed, correctly checksummed, and hold rows that were
//! never committed.
//!
//! Frames 1, 2, 3, 5 and 15 are the commit frames, so the log ends at frame
//! 15 with a database of 11 pages, and frames 16 to 38 are the rolled back
//! transaction.

use quanty_sqlite::{FileSource, SliceSource, SqliteError, Wal};

fn data_path(name: &str) -> String {
    format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"))
}

fn wal_bytes() -> Vec<u8> {
    std::fs::read(data_path("wal_mode.sqlite-wal")).expect("the fixture is checked in")
}

fn open(bytes: &[u8]) -> Wal<SliceSource<'_>> {
    Wal::open(SliceSource::new(bytes)).expect("the log parses")
}

fn open_err(bytes: &[u8]) -> SqliteError {
    match Wal::open(SliceSource::new(bytes)) {
        Ok(_) => panic!("expected an error, but the log was accepted"),
        Err(e) => e,
    }
}

/// Recompute the header checksum after changing a field, the way a writer
/// would. Without this, every header mutation fails the checksum first and
/// the field checks are never reached.
///
/// This is the checksum algorithm written out a second time, deliberately:
/// a test that reuses the implementation it is checking proves nothing
/// about it.
fn reseal_header(bytes: &mut [u8]) {
    let (mut s0, mut s1) = (0u32, 0u32);
    for chunk in bytes[..24].as_chunks::<8>().0 {
        let x = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        let y = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
        s0 = s0.wrapping_add(x).wrapping_add(s1);
        s1 = s1.wrapping_add(y).wrapping_add(s0);
    }
    bytes[24..28].copy_from_slice(&s0.to_be_bytes());
    bytes[28..32].copy_from_slice(&s1.to_be_bytes());
}

/// Byte offset of frame `number`, counting from 1.
fn frame_at(number: u32, page_size: usize) -> usize {
    32 + (number as usize - 1) * (24 + page_size)
}

#[test]
fn the_log_ends_at_its_last_commit() {
    let wal = Wal::open(FileSource::open(data_path("wal_mode.sqlite-wal")).unwrap()).unwrap();

    assert_eq!(wal.page_size(), 512);
    assert!(!wal.is_empty());
    let (total, committed) = wal.frame_counts();
    assert_eq!(total, 38, "the file holds 38 frames");
    assert_eq!(committed, 15, "only 15 of them were committed");
    assert_eq!(
        wal.page_count(),
        11,
        "the database is 11 pages as of that commit"
    );
}

#[test]
fn a_page_written_several_times_comes_back_at_its_newest_version() {
    let bytes = wal_bytes();
    let wal = open(&bytes);

    // page 2 was rewritten by each of the three updates. the newest
    // committed version holds the third rewrite, so the older strings must
    // not be what comes out.
    let mut page = vec![0u8; 512];
    assert!(wal.read_page_part(2, 0, &mut page).unwrap());
    let text = String::from_utf8_lossy(&page).to_string();
    assert!(
        text.contains("rewritten-2"),
        "the newest version is missing"
    );
    assert!(!text.contains("rewritten-1"), "an older version came back");
    assert!(!text.contains("rewritten-0"), "an older version came back");
}

#[test]
fn rolled_back_frames_are_not_read() {
    let bytes = wal_bytes();
    let wal = open(&bytes);

    // the rolled back transaction wrote hundreds of rows into pages past
    // the committed size, and set id=2 to a marker string. none of it may
    // surface.
    for number in 1..=wal.page_count() {
        let mut page = vec![0u8; 512];
        if wal.read_page_part(number, 0, &mut page).unwrap() {
            let text = String::from_utf8_lossy(&page).to_string();
            assert!(
                !text.contains("never-committed"),
                "page {number} holds rows from the rolled back transaction"
            );
        }
    }

    // and pages that only the rolled back transaction created are not
    // served at all
    let mut page = vec![0u8; 512];
    assert!(
        !wal.read_page_part(wal.page_count() + 1, 0, &mut page)
            .unwrap(),
        "a page past the committed size was served"
    );
}

#[test]
fn a_broken_checksum_ends_the_log_where_it_breaks() {
    let mut bytes = wal_bytes();
    // damage the page data of frame 6, which sits between the commit at
    // frame 5 and the one at frame 15
    let at = frame_at(6, 512) + 24;
    bytes[at] ^= 0xff;

    let wal = open(&bytes);
    let (total, committed) = wal.frame_counts();
    assert_eq!(total, 38, "the file still holds 38 frames");
    assert_eq!(
        committed, 5,
        "the log now ends at the commit before the damage"
    );
    assert_eq!(wal.page_count(), 3, "which is a database of 3 pages");
}

#[test]
fn damage_after_the_last_commit_changes_nothing() {
    let mut bytes = wal_bytes();
    // frame 20 is inside the rolled back transaction, which is ignored
    // anyway, so breaking it must not move the boundary
    let at = frame_at(20, 512) + 24;
    bytes[at] ^= 0xff;

    let wal = open(&bytes);
    assert_eq!(wal.frame_counts().1, 15);
    assert_eq!(wal.page_count(), 11);
}

#[test]
fn frames_from_an_earlier_generation_are_ignored() {
    let mut bytes = wal_bytes();
    // a checkpoint bumps the salts and starts writing at the front again,
    // so a frame whose salt does not match belongs to a log that no longer
    // exists. change frame 4's salt-1.
    let at = frame_at(4, 512) + 8;
    bytes[at] ^= 0x01;

    let wal = open(&bytes);
    assert_eq!(
        wal.frame_counts().1,
        3,
        "the scan stops at the stale frame, so the commit at frame 3 is the last"
    );
    assert_eq!(wal.page_count(), 2);
}

#[test]
fn a_log_with_no_commit_at_all_contributes_nothing() {
    let mut bytes = wal_bytes();
    // break the very first frame: nothing after it can be trusted either
    let at = frame_at(1, 512) + 24;
    bytes[at] ^= 0xff;

    let wal = open(&bytes);
    assert!(wal.is_empty());
    assert_eq!(wal.page_count(), 0);
    assert_eq!(wal.frame_counts().1, 0);
}

#[test]
fn an_empty_log_is_not_an_error() {
    // sqlite leaves a zero length -wal behind after a checkpoint
    let wal = open(&[]);
    assert!(wal.is_empty());
    assert_eq!(wal.page_count(), 0);
    assert_eq!(wal.page_size(), 0);

    // and a file too short to hold a header is the same case
    let wal = open(&[0u8; 20]);
    assert!(wal.is_empty());
}

#[test]
fn a_truncated_log_stops_at_its_last_whole_frame() {
    let mut bytes = wal_bytes();
    // cut in the middle of frame 8
    bytes.truncate(frame_at(8, 512) + 100);

    let wal = open(&bytes);
    assert_eq!(wal.frame_counts().0, 7, "seven whole frames remain");
    assert_eq!(wal.frame_counts().1, 5, "the commit at frame 5 is the last");
    assert_eq!(wal.page_count(), 3);
}

#[test]
fn a_file_that_is_not_a_log_is_refused() {
    let mut bytes = wal_bytes();
    bytes[1] ^= 0xff;
    let err = open_err(&bytes);
    assert!(err.to_string().contains("magic"), "message was: {err}");

    // a sqlite database is not a log either
    let db = std::fs::read(data_path("wal_mode.sqlite")).unwrap();
    assert!(matches!(open_err(&db), SqliteError::Malformed { .. }));
}

#[test]
fn a_header_that_contradicts_itself_is_refused() {
    // the header checksum covers its first 24 bytes, so changing the page
    // size invalidates it, and a header we cannot trust cannot be used to
    // judge the frames either
    let mut bytes = wal_bytes();
    bytes[9] = 0x08;
    let err = open_err(&bytes);
    assert!(err.to_string().contains("checksum"), "message was: {err}");
}

#[test]
fn a_newer_log_format_is_refused_by_name() {
    let mut bytes = wal_bytes();
    bytes[4..8].copy_from_slice(&4_000_000u32.to_be_bytes());
    reseal_header(&mut bytes);
    let err = open_err(&bytes);
    assert!(matches!(err, SqliteError::Unsupported(_)), "was: {err}");
    assert!(err.to_string().contains("4000000"), "message was: {err}");
}

#[test]
fn an_impossible_page_size_is_refused() {
    let mut bytes = wal_bytes();
    bytes[8..12].copy_from_slice(&1000u32.to_be_bytes());
    reseal_header(&mut bytes);
    let err = open_err(&bytes);
    assert!(err.to_string().contains("page size"), "message was: {err}");
}

#[test]
fn a_read_past_the_end_of_a_page_is_refused() {
    let bytes = wal_bytes();
    let wal = open(&bytes);
    let mut buf = vec![0u8; 100];
    assert!(wal.read_page_part(2, 500, &mut buf).is_err());
    // and a page the log does not hold simply is not served
    assert!(!wal.read_page_part(9999, 0, &mut buf).unwrap());
}
