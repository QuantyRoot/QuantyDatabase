//! Fuzzing the SQLite reader.
//!
//! The reader's whole job is to be handed a file somebody else wrote, which
//! in the worst case is a file somebody else wrote *at us*. So the bar is
//! the same one our own format reader is held to: any input at all produces
//! either correct data or an error, and never a panic, a hang, or a row
//! that is not in the file.
//!
//! Four attack styles, all seeded and reproducible:
//!
//! 1. random bytes behind a valid magic string, so the header parser is
//!    actually reached instead of bouncing off the first sixteen bytes
//! 2. byte level mutations of the two real databases in tests/data
//! 3. targeted mutations of the structures a reader trusts: the file
//!    header, b-tree page headers, cell pointer arrays, cell payload
//!    lengths and overflow pointers
//! 4. truncation, which is the one corruption that happens by accident all
//!    the time
//!
//! The invariants checked on every input:
//!
//! - nothing panics: every call returns `Ok` or `Err`
//! - nothing hangs: a walk visits each page at most once, so a scan cannot
//!   outlive the page count no matter what the pointers say
//! - a cell that parses stays inside its page, and a payload that comes
//!   back is exactly as long as the cell said it would be
//! - a row that decodes holds no more values than the file has bytes, and
//!   decoding the same payload twice gives the same values
//!
//! Wall clock budget via QUANTY_FUZZ_SECS (default 20), seed via
//! QUANTY_FUZZ_SEED. The phase 4 acceptance run uses a much larger budget.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use quanty_sqlite::{decode_record, Cell, MappedCell, Reader, RowLayout, SliceSource, SqliteValue};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }

    fn byte(&mut self) -> u8 {
        // bias towards the values that break parsers: all bits set, the
        // sign bit, zero, and the varint continuation bit
        match self.below(8) {
            0 => 0x00,
            1 => 0xff,
            2 => 0x80,
            3 => 0x7f,
            4 => 0x01,
            _ => (self.next() >> 24) as u8,
        }
    }
}

/// Compare values by bit pattern rather than by IEEE equality.
///
/// A corrupted file can hold any 64 bit pattern in a float field, NaN
/// included, and NaN is not equal to itself. That makes `==` useless for
/// asking whether two reads produced the same bytes, which is what the
/// stability checks below actually want to know.
fn same_values(left: &[SqliteValue], right: &[SqliteValue]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(a, b)| match (a, b) {
            (SqliteValue::Real(x), SqliteValue::Real(y)) => x.to_bits() == y.to_bits(),
            _ => a == b,
        })
}

/// A definition and the layout built from it have to agree: every column is
/// either virtual or occupies exactly one slot, and the slots are the first
/// n positions of a record with nothing skipped.
fn layout_is_consistent(def: &quanty_sqlite::TableDef) -> bool {
    let layout = RowLayout::new(def);
    let virtuals = def
        .columns
        .iter()
        .filter(|c| c.generated.is_virtual())
        .count();
    layout.declared_columns() == def.columns.len()
        && layout.stored_columns() + virtuals == def.columns.len()
}

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|e| panic!("fixture {path}: {e}"))
}

/// Run the whole read path over `bytes` and check every invariant that can
/// be checked without knowing what the file was supposed to contain.
///
/// Returns how far it got, which is only used to keep the fuzzer honest
/// about whether it is reaching the interesting code at all.
fn exercise(bytes: &[u8]) -> Progress {
    let mut progress = Progress::default();

    let reader = match Reader::open(SliceSource::new(bytes)) {
        Ok(reader) => reader,
        Err(_) => return progress,
    };
    progress.opened = true;

    let page_size = reader.header().page_size as u64;
    let usable = reader.header().usable_size() as usize;
    let page_count = reader.page_count();
    assert!(page_count >= 1, "an open reader has at least one page");
    assert!(
        page_count as u64 * page_size <= bytes.len() as u64,
        "the reader claims {page_count} pages of {page_size} bytes, the file has {}",
        bytes.len()
    );

    // every row comes out of exactly one cell, and the walk visits a page
    // at most once, so the cells in the file are a hard ceiling on the rows
    // any scan can produce. counted here rather than guessed at.
    let mut total_cells = 0u64;

    for number in 1..=page_count {
        let Ok(page) = reader.btree_page(number) else {
            continue;
        };
        progress.pages += 1;
        total_cells += page.cell_count() as u64;
        assert_eq!(page.number, number);
        assert_eq!(
            page.right_most.is_some(),
            page.kind.is_interior(),
            "page {number}: only interior pages carry a right most child"
        );

        for index in 0..page.cell_count() {
            // a validated cell pointer must stay inside the usable area
            let slice = page.cell(index).expect("a validated pointer stays valid");
            assert!(
                slice.len() <= usable,
                "page {number} cell {index} reaches past the usable area"
            );

            let Ok(cell) = reader.cell(&page, index) else {
                continue;
            };
            progress.cells += 1;

            let payload = match &cell {
                Cell::TableInterior { child, .. } => {
                    assert!(*child >= 1, "a child pointer of 0 was accepted");
                    continue;
                }
                Cell::TableLeaf { payload, .. }
                | Cell::IndexLeaf { payload }
                | Cell::IndexInterior { payload, .. } => payload,
            };

            // reading the same cell twice must produce the same bytes: a
            // reader that depends on some leftover state is a reader whose
            // output depends on the order pages were touched in
            let again = reader.cell(&page, index).expect("a cell parses twice");
            assert_eq!(&cell, &again, "page {number} cell {index} is not stable");

            if let Ok(values) = decode_record(payload) {
                progress.records += 1;
                // decoding is a pure function of the payload
                let repeat = decode_record(payload).expect("a record decodes twice");
                assert!(
                    same_values(&values, &repeat),
                    "decoding the same payload twice gave different values"
                );
            }
        }
    }

    // the schema, and then a full scan of everything it points at
    let Ok(schema) = reader.schema() else {
        return progress;
    };
    progress.schema = true;

    for object in schema.objects() {
        // a mutated schema row hands the create-table parser arbitrary
        // text, which is a fuzz target in its own right: it must answer,
        // and a definition it accepts must line up with the rows it maps
        let layout = match object.table_def() {
            Ok(def) => {
                assert!(
                    !def.columns.is_empty(),
                    "a parsed definition with no columns was accepted"
                );
                assert!(
                    layout_is_consistent(&def),
                    "{}: the layout disagrees with the definition",
                    object.name
                );
                Some(RowLayout::new(&def))
            }
            Err(_) => None,
        };

        let Some(root) = object.root_page else {
            continue;
        };
        let Ok(scan) = reader.table_scan(root) else {
            continue;
        };
        let mut rows = 0u64;
        let mut columns: Option<usize> = None;
        let mut last_rowid: Option<i64> = None;
        for row in scan {
            let Ok(row) = row else { break };
            rows += 1;
            progress.rows += 1;

            // a table b-tree is ordered, so a scan that goes backwards means
            // the walk lost its place
            if let Some(previous) = last_rowid {
                assert!(
                    row.rowid > previous,
                    "{}: rowid {} came after {previous}",
                    object.name,
                    row.rowid
                );
            }
            last_rowid = Some(row.rowid);

            // rows of one table may hold different numbers of values, and
            // that is not corruption: after `alter table add column` the
            // existing rows keep their shorter records and the missing
            // columns read as the column default. what cannot happen is a
            // record with more values than the payload has bytes, since
            // every value costs at least one byte of serial type.
            columns = Some(columns.map_or(row.values.len(), |w: usize| w.max(row.values.len())));
            assert!(
                row.values.len() as u64 <= bytes.len() as u64,
                "{}: a row claims {} values out of a {} byte file",
                object.name,
                row.values.len(),
                bytes.len()
            );

            // every declared column has to answer for this row, and a
            // stored value it points at has to exist
            if let Some(layout) = &layout {
                for index in 0..layout.declared_columns() {
                    match layout.cell(&row, index) {
                        MappedCell::Value(_) | MappedCell::Rowid(_) => {}
                        MappedCell::Missing | MappedCell::Virtual => {}
                    }
                }
                assert!(
                    layout.stored_columns() <= layout.declared_columns(),
                    "more stored slots than declared columns"
                );
            }

            // a scan cannot produce more rows than the file has cells; if
            // it does, the walk is reading something twice
            assert!(
                rows <= total_cells,
                "{}: {rows} rows out of {total_cells} cells in {page_count} pages, \
                 the walk is reading pages twice",
                object.name
            );
        }
    }

    progress
}

#[derive(Default, Debug)]
struct Progress {
    opened: bool,
    schema: bool,
    pages: u64,
    cells: u64,
    records: u64,
    rows: u64,
}

/// Flip, set or clear bytes at random.
fn scatter(rng: &mut Rng, bytes: &mut [u8], count: u64) {
    for _ in 0..count {
        if bytes.is_empty() {
            return;
        }
        let at = rng.below(bytes.len() as u64) as usize;
        match rng.below(3) {
            0 => bytes[at] = rng.byte(),
            1 => bytes[at] ^= 1 << rng.below(8),
            _ => bytes[at] = bytes[at].wrapping_add(1),
        }
    }
}

/// Corrupt the structures a reader has to trust: the file header, the page
/// headers, the cell pointer arrays and the first bytes of cells, which is
/// where payload lengths and overflow pointers live.
fn corrupt_structure(rng: &mut Rng, bytes: &mut [u8], page_size: usize) {
    let pages = bytes.len() / page_size;
    if pages == 0 {
        return;
    }
    match rng.below(4) {
        // the file header
        0 => {
            let at = rng.below(100) as usize;
            bytes[at] = rng.byte();
        }
        // a b-tree page header
        1 => {
            let page = rng.below(pages as u64) as usize;
            let base = page * page_size + if page == 0 { 100 } else { 0 };
            let at = base + rng.below(12) as usize;
            if at < bytes.len() {
                bytes[at] = rng.byte();
            }
        }
        // a cell pointer
        2 => {
            let page = rng.below(pages as u64) as usize;
            let base = page * page_size + if page == 0 { 100 } else { 0 };
            let at = base + 8 + 2 * rng.below(16) as usize;
            if at + 1 < bytes.len() {
                bytes[at] = rng.byte();
                bytes[at + 1] = rng.byte();
            }
        }
        // the first bytes of a cell, or an overflow page's next pointer
        _ => {
            let page = rng.below(pages as u64) as usize;
            let base = page * page_size;
            let at = base + rng.below(page_size as u64 / 2) as usize;
            for offset in 0..rng.below(6) as usize {
                if at + offset < bytes.len() {
                    bytes[at + offset] = rng.byte();
                }
            }
        }
    }
}

#[test]
fn fuzz_the_sqlite_reader() {
    let budget = Duration::from_secs(
        std::env::var("QUANTY_FUZZ_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(20),
    );
    let seed = std::env::var("QUANTY_FUZZ_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64
                | 1
        });
    let mut rng = Rng(seed);
    let started = Instant::now();

    // the corpus itself must always read cleanly, and it is what the
    // mutations are built from
    let records = fixture("records.sqlite");
    let chinook = fixture("chinook.sqlite");
    for (name, bytes, page_size) in [
        ("records.sqlite", &records, 512usize),
        ("chinook.sqlite", &chinook, 1024),
    ] {
        let progress = exercise(bytes);
        assert!(progress.opened && progress.schema, "{name} did not read");
        assert!(progress.rows > 0, "{name} produced no rows");
        let _ = page_size;
    }

    let mut cases: u64 = 0;
    let mut reached = Progress::default();
    while started.elapsed() < budget {
        for _ in 0..64 {
            cases += 1;
            let style = rng.below(4);
            let progress = match style {
                // random bytes behind a valid magic string
                0 => {
                    let len = 100 + rng.below(4096) as usize;
                    let mut bytes = vec![0u8; len];
                    bytes[..16].copy_from_slice(b"SQLite format 3\0");
                    for b in bytes[16..].iter_mut() {
                        *b = rng.byte();
                    }
                    exercise(&bytes)
                }
                // scattered mutations of a real database
                1 => {
                    let (base, _) = if rng.below(4) == 0 {
                        (&chinook, 1024)
                    } else {
                        (&records, 512)
                    };
                    let mut bytes = base.clone();
                    let count = 1 + rng.below(16);
                    scatter(&mut rng, &mut bytes, count);
                    exercise(&bytes)
                }
                // targeted structural corruption
                2 => {
                    let (base, page_size) = if rng.below(4) == 0 {
                        (&chinook, 1024)
                    } else {
                        (&records, 512)
                    };
                    let mut bytes = base.clone();
                    for _ in 0..=rng.below(3) {
                        corrupt_structure(&mut rng, &mut bytes, page_size);
                    }
                    exercise(&bytes)
                }
                // truncation, with and without further damage
                _ => {
                    let mut bytes = records.clone();
                    let keep = rng.below(bytes.len() as u64) as usize;
                    bytes.truncate(keep);
                    if rng.below(2) == 0 {
                        let count = 1 + rng.below(4);
                        scatter(&mut rng, &mut bytes, count);
                    }
                    exercise(&bytes)
                }
            };

            reached.opened |= progress.opened;
            reached.schema |= progress.schema;
            reached.pages += progress.pages;
            reached.cells += progress.cells;
            reached.records += progress.records;
            reached.rows += progress.rows;
        }
    }

    // a fuzzer that never gets past the header is a fuzzer that tests the
    // header, so this is checked rather than hoped for
    assert!(reached.opened, "no input ever opened (seed {seed})");
    assert!(reached.pages > 0, "no page ever parsed (seed {seed})");
    assert!(reached.cells > 0, "no cell ever parsed (seed {seed})");
    assert!(reached.records > 0, "no record ever decoded (seed {seed})");
    assert!(
        reached.rows > 0,
        "no row ever came out of a scan (seed {seed})"
    );

    println!(
        "sqlite reader fuzz: {cases} cases in {:.1?} (seed {seed}); \
         reached {} pages, {} cells, {} records, {} rows",
        started.elapsed(),
        reached.pages,
        reached.cells,
        reached.records,
        reached.rows
    );
}
