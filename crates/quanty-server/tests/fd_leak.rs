//! Descriptor accounting. One test in its own binary, because a process-wide
//! count is meaningless while other tests open sockets on other threads.

#![cfg(target_os = "linux")]

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use quanty_server::{Idle, Interest, Poller, Token, Worker};

fn open_fds() -> usize {
    std::fs::read_dir("/proc/self/fd").expect("procfs").count()
}

fn listener() -> (Arc<TcpListener>, std::net::SocketAddr) {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    l.set_nonblocking(true).expect("nonblocking");
    let addr = l.local_addr().expect("addr");
    (Arc::new(l), addr)
}

fn poller_cycle() {
    let mut p = Poller::new(8).expect("poller");
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = l.local_addr().expect("addr");
    let mut client = TcpStream::connect(addr).expect("connect");
    let (server, _) = l.accept().expect("accept");
    server.set_nonblocking(true).expect("nonblocking");
    p.register(&server, Token::new(1, 1), Interest::READABLE)
        .expect("reg");
    client.write_all(b"x").expect("write");
    p.poll(200, |_| {}).expect("poll");
    p.deregister(&server).expect("del");
}

fn worker_cycle() {
    let (l, addr) = listener();
    let flag = Arc::new(AtomicBool::new(true));
    let mut w = Worker::new(l, flag).expect("worker");
    let clients: Vec<TcpStream> = (0..5)
        .map(|_| TcpStream::connect(addr).expect("connect"))
        .collect();
    let deadline = Instant::now() + Duration::from_secs(3);
    while w.len() < 5 && Instant::now() < deadline {
        w.turn(50, &Idle).expect("turn");
    }
    assert_eq!(w.len(), 5, "accepted {} of 5", w.len());
    drop(clients);
    w.shutdown(&Idle);
}

#[test]
fn descriptors_come_back() {
    poller_cycle();
    worker_cycle();

    let before = open_fds();
    for _ in 0..20 {
        poller_cycle();
        worker_cycle();
    }
    let after = open_fds();

    println!("descriptors: {before} before, {after} after 20 cycles");
    assert_eq!(after, before, "descriptors went {before} -> {after}");
}
