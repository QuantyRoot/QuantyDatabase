//! The random source, held to the little that can be checked from
//! outside.
//!
//! A generator cannot be tested for being unguessable. What it can be
//! tested for is answering at all, answering differently each time, and
//! filling exactly what it was given, which is where the bugs that do
//! happen live.

use quanty_sys::random;

#[test]
fn it_answers_on_this_platform() {
    // The point of the test on Windows and macOS, where this was
    // /dev/urandom and simply failed.
    random::bytes32().expect("no random source");
}

#[test]
fn two_draws_differ() {
    let a = random::bytes32().expect("first");
    let b = random::bytes32().expect("second");
    assert_ne!(a, b, "the same bytes twice is not a generator");
}

#[test]
fn it_fills_exactly_what_it_was_given() {
    for len in [0usize, 1, 7, 32, 33, 4096] {
        let mut buf = vec![0u8; len + 2];
        buf[len] = 0xAB;
        buf[len + 1] = 0xCD;
        random::fill(&mut buf[..len]).expect("fill");
        assert_eq!(buf[len], 0xAB, "wrote past the end at len {len}");
        assert_eq!(buf[len + 1], 0xCD, "wrote past the end at len {len}");
    }
}

#[test]
fn a_long_draw_is_not_left_half_done() {
    // A short read is the failure a /dev/urandom implementation has, and
    // a length that does not fit a u32 is the one the Windows call has.
    let mut buf = vec![0u8; 100_000];
    random::fill(&mut buf).expect("fill");
    assert!(
        buf.iter().any(|&b| b != 0),
        "a hundred thousand zero bytes is not randomness"
    );
    let zeros = buf.iter().filter(|&&b| b == 0).count();
    assert!(
        zeros < buf.len() / 100,
        "{zeros} zero bytes out of {} looks like a partial fill",
        buf.len()
    );
}
