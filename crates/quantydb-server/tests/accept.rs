//! Accept path: distribution across workers, stale tokens, descriptor accounting.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use quantydb_server::registry::Registry;
use quantydb_server::{ConnId, Dispatch, Idle, Job, Worker};

/// A dispatcher that answers on the spot.
///
/// The worker still takes the long way round, through the outbox and a
/// wakeup, so these tests exercise the asynchronous path even though
/// nothing here actually waits.
struct Answer<F>(F);

impl<F> Dispatch for Answer<F>
where
    F: Fn(&quantydb_proto::ClientMessage) -> Vec<quantydb_proto::ServerMessage>,
{
    fn submit(&self, job: Job) {
        let messages = (self.0)(&job.request);
        let _ = job.answer(messages);
    }

    fn closed(&self, _id: ConnId) {}
}

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
        w.turn(100, &Idle).expect("turn");
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
        w.turn(100, &Idle).expect("turn");
    }
    assert_eq!(w.len(), 4);

    drop(clients);

    let deadline = Instant::now() + Duration::from_secs(3);
    while !w.is_empty() && Instant::now() < deadline {
        w.turn(100, &Idle).expect("turn");
    }
    assert_eq!(w.len(), 0, "still holding {} dead connections", w.len());
}

/// Every worker must be able to accept from a shared listener, or the
/// listener is effectively single threaded.
///
/// How it gets there differs. `EPOLLEXCLUSIVE` wakes one worker per
/// connection; kqueue has no such thing and wakes all of them, so all but
/// one find nothing and get `WouldBlock`. What is asserted here is what
/// both promise: every connection is accepted exactly once, by somebody.
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
            w.turn(20, &Idle).expect("turn");
        }
    }

    let counts: Vec<usize> = workers.iter().map(|w| w.len()).collect();
    let total: usize = counts.iter().sum();
    assert_eq!(total, 64, "accepted {total} of 64, spread {counts:?}");
    // Printed, and not a measurement of anything the kernel did. The
    // workers are turned in order on this thread and `accept_all` drains
    // until it would block, so whichever one runs first takes everything
    // queued, whoever the kernel meant to wake. Linux prints [64, 0, 0, 0]
    // here. Answering how a shared listener really spreads needs workers
    // on their own threads, which this test is not.
    println!("counts after a sequential drain: {counts:?}");

    drop(clients);
    for w in workers.iter_mut() {
        w.shutdown(&Idle);
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
        let idle = Idle;
        w.run(&idle).expect("run")
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
fn reuseport_spreads_where_a_shared_listener_does_not() {
    use quantydb_server::bind_reuseport;

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

    // Connect in chunks and let the workers drain between them.
    //
    // Connecting all two hundred first and accepting afterwards asks the
    // kernel to queue two hundred pending connections. Linux honours the
    // backlog this listener asked for; Darwin clamps `listen` to
    // `kern.ipc.somaxconn`, 128 by default, and drops the rest, which the
    // client sees as a connect that times out rather than as a refusal.
    // The macOS CI job is what found that, after the test had implied a
    // deep backlog for as long as it had existed.
    //
    // The measurement is unchanged: the kernel picks the listening socket
    // when the SYN arrives, so accepting sooner does not move a
    // connection to a different worker.
    const CHUNK: usize = 50;
    let mut clients: Vec<TcpStream> = Vec::with_capacity(200);
    for _ in 0..4 {
        for _ in 0..CHUNK {
            clients.push(TcpStream::connect(addr).expect("connect"));
        }
        for w in workers.iter_mut() {
            w.turn(20, &Idle).expect("turn");
        }
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let total: usize = workers.iter().map(|w| w.len()).sum();
        if total >= 200 || Instant::now() >= deadline {
            break;
        }
        for w in workers.iter_mut() {
            w.turn(20, &Idle).expect("turn");
        }
    }

    let counts: Vec<usize> = workers.iter().map(|w| w.len()).collect();
    let total: usize = counts.iter().sum();
    assert_eq!(total, 200, "accepted {total} of 200, spread {counts:?}");
    println!("reuseport spread: {counts:?}");

    // The spread is a Linux promise, not a POSIX one. Darwin has the
    // option and gives it different delivery, so asserting the Linux
    // property there would be asserting something no kernel promised.
    // The count is printed on every platform so the macOS job answers the
    // question instead of this comment guessing at it. ADR-038.
    let worst = counts.iter().copied().max().expect("counts");
    if cfg!(target_os = "linux") {
        assert!(
            worst * 2 <= total,
            "one worker took {worst} of {total}, which is not a spread: {counts:?}"
        );
    }

    drop(clients);
    for w in workers.iter_mut() {
        w.shutdown(&Idle);
    }
}

#[test]
fn a_whole_request_crosses_a_real_socket() {
    use quantydb_proto::{ClientHello, ClientMessage, ServerMessage, VERSION};
    use std::io::Read;

    let svc = Answer(|r: &ClientMessage| match r {
        ClientMessage::Query(s) => vec![ServerMessage::Lines(vec![s.clone()])],
        _ => vec![ServerMessage::Ok],
    });

    let (listener, addr) = shared_listener();
    let flag = Arc::new(AtomicBool::new(true));
    let mut w = Worker::new(listener, flag).expect("worker");

    let mut client = TcpStream::connect(addr).expect("connect");
    client
        .set_read_timeout(Some(Duration::from_millis(200)))
        .expect("timeout");

    let mut request = ClientHello { version: VERSION }.encode().to_vec();
    request.extend_from_slice(&ClientMessage::Query("get users".into()).encode().unwrap());
    client.write_all(&request).expect("write");

    let mut got = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while got.len() < 4 + 5 + 5 + 4 + 9 && Instant::now() < deadline {
        w.turn(20, &svc).expect("turn");
        let mut buf = [0u8; 256];
        match client.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(_) => {}
        }
    }

    assert!(got.len() >= 4, "no handshake reply: {got:?}");
    assert_eq!(got[0], 0x01, "handshake refused");
    assert!(
        got.windows(9).any(|w| w == b"get users"),
        "the answer never came back: {} bytes",
        got.len()
    );
}

/// A drained output buffer must not stay registered for writability, or the
/// loop spins at full speed on a connection with nothing to say.
#[test]
fn a_finished_reply_stops_asking_to_write() {
    use quantydb_proto::{ClientHello, ClientMessage, ServerMessage, VERSION};

    let svc = Answer(|_: &ClientMessage| {
        vec![ServerMessage::Lines(
            (0..2000).map(|i| format!("l{i}")).collect(),
        )]
    });

    let (listener, addr) = shared_listener();
    let flag = Arc::new(AtomicBool::new(true));
    let mut w = Worker::new(listener, flag).expect("worker");

    let mut client = TcpStream::connect(addr).expect("connect");
    let mut request = ClientHello { version: VERSION }.encode().to_vec();
    request.extend_from_slice(&ClientMessage::Query("big".into()).encode().unwrap());
    client.write_all(&request).expect("write");

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let turn = w.turn(20, &svc).expect("turn");
        if turn.ready == 0 && turn.accepted == 0 && turn.answered == 0 {
            break;
        }
        let mut buf = [0u8; 4096];
        use std::io::Read;
        client
            .set_read_timeout(Some(Duration::from_millis(20)))
            .ok();
        let _ = client.read(&mut buf);
    }

    let start = Instant::now();
    w.turn(200, &svc).expect("turn");
    assert!(
        start.elapsed() >= Duration::from_millis(150),
        "the loop returned immediately, so something is still registered for \
         writing with nothing to write"
    );
}

/// Does a shared listener spread across workers that are running at the
/// same time?
///
/// Its neighbour above cannot answer that. It turns four workers in order
/// on this thread and `accept_all` drains until it would block, so the
/// first one takes the whole queue whoever the kernel woke: those counts
/// measure the loop. This one puts each worker on its own thread, which
/// is the only shape in which the question means anything.
///
/// Nothing about the spread is asserted, on either kernel. Linux wakes one
/// worker per connection with `EPOLLEXCLUSIVE`, and ADR-025 measured that
/// it favours whoever sits first on the wait queue. kqueue has no
/// exclusive wakeup and wakes all of them, so all but one get
/// `WouldBlock`. Neither promises a distribution. What both do promise is
/// asserted: every connection is accepted exactly once, by somebody, with
/// four threads racing for it. The counts are printed for ADR-038.
#[test]
fn a_shared_listener_across_workers_that_are_running() {
    const WORKERS: usize = 4;
    const CHUNK: usize = 50;
    const TOTAL: usize = 200;

    let (listener, addr) = shared_listener();
    let flag = Arc::new(AtomicBool::new(true));
    let counts: Vec<Arc<AtomicUsize>> = (0..WORKERS)
        .map(|_| Arc::new(AtomicUsize::new(0)))
        .collect();

    let mut threads = Vec::new();
    for count in &counts {
        let mut w = Worker::new(listener.clone(), flag.clone()).expect("worker");
        let count = count.clone();
        let flag = flag.clone();
        threads.push(thread::spawn(move || {
            while flag.load(Ordering::Relaxed) {
                let turn = w.turn(20, &Idle).expect("turn");
                if turn.accepted > 0 {
                    count.fetch_add(turn.accepted, Ordering::Relaxed);
                }
            }
            w.shutdown(&Idle);
        }));
    }

    let accepted = || {
        counts
            .iter()
            .map(|c| c.load(Ordering::Relaxed))
            .sum::<usize>()
    };

    // Paced by the workers rather than by a sleep: open a chunk, wait for
    // it to be taken, open the next. Darwin clamps the listen backlog to
    // `kern.ipc.somaxconn`, so two hundred outstanding at once would be
    // dropped rather than queued, which is what the test above learned.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut clients: Vec<TcpStream> = Vec::with_capacity(TOTAL);
    while clients.len() < TOTAL {
        for _ in 0..CHUNK {
            clients.push(TcpStream::connect(addr).expect("connect"));
        }
        let want = clients.len();
        while accepted() < want && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
    }

    flag.store(false, Ordering::Relaxed);
    for t in threads {
        t.join().expect("worker thread");
    }

    let spread: Vec<usize> = counts.iter().map(|c| c.load(Ordering::Relaxed)).collect();
    let total: usize = spread.iter().sum();
    println!("shared listener across running workers: {spread:?}");
    assert_eq!(
        total, TOTAL,
        "accepted {total} of {TOTAL}, spread {spread:?}"
    );
}
