//! One event loop thread: accepts from the shared listener, owns what it accepts.

use std::io;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::poll::{Event, Interest, Poller, Token, Waker};
use crate::registry::Registry;

/// Token the shared listener is registered under.
const LISTENER: Token = Token(u64::MAX - 1);

/// How many readiness events one turn of the loop may report.
const EVENTS_PER_TURN: usize = 1024;

/// What a worker did on one turn, for tests and for the acceptance harness.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Turn {
    /// Connections accepted.
    pub accepted: usize,
    /// Connections closed, by peer or by error.
    pub closed: usize,
    /// Readiness events dispatched to live connections.
    pub ready: usize,
    /// Events dropped because their token named a reused slot.
    pub stale: usize,
}

impl Turn {
    fn add(&mut self, other: Turn) {
        self.accepted += other.accepted;
        self.closed += other.closed;
        self.ready += other.ready;
        self.stale += other.stale;
    }
}

/// What a worker does with a connection that has something to say.
pub trait Handler {
    /// Called for each readiness event. Returning false closes the connection.
    fn ready(&mut self, conn: &mut TcpStream, event: Event) -> bool;
}

/// A handler that accepts connections and never reads them.
///
/// What the 10k idle half of the acceptance criterion needs, and nothing more.
pub struct Idle;

impl Handler for Idle {
    fn ready(&mut self, _conn: &mut TcpStream, event: Event) -> bool {
        !event.is_error() && !event.is_read_closed()
    }
}

/// One event loop and the connections it owns.
pub struct Worker {
    poller: Poller,
    listener: Arc<TcpListener>,
    conns: Registry,
    running: Arc<AtomicBool>,
}

impl Worker {
    /// Build a worker over a listener shared with its peers.
    pub fn new(listener: Arc<TcpListener>, running: Arc<AtomicBool>) -> io::Result<Self> {
        Self::build(listener, running, true)
    }

    /// Build a worker over a listener only it accepts from.
    pub fn owning(listener: TcpListener, running: Arc<AtomicBool>) -> io::Result<Self> {
        Self::build(Arc::new(listener), running, false)
    }

    fn build(
        listener: Arc<TcpListener>,
        running: Arc<AtomicBool>,
        shared: bool,
    ) -> io::Result<Self> {
        let poller = Poller::new(EVENTS_PER_TURN)?;
        poller.register_listener(&*listener, LISTENER, shared)?;
        Ok(Worker {
            poller,
            listener,
            conns: Registry::with_capacity(1024),
            running,
        })
    }

    /// A handle for waking this worker from another thread.
    pub fn waker(&self) -> Waker {
        self.poller.waker()
    }

    /// Connections currently held.
    pub fn len(&self) -> usize {
        self.conns.len()
    }

    /// Whether this worker holds nothing.
    pub fn is_empty(&self) -> bool {
        self.conns.is_empty()
    }

    /// Run one turn of the loop.
    pub fn turn(&mut self, timeout_ms: i32, handler: &mut impl Handler) -> io::Result<Turn> {
        let mut events = Vec::new();
        self.poller.poll(timeout_ms, |e| events.push(e))?;

        let mut turn = Turn::default();
        for event in events {
            if event.token == LISTENER {
                turn.add(self.accept_all()?);
            } else {
                turn.add(self.dispatch(event, handler));
            }
        }
        Ok(turn)
    }

    /// Run until the shared flag goes false.
    pub fn run(&mut self, handler: &mut impl Handler) -> io::Result<Turn> {
        let mut total = Turn::default();
        while self.running.load(Ordering::Relaxed) {
            total.add(self.turn(100, handler)?);
        }
        Ok(total)
    }

    fn accept_all(&mut self) -> io::Result<Turn> {
        let mut turn = Turn::default();
        loop {
            match self.listener.accept() {
                Ok((socket, _)) => {
                    socket.set_nonblocking(true)?;
                    socket.set_nodelay(true)?;
                    let token = self.conns.insert(socket);
                    let conn = self.conns.get_mut(token).expect("just inserted");
                    self.poller
                        .register(&conn.socket, token, Interest::READABLE)?;
                    turn.accepted += 1;
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(turn),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if is_transient(&e) => continue,
                Err(e) => return Err(e),
            }
        }
    }

    fn dispatch(&mut self, event: Event, handler: &mut impl Handler) -> Turn {
        let mut turn = Turn::default();
        let Some(conn) = self.conns.get_mut(event.token) else {
            turn.stale += 1;
            return turn;
        };
        turn.ready += 1;
        let keep = handler.ready(&mut conn.socket, event);
        if !keep {
            self.close(event.token);
            turn.closed += 1;
        }
        turn
    }

    fn close(&mut self, token: Token) {
        if let Some(conn) = self.conns.remove(token) {
            let _ = self.poller.deregister(&conn.socket);
        }
    }

    /// Close every connection this worker owns.
    pub fn shutdown(&mut self) {
        let tokens: Vec<Token> = self.conns.iter_mut().map(|c| c.token).collect();
        for t in tokens {
            self.close(t);
        }
    }
}

/// Errors that describe one connection, not the listener.
///
/// A peer that resets between the kernel queueing it and us accepting it must
/// not take the loop down with it.
fn is_transient(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionAborted | io::ErrorKind::ConnectionReset | io::ErrorKind::TimedOut
    )
}
