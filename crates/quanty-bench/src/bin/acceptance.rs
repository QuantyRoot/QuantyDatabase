//! Drives the phase 5 acceptance criterion: many idle connections, a paced
//! stream of statements across a few more, and one machine-readable line at
//! the end. See docs/ACCEPTANCE.md for how to run it honestly.

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use quanty_proto::frame::HEADER_LEN;
use quanty_proto::{
    ClientHello, ClientMessage, FrameHeader, ServerHello, ServerMessage, SERVER_HELLO_LEN, VERSION,
};

struct Options {
    addr: String,
    idle: usize,
    active: usize,
    qps: u64,
    duration: Duration,
    /// What the active connections send. `{n}` becomes a number unique to
    /// each request, so a write load does not fight over one key.
    statement: String,
    /// One write in every `write_every` requests, the rest reads. Zero
    /// means the statement above is sent unchanged every time.
    ///
    /// The acceptance criterion asks for mixed traffic and does not say in
    /// what proportion. Nine reads to one write is an ordinary online
    /// transaction mix, and the ratio is printed with the result so the
    /// number stays interpretable.
    write_every: u64,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            addr: "127.0.0.1:7878".into(),
            idle: 10_000,
            active: 32,
            qps: 1000,
            duration: Duration::from_secs(1800),
            statement: "get t".to_string(),
            write_every: 0,
        }
    }
}

#[derive(Default)]
struct Counters {
    sent: AtomicU64,
    answered: AtomicU64,
    failed: AtomicU64,
    latency_sum_us: AtomicU64,
    latency_max_us: AtomicU64,
}

fn main() {
    match run() {
        Ok(()) => {}
        Err(e) => {
            eprintln!("acceptance: {e}");
            std::process::exit(1);
        }
    }
}

fn parse() -> Result<Options, String> {
    let mut o = Options::default();
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let mut value = || args.next().ok_or_else(|| format!("{flag} wants a value"));
        match flag.as_str() {
            "--addr" => o.addr = value()?,
            "--connections" => o.idle = value()?.parse().map_err(|_| "bad --connections")?,
            "--active" => o.active = value()?.parse().map_err(|_| "bad --active")?,
            "--qps" => o.qps = value()?.parse().map_err(|_| "bad --qps")?,
            "--statement" => o.statement = value()?,
            // `{n}` in the statement becomes a number unique to the sender,
            // which is how a write load avoids fighting over one key.
            "--writes" => o.statement = "put t { id: {n}, n: {n} }".to_string(),
            // Reads and writes on the same connections, which is what the
            // acceptance criterion means by mixed.
            "--mixed" => o.write_every = 10,
            "--write-every" => o.write_every = value()?.parse().map_err(|_| "bad --write-every")?,
            "--duration" => {
                let raw = value()?;
                o.duration = parse_duration(&raw)?;
            }
            "--help" => {
                println!(
                    "quanty-acceptance [--addr host:port] [--connections n] [--active n] \
                     [--qps n] [--duration 30m]"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown flag {other}")),
        }
    }
    Ok(o)
}

fn parse_duration(raw: &str) -> Result<Duration, String> {
    let (number, unit) = raw.split_at(raw.len().saturating_sub(1));
    let scale = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        _ => return Err(format!("duration wants a unit, got {raw}")),
    };
    let n: u64 = number.parse().map_err(|_| format!("bad duration {raw}"))?;
    Ok(Duration::from_secs(n * scale))
}

fn handshake(socket: &mut TcpStream) -> io::Result<()> {
    socket.set_nodelay(true)?;
    socket.write_all(&ClientHello { version: VERSION }.encode())?;
    let mut reply = [0u8; SERVER_HELLO_LEN];
    socket.read_exact(&mut reply)?;
    match ServerHello::decode(&reply) {
        Ok(ServerHello::Accepted { .. }) => {}
        Ok(ServerHello::Refused(r)) => {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionRefused,
                format!("handshake refused: {r:?}"),
            ))
        }
        Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
    }
    read_message(socket)?;
    Ok(())
}

fn read_message(socket: &mut TcpStream) -> io::Result<ServerMessage> {
    let mut head = [0u8; HEADER_LEN];
    socket.read_exact(&mut head)?;
    let header = FrameHeader::decode(&head)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    let mut body = vec![0u8; header.body_len];
    socket.read_exact(&mut body)?;
    ServerMessage::decode(header.msg_type, &body)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

/// Read until a message that ends a request, so the connection is clean for
/// the next one.
fn read_reply(socket: &mut TcpStream) -> io::Result<()> {
    for _ in 0..1_000_000 {
        if read_message(socket)?.is_terminal() {
            return Ok(());
        }
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "reply never ended",
    ))
}

fn run() -> Result<(), String> {
    let opts = parse()?;
    let addr = opts
        .addr
        .to_socket_addrs()
        .map_err(|e| format!("{}: {e}", opts.addr))?
        .next()
        .ok_or_else(|| format!("{} resolved to nothing", opts.addr))?;

    eprintln!("opening {} idle connections to {addr}", opts.idle);
    let start_open = Instant::now();
    let mut idle = Vec::with_capacity(opts.idle);
    let mut refused = 0u64;
    for _ in 0..opts.idle {
        match TcpStream::connect(addr).and_then(|mut s| handshake(&mut s).map(|_| s)) {
            Ok(s) => idle.push(s),
            Err(_) => refused += 1,
        }
    }
    eprintln!(
        "held {} of {} after {:?}, {refused} refused",
        idle.len(),
        opts.idle,
        start_open.elapsed()
    );
    if idle.is_empty() && opts.idle > 0 {
        return Err("no connection survived the handshake".into());
    }

    let counters = Arc::new(Counters::default());
    let running = Arc::new(AtomicBool::new(true));
    // The gap between one thread's requests, computed from the total rate
    // rather than from a per-thread integer rate.
    //
    // Dividing first truncates: 1000 qps across 32 threads is 31.25 each,
    // which becomes 31, and 31 times 32 is 992. The generator then quietly
    // asks for less than the run claims to be measuring, and a server that
    // answers all of it looks like it met a criterion it was never given.
    let gap = Duration::from_nanos(
        1_000_000_000u64
            .saturating_mul(opts.active.max(1) as u64)
            .checked_div(opts.qps.max(1))
            .unwrap_or(1_000_000_000),
    );

    let mut workers = Vec::with_capacity(opts.active);
    for id in 0..opts.active as u64 {
        let counters = counters.clone();
        let running = running.clone();
        let template = opts.statement.clone();
        let write_every = opts.write_every;
        workers.push(thread::spawn(move || {
            let mut serial: u64 = 0;
            let mut socket =
                match TcpStream::connect(addr).and_then(|mut s| handshake(&mut s).map(|_| s)) {
                    Ok(s) => s,
                    Err(_) => {
                        counters.failed.fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                };
            let _ = socket.set_read_timeout(Some(Duration::from_secs(10)));
            let mut next = Instant::now();
            while running.load(Ordering::Relaxed) {
                let now = Instant::now();
                if now < next {
                    thread::sleep(next - now);
                }
                next += gap;

                let sent_at = Instant::now();
                serial += 1;
                let unique = id * 10_000_000 + serial;
                let source = if write_every > 0 {
                    if serial % write_every == 0 {
                        format!("put t {{ id: {unique}, n: {unique} }}")
                    } else {
                        "get t where id = 1".to_string()
                    }
                } else if template.contains("{n}") {
                    template.replace("{n}", &format!("{unique}"))
                } else {
                    template.clone()
                };
                let request = match ClientMessage::Query(source).encode() {
                    Ok(bytes) => bytes,
                    Err(_) => break,
                };
                if socket.write_all(&request).is_err() {
                    counters.failed.fetch_add(1, Ordering::Relaxed);
                    break;
                }
                counters.sent.fetch_add(1, Ordering::Relaxed);
                match read_reply(&mut socket) {
                    Ok(()) => {
                        let us = sent_at.elapsed().as_micros() as u64;
                        counters.answered.fetch_add(1, Ordering::Relaxed);
                        counters.latency_sum_us.fetch_add(us, Ordering::Relaxed);
                        counters.latency_max_us.fetch_max(us, Ordering::Relaxed);
                    }
                    Err(_) => {
                        counters.failed.fetch_add(1, Ordering::Relaxed);
                        break;
                    }
                }
            }
        }));
    }

    let began = Instant::now();
    let mut last = 0u64;
    while began.elapsed() < opts.duration {
        thread::sleep(Duration::from_secs(5));
        let answered = counters.answered.load(Ordering::Relaxed);
        eprintln!(
            "t={:>4}s answered={answered} rate={}/s failed={}",
            began.elapsed().as_secs(),
            (answered - last) / 5,
            counters.failed.load(Ordering::Relaxed)
        );
        last = answered;
    }
    running.store(false, Ordering::Relaxed);
    for w in workers {
        let _ = w.join();
    }

    let elapsed = began.elapsed().as_secs_f64().max(0.001);
    let answered = counters.answered.load(Ordering::Relaxed);
    let mean_us = counters
        .latency_sum_us
        .load(Ordering::Relaxed)
        .checked_div(answered)
        .unwrap_or(0);
    let still_open = idle.iter().filter(|s| s.peer_addr().is_ok()).count();

    // The mix is part of the record: a rate means nothing without knowing
    // what was being run at it.
    let mix = if opts.write_every > 0 {
        format!("1-write-in-{}", opts.write_every)
    } else {
        "single-statement".to_string()
    };
    println!(
        "ACCEPTANCE idle_held={} idle_target={} idle_refused={refused} still_open={still_open} \
         answered={answered} failed={} rate={:.1} mean_us={mean_us} max_us={} seconds={:.1} \
         mix={mix}",
        idle.len(),
        opts.idle,
        counters.failed.load(Ordering::Relaxed),
        answered as f64 / elapsed,
        counters.latency_max_us.load(Ordering::Relaxed),
        elapsed
    );
    Ok(())
}
