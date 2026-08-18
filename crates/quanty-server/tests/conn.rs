//! The connection state machine under sockets that misbehave in the two ways
//! real ones do: giving back less than was asked, taking less than was offered.

use std::io::{self, Read, Write};

use quanty_proto::frame::HEADER_LEN;
use quanty_proto::{
    ClientHello, ClientMessage, FrameHeader, ServerHello, ServerMessage, CLIENT_HELLO_LEN,
    SERVER_HELLO_LEN, VERSION,
};
use quanty_server::{Conn, Service, Step};

/// A socket that hands over `chunk` bytes per read and accepts `chunk` per
/// write, then reports WouldBlock. One is the interesting value.
struct Choked {
    input: Vec<u8>,
    read_pos: usize,
    output: Vec<u8>,
    chunk: usize,
    stalled: bool,
}

impl Choked {
    fn new(input: Vec<u8>, chunk: usize) -> Self {
        Choked {
            input,
            read_pos: 0,
            output: Vec::new(),
            chunk,
            stalled: false,
        }
    }

    fn feed(&mut self, more: &[u8]) {
        self.input.extend_from_slice(more);
    }
}

impl Read for Choked {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.input.len() - self.read_pos;
        if left == 0 {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let n = left.min(self.chunk).min(buf.len());
        buf[..n].copy_from_slice(&self.input[self.read_pos..self.read_pos + n]);
        self.read_pos += n;
        Ok(n)
    }
}

impl Write for Choked {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.stalled {
            self.stalled = false;
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let n = buf.len().min(self.chunk);
        self.output.extend_from_slice(&buf[..n]);
        self.stalled = true;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Echo;

impl Service for Echo {
    fn call(&mut self, request: ClientMessage) -> Vec<ServerMessage> {
        match request {
            ClientMessage::Query(s) | ClientMessage::QuerySql(s) => {
                vec![ServerMessage::Lines(vec![s])]
            }
            ClientMessage::Auth(_) => vec![ServerMessage::Ready],
            ClientMessage::Close => vec![],
        }
    }
}

fn hello() -> Vec<u8> {
    ClientHello { version: VERSION }.encode().to_vec()
}

/// Drive a connection to a standstill: read, step, write, until nothing moves.
///
/// A stalled write is not the end of the conversation, only the end of this
/// turn, so a standstill needs several idle rounds in a row rather than one.
fn pump(conn: &mut Conn, socket: &mut Choked, service: &mut impl Service) -> Step {
    let mut step = Step::Open;
    let mut idle = 0;
    for _ in 0..200_000 {
        let before = (socket.read_pos, socket.output.len(), conn.wants_write());
        let _ = conn.fill(socket).expect("fill");
        step = conn.step(service);
        conn.flush(socket).expect("flush");
        if (socket.read_pos, socket.output.len(), conn.wants_write()) == before {
            idle += 1;
            if idle >= 4 {
                break;
            }
        } else {
            idle = 0;
        }
    }
    step
}

/// Split the server's output into the four byte hello and the frames after it.
fn parse_replies(bytes: &[u8]) -> (ServerHello, Vec<ServerMessage>) {
    let mut sh = [0u8; SERVER_HELLO_LEN];
    sh.copy_from_slice(&bytes[..SERVER_HELLO_LEN]);
    let hello = ServerHello::decode(&sh).expect("server hello");

    let mut out = Vec::new();
    let mut i = SERVER_HELLO_LEN;
    while i + HEADER_LEN <= bytes.len() {
        let mut head = [0u8; HEADER_LEN];
        head.copy_from_slice(&bytes[i..i + HEADER_LEN]);
        let h = FrameHeader::decode(&head).expect("header");
        let start = i + HEADER_LEN;
        let end = start + h.body_len;
        assert!(end <= bytes.len(), "truncated frame");
        out.push(ServerMessage::decode(h.msg_type, &bytes[start..end]).expect("message"));
        i = end;
    }
    (hello, out)
}

#[test]
fn a_whole_conversation_survives_one_byte_at_a_time() {
    let mut input = hello();
    input.extend_from_slice(&ClientMessage::Query("get users".into()).encode().unwrap());
    input.extend_from_slice(&ClientMessage::Close.encode().unwrap());

    let mut socket = Choked::new(input, 1);
    let mut conn = Conn::new();
    let step = pump(&mut conn, &mut socket, &mut Echo);

    let (hello, messages) = parse_replies(&socket.output);
    assert_eq!(hello, ServerHello::Accepted { version: VERSION });
    assert_eq!(
        messages,
        vec![
            ServerMessage::Ready,
            ServerMessage::Lines(vec!["get users".into()])
        ]
    );
    assert_eq!(step, Step::Closed);
}

#[test]
fn the_same_conversation_in_one_gulp_gives_the_same_bytes() {
    let mut input = hello();
    input.extend_from_slice(&ClientMessage::Query("get users".into()).encode().unwrap());
    input.extend_from_slice(&ClientMessage::Close.encode().unwrap());

    let mut slow = Choked::new(input.clone(), 1);
    let mut fast = Choked::new(input, 1 << 20);
    let mut a = Conn::new();
    let mut b = Conn::new();
    pump(&mut a, &mut slow, &mut Echo);
    pump(&mut b, &mut fast, &mut Echo);

    assert_eq!(
        slow.output, fast.output,
        "chunking changed the answer, which means a partial read was mistaken \
         for a whole one"
    );
}

#[test]
fn a_frame_split_across_reads_is_not_acted_on_early() {
    let query = ClientMessage::Query("get users".into()).encode().unwrap();
    let mut input = hello();
    input.extend_from_slice(&query[..query.len() - 3]);

    let mut socket = Choked::new(input, 1);
    let mut conn = Conn::new();
    pump(&mut conn, &mut socket, &mut Echo);

    let (_, messages) = parse_replies(&socket.output);
    assert_eq!(
        messages,
        vec![ServerMessage::Ready],
        "answered a statement it had not finished reading"
    );

    socket.feed(&query[query.len() - 3..]);
    pump(&mut conn, &mut socket, &mut Echo);
    let (_, messages) = parse_replies(&socket.output);
    assert_eq!(
        messages,
        vec![
            ServerMessage::Ready,
            ServerMessage::Lines(vec!["get users".into()])
        ]
    );
}

#[test]
fn a_bad_handshake_is_refused_and_the_connection_ends() {
    let mut bad = [0u8; CLIENT_HELLO_LEN];
    bad[0] = b'X';

    let mut socket = Choked::new(bad.to_vec(), 1);
    let mut conn = Conn::new();
    let step = pump(&mut conn, &mut socket, &mut Echo);

    assert_eq!(
        socket.output.len(),
        SERVER_HELLO_LEN,
        "sent more than a refusal"
    );
    let mut sh = [0u8; SERVER_HELLO_LEN];
    sh.copy_from_slice(&socket.output);
    assert!(matches!(
        ServerHello::decode(&sh).expect("hello"),
        ServerHello::Refused(_)
    ));
    assert_eq!(step, Step::Closed);
}

#[test]
fn a_version_from_the_future_is_refused_cleanly() {
    let input = ClientHello {
        version: VERSION + 1,
    }
    .encode()
    .to_vec();
    let mut socket = Choked::new(input, 1);
    let mut conn = Conn::new();
    pump(&mut conn, &mut socket, &mut Echo);

    let mut sh = [0u8; SERVER_HELLO_LEN];
    sh.copy_from_slice(&socket.output[..SERVER_HELLO_LEN]);
    assert_eq!(
        ServerHello::decode(&sh).expect("hello"),
        ServerHello::Refused(quanty_proto::Refusal::VersionTooNew)
    );
}

#[test]
fn a_malformed_frame_gets_an_error_not_a_guess() {
    let mut input = hello();
    input.extend_from_slice(&[0xEE, 0x00, 0x00, 0x00, 0x00]);

    let mut socket = Choked::new(input, 1);
    let mut conn = Conn::new();
    let step = pump(&mut conn, &mut socket, &mut Echo);

    let (_, messages) = parse_replies(&socket.output);
    assert_eq!(messages.len(), 2);
    assert!(matches!(messages[1], ServerMessage::Error { .. }));
    assert_eq!(step, Step::Closed);
}

#[test]
fn a_large_reply_drains_over_many_writes() {
    struct Big;
    impl Service for Big {
        fn call(&mut self, _: ClientMessage) -> Vec<ServerMessage> {
            vec![ServerMessage::Lines(
                (0..500).map(|i| format!("line {i}")).collect(),
            )]
        }
    }

    let mut input = hello();
    input.extend_from_slice(&ClientMessage::Query("big".into()).encode().unwrap());

    let mut socket = Choked::new(input, 7);
    let mut conn = Conn::new();
    pump(&mut conn, &mut socket, &mut Big);

    assert!(!conn.wants_write(), "output left stuck in the buffer");
    let (_, messages) = parse_replies(&socket.output);
    assert_eq!(messages.len(), 2);
    match &messages[1] {
        ServerMessage::Lines(lines) => assert_eq!(lines.len(), 500),
        other => panic!("expected lines, got {other:?}"),
    }
}

#[test]
fn nothing_is_read_while_a_reply_is_still_going_out() {
    let mut input = hello();
    for i in 0..3 {
        input.extend_from_slice(&ClientMessage::Query(format!("q{i}")).encode().unwrap());
    }

    let mut socket = Choked::new(input, 1);
    let mut conn = Conn::new();
    pump(&mut conn, &mut socket, &mut Echo);

    let (_, messages) = parse_replies(&socket.output);
    assert_eq!(messages.len(), 4, "expected Ready plus three answers");
    for (i, m) in messages[1..].iter().enumerate() {
        assert_eq!(m, &ServerMessage::Lines(vec![format!("q{i}")]));
    }
}
