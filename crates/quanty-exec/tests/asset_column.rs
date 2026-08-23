//! The `asset` column: a descriptor in the row, chunks counted by it
//! (ADR-033, ADR-034).

use quanty_core::{BlobRef, Db, MemStorage};
use quanty_exec::{Output, Session};

fn session() -> Session<MemStorage> {
    let db = Db::in_memory().expect("open");
    let mut s = Session::new(db);
    s.execute("table files { id: int @key, body: asset }")
        .expect("define");
    s
}

fn pattern(len: usize) -> Vec<u8> {
    let mut state = 0x2545_f491_4f6c_dd1du64;
    (0..len)
        .map(|_| {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (state >> 33) as u8
        })
        .collect()
}

/// Store bytes and hand back the descriptor a row would keep.
fn stored(s: &Session<MemStorage>, bytes: &[u8]) -> BlobRef {
    s.db().write_blob(bytes).expect("write_blob")
}

fn hex(blob: &BlobRef) -> String {
    blob.encode()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

fn unclaimed(s: &Session<MemStorage>) -> u64 {
    s.db().check_blobs().expect("check").unclaimed
}

fn sound(s: &Session<MemStorage>) -> u64 {
    s.db().check_blobs().expect("check").sound
}

#[test]
fn a_row_that_names_a_blob_claims_its_chunks() {
    let mut s = session();
    let blob = stored(&s, &pattern(3 * quanty_core::CHUNK_SIZE));
    assert_eq!(unclaimed(&s), 3, "write_blob should leave them unclaimed");

    s.execute(&format!("put files {{ id: 1, body: x\"{}\" }}", hex(&blob)))
        .expect("put");

    assert_eq!(sound(&s), 3, "the row did not claim its chunks");
    assert_eq!(unclaimed(&s), 0);
    assert!(s.db().check_blobs().unwrap().is_sound());
}

#[test]
fn deleting_the_row_gives_the_chunks_back() {
    let mut s = session();
    let blob = stored(&s, &pattern(2 * quanty_core::CHUNK_SIZE));
    s.execute(&format!("put files {{ id: 1, body: x\"{}\" }}", hex(&blob)))
        .expect("put");
    assert_eq!(sound(&s), 2);

    s.execute("del files where id = 1").expect("del");

    let report = s.db().check_blobs().expect("check");
    assert!(report.is_sound());
    assert_eq!(report.sound, 0, "the chunks outlived the row");
    assert_eq!(report.unclaimed, 0, "the bytes stayed without their count");
}

#[test]
fn two_rows_naming_one_blob_both_have_to_go() {
    let mut s = session();
    let blob = stored(&s, &pattern(quanty_core::CHUNK_SIZE));
    let literal = hex(&blob);
    s.execute(&format!("put files {{ id: 1, body: x\"{literal}\" }}"))
        .expect("put 1");
    s.execute(&format!("put files {{ id: 2, body: x\"{literal}\" }}"))
        .expect("put 2");
    assert_eq!(sound(&s), 1, "one blob, one chunk");

    s.execute("del files where id = 1").expect("del 1");
    assert_eq!(sound(&s), 1, "the other row still names it");

    s.execute("del files where id = 2").expect("del 2");
    assert_eq!(sound(&s), 0);
    assert_eq!(unclaimed(&s), 0);
}

#[test]
fn overwriting_swaps_one_blob_for_another() {
    let mut s = session();
    let first = stored(&s, &pattern(quanty_core::CHUNK_SIZE));
    let second = stored(&s, &pattern(2 * quanty_core::CHUNK_SIZE + 7));

    s.execute(&format!(
        "put files {{ id: 1, body: x\"{}\" }}",
        hex(&first)
    ))
    .expect("put");
    assert_eq!(sound(&s), 1);

    s.execute(&format!(
        "set files where id = 1 {{ body = x\"{}\" }}",
        hex(&second)
    ))
    .expect("set");

    let report = s.db().check_blobs().expect("check");
    assert!(report.is_sound());
    // the replacement is two full chunks and a tail, so three
    assert_eq!(report.sound, 3, "the new blob is not claimed");
    assert_eq!(report.unclaimed, 0, "the old blob was not released");
}

#[test]
fn dropping_the_table_releases_what_its_rows_held() {
    // drop_table wipes the key range without reading it, so this is the
    // one path that has to go looking. Proved to catch: taking the
    // release pass out of drop_table leaves three sound chunks that no
    // row names.
    let mut s = session();
    for i in 1..=3 {
        let blob = stored(&s, &pattern(quanty_core::CHUNK_SIZE + i));
        s.execute(&format!(
            "put files {{ id: {i}, body: x\"{}\" }}",
            hex(&blob)
        ))
        .expect("put");
    }
    // pattern() runs from one seed, so all three blobs share their first
    // chunk and differ only in the tail: one shared plus three tails.
    assert_eq!(sound(&s), 4);

    s.execute("drop table files").expect("drop");

    let report = s.db().check_blobs().expect("check");
    assert!(report.is_sound());
    assert_eq!(report.sound, 0, "the dropped rows kept their chunks");
    assert_eq!(report.unclaimed, 0);
}

#[test]
fn an_asset_column_refuses_anything_that_is_not_a_descriptor() {
    let mut s = session();

    let err = s
        .execute("put files { id: 1, body: x\"deadbeef\" }")
        .expect_err("not a descriptor");
    assert!(
        err.to_string().contains("not one"),
        "unhelpful message: {err}"
    );

    let err = s
        .execute("put files { id: 1, body: \"a string\" }")
        .expect_err("wrong type");
    assert!(err.to_string().contains("asset"), "{err}");
}

#[test]
fn a_table_without_an_asset_column_stays_at_catalog_version_one() {
    // The version is the lowest that can express the definition, so an
    // older reader keeps working on tables that gained nothing.
    let mut s = Session::new(Db::in_memory().unwrap());
    s.execute("table plain { id: int @key, n: text }").unwrap();
    s.execute("table rich { id: int @key, body: asset }")
        .unwrap();

    let out = s.execute("show tables").expect("show");
    match out {
        Output::Lines(names) => assert_eq!(names, vec!["plain", "rich"]),
        other => panic!("unexpected {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// gc blobs: reachability over rows (ADR-033's open criterion)
// ---------------------------------------------------------------------------

fn swept(s: &mut Session<MemStorage>) -> String {
    match s.execute("gc blobs").expect("gc blobs") {
        Output::Lines(l) => l.join("\n"),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn a_blob_nobody_stored_is_collected() {
    // This is exactly what ADR-033 could not decide: chunks left by a run
    // that died between write_blob and storing the descriptor. Reference
    // counts cannot tell that from an upload in flight; the rows can.
    let mut s = session();
    let orphan = stored(&s, &pattern(2 * quanty_core::CHUNK_SIZE));
    assert_eq!(unclaimed(&s), 2);

    let text = swept(&mut s);
    assert!(text.contains("dropped 2 chunks"), "{text}");

    let report = s.db().check_blobs().expect("check");
    assert_eq!(report.unclaimed, 0);
    assert_eq!(report.sound, 0);
    assert!(report.is_sound(), "half a chunk was left behind");

    // and the descriptor now names nothing, loudly
    let err = s
        .execute(&format!(
            "put files {{ id: 1, body: x\"{}\" }}",
            hex(&orphan)
        ))
        .expect_err("its chunks are gone");
    assert!(err.to_string().contains("chunk"), "{err}");
}

#[test]
fn a_blob_a_row_names_survives_the_sweep() {
    let mut s = session();
    let kept = stored(&s, &pattern(quanty_core::CHUNK_SIZE));
    let orphan = stored(&s, &pattern(3 * quanty_core::CHUNK_SIZE + 11));
    s.execute(&format!("put files {{ id: 1, body: x\"{}\" }}", hex(&kept)))
        .expect("put");

    // the orphan shares its first chunk with the stored one, so the sweep
    // has to keep that and drop only the rest
    let text = swept(&mut s);
    assert!(text.contains("kept 1"), "{text}");

    let report = s.db().check_blobs().expect("check");
    assert_eq!(report.sound, 1);
    assert_eq!(report.unclaimed, 0);
    assert!(!orphan.chunks.is_empty());

    // the row still reads back
    let out = s.execute("get files { id }").expect("get");
    assert!(matches!(out, Output::Rows { .. }));
}

#[test]
fn sweeping_twice_drops_nothing_the_second_time() {
    let mut s = session();
    let blob = stored(&s, &pattern(2 * quanty_core::CHUNK_SIZE));
    s.execute(&format!("put files {{ id: 1, body: x\"{}\" }}", hex(&blob)))
        .expect("put");
    stored(&s, &pattern(quanty_core::CHUNK_SIZE + 3));

    let first = swept(&mut s);
    assert!(first.contains("dropped 1 chunks"), "{first}");
    let second = swept(&mut s);
    assert!(second.contains("dropped 0 chunks"), "{second}");
    assert!(second.contains("kept 2"), "{second}");
}

#[test]
fn a_deleted_row_leaves_nothing_for_the_sweep_to_find() {
    // del already releases, so the count reaches zero and the entry goes.
    // The sweep is for what never got a reference, not for cleaning up
    // after the paths that work.
    let mut s = session();
    let blob = stored(&s, &pattern(2 * quanty_core::CHUNK_SIZE));
    s.execute(&format!("put files {{ id: 1, body: x\"{}\" }}", hex(&blob)))
        .expect("put");
    s.execute("del files where id = 1").expect("del");

    let text = swept(&mut s);
    assert!(text.contains("dropped 0 chunks"), "{text}");
    assert!(text.contains("kept 0"), "{text}");
}

#[test]
fn a_table_without_asset_columns_is_not_scanned_into_the_reachable_set() {
    // A plain table must not make chunks reachable by accident, and a
    // bytes column that happens to hold descriptor-shaped bytes is still
    // just bytes.
    let mut s = session();
    let blob = stored(&s, &pattern(quanty_core::CHUNK_SIZE));
    s.execute("table plain { id: int @key, body: bytes }")
        .expect("table");
    s.execute(&format!("put plain {{ id: 1, body: x\"{}\" }}", hex(&blob)))
        .expect("put");

    let text = swept(&mut s);
    assert!(
        text.contains("dropped 1 chunks"),
        "a bytes column held it: {text}"
    );
}
