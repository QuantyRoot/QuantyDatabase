//! Round trips and the edges of the format.
//!
//! The fuzzer next door proves that garbage does not panic. This file
//! proves the format means what docs/PROTOCOL.md says it means.

use quanty_core::Value;
use quanty_proto::codec::Writer;
use quanty_proto::error::ProtoError;
use quanty_proto::frame::{FrameHeader, HEADER_LEN};
use quanty_proto::message::{T_QUERY, T_ROW_BATCH};
use quanty_proto::value::{encoded_row_len, encoded_value_len};
use quanty_proto::write_value;
use quanty_proto::{
    batch_rows, negotiate, ClientHello, ClientMessage, Refusal, ServerHello, ServerMessage,
    MAX_BODY, VERSION,
};

fn roundtrip_client(m: &ClientMessage) {
    let bytes = m.encode().expect("encode");
    let mut head = [0u8; HEADER_LEN];
    head.copy_from_slice(&bytes[..HEADER_LEN]);
    let h = FrameHeader::decode(&head).expect("header");
    assert_eq!(h.msg_type, m.msg_type());
    assert_eq!(h.body_len, bytes.len() - HEADER_LEN);
    let back = ClientMessage::decode(h.msg_type, &bytes[HEADER_LEN..]).expect("decode");
    assert_eq!(&back, m);
}

fn roundtrip_server(m: &ServerMessage) {
    let bytes = m.encode().expect("encode");
    let mut head = [0u8; HEADER_LEN];
    head.copy_from_slice(&bytes[..HEADER_LEN]);
    let h = FrameHeader::decode(&head).expect("header");
    let back = ServerMessage::decode(h.msg_type, &bytes[HEADER_LEN..]).expect("decode");
    assert_eq!(&back, m);
}

#[test]
fn client_messages_round_trip() {
    for m in [
        ClientMessage::Close,
        ClientMessage::Auth(vec![]),
        ClientMessage::Auth(vec![0, 255, 7, 0]),
        ClientMessage::Query("get users".into()),
        ClientMessage::QuerySql("select 1".into()),
        ClientMessage::Query(String::new()),
        // Non-ASCII must survive; the repo is ASCII, the wire is not.
        ClientMessage::Query("get \u{263a} where name = 'zurich'".into()),
    ] {
        roundtrip_client(&m);
    }
}

#[test]
fn server_messages_round_trip() {
    for m in [
        ServerMessage::Ready,
        ServerMessage::Ok,
        ServerMessage::RowsEnd,
        ServerMessage::Count {
            verb: "put".into(),
            n: u64::MAX,
        },
        ServerMessage::RowsBegin {
            columns: vec!["id".into(), "name".into()],
        },
        ServerMessage::RowsBegin { columns: vec![] },
        ServerMessage::Lines(vec!["a".into(), String::new()]),
        ServerMessage::Lines(vec![]),
        ServerMessage::Error {
            code: 0x0006,
            message: "no such table".into(),
        },
        ServerMessage::RowBatch { rows: vec![] },
    ] {
        roundtrip_server(&m);
    }
}

#[test]
fn every_value_kind_survives() {
    let rows = vec![vec![
        Value::Null,
        Value::Bool(true),
        Value::Bool(false),
        Value::Int(i64::MIN),
        Value::Int(i64::MAX),
        Value::Int(0),
        Value::Float(0.0),
        Value::Float(-0.0),
        Value::Float(f64::MIN),
        Value::Float(f64::MAX),
        Value::Text(String::new()),
        Value::Text("\u{1f600} mixed \u{00e4}\u{00f6}\u{00fc}".into()),
        Value::Bytes(vec![]),
        Value::Bytes(vec![0, 0, 0, 255]),
    ]];
    roundtrip_server(&ServerMessage::RowBatch { rows });
}

#[test]
fn float_edge_cases_keep_their_meaning() {
    // Bits, not a rendering: these are exactly the values a decimal
    // round trip would quietly destroy.
    for f in [f64::INFINITY, f64::NEG_INFINITY] {
        let m = ServerMessage::RowBatch {
            rows: vec![vec![Value::Float(f)]],
        };
        let bytes = m.encode().unwrap();
        let back = ServerMessage::decode(T_ROW_BATCH, &bytes[HEADER_LEN..]).unwrap();
        assert_eq!(back, m);
    }

    // NaN is not equal to itself, so compare the bits.
    let m = ServerMessage::RowBatch {
        rows: vec![vec![Value::Float(f64::NAN)]],
    };
    let bytes = m.encode().unwrap();
    let back = ServerMessage::decode(T_ROW_BATCH, &bytes[HEADER_LEN..]).unwrap();
    match back {
        ServerMessage::RowBatch { rows } => match rows[0][0] {
            Value::Float(f) => assert!(f.is_nan()),
            ref other => panic!("expected float, got {other:?}"),
        },
        other => panic!("expected row batch, got {other:?}"),
    }
}

#[test]
fn handshake_round_trips_and_negotiates() {
    let hello = ClientHello { version: VERSION };
    assert_eq!(ClientHello::decode(&hello.encode()).unwrap(), hello);

    assert_eq!(
        negotiate(ClientHello { version: VERSION }),
        ServerHello::Accepted { version: VERSION }
    );
    assert_eq!(
        negotiate(ClientHello { version: 0 }),
        ServerHello::Refused(Refusal::VersionTooOld)
    );
    assert_eq!(
        negotiate(ClientHello {
            version: VERSION + 1
        }),
        ServerHello::Refused(Refusal::VersionTooNew)
    );

    for h in [
        ServerHello::Accepted { version: VERSION },
        ServerHello::Refused(Refusal::BadMagic),
        ServerHello::Refused(Refusal::VersionTooNew),
    ] {
        assert_eq!(ServerHello::decode(&h.encode()).unwrap(), h);
    }
}

#[test]
fn bad_magic_is_refused_not_guessed() {
    let mut bytes = ClientHello { version: 1 }.encode();
    bytes[0] = b'X';
    assert_eq!(ClientHello::decode(&bytes), Err(ProtoError::BadMagic));
}

#[test]
fn reserved_byte_must_be_zero() {
    let mut bytes = ClientHello { version: 1 }.encode();
    bytes[8] = 1;
    assert!(ClientHello::decode(&bytes).is_err());
}

#[test]
fn oversized_frame_is_refused_before_the_body_is_read() {
    let mut head = [0u8; HEADER_LEN];
    head[0] = T_QUERY;
    head[1..5].copy_from_slice(&u32::MAX.to_le_bytes());
    match FrameHeader::decode(&head) {
        Err(ProtoError::TooLarge { declared, limit }) => {
            assert_eq!(declared, u32::MAX as u64);
            assert_eq!(limit, MAX_BODY as u64);
        }
        other => panic!("expected TooLarge, got {other:?}"),
    }
}

#[test]
fn a_lying_length_costs_an_error_and_not_memory() {
    // Claims four billion bytes of text in a four byte body. If this
    // allocated first and checked second, it would be a way to ask a
    // server for memory.
    let body = u32::MAX.to_le_bytes().to_vec();
    assert!(ClientMessage::decode(T_QUERY, &body).is_err());

    // Same trick one level down: a row batch claiming a huge row count.
    assert!(ServerMessage::decode(T_ROW_BATCH, &body).is_err());
}

#[test]
fn trailing_bytes_are_an_error() {
    let mut body = ClientMessage::Query("get x".into()).body().unwrap();
    body.push(0);
    match ClientMessage::decode(T_QUERY, &body) {
        Err(ProtoError::TrailingBytes(1)) => {}
        other => panic!("expected TrailingBytes, got {other:?}"),
    }
}

#[test]
fn unknown_message_type_is_an_error_not_a_guess() {
    assert_eq!(
        ClientMessage::decode(0xEE, &[]),
        Err(ProtoError::UnknownTag(0xEE))
    );
    assert_eq!(
        ServerMessage::decode(0x00, &[]),
        Err(ProtoError::UnknownTag(0x00))
    );
}

#[test]
fn invalid_utf8_is_refused_not_replaced() {
    let mut body = Vec::new();
    body.extend_from_slice(&2u32.to_le_bytes());
    body.extend_from_slice(&[0xff, 0xfe]);
    assert_eq!(
        ClientMessage::decode(T_QUERY, &body),
        Err(ProtoError::BadUtf8)
    );
}

/// The size function is a second description of the encoder and could
/// drift from it, which would silently produce oversized frames. This is
/// what makes that impossible to ship.
#[test]
fn encoded_len_matches_encoder() {
    let values = [
        Value::Null,
        Value::Bool(true),
        Value::Int(-1),
        Value::Float(0.5),
        Value::Text(String::new()),
        Value::Text("hello".into()),
        Value::Bytes(vec![]),
        Value::Bytes(vec![0; 300]),
    ];
    for v in &values {
        let mut w = Writer::new();
        write_value(&mut w, v);
        let written = w.finish().unwrap().len();
        assert_eq!(written, encoded_value_len(v), "size drifted for {v:?}");
    }
    let row: Vec<Value> = values.to_vec();
    let mut w = Writer::new();
    quanty_proto::value::write_row(&mut w, &row);
    assert_eq!(w.finish().unwrap().len(), encoded_row_len(&row));
}

#[test]
fn a_count_above_the_protocol_cap_is_refused() {
    use quanty_proto::{MAX_ROWS_PER_BATCH, MAX_VALUES_PER_ROW};
    // Encoding refuses it too, so a bad frame never leaves this process.
    let rows: Vec<Vec<Value>> = (0..MAX_ROWS_PER_BATCH + 1).map(|_| vec![]).collect();
    assert!(ServerMessage::RowBatch { rows }.encode().is_err());

    let wide = vec![Value::Null; MAX_VALUES_PER_ROW + 1];
    assert!(ServerMessage::RowBatch { rows: vec![wide] }
        .encode()
        .is_err());
}

#[test]
fn batching_keeps_every_row_and_fits_every_frame() {
    // Rows big enough that one frame cannot hold them all.
    let row = || vec![Value::Bytes(vec![7u8; 1024 * 1024])];
    let rows: Vec<Vec<Value>> = (0..40).map(|_| row()).collect();
    let batches = batch_rows(rows).expect("batch");
    assert!(batches.len() > 1, "expected a split, got {}", batches.len());

    let mut seen = 0;
    for b in &batches {
        let encoded = b.encode().expect("every batch must fit a frame");
        assert!(encoded.len() <= HEADER_LEN + MAX_BODY);
        match b {
            ServerMessage::RowBatch { rows } => {
                assert!(!rows.is_empty(), "a batch must carry at least one row");
                seen += rows.len();
            }
            other => panic!("expected row batch, got {other:?}"),
        }
    }
    assert_eq!(seen, 40);
}

#[test]
fn empty_result_set_is_one_empty_sequence() {
    assert!(batch_rows(vec![]).unwrap().is_empty());
}

#[test]
fn a_row_too_big_for_any_frame_is_reported_not_dropped() {
    let rows = vec![vec![Value::Bytes(vec![0u8; MAX_BODY + 1])]];
    assert!(matches!(batch_rows(rows), Err(ProtoError::TooLarge { .. })));
}

#[test]
fn terminal_messages_are_the_ones_that_end_a_request() {
    assert!(ServerMessage::Ok.is_terminal());
    assert!(ServerMessage::RowsEnd.is_terminal());
    assert!(ServerMessage::Lines(vec![]).is_terminal());
    assert!(ServerMessage::error(quanty_proto::ErrorCode::Parse, "x").is_terminal());
    assert!(!ServerMessage::Ready.is_terminal());
    assert!(!ServerMessage::RowsBegin { columns: vec![] }.is_terminal());
    assert!(!ServerMessage::RowBatch { rows: vec![] }.is_terminal());
}

#[test]
fn error_codes_survive_the_wire() {
    use quanty_proto::ErrorCode;
    for c in [
        ErrorCode::Protocol,
        ErrorCode::UnsupportedVersion,
        ErrorCode::NotAuthenticated,
        ErrorCode::AuthFailed,
        ErrorCode::Parse,
        ErrorCode::Execution,
        ErrorCode::WriteQueue,
        ErrorCode::ShuttingDown,
    ] {
        assert_eq!(ErrorCode::from_u16(c.as_u16()), Some(c));
    }
    // An unknown code is readable as "some failure", not a dropped
    // connection: a version 1 client may meet a server that added codes.
    assert_eq!(ErrorCode::from_u16(0xFFFF), None);
    let m = ServerMessage::Error {
        code: 0xFFFF,
        message: "from the future".into(),
    };
    roundtrip_server(&m);
}
