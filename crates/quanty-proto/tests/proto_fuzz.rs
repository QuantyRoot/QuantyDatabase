//! Protocol fuzzing.
//!
//! Three attack styles, all seeded and reproducible:
//!
//! 1. random byte soup fed straight to the decoders
//! 2. structurally plausible frames with hostile lengths and counts
//! 3. byte-level mutations of a corpus of valid encodings
//!
//! Two invariants, checked on every single input:
//!
//! - no decoder ever panics; garbage comes back as `Err`, always
//! - whenever a decoder accepts an input, re-encoding it must produce
//!   the same bytes again (encode . decode . encode == encode)
//!
//! The second one is what catches a decoder that is merely permissive. A
//! codec that accepts a frame and then hands back something it would not
//! itself have written is how two versions of a client end up disagreeing
//! about a row.
//!
//! Memory is an invariant here too, not just time. A decoder that allocates
//! from an untrusted length is a way to ask a server for gigabytes, so the
//! hostile-length family below exists specifically to aim four billion at
//! every length field in the format.
//!
//! Wall clock budget via QUANTY_FUZZ_SECS (default 20). CI uses 600.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use quanty_core::Value;
use quanty_proto::frame::{FrameHeader, HEADER_LEN};
use quanty_proto::{ClientHello, ClientMessage, ServerHello, ServerMessage};

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
        (self.next() >> 24) as u8
    }
}

/// Every message type byte the protocol defines, plus neighbours in the
/// reserved gaps so that "unknown type" is exercised as often as known.
const TYPES: &[u8] = &[
    0x00, 0x0F, 0x10, 0x11, 0x12, 0x13, 0x14, 0x1F, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27,
    0x28, 0x2F, 0xEE, 0xFF,
];

/// Feed one body to both decoders under every type byte.
///
/// Neither may panic. Whatever is accepted must survive a re-encode.
///
/// The comparison is on bytes rather than on the decoded values, and that
/// is not a shortcut. `Value::Float(NaN)` is not equal to itself, so a
/// value-level assertion here reports a difference on every NaN that
/// crosses the wire while the bits are in fact intact. Bytes are the
/// well-defined statement of the same invariant, and they prove something
/// slightly stronger on top: that encoding is canonical, so one message has
/// exactly one encoding.
fn hammer(msg_type: u8, body: &[u8]) {
    if let Ok(m) = ClientMessage::decode(msg_type, body) {
        let once = m.encode().expect("an accepted message must be encodable");
        let again = ClientMessage::decode(msg_type, &once[HEADER_LEN..])
            .expect("re-encoded bytes must decode")
            .encode()
            .expect("and must encode again");
        assert_eq!(once, again, "client encoding is not canonical");
    }
    if let Ok(m) = ServerMessage::decode(msg_type, body) {
        let once = m.encode().expect("an accepted message must be encodable");
        let again = ServerMessage::decode(msg_type, &once[HEADER_LEN..])
            .expect("re-encoded bytes must decode")
            .encode()
            .expect("and must encode again");
        assert_eq!(once, again, "server encoding is not canonical");
    }
}

fn hammer_handshake(rng: &mut Rng) {
    let mut hello = [0u8; 9];
    for b in hello.iter_mut() {
        *b = rng.byte();
    }
    let _ = ClientHello::decode(&hello);

    // The same bytes with the magic corrected, so version handling gets
    // exercised instead of every input dying at the magic check.
    hello[0..6].copy_from_slice(b"QUANTY");
    if let Ok(h) = ClientHello::decode(&hello) {
        let reply = quanty_proto::negotiate(h);
        assert_eq!(ServerHello::decode(&reply.encode()).unwrap(), reply);
    }

    let mut sh = [0u8; 4];
    for b in sh.iter_mut() {
        *b = rng.byte();
    }
    let _ = ServerHello::decode(&sh);

    let mut head = [0u8; HEADER_LEN];
    for b in head.iter_mut() {
        *b = rng.byte();
    }
    let _ = FrameHeader::decode(&head);
}

/// Style 1: unstructured bytes.
fn soup(rng: &mut Rng) {
    let len = rng.below(64) as usize;
    let mut body = Vec::with_capacity(len);
    for _ in 0..len {
        body.push(rng.byte());
    }
    let t = TYPES[rng.below(TYPES.len() as u64) as usize];
    hammer(t, &body);
}

/// Style 2: well-formed shapes with hostile numbers in the length and
/// count fields. This is the family that would find an allocation driven
/// by an attacker-chosen size.
fn hostile_lengths(rng: &mut Rng) {
    let evil = [
        u32::MAX,
        u32::MAX - 1,
        0x7fff_ffff,
        0xffff_ff00,
        1 << 30,
        1 << 24,
        0,
        1,
    ];
    let n = evil[rng.below(evil.len() as u64) as usize];

    let mut body = Vec::new();
    body.extend_from_slice(&n.to_le_bytes());
    // A little plausible payload after the lie, sometimes.
    let extra = rng.below(24) as usize;
    for _ in 0..extra {
        body.push(rng.byte());
    }
    let t = TYPES[rng.below(TYPES.len() as u64) as usize];
    hammer(t, &body);

    // Nested: a row batch whose row count is honest but whose inner value
    // count is not, and vice versa.
    let mut nested = Vec::new();
    nested.extend_from_slice(&1u32.to_le_bytes());
    nested.extend_from_slice(&n.to_le_bytes());
    for _ in 0..rng.below(16) {
        nested.push(rng.byte());
    }
    hammer(0x24, &nested);
}

fn corpus() -> Vec<(u8, Vec<u8>)> {
    let mut out = Vec::new();
    let client = [
        ClientMessage::Close,
        ClientMessage::Auth(vec![1, 2, 3]),
        ClientMessage::Query("get users where id = 3".into()),
        ClientMessage::QuerySql("select * from t".into()),
    ];
    for m in client.iter() {
        out.push((m.msg_type(), m.body()));
    }
    let server = [
        ServerMessage::Ready,
        ServerMessage::Ok,
        ServerMessage::RowsEnd,
        ServerMessage::Count {
            verb: "put".into(),
            n: 7,
        },
        ServerMessage::RowsBegin {
            columns: vec!["id".into(), "name".into()],
        },
        ServerMessage::RowBatch {
            rows: vec![
                vec![Value::Int(1), Value::Text("a".into())],
                vec![Value::Null, Value::Bytes(vec![0, 255])],
                vec![Value::Bool(true), Value::Float(1.5)],
            ],
        },
        ServerMessage::Lines(vec!["one".into(), "two".into()]),
        ServerMessage::Error {
            code: 6,
            message: "no such table".into(),
        },
    ];
    for m in server.iter() {
        out.push((m.msg_type(), m.body()));
    }
    out
}

/// Style 3: bit flips, truncations and splices of valid encodings.
fn mutate(rng: &mut Rng, corpus: &[(u8, Vec<u8>)]) {
    let (t, base) = &corpus[rng.below(corpus.len() as u64) as usize];
    let mut body = base.clone();

    match rng.below(4) {
        0 if !body.is_empty() => {
            let i = rng.below(body.len() as u64) as usize;
            body[i] ^= 1 << rng.below(8);
        }
        1 if !body.is_empty() => {
            let cut = rng.below(body.len() as u64) as usize;
            body.truncate(cut);
        }
        2 => {
            for _ in 0..rng.below(8) {
                body.push(rng.byte());
            }
        }
        _ if !body.is_empty() => {
            let i = rng.below(body.len() as u64) as usize;
            body[i] = rng.byte();
        }
        _ => {}
    }

    // Under its own type, and under a wrong one.
    hammer(*t, &body);
    hammer(TYPES[rng.below(TYPES.len() as u64) as usize], &body);
}

#[test]
fn fuzz_protocol() {
    let secs: u64 = std::env::var("QUANTY_FUZZ_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);
    let seed = std::env::var("QUANTY_FUZZ_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x2545F4914F6CDD1D)
        });
    let seed = if seed == 0 { 0x2545F4914F6CDD1D } else { seed };

    println!("protocol fuzz: seed {seed}, budget {secs}s");
    println!("reproduce with QUANTY_FUZZ_SEED={seed}");

    let mut rng = Rng(seed);
    let corpus = corpus();
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut iters: u64 = 0;

    while Instant::now() < deadline {
        for _ in 0..500 {
            match rng.below(4) {
                0 => soup(&mut rng),
                1 => hostile_lengths(&mut rng),
                2 => mutate(&mut rng, &corpus),
                _ => hammer_handshake(&mut rng),
            }
            iters += 1;
        }
    }

    println!("protocol fuzz: {iters} inputs, no panics");
    assert!(iters > 0);
}
