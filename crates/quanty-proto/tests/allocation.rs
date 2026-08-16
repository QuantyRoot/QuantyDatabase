//! What a decoder can be made to allocate, measured.
//!
//! The claim in lib.rs is that memory a decoder commits is bounded by a
//! constant rather than by a multiple of the input. That is exactly the
//! kind of claim that is easy to write, easy to believe and wrong: the
//! first version of this crate carried it in a doc comment while a single
//! 16 MiB frame could drive half a gigabyte of allocation, because the row
//! and value counts were bounded only by what the frame could hold and a
//! `Null` costs one byte on the wire against 32 in memory.
//!
//! So this file counts bytes through a global allocator instead of
//! trusting the reasoning. One test only, and no others in this binary,
//! because a peak measured while other tests allocate on other threads is
//! not a measurement.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use quanty_proto::limits::{MAX_BODY, MAX_ROWS_PER_BATCH, MAX_VALUES_PER_ROW};
use quanty_proto::message::{T_LINES, T_ROWS_BEGIN, T_ROW_BATCH};
use quanty_proto::{ClientMessage, ServerMessage};

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
static WATCHING: AtomicUsize = AtomicUsize::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = System.alloc(layout);
        if !p.is_null() && WATCHING.load(Ordering::Relaxed) == 1 {
            let now = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(now, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, p: *mut u8, layout: Layout) {
        if WATCHING.load(Ordering::Relaxed) == 1 {
            LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
        }
        System.dealloc(p, layout);
    }

    unsafe fn realloc(&self, p: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let np = System.realloc(p, layout, new_size);
        if !np.is_null() && WATCHING.load(Ordering::Relaxed) == 1 {
            if new_size >= layout.size() {
                let d = new_size - layout.size();
                let now = LIVE.fetch_add(d, Ordering::Relaxed) + d;
                PEAK.fetch_max(now, Ordering::Relaxed);
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        np
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

/// Run `f` and report the highest number of live bytes seen while it ran.
fn peak_bytes(f: impl FnOnce()) -> usize {
    LIVE.store(0, Ordering::SeqCst);
    PEAK.store(0, Ordering::SeqCst);
    WATCHING.store(1, Ordering::SeqCst);
    f();
    WATCHING.store(0, Ordering::SeqCst);
    PEAK.load(Ordering::SeqCst)
}

/// A body that declares `count` elements and then stops.
///
/// The shape every one of these attacks takes: a big number, then nothing
/// to back it up. A decoder that reserves before it reads is one that can
/// be handed this.
fn lying_count(count: u32) -> Vec<u8> {
    count.to_le_bytes().to_vec()
}

/// A body that declares many elements and does supply them, as cheaply as
/// the format permits. This is the expensive case, because nothing here is
/// a lie: the decoder really does have to hold what arrives.
fn honest_null_rows(rows: usize) -> Vec<u8> {
    let mut b = Vec::with_capacity(4 + rows * 5);
    b.extend_from_slice(&(rows as u32).to_le_bytes());
    for _ in 0..rows {
        b.extend_from_slice(&1u32.to_le_bytes()); // one value in this row
        b.push(0x01); // Null
    }
    b
}

#[test]
fn a_frame_cannot_be_turned_into_arbitrary_memory() {
    // The budget. Every case below has to come in under this, and it is a
    // constant: it does not scale with MAX_BODY, which is the whole point.
    // 32 MiB leaves room for the input buffer itself plus the decoded
    // structures, and is far below the 536 MB a single frame used to buy.
    const BUDGET: usize = 32 * 1024 * 1024;

    // 1. Lies. Four billion elements claimed in a four byte body, aimed at
    //    every count field the format has.
    for t in [T_ROW_BATCH, T_ROWS_BEGIN, T_LINES] {
        for n in [u32::MAX, u32::MAX / 2, 1 << 30, 1 << 20] {
            let body = lying_count(n);
            let peak = peak_bytes(|| {
                let r = ServerMessage::decode(t, &body);
                assert!(r.is_err(), "type {t:#x} count {n} must be refused");
            });
            assert!(
                peak < 1024 * 1024,
                "type {t:#x} count {n} allocated {peak} bytes for a body of {}",
                body.len()
            );
        }
    }

    // 2. A lie nested one level down: an honest row count wrapping a value
    //    count that is not.
    let mut nested = 1u32.to_le_bytes().to_vec();
    nested.extend_from_slice(&u32::MAX.to_le_bytes());
    let peak = peak_bytes(|| {
        assert!(ServerMessage::decode(T_ROW_BATCH, &nested).is_err());
    });
    assert!(peak < 1024 * 1024, "nested count allocated {peak} bytes");

    // 3. The honest worst case, and the one that actually costs: as many
    //    single-Null rows as the protocol permits, all really present.
    let body = honest_null_rows(MAX_ROWS_PER_BATCH);
    assert!(body.len() < MAX_BODY, "the attack must fit a legal frame");
    let sent = body.len();
    let peak = peak_bytes(|| {
        ServerMessage::decode(T_ROW_BATCH, &body).expect("this one is legal and must decode");
    });
    println!(
        "honest worst case: {sent} bytes on the wire, {peak} bytes peak allocation, ratio {:.1}x",
        peak as f64 / sent as f64
    );
    assert!(
        peak < BUDGET,
        "{sent} bytes on the wire drove {peak} bytes of allocation"
    );

    // 4. One row past the cap must cost nothing, however honest it looks.
    let body = honest_null_rows(MAX_ROWS_PER_BATCH + 1);
    let peak = peak_bytes(|| {
        assert!(
            ServerMessage::decode(T_ROW_BATCH, &body).is_err(),
            "a batch above MAX_ROWS_PER_BATCH must be refused"
        );
    });
    assert!(peak < 1024 * 1024, "refusing still allocated {peak} bytes");

    // 5. A row wider than the protocol allows, refused before it is held.
    let mut wide = 1u32.to_le_bytes().to_vec();
    wide.extend_from_slice(&((MAX_VALUES_PER_ROW + 1) as u32).to_le_bytes());
    wide.resize(wide.len() + MAX_VALUES_PER_ROW + 1, 0x01);
    let peak = peak_bytes(|| {
        assert!(ServerMessage::decode(T_ROW_BATCH, &wide).is_err());
    });
    assert!(peak < 1024 * 1024, "wide row allocated {peak} bytes");

    // 6. The client side has length fields too.
    let body = lying_count(u32::MAX);
    let peak = peak_bytes(|| {
        assert!(ClientMessage::decode(0x11, &body).is_err());
        assert!(ClientMessage::decode(0x10, &body).is_err());
    });
    assert!(peak < 1024 * 1024, "client decode allocated {peak} bytes");
}
