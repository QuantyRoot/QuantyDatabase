//! The server closes when it is asked to, rather than being killed.
//!
//! The crash harness next door proves the file survives a `kill -9`, which
//! is a different promise: that is recovery. This is the ordinary case, the
//! one an init system uses on every restart. It asserts three things, and
//! the third is the one that would rot quietly:
//!
//! - the process exits, and exits successfully
//! - it said what it was doing on the way out
//! - the database opens afterwards, which is only true if the executor
//!   thread was joined and the file lock given up rather than taken away
//!   by the kernel

#![cfg(target_os = "linux")]

mod common;

use std::io::BufRead;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use common::TestDir;

#[test]
fn sigterm_closes_the_server() {
    asking_it_to_stop_works("-TERM");
}

#[test]
fn sigint_closes_the_server() {
    asking_it_to_stop_works("-INT");
}

fn asking_it_to_stop_works(signal: &str) {
    let dir = TestDir::new();
    let db = dir.path().join("shutdown.qdb");
    let db = db.to_str().expect("utf-8 path");

    let made = Command::new(env!("CARGO_BIN_EXE_quanty"))
        .args(["create", db])
        .status()
        .expect("the binary runs");
    assert!(made.success(), "create failed");

    let mut child = Command::new(env!("CARGO_BIN_EXE_quanty"))
        .args(["serve", db, "--listen", "127.0.0.1:0", "--workers", "2"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("the binary runs");

    // Read in a thread, or the pipe fills and the server blocks writing its
    // own status line, which looks exactly like a server that will not stop.
    let stdout = child.stdout.take().expect("piped");
    let (ready, listening) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut lines = Vec::new();
        for line in std::io::BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if line.starts_with("listening on ") {
                let _ = ready.send(());
            }
            lines.push(line);
        }
        lines
    });

    if listening.recv_timeout(Duration::from_secs(30)).is_err() {
        let _ = child.kill();
        let _ = child.wait();
        panic!("the server never said where it was listening");
    }

    let asked = Command::new("kill")
        .args([signal, &child.id().to_string()])
        .status()
        .expect("kill runs");
    assert!(asked.success(), "could not send {signal}");

    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        match child.try_wait().expect("wait") {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("still running thirty seconds after {signal}");
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    let lines = reader.join().expect("reader thread");

    assert!(
        status.success(),
        "exited {status:?} after {signal}: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.starts_with("stopping")),
        "never said it was stopping: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l == "closed"),
        "never said it closed: {lines:?}"
    );

    // The interesting one. A server that was killed leaves the lock to the
    // kernel and the executor thread unjoined; this only passes if the
    // shutdown ran to the end.
    let after = Command::new(env!("CARGO_BIN_EXE_quanty"))
        .args(["tables", db])
        .output()
        .expect("the binary runs");
    assert!(
        after.status.success(),
        "the database did not open after {signal}: {}",
        String::from_utf8_lossy(&after.stderr)
    );
}
