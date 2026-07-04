//! Pager integration tests against both storage backends, including the
//! "hostile file" cases: a corrupted database must produce errors, never
//! panics and never silently wrong data.

use quanty_core::{Error, FileStorage, MemStorage, PageType, Pager, PagerOptions};

fn opts(page_size: u32) -> PagerOptions {
    PagerOptions {
        page_size,
        cache_pages: 16,
    }
}

fn fill_body(buf: &mut [u8], seed: u8) {
    for (i, b) in buf
        .iter_mut()
        .enumerate()
        .skip(quanty_core::PAGE_HEADER_LEN)
    {
        *b = seed.wrapping_add(i as u8);
    }
}

fn check_body(buf: &[u8], seed: u8) {
    for (i, b) in buf.iter().enumerate().skip(quanty_core::PAGE_HEADER_LEN) {
        assert_eq!(*b, seed.wrapping_add(i as u8), "byte {i} differs");
    }
}

#[test]
fn write_commit_reopen_read_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("roundtrip.qdb");

    let mut ids = Vec::new();
    {
        let pager = Pager::create(FileStorage::create(&path).unwrap(), opts(512)).unwrap();
        let mut batch = pager.begin();
        for seed in 0..10u8 {
            let id = batch.allocate(PageType::Leaf);
            fill_body(batch.page_mut(id).unwrap(), seed);
            ids.push(id);
        }
        batch.set_data_root(ids[0]);
        assert_eq!(batch.commit().unwrap(), 1);

        // readable straight from the same pager
        for (seed, &id) in ids.iter().enumerate() {
            check_body(&pager.read_page(id).unwrap(), seed as u8);
        }
    }

    // and after a clean reopen from disk
    let pager = Pager::open(FileStorage::open(&path).unwrap(), PagerOptions::default()).unwrap();
    let meta = pager.committed_meta();
    assert_eq!(meta.txid, 1);
    assert_eq!(meta.page_size, 512);
    assert_eq!(meta.data_root, ids[0]);
    assert_eq!(meta.page_count, 2 + ids.len() as u64);
    for (seed, &id) in ids.iter().enumerate() {
        check_body(&pager.read_page(id).unwrap(), seed as u8);
    }
}

#[test]
fn multiple_commits_alternate_meta_slots_and_accumulate() {
    let pager = Pager::create(MemStorage::new(), opts(512)).unwrap();
    for txid in 1..=20u64 {
        let mut batch = pager.begin();
        let id = batch.allocate(PageType::Leaf);
        fill_body(batch.page_mut(id).unwrap(), txid as u8);
        assert_eq!(batch.commit().unwrap(), txid);
    }
    let meta = pager.committed_meta();
    assert_eq!(meta.txid, 20);
    assert_eq!(meta.page_count, 22);
    for id in 2..22u64 {
        check_body(&pager.read_page(id).unwrap(), (id - 1) as u8);
    }
}

#[test]
fn committed_pages_are_not_writable() {
    let pager = Pager::create(MemStorage::new(), opts(512)).unwrap();
    let first = {
        let mut batch = pager.begin();
        let id = batch.allocate(PageType::Leaf);
        batch.commit().unwrap();
        id
    };
    let mut batch = pager.begin();
    match batch.page_mut(first) {
        Err(Error::PageNotWritable(id)) => assert_eq!(id, first),
        other => panic!("expected PageNotWritable, got {other:?}"),
    }
}

#[test]
fn dropped_batch_leaves_no_trace() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dropped.qdb");
    {
        let pager = Pager::create(FileStorage::create(&path).unwrap(), opts(512)).unwrap();
        let mut batch = pager.begin();
        let id = batch.allocate(PageType::Leaf);
        fill_body(batch.page_mut(id).unwrap(), 0xAB);
        drop(batch); // no commit

        let meta = pager.committed_meta();
        assert_eq!(meta.txid, 0);
        assert_eq!(meta.page_count, 2);
        assert!(matches!(
            pager.read_page(id),
            Err(Error::PageOutOfBounds(_))
        ));
    }
    let pager = Pager::open(FileStorage::open(&path).unwrap(), PagerOptions::default()).unwrap();
    assert_eq!(pager.committed_meta().page_count, 2);
}

#[test]
fn bit_flip_in_data_page_is_detected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("flip.qdb");
    let id = {
        let pager = Pager::create(FileStorage::create(&path).unwrap(), opts(512)).unwrap();
        let mut batch = pager.begin();
        let id = batch.allocate(PageType::Leaf);
        fill_body(batch.page_mut(id).unwrap(), 7);
        batch.commit().unwrap();
        id
    };

    // flip one bit in the payload of the committed page
    flip_bit(&path, id * 512 + 100);

    let pager = Pager::open(FileStorage::open(&path).unwrap(), PagerOptions::default()).unwrap();
    match pager.read_page(id) {
        Err(Error::Corrupted { page, .. }) => assert_eq!(page, Some(id)),
        other => panic!("expected Corrupted, got {other:?}"),
    }
}

#[test]
fn one_broken_meta_slot_recovers_from_the_other() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("meta1.qdb");
    {
        let pager = Pager::create(FileStorage::create(&path).unwrap(), opts(512)).unwrap();
        let mut batch = pager.begin();
        batch.allocate(PageType::Leaf);
        batch.commit().unwrap(); // txid 1 lives in slot 1
    }
    flip_bit(&path, 512 + 40); // damage slot 1

    // recovery falls back to slot 0 (txid 0), the database still opens
    let pager = Pager::open(FileStorage::open(&path).unwrap(), PagerOptions::default()).unwrap();
    assert_eq!(pager.committed_meta().txid, 0);
}

#[test]
fn both_meta_slots_broken_is_an_error_not_a_panic() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("meta2.qdb");
    {
        Pager::create(FileStorage::create(&path).unwrap(), opts(512)).unwrap();
    }
    flip_bit(&path, 40);
    flip_bit(&path, 512 + 40);

    match Pager::open(FileStorage::open(&path).unwrap(), PagerOptions::default()) {
        Err(Error::Corrupted { .. }) => {}
        Err(other) => panic!("expected Corrupted, got {other:?}"),
        Ok(_) => panic!("expected Corrupted, but the database opened"),
    }
}

#[test]
fn garbage_file_is_rejected_as_invalid_format() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("garbage.bin");
    std::fs::write(&path, vec![0x5A; 4096]).unwrap();
    match Pager::open(FileStorage::open(&path).unwrap(), PagerOptions::default()) {
        Err(Error::InvalidFormat(_)) => {}
        Err(other) => panic!("expected InvalidFormat, got {other:?}"),
        Ok(_) => panic!("expected InvalidFormat, but the database opened"),
    }
}

#[test]
fn create_rejects_bad_page_sizes_and_non_empty_storage() {
    assert!(Pager::create(MemStorage::new(), opts(1000)).is_err());
    assert!(Pager::create(MemStorage::new(), opts(256)).is_err());

    let storage = MemStorage::new();
    {
        use quanty_core::Storage;
        storage.write_at(0, b"junk").unwrap();
    }
    assert!(Pager::create(storage, opts(512)).is_err());
}

#[test]
fn bulk_load_one_million_pages_in_memory() {
    // Keeps an eye on pathological memory or time blowups in the write
    // path. 1M pages at 512 bytes is ~512 MiB through the batch, committed
    // in chunks the way a real bulk load would.
    let pager = Pager::create(MemStorage::new(), opts(512)).unwrap();
    let chunk = 50_000;
    for _ in 0..20 {
        let mut batch = pager.begin();
        for _ in 0..chunk {
            let id = batch.allocate(PageType::Leaf);
            let body = batch.page_mut(id).unwrap();
            body[quanty_core::PAGE_HEADER_LEN] = (id % 251) as u8;
        }
        batch.commit().unwrap();
    }
    let meta = pager.committed_meta();
    assert_eq!(meta.page_count, 2 + 20 * chunk);
    // spot check a few pages across the range
    for id in [2u64, 123_456, 999_999] {
        let page = pager.read_page(id).unwrap();
        assert_eq!(page[quanty_core::PAGE_HEADER_LEN], (id % 251) as u8);
    }
}

fn flip_bit(path: &std::path::Path, offset: u64) {
    use std::io::{Read, Seek, SeekFrom, Write};
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let mut byte = [0u8; 1];
    f.seek(SeekFrom::Start(offset)).unwrap();
    f.read_exact(&mut byte).unwrap();
    byte[0] ^= 0x01;
    f.seek(SeekFrom::Start(offset)).unwrap();
    f.write_all(&byte).unwrap();
}
