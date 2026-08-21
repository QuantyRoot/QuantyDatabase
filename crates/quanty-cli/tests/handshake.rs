//! The handshake, against a running server rather than against the codec.
//!
//! `negotiate` has unit tests and they pass on bytes. What they cannot say
//! is what a real server does with a client it will not talk to: whether it
//! answers at all before hanging up, whether the four bytes are readable by
//! a client that understands nothing else, and whether one refused
//! connection leaves the next one fine. That is the third acceptance
//! criterion of phase 5, and it is about the process, not the function.
//!
//! The handshake is nine bytes out and four back and is frozen by design:
//! it is where the version is agreed, so it cannot itself be negotiable. A
//! client from any future version can always read a refusal.

#![cfg(target_os = "linux")]

mod common;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use quanty_proto::{ClientHello, Refusal, ServerHello, SERVER_HELLO_LEN, VERSION};

use common::TestDir;

fn quanty(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_quanty"))
        .args(args)
        .output()
        .expect("the binary runs")
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

struct Served {
    child: Child,
    addr: String,
}

impl Served {
    fn start(database: &str) -> Served {
        for _ in 0..5 {
            let addr = format!("127.0.0.1:{}", free_port());
            let child = Command::new(env!("CARGO_BIN_EXE_quanty"))
                .args(["serve", database, "--listen", &addr, "--workers", "1"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("the binary runs");
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if TcpStream::connect(&addr).is_ok() {
                    return Served { child, addr };
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let mut child = child;
            let _ = child.kill();
            let _ = child.wait();
        }
        panic!("the server never came up");
    }
}

impl Drop for Served {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Say hello with `bytes` and report the four bytes that come back, plus
/// whether the server then hung up.
fn greet(addr: &str, bytes: &[u8]) -> (Option<[u8; SERVER_HELLO_LEN]>, bool) {
    let mut socket = TcpStream::connect(addr).expect("connect");
    socket.set_nodelay(true).expect("nodelay");
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    socket.write_all(bytes).expect("write");

    let mut head = [0u8; SERVER_HELLO_LEN];
    let mut got = 0;
    while got < head.len() {
        match socket.read(&mut head[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(_) => break,
        }
    }
    if got < head.len() {
        return (None, true);
    }

    // Whether the server closed after answering: one more read that ends
    // the stream rather than blocking.
    let mut rest = [0u8; 64];
    let hung_up = matches!(socket.read(&mut rest), Ok(0));
    (Some(head), hung_up)
}

fn server() -> (TestDir, Served) {
    let dir = TestDir::new();
    let path = dir.path().join("hello.qdb");
    let path = path.to_str().expect("utf8 path").to_string();
    assert!(quanty(&["create", &path]).status.success());
    assert!(quanty(&["run", &path, "table t { id: int @key }"])
        .status
        .success());
    let served = Served::start(&path);
    (dir, served)
}

/// A client from a version the server does not know yet is refused, told
/// why, and disconnected. It is not left hanging and it is not accepted.
#[test]
fn a_client_from_the_future_is_refused_and_told_why() {
    let (_dir, served) = server();

    let hello = ClientHello {
        version: VERSION + 1,
    };
    let (reply, hung_up) = greet(&served.addr, &hello.encode());
    let reply = reply.expect("the server answered nothing at all");

    match ServerHello::decode(&reply).expect("four readable bytes") {
        ServerHello::Refused(Refusal::VersionTooNew) => {}
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(hung_up, "the server refused and then held the connection");
}

/// Version zero is the other end of the same rule.
#[test]
fn a_client_from_before_version_one_is_refused_as_too_old() {
    let (_dir, served) = server();

    let (reply, hung_up) = greet(&served.addr, &ClientHello { version: 0 }.encode());
    let reply = reply.expect("the server answered nothing at all");
    match ServerHello::decode(&reply).expect("four readable bytes") {
        ServerHello::Refused(Refusal::VersionTooOld) => {}
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(hung_up);
}

/// Something that is not this protocol at all: a browser, a port scanner,
/// a fat fingered `curl`.
#[test]
fn a_stranger_is_refused_on_the_magic_rather_than_parsed() {
    let (_dir, served) = server();

    let (reply, hung_up) = greet(&served.addr, b"GET / HTTP/1.1\r\n\r\n");
    let reply = reply.expect("the server answered nothing at all");
    match ServerHello::decode(&reply).expect("four readable bytes") {
        ServerHello::Refused(Refusal::BadMagic) => {}
        other => panic!("expected a refusal, got {other:?}"),
    }
    assert!(hung_up);
}

/// A refusal is about one connection, not about the server.
#[test]
fn a_refused_connection_does_not_spoil_the_next_one() {
    let (_dir, served) = server();

    for _ in 0..5 {
        let (reply, _) = greet(
            &served.addr,
            &ClientHello {
                version: VERSION + 7,
            }
            .encode(),
        );
        assert!(matches!(
            ServerHello::decode(&reply.expect("answer")).expect("decode"),
            ServerHello::Refused(_)
        ));
    }

    let (reply, hung_up) = greet(&served.addr, &ClientHello { version: VERSION }.encode());
    match ServerHello::decode(&reply.expect("answer")).expect("decode") {
        ServerHello::Accepted { version } => assert_eq!(version, VERSION),
        other => panic!("a good client was turned away after bad ones: {other:?}"),
    }
    assert!(
        !hung_up,
        "an accepted connection was closed instead of served"
    );

    // And it still works, which is what "not spoiled" has to mean.
    let out = quanty(&["connect", &served.addr, "show tables"]);
    assert!(
        out.status.success(),
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A hello that arrives one byte at a time is still a hello.
///
/// Nine bytes fit in one packet in every ordinary case, which is exactly
/// why the split case goes untested until something splits it.
#[test]
fn a_hello_split_across_packets_is_still_read() {
    let (_dir, served) = server();

    let hello = ClientHello { version: VERSION }.encode();
    let mut socket = TcpStream::connect(&served.addr).expect("connect");
    socket.set_nodelay(true).expect("nodelay");
    socket
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    for byte in hello.iter() {
        socket.write_all(&[*byte]).expect("write");
        std::thread::sleep(Duration::from_millis(5));
    }

    let mut head = [0u8; SERVER_HELLO_LEN];
    let mut got = 0;
    while got < head.len() {
        match socket.read(&mut head[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(_) => break,
        }
    }
    assert_eq!(got, head.len(), "the server never finished the handshake");
    match ServerHello::decode(&head).expect("decode") {
        ServerHello::Accepted { version } => assert_eq!(version, VERSION),
        other => panic!("a dribbled hello was refused: {other:?}"),
    }
}
