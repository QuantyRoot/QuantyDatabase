//! What the readiness layer promises, checked rather than claimed.

#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use quanty_server::{Interest, Poller, Token};

/// Count this process's open descriptors.
fn connected_pair() -> (TcpStream, TcpStream) {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = l.local_addr().expect("addr");
    let client = TcpStream::connect(addr).expect("connect");
    let (server, _) = l.accept().expect("accept");
    client.set_nonblocking(true).expect("nonblocking");
    server.set_nonblocking(true).expect("nonblocking");
    (client, server)
}

/// Collect one poll's worth of events.
fn drain_once(p: &mut Poller, timeout_ms: i32) -> Vec<(Token, bool, bool, bool)> {
    let mut out = Vec::new();
    p.poll(timeout_ms, |e| {
        out.push((e.token, e.is_readable(), e.is_writable(), e.is_error()));
    })
    .expect("poll");
    out
}

/// The kernel's struct is packed on x86_64 and not elsewhere. Getting it
#[test]
fn layout_matches_the_kernel() {
    let expected = if cfg!(target_arch = "x86_64") { 12 } else { 16 };
    let mut p = Poller::new(8).expect("poller");
    let (_client, server) = connected_pair();
    let token = Token(0x0123_4567_89ab_cdef);
    p.register(&server, token, Interest::WRITABLE).expect("reg");
    let evs = drain_once(&mut p, 1000);
    assert_eq!(evs.len(), 1, "a fresh socket should be writable");
    assert_eq!(
        evs[0].0, token,
        "token came back wrong, which means the event struct layout is \
         wrong for this architecture (expected {expected} bytes)"
    );
}

#[test]
fn readable_only_when_there_is_something_to_read() {
    let mut p = Poller::new(8).expect("poller");
    let (mut client, server) = connected_pair();
    p.register(&server, Token(1), Interest::READABLE)
        .expect("reg");

    let evs = drain_once(&mut p, 50);
    assert!(
        evs.is_empty(),
        "reported readable with nothing sent: {evs:?}"
    );

    client.write_all(b"ping").expect("write");
    let evs = drain_once(&mut p, 1000);
    assert_eq!(evs.len(), 1);
    assert!(evs[0].1, "should be readable");
}

/// Level-triggered, pinned. ADR-023 chose it because the failure mode of
#[test]
fn unread_data_is_reported_again() {
    let mut p = Poller::new(8).expect("poller");
    let (mut client, mut server) = connected_pair();
    p.register(&server, Token(7), Interest::READABLE)
        .expect("reg");

    client.write_all(b"twelve bytes").expect("write");

    let first = drain_once(&mut p, 1000);
    assert_eq!(first.len(), 1, "first report");

    let mut one = [0u8; 1];
    server.read_exact(&mut one).expect("partial read");

    let second = drain_once(&mut p, 1000);
    assert_eq!(
        second.len(),
        1,
        "level-triggered must report again while data remains"
    );
}

#[test]
fn hangup_is_reported_without_being_requested() {
    let mut p = Poller::new(8).expect("poller");
    let (client, server) = connected_pair();
    p.register(&server, Token(3), Interest::WRITABLE)
        .expect("reg");

    drop(client);

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut saw = false;
    while Instant::now() < deadline && !saw {
        for (_, readable, _, error) in drain_once(&mut p, 100) {
            saw |= readable || error;
        }
    }
    assert!(saw, "a closed peer was never reported");
}

#[test]
fn interest_can_be_changed_and_withdrawn() {
    let mut p = Poller::new(8).expect("poller");
    let (mut client, server) = connected_pair();
    p.register(&server, Token(1), Interest::WRITABLE)
        .expect("reg");
    assert_eq!(drain_once(&mut p, 500).len(), 1, "writable");

    p.reregister(&server, Token(1), Interest::READABLE)
        .expect("mod");
    assert!(
        drain_once(&mut p, 50).is_empty(),
        "still reporting writable after switching to readable"
    );

    client.write_all(b"x").expect("write");
    assert_eq!(drain_once(&mut p, 1000).len(), 1, "readable after switch");

    p.deregister(&server).expect("del");
    assert!(
        drain_once(&mut p, 50).is_empty(),
        "still reporting after deregister"
    );
}

#[test]
fn a_parked_worker_can_be_woken_from_another_thread() {
    let mut p = Poller::new(8).expect("poller");
    let waker = p.waker();
    let (tx, rx) = mpsc::channel();

    let h = thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        waker.wake().expect("wake");
        tx.send(()).expect("send");
    });

    let start = Instant::now();
    let evs = drain_once(&mut p, 10_000);
    let waited = start.elapsed();

    h.join().expect("join");
    rx.recv_timeout(Duration::from_secs(1)).expect("signalled");

    assert!(waited < Duration::from_secs(5), "wakeup did not arrive");
    assert!(
        evs.is_empty(),
        "the wake token must not be dispatched to the handler: {evs:?}"
    );
}

/// Several wakeups before the worker runs collapse into one, and the next
#[test]
fn repeated_wakeups_do_not_leave_the_loop_spinning() {
    let mut p = Poller::new(8).expect("poller");
    let waker = p.waker();
    for _ in 0..5 {
        waker.wake().expect("wake");
    }
    drain_once(&mut p, 1000);

    let start = Instant::now();
    drain_once(&mut p, 200);
    assert!(
        start.elapsed() >= Duration::from_millis(150),
        "poll returned immediately, so the eventfd was left undrained"
    );
}

/// A poller whose buffer is smaller than the number of ready descriptors
#[test]
fn more_ready_than_the_buffer_holds_is_not_lost() {
    let mut p = Poller::new(2).expect("poller");
    let mut kept = Vec::new();
    for i in 0..6u64 {
        let (mut client, server) = connected_pair();
        client.write_all(b"x").expect("write");
        p.register(&server, Token(i), Interest::READABLE)
            .expect("reg");
        kept.push((client, server));
    }

    let mut seen = std::collections::BTreeSet::new();
    let deadline = Instant::now() + Duration::from_secs(3);
    while seen.len() < 6 && Instant::now() < deadline {
        for (t, _, _, _) in drain_once(&mut p, 200) {
            seen.insert(t.0);
        }
    }
    assert_eq!(seen.len(), 6, "lost events: saw {seen:?}");
}
