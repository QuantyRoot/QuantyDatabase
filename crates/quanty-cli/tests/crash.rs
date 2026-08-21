//! Kill the server mid-write and check what it promised is still there.
//!
//! The pager and the transaction layer each have a harness that kills the
//! process a thousand times and reopens the file. Neither of them crosses
//! the protocol. That matters because the promise a network client is given
//! is stronger than "the file is intact": when the server answers a write
//! with a row count, the client is entitled to believe that write survived,
//! and after ADR-028 that answer is held back until a commit shared with
//! other connections has been written and fsynced. Whether the answer really
//! waits for the fsync, or only looks like it does, is not something the
//! in-process tests can see.
//!
//! So: write from several connections, `kill -9` the server in the middle,
//! reopen the file, and require that **every write that was acknowledged is
//! present**.
//!
//! Only that direction is required. A write that was committed by the
//! executor and whose reply died with the process may or may not be in the
//! file, and demanding either would be demanding something the design never
//! offered. Rows beyond the acknowledged ones are therefore fine; a missing
//! acknowledged row is a broken promise.
//!
//! Iterations via `QUANTY_SERVER_CRASH_ITERS`, default 8.

#![cfg(target_os = "linux")]

mod common;

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use quanty_proto::frame::{FrameHeader, HEADER_LEN};
use quanty_proto::{
    ClientHello, ClientMessage, ServerHello, ServerMessage, SERVER_HELLO_LEN, VERSION,
};

use common::TestDir;

/// One connection speaking the protocol by hand.
struct Wire {
    socket: TcpStream,
    buf: Vec<u8>,
}

impl Wire {
    /// Connect and finish the handshake, or give up.
    fn open(addr: &str) -> Option<Wire> {
        let socket = TcpStream::connect(addr).ok()?;
        socket.set_nodelay(true).ok()?;
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .ok()?;
        let mut wire = Wire {
            socket,
            buf: Vec::new(),
        };
        wire.socket
            .write_all(&ClientHello { version: VERSION }.encode())
            .ok()?;
        let mut head = [0u8; SERVER_HELLO_LEN];
        wire.fill(&mut head)?;
        match ServerHello::decode(&head).ok()? {
            ServerHello::Accepted { .. } => {}
            _ => return None,
        }
        // The Ready that follows an accepted handshake.
        match wire.reply()? {
            ServerMessage::Ready => Some(wire),
            _ => None,
        }
    }

    /// Send a statement and read the single message that answers it.
    fn ask(&mut self, statement: &str) -> Option<ServerMessage> {
        let bytes = ClientMessage::Query(statement.to_string()).encode().ok()?;
        self.socket.write_all(&bytes).ok()?;
        self.reply()
    }

    fn reply(&mut self) -> Option<ServerMessage> {
        let mut head = [0u8; HEADER_LEN];
        self.fill(&mut head)?;
        let header = FrameHeader::decode(&head).ok()?;
        let mut body = vec![0u8; header.body_len];
        self.fill(&mut body)?;
        ServerMessage::decode(header.msg_type, &body).ok()
    }

    fn fill(&mut self, out: &mut [u8]) -> Option<()> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.buf.len() < out.len() {
            if Instant::now() >= deadline {
                return None;
            }
            let mut chunk = [0u8; 4096];
            match self.socket.read(&mut chunk) {
                // The server died. That is the point of this test, not an
                // error in it.
                Ok(0) => return None,
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return None,
            }
        }
        out.copy_from_slice(&self.buf[..out.len()]);
        self.buf.drain(..out.len());
        Some(())
    }
}

fn quanty(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_quanty"))
        .args(args)
        .output()
        .expect("the binary runs")
}

fn said(out: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("bind")
        .local_addr()
        .expect("addr")
        .port()
}

/// A serving process, killed rather than asked to stop.
struct Server {
    child: Child,
    addr: String,
}

impl Server {
    fn start(database: &str) -> Server {
        for _ in 0..5 {
            let addr = format!("127.0.0.1:{}", free_port());
            let child = Command::new(env!("CARGO_BIN_EXE_quanty"))
                .args(["serve", database, "--listen", &addr, "--workers", "2"])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("the binary runs");
            let deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < deadline {
                if TcpStream::connect(&addr).is_ok() {
                    return Server { child, addr };
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            let mut child = child;
            let _ = child.kill();
            let _ = child.wait();
        }
        panic!("the server never came up");
    }

    /// SIGKILL: no chance to flush, close or tidy up.
    fn kill_now(&mut self) {
        self.child.kill().expect("kill");
        self.child.wait().expect("wait");
    }
}

/// Every `id` present in the database, read by reopening the file.
fn rows_in(database: &Path) -> BTreeSet<i64> {
    let path = database.to_str().expect("utf8 path");
    let out = quanty(&["run", path, "get t { id }"]);
    assert!(
        out.status.success(),
        "the database did not reopen after the kill: {}",
        said(&out)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse::<i64>().ok())
        .collect()
}

#[test]
fn every_acknowledged_write_survives_a_kill() {
    let iterations: usize = std::env::var("QUANTY_SERVER_CRASH_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8);

    let dir = TestDir::new();
    let mut total_acked = 0usize;

    for round in 0..iterations {
        let database = dir.path().join(format!("crash-{round}.qdb"));
        let path = database.to_str().expect("utf8 path").to_string();
        assert!(quanty(&["create", &path]).status.success());
        assert!(quanty(&["run", &path, "table t { id: int @key, n: int }"])
            .status
            .success());

        let mut server = Server::start(&path);
        let acked: Arc<Mutex<BTreeSet<i64>>> = Arc::new(Mutex::new(BTreeSet::new()));
        let running = Arc::new(AtomicBool::new(true));
        let connected = Arc::new(AtomicUsize::new(0));

        let mut writers = Vec::new();
        for lane in 0..4i64 {
            let addr = server.addr.clone();
            let acked = Arc::clone(&acked);
            let running = Arc::clone(&running);
            let connected = Arc::clone(&connected);
            writers.push(std::thread::spawn(move || {
                let Some(mut wire) = Wire::open(&addr) else {
                    return;
                };
                connected.fetch_add(1, Ordering::Relaxed);
                let mut key = lane * 1_000_000;
                while running.load(Ordering::Relaxed) {
                    key += 1;
                    match wire.ask(&format!("put t {{ id: {key}, n: {key} }}")) {
                        // Acknowledged. From here the client is entitled to
                        // believe this row exists, and this test is the
                        // thing that holds the server to it.
                        Some(ServerMessage::Count { n: 1, .. }) => {
                            acked.lock().expect("lock").insert(key);
                        }
                        Some(_) => {}
                        // The socket died with the process.
                        None => return,
                    }
                }
            }));
        }

        // Wait for the writers to actually be landing writes, then let a
        // few more go before killing.
        //
        // A fixed sleep assumes the first write completes inside it, and an
        // fsync on a loaded machine does not have to. That assumption is
        // what a fixed sleep hides: the round then kills a server that has
        // acknowledged nothing, and the guard below reports a timing
        // accident as though it were a finding.
        let ready_by = Instant::now() + Duration::from_secs(30);
        loop {
            if !acked.lock().expect("lock").is_empty() {
                break;
            }
            assert!(
                Instant::now() < ready_by,
                "round {round}: no write was acknowledged in 30s; {} of 4 \
                 writers got as far as connecting",
                connected.load(Ordering::Relaxed)
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        // Varied, so the kill does not always land at the same point in a
        // commit.
        let extra = 20 + (round as u64 * 37) % 130;
        std::thread::sleep(Duration::from_millis(extra));
        server.kill_now();
        running.store(false, Ordering::Relaxed);
        for writer in writers {
            let _ = writer.join();
        }

        let promised = acked.lock().expect("lock").clone();
        assert!(
            !promised.is_empty(),
            "round {round}: the acknowledged set emptied itself, which is \
             a bug in this test rather than in the server"
        );
        total_acked += promised.len();

        let present = rows_in(&database);
        let lost: Vec<i64> = promised.difference(&present).copied().collect();
        assert!(
            lost.is_empty(),
            "round {round}: {} writes were acknowledged and are gone after \
             the kill: {:?}",
            lost.len(),
            &lost[..lost.len().min(10)]
        );

        let _ = std::fs::remove_file(&database);
    }

    println!("server crash: {iterations} kills, {total_acked} acknowledged writes, none lost");
}
