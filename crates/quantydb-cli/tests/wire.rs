//! The wire format, spoken by a client that does not share code with it.
//!
//! Every other test here reaches for `quantydb-proto`, which means the
//! server and the client agree by construction: change both and nothing
//! notices. This one encodes and decodes the bytes by hand, from
//! `docs/PROTOCOL.md` and nothing else. If the document and the
//! implementation drift apart, this fails, and it fails on the side that
//! an outside client would.
//!
//! That is not hypothetical. The document had three errors in it and all
//! three were found by handing it to somebody with no access to the
//! source: `Query` was documented without the length its body carries, the
//! unprompted `Ready` was promised only from servers wanting no token, and
//! the token was called opaque without saying which bytes go on the wire.
//! Each cost a working client a wrong turn. This file is what stops the
//! next three.
//!
//! It is deliberately dumb. No helpers from the crate under test, no
//! shared constants, no derive. The duplication is the test.

#![cfg(target_os = "linux")]

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use common::TestDir;

// ---------------------------------------------------------------------------
// the format, transcribed from the document
// ---------------------------------------------------------------------------

const MAGIC: &[u8; 6] = b"QUANTY";

const T_AUTH: u8 = 0x10;
const T_QUERY: u8 = 0x11;
const T_QUERY_SQL: u8 = 0x12;
const T_CLOSE: u8 = 0x13;

const T_READY: u8 = 0x20;
const T_OK: u8 = 0x21;
const T_COUNT: u8 = 0x22;
const T_ROWS_BEGIN: u8 = 0x23;
const T_ROW_BATCH: u8 = 0x24;
const T_ROWS_END: u8 = 0x25;
const T_LINES: u8 = 0x26;
const T_ERROR: u8 = 0x27;

const V_NULL: u8 = 0x01;
const V_BOOL: u8 = 0x02;
const V_INT: u8 = 0x03;
const V_FLOAT: u8 = 0x04;
const V_TEXT: u8 = 0x05;
const V_BYTES: u8 = 0x06;

/// A value as the document describes one, decoded far enough to compare.
#[derive(Debug, PartialEq)]
enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
}

struct Wire {
    sock: TcpStream,
}

impl Wire {
    /// Connect and shake hands, then take the unprompted `Ready`.
    fn open(addr: &str, version: u16) -> Wire {
        let sock = TcpStream::connect(addr).expect("connect");
        sock.set_read_timeout(Some(Duration::from_secs(20)))
            .expect("timeout");
        let mut wire = Wire { sock };

        let mut hello = Vec::new();
        hello.extend_from_slice(MAGIC);
        hello.extend_from_slice(&version.to_le_bytes());
        hello.push(0);
        assert_eq!(hello.len(), 9, "the client hello is nine bytes");
        wire.sock.write_all(&hello).expect("send hello");

        let reply = wire.exactly(4);
        assert_eq!(reply[0], 0x01, "handshake refused: {reply:?}");
        let negotiated = u16::from_le_bytes([reply[1], reply[2]]);
        assert_eq!(negotiated, version, "the server chose another version");

        let (kind, body) = wire.frame();
        assert_eq!(
            kind, T_READY,
            "every accepted handshake is followed by an unprompted Ready"
        );
        assert!(body.is_empty(), "Ready has no body");
        wire
    }

    fn exactly(&mut self, n: usize) -> Vec<u8> {
        let mut buf = vec![0u8; n];
        self.sock.read_exact(&mut buf).expect("read");
        buf
    }

    /// One frame: a type byte, a u32 body length, that many bytes.
    fn frame(&mut self) -> (u8, Vec<u8>) {
        let head = self.exactly(5);
        let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
        (head[0], self.exactly(len))
    }

    fn send(&mut self, kind: u8, body: &[u8]) {
        let mut out = Vec::with_capacity(5 + body.len());
        out.push(kind);
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(body);
        self.sock.write_all(&out).expect("send");
    }

    /// `text` and `bytes` are the same shape: a u32 length, then the bytes.
    fn length_prefixed(payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + payload.len());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn auth(&mut self, token: &str) {
        self.send(T_AUTH, &Self::length_prefixed(token.as_bytes()));
        let (kind, body) = self.frame();
        assert_eq!(kind, T_READY, "Auth refused: {}", describe(kind, &body));
    }

    fn ask(&mut self, kind: u8, statement: &str) -> Vec<(u8, Vec<u8>)> {
        self.send(kind, &Self::length_prefixed(statement.as_bytes()));
        let mut out = Vec::new();
        loop {
            let (kind, body) = self.frame();
            let done = matches!(kind, T_OK | T_COUNT | T_ROWS_END | T_LINES | T_ERROR);
            out.push((kind, body));
            if done {
                return out;
            }
        }
    }
}

fn read_u32(body: &[u8], at: &mut usize) -> u32 {
    let n = u32::from_le_bytes(body[*at..*at + 4].try_into().expect("four bytes"));
    *at += 4;
    n
}

fn read_text(body: &[u8], at: &mut usize) -> String {
    let len = read_u32(body, at) as usize;
    let s = String::from_utf8(body[*at..*at + len].to_vec()).expect("utf-8");
    *at += len;
    s
}

fn read_value(body: &[u8], at: &mut usize) -> Value {
    let tag = body[*at];
    *at += 1;
    match tag {
        V_NULL => Value::Null,
        V_BOOL => {
            let b = body[*at];
            *at += 1;
            assert!(b < 2, "Bool carries 0 or 1, got {b}");
            Value::Bool(b == 1)
        }
        V_INT => {
            let n = i64::from_le_bytes(body[*at..*at + 8].try_into().expect("eight"));
            *at += 8;
            Value::Int(n)
        }
        V_FLOAT => {
            let bits = u64::from_le_bytes(body[*at..*at + 8].try_into().expect("eight"));
            *at += 8;
            Value::Float(f64::from_bits(bits))
        }
        V_TEXT => Value::Text(read_text(body, at)),
        V_BYTES => {
            let len = read_u32(body, at) as usize;
            let b = body[*at..*at + len].to_vec();
            *at += len;
            Value::Bytes(b)
        }
        other => panic!("value tag 0x{other:02x} is not in the document"),
    }
}

fn describe(kind: u8, body: &[u8]) -> String {
    if kind == T_ERROR {
        let mut at = 0;
        let code = u16::from_le_bytes([body[0], body[1]]);
        at += 2;
        format!("error 0x{code:04x}: {}", read_text(body, &mut at))
    } else {
        format!("message 0x{kind:02x}")
    }
}

/// Column names, then every row, from a full answer.
fn rows_of(answer: &[(u8, Vec<u8>)]) -> (Vec<String>, Vec<Vec<Value>>) {
    let mut columns = Vec::new();
    let mut rows = Vec::new();
    for (kind, body) in answer {
        match *kind {
            T_ROWS_BEGIN => {
                let mut at = 0;
                let n = read_u32(body, &mut at);
                for _ in 0..n {
                    columns.push(read_text(body, &mut at));
                }
                assert_eq!(at, body.len(), "RowsBegin had bytes left over");
            }
            T_ROW_BATCH => {
                let mut at = 0;
                let n = read_u32(body, &mut at);
                for _ in 0..n {
                    let width = read_u32(body, &mut at);
                    let mut row = Vec::new();
                    for _ in 0..width {
                        row.push(read_value(body, &mut at));
                    }
                    rows.push(row);
                }
                assert_eq!(at, body.len(), "RowBatch had bytes left over");
            }
            T_ERROR => panic!("{}", describe(*kind, body)),
            _ => {}
        }
    }
    (columns, rows)
}

// ---------------------------------------------------------------------------
// a server to talk to
// ---------------------------------------------------------------------------

struct Server {
    child: Child,
    addr: String,
    token: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn tool() -> &'static str {
    env!("CARGO_BIN_EXE_quantydb")
}

fn run(args: &[&str]) -> String {
    let out = Command::new(tool())
        .args(args)
        .output()
        .expect("the tool runs");
    assert!(
        out.status.success(),
        "{args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// A database with one row of every value type, and a server in front.
fn serving(dir: &TestDir) -> Server {
    let db = dir.path().join("wire.qdb");
    let db = db.to_str().expect("utf-8");
    let tokens = dir.path().join("wire.tokens");

    run(&["create", db]);
    run(&[
        "run",
        db,
        "table every { id: int @key, t: text, f: float, b: bool, \
         raw: bytes, maybe: text @null }",
    ]);
    run(&[
        "run",
        db,
        r#"put every { id: 1, t: "hello", f: 1.5, b: true, raw: x"c0ffee" }"#,
    ]);
    run(&[
        "run",
        db,
        r#"put every { id: 2, t: "zwei", f: -0.25, b: false, raw: x"00", maybe: "here" }"#,
    ]);

    let minted = run(&["token", "wire"]);
    let mut token = String::new();
    let mut line = String::new();
    for l in minted.lines() {
        if let Some(rest) = l.strip_prefix("token ") {
            token = rest.trim().to_string();
        }
        if let Some(rest) = l.strip_prefix("line  ") {
            line = rest.trim().to_string();
        }
    }
    assert!(!token.is_empty() && !line.is_empty(), "no token: {minted}");
    std::fs::write(&tokens, format!("{line}\n")).expect("write tokens");

    let mut child = Command::new(tool())
        .args(["serve", db, "--listen", "127.0.0.1:0", "--tokens"])
        .arg(&tokens)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("serve");

    // The port is chosen by the kernel, so it comes back on stdout.
    let stdout = child.stdout.take().expect("piped");
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if let Some(rest) = line.strip_prefix("listening on ") {
                let addr = rest.split(',').next().unwrap_or("").trim().to_string();
                let _ = tx.send(addr);
            }
        }
    });
    let addr = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("the server never said where it was listening");

    Server { child, addr, token }
}

// ---------------------------------------------------------------------------
// the tests
// ---------------------------------------------------------------------------

#[test]
fn every_value_type_survives_the_wire() {
    let dir = TestDir::new();
    let server = serving(&dir);
    let mut wire = Wire::open(&server.addr, 1);
    wire.auth(&server.token);

    let answer = wire.ask(T_QUERY, "get every { id, t, f, b, raw, maybe } order by id");
    let (columns, rows) = rows_of(&answer);

    assert_eq!(columns, ["id", "t", "f", "b", "raw", "maybe"]);
    assert_eq!(
        rows[0],
        vec![
            Value::Int(1),
            Value::Text("hello".into()),
            Value::Float(1.5),
            Value::Bool(true),
            Value::Bytes(vec![0xc0, 0xff, 0xee]),
            Value::Null,
        ]
    );
    assert_eq!(
        rows[1],
        vec![
            Value::Int(2),
            Value::Text("zwei".into()),
            Value::Float(-0.25),
            Value::Bool(false),
            Value::Bytes(vec![0x00]),
            Value::Text("here".into()),
        ]
    );
}

/// The document says Float carries bits so that NaN and the infinities
/// survive. Nothing else in the suite would notice if it started carrying
/// a decimal rendering instead.
#[test]
fn float_carries_bits_and_not_a_rendering() {
    let dir = TestDir::new();
    let server = serving(&dir);
    let mut wire = Wire::open(&server.addr, 1);
    wire.auth(&server.token);

    let answer = wire.ask(T_QUERY, "get every { f } where id = 1");
    let (_, rows) = rows_of(&answer);
    match rows[0][0] {
        Value::Float(f) => assert_eq!(f.to_bits(), 1.5f64.to_bits(), "bit for bit"),
        ref other => panic!("expected a float, got {other:?}"),
    }
}

#[test]
fn a_write_answers_with_a_count_and_its_verb() {
    let dir = TestDir::new();
    let server = serving(&dir);
    let mut wire = Wire::open(&server.addr, 1);
    wire.auth(&server.token);

    let answer = wire.ask(
        T_QUERY,
        "put every { id: 9, t: \"nine\", f: 0.0, b: true, raw: x\"\" }",
    );
    assert_eq!(answer.len(), 1, "one message, not a result set");
    let (kind, body) = &answer[0];
    assert_eq!(*kind, T_COUNT, "{}", describe(*kind, body));

    let mut at = 0;
    let verb = read_text(body, &mut at);
    let n = u64::from_le_bytes(body[at..at + 8].try_into().expect("eight"));
    at += 8;
    assert_eq!(verb, "put");
    assert_eq!(n, 1);
    assert_eq!(at, body.len(), "Count had bytes left over");
}

#[test]
fn show_answers_with_lines() {
    let dir = TestDir::new();
    let server = serving(&dir);
    let mut wire = Wire::open(&server.addr, 1);
    wire.auth(&server.token);

    let answer = wire.ask(T_QUERY, "show branches");
    let (kind, body) = &answer[0];
    assert_eq!(*kind, T_LINES, "{}", describe(*kind, body));

    let mut at = 0;
    let n = read_u32(body, &mut at);
    let mut lines = Vec::new();
    for _ in 0..n {
        lines.push(read_text(body, &mut at));
    }
    assert_eq!(at, body.len(), "Lines had bytes left over");
    assert!(
        lines.iter().any(|l| l.contains("main")),
        "no main branch in {lines:?}"
    );
}

#[test]
fn the_sql_front_end_answers_on_its_own_message_type() {
    let dir = TestDir::new();
    let server = serving(&dir);
    let mut wire = Wire::open(&server.addr, 1);
    wire.auth(&server.token);

    let answer = wire.ask(T_QUERY_SQL, "SELECT t FROM every WHERE id = 1");
    let (columns, rows) = rows_of(&answer);
    assert_eq!(columns, ["t"]);
    assert_eq!(rows, vec![vec![Value::Text("hello".into())]]);
}

/// Everything before a token is refused, and refused with the code the
/// document names rather than a parse error or a hang.
#[test]
fn a_query_before_auth_is_refused_with_0x0003() {
    let dir = TestDir::new();
    let server = serving(&dir);
    let mut wire = Wire::open(&server.addr, 1);

    wire.send(T_QUERY, &Wire::length_prefixed(b"get every { id }"));
    let (kind, body) = wire.frame();
    assert_eq!(kind, T_ERROR, "{}", describe(kind, &body));
    let code = u16::from_le_bytes([body[0], body[1]]);
    assert_eq!(code, 0x0003, "{}", describe(kind, &body));
}

#[test]
fn a_wrong_token_is_refused_with_0x0004() {
    let dir = TestDir::new();
    let server = serving(&dir);
    let mut wire = Wire::open(&server.addr, 1);

    wire.send(T_AUTH, &Wire::length_prefixed(b"not the token"));
    let (kind, body) = wire.frame();
    assert_eq!(kind, T_ERROR, "{}", describe(kind, &body));
    let code = u16::from_le_bytes([body[0], body[1]]);
    assert_eq!(code, 0x0004, "{}", describe(kind, &body));
}

#[test]
fn a_statement_that_does_not_parse_says_so_and_the_connection_lives() {
    let dir = TestDir::new();
    let server = serving(&dir);
    let mut wire = Wire::open(&server.addr, 1);
    wire.auth(&server.token);

    let answer = wire.ask(T_QUERY, "this is not a statement");
    let (kind, body) = &answer[0];
    assert_eq!(*kind, T_ERROR, "{}", describe(*kind, body));
    let code = u16::from_le_bytes([body[0], body[1]]);
    assert_eq!(code, 0x0005, "{}", describe(*kind, body));

    // The point of the test: a refused statement is not a refused
    // connection.
    let (columns, _) = rows_of(&wire.ask(T_QUERY, "get every { id } where id = 1"));
    assert_eq!(columns, ["id"]);
}

/// The token is the printed characters. Sending the bytes they spell is
/// the mistake the document used to invite, and it has to be refused
/// rather than quietly accepted.
#[test]
fn the_token_is_the_characters_and_not_the_bytes_they_spell() {
    let dir = TestDir::new();
    let server = serving(&dir);
    let mut wire = Wire::open(&server.addr, 1);

    let decoded: Vec<u8> = server
        .token
        .as_bytes()
        .chunks(2)
        .map(|pair| {
            let s = std::str::from_utf8(pair).expect("ascii");
            u8::from_str_radix(s, 16).expect("hex")
        })
        .collect();
    assert_eq!(decoded.len(), 32, "the token spells thirty-two bytes");

    wire.send(T_AUTH, &Wire::length_prefixed(&decoded));
    let (kind, body) = wire.frame();
    assert_eq!(kind, T_ERROR, "the decoded token was accepted");
    let code = u16::from_le_bytes([body[0], body[1]]);
    assert_eq!(code, 0x0004);
}

#[test]
fn close_ends_the_connection_without_an_error() {
    let dir = TestDir::new();
    let server = serving(&dir);
    let mut wire = Wire::open(&server.addr, 1);
    wire.auth(&server.token);

    wire.send(T_CLOSE, &[]);
    let mut buf = [0u8; 1];
    match wire.sock.read(&mut buf) {
        Ok(0) => {}
        Ok(_) => panic!("Close was answered with data"),
        Err(e) => panic!("Close was answered with {e}"),
    }
}

/// A version the server does not speak is refused rather than guessed at.
#[test]
fn an_unknown_version_is_refused_in_the_handshake() {
    let dir = TestDir::new();
    let server = serving(&dir);

    let mut sock = TcpStream::connect(&server.addr).expect("connect");
    sock.set_read_timeout(Some(Duration::from_secs(20)))
        .expect("timeout");
    let mut hello = Vec::new();
    hello.extend_from_slice(MAGIC);
    hello.extend_from_slice(&999u16.to_le_bytes());
    hello.push(0);
    sock.write_all(&hello).expect("send");

    let mut reply = [0u8; 4];
    sock.read_exact(&mut reply).expect("read");
    assert_eq!(reply[0], 0x00, "a version from the future was accepted");
}

#[test]
fn bad_magic_is_refused_in_the_handshake() {
    let dir = TestDir::new();
    let server = serving(&dir);

    let mut sock = TcpStream::connect(&server.addr).expect("connect");
    sock.set_read_timeout(Some(Duration::from_secs(20)))
        .expect("timeout");
    sock.write_all(b"NOTQTY\x01\x00\x00").expect("send");

    let mut reply = [0u8; 4];
    sock.read_exact(&mut reply).expect("read");
    assert_eq!(reply[0], 0x00, "a client with the wrong magic was accepted");
}
