//! Accept path: distribution across workers, stale tokens, descriptor accounting.

#![cfg(target_os = "linux")]

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use quanty_server::registry::Registry;
use quanty_server::{Idle, Worker};

fn shared_listener() -> (Arc<TcpListener>, std::net::SocketAddr) {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind");
    l.set_nonblocking(true).expect("nonblocking");
    let addr = l.local_addr().expect("addr");
    (Arc::new(l), addr)
}

#[test]
fn a_worker_accepts_and_holds() {
    let (listener, addr) = shared_listener();
    let flag = Arc::new(AtomicBool::new(true));
    let mut w = Worker::new(listener, flag).expect("worker");

    let clients: Vec<TcpStream> = (0..8)
        .map(|_| TcpStream::connect(addr).expect("connect"))
        .collect();

    let deadline = Instant::now() + Duration::from_secs(3);
    while w.len() < 8 && Instant::now() < deadline {
        w.turn(100, &mut Idle).expect("turn");
    }

    assert_eq!(w.len(), 8, "accepted {} of 8", w.len());
    drop(clients);
}

#[test]
fn a_closed_peer_is_dropped_not_held() {
    let (listener, addr) = shared_listener();
    let flag = Arc::new(AtomicBool::new(true));
    let mut w = Worker::new(listener, flag).expect("worker");

    let clients: Vec<TcpStream> = (0..4)
        .map(|_| TcpStream::connect(addr).expect("connect"))
        .collect();
    let deadline = Instant::now() + Duration::from_secs(3);
    while w.len() < 4 && Instant::now() < deadline {
        w.turn(100, &mut Idle).expect("turn");
    }
    assert_eq!(w.len(), 4);

    drop(clients);

    let deadline = Instant::now() + Duration::from_secs(3);
    while !w.is_empty() && Instant::now() < deadline {
        w.turn(100, &mut Idle).expect("turn");
    }
    assert_eq!(w.len(), 0, "still holding {} dead connections", w.len());
}

/// EPOLLEXCLUSIVE wakes one worker per connection. Every worker must still
/// be able to accept, or the listener is effectively single threaded.
#[test]
fn several_workers_share_one_listener() {
    let (listener, addr) = shared_listener();
    let flag = Arc::new(AtomicBool::new(true));

    let mut workers: Vec<Worker> = (0..4)
        .map(|_| Worker::new(listener.clone(), flag.clone()).expect("worker"))
        .collect();

    let clients: Vec<TcpStream> = (0..64)
        .map(|_| TcpStream::connect(addr).expect("connect"))
        .collect();

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let total: usize = workers.iter().map(|w| w.len()).sum();
        if total >= 64 || Instant::now() >= deadline {
            break;
        }
        for w in workers.iter_mut() {
            w.turn(20, &mut Idle).expect("turn");
        }
    }

    let counts: Vec<usize> = workers.iter().map(|w| w.len()).collect();
    let total: usize = counts.iter().sum();
    assert_eq!(total, 64, "accepted {total} of 64, spread {counts:?}");
    println!("spread across workers: {counts:?}");

    drop(clients);
    for w in workers.iter_mut() {
        w.shutdown();
    }
}

/// The reason tokens carry a generation. A stale token must find nothing
/// rather than the connection that took over its slot.
#[test]
fn a_reused_slot_does_not_answer_for_its_predecessor() {
    let (listener, addr) = shared_listener();
    let mut reg = Registry::with_capacity(4);

    let _c1 = TcpStream::connect(addr).expect("connect");
    let (s1, _) = accept(&listener);
    let first = reg.insert(s1);
    assert!(reg.get_mut(first).is_some());

    reg.remove(first).expect("removed");
    assert!(reg.get_mut(first).is_none(), "stale token still resolves");

    let _c2 = TcpStream::connect(addr).expect("connect");
    let (s2, _) = accept(&listener);
    let second = reg.insert(s2);

    assert_eq!(second.index(), first.index(), "slot should be reused");
    assert_ne!(second, first, "reused slot must get a new token");
    assert!(
        reg.get_mut(first).is_none(),
        "old token reached the new connection"
    );
    assert!(reg.get_mut(second).is_some());
}

#[test]
fn a_worker_stops_when_the_flag_drops() {
    let (listener, addr) = shared_listener();
    let flag = Arc::new(AtomicBool::new(true));
    let mut w = Worker::new(listener, flag.clone()).expect("worker");
    let waker = w.waker();

    let h = thread::spawn(move || {
        let mut idle = Idle;
        w.run(&mut idle).expect("run")
    });

    let _c = TcpStream::connect(addr).expect("connect");
    thread::sleep(Duration::from_millis(150));

    flag.store(false, Ordering::Relaxed);
    waker.wake().expect("wake");

    let start = Instant::now();
    let total = h.join().expect("join");
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "worker did not stop promptly"
    );
    assert!(total.accepted >= 1, "never accepted: {total:?}");
}

fn accept(l: &TcpListener) -> (TcpStream, std::net::SocketAddr) {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match l.accept() {
            Ok(pair) => return pair,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "accept timed out");
                thread::sleep(Duration::from_millis(5));
            }
            Err(e) => panic!("accept: {e}"),
        }
    }
}

#[test]
fn a_handler_sees_what_the_peer_sent() {
    struct Echo(Vec<u8>);
    impl quanty_server::Handler for Echo {
        fn ready(&mut self, conn: &mut TcpStream, event: quanty_server::Event) -> bool {
            use std::io::Read;
            if event.is_error() {
                return false;
            }
            let mut buf = [0u8; 64];
            match conn.read(&mut buf) {
                Ok(0) => false,
                Ok(n) => {
                    self.0.extend_from_slice(&buf[..n]);
                    true
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => true,
                Err(_) => false,
            }
        }
    }

    let (listener, addr) = shared_listener();
    let flag = Arc::new(AtomicBool::new(true));
    let mut w = Worker::new(listener, flag).expect("worker");
    let mut echo = Echo(Vec::new());

    let mut c = TcpStream::connect(addr).expect("connect");
    let deadline = Instant::now() + Duration::from_secs(3);
    while w.is_empty() && Instant::now() < deadline {
        w.turn(50, &mut echo).expect("turn");
    }
    c.write_all(b"hello").expect("write");

    let deadline = Instant::now() + Duration::from_secs(3);
    while echo.0.len() < 5 && Instant::now() < deadline {
        w.turn(50, &mut echo).expect("turn");
    }
    assert_eq!(&echo.0, b"hello");
}

#[test]
fn reuseport_spreads_where_a_shared_listener_does_not() {
    use quanty_server::bind_reuseport;

    let probe = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = probe.local_addr().expect("addr");
    drop(probe);

    let flag = Arc::new(AtomicBool::new(true));
    let mut workers: Vec<Worker> = (0..4)
        .map(|_| {
            let l = bind_reuseport(addr).expect("reuseport");
            l.set_nonblocking(true).expect("nonblocking");
            Worker::owning(l, flag.clone()).expect("worker")
        })
        .collect();

    let clients: Vec<TcpStream> = (0..200)
        .map(|_| TcpStream::connect(addr).expect("connect"))
        .collect();

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let total: usize = workers.iter().map(|w| w.len()).sum();
        if total >= 200 || Instant::now() >= deadline {
            break;
        }
        for w in workers.iter_mut() {
            w.turn(20, &mut Idle).expect("turn");
        }
    }

    let counts: Vec<usize> = workers.iter().map(|w| w.len()).collect();
    let total: usize = counts.iter().sum();
    assert_eq!(total, 200, "accepted {total} of 200, spread {counts:?}");
    println!("reuseport spread: {counts:?}");

    let worst = counts.iter().copied().max().expect("counts");
    assert!(
        worst * 2 <= total,
        "one worker took {worst} of {total}, which is not a spread: {counts:?}"
    );

    drop(clients);
    for w in workers.iter_mut() {
        w.shutdown();
    }
}
