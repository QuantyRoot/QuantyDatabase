//! One event loop thread: accepts from its listener, owns what it accepts.

use std::io;
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::conn::Step;
use crate::dispatch::{Dispatch, Outbox, Postbox};
use crate::poll::{Event, Interest, Poller, Token, Waker, WAKE_TOKEN};
use crate::registry::Registry;

/// Token the listener is registered under.
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
    /// Requests handed to the dispatcher.
    pub submitted: usize,
    /// Answers delivered back to connections.
    pub answered: usize,
}

impl Turn {
    fn add(&mut self, other: Turn) {
        self.accepted += other.accepted;
        self.closed += other.closed;
        self.ready += other.ready;
        self.stale += other.stale;
        self.submitted += other.submitted;
        self.answered += other.answered;
    }
}

/// One event loop and the connections it owns.
pub struct Worker {
    poller: Poller,
    listener: Arc<TcpListener>,
    conns: Registry,
    running: Arc<AtomicBool>,
    outbox: Arc<Outbox>,
    postbox: Postbox,
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
        let outbox = Outbox::new(poller.waker());
        Ok(Worker {
            poller,
            listener,
            conns: Registry::with_capacity(1024),
            running,
            postbox: Postbox::new(Arc::clone(&outbox)),
            outbox,
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
    pub fn turn(&mut self, timeout_ms: i32, dispatch: &impl Dispatch) -> io::Result<Turn> {
        let mut events = Vec::new();
        self.poller.poll(timeout_ms, |e| events.push(e))?;

        let mut turn = Turn::default();
        for event in events {
            if event.token == LISTENER {
                turn.add(self.accept_all()?);
            } else if event.token == WAKE_TOKEN {
                turn.add(self.deliver(dispatch));
            } else {
                turn.add(self.on_ready(event, dispatch));
            }
        }
        Ok(turn)
    }

    /// Run until the shared flag goes false.
    pub fn run(&mut self, dispatch: &impl Dispatch) -> io::Result<Turn> {
        let mut total = Turn::default();
        while self.running.load(Ordering::Relaxed) {
            total.add(self.turn(100, dispatch)?);
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

    /// Hand every answer the executor left to the connection it belongs to.
    fn deliver(&mut self, dispatch: &impl Dispatch) -> Turn {
        let mut turn = Turn::default();
        for reply in self.outbox.take() {
            match self.conns.get_mut(reply.token) {
                // The token carries a generation, so a slot handed out
                // again since the request was submitted is caught here and
                // the answer dropped rather than sent to a stranger.
                Some(conn) if conn.id == reply.id => conn.state.resume(reply.messages),
                _ => {
                    turn.stale += 1;
                    continue;
                }
            }
            turn.answered += 1;
            turn.add(self.drive(reply.token, dispatch));
        }
        turn
    }

    fn on_ready(&mut self, event: Event, dispatch: &impl Dispatch) -> Turn {
        let mut turn = Turn::default();
        let Some(conn) = self.conns.get_mut(event.token) else {
            turn.stale += 1;
            return turn;
        };
        turn.ready += 1;

        let mut broken = event.is_error();
        if !broken && event.is_readable() {
            match conn.state.fill(&mut conn.socket) {
                Ok(true) => {}
                Ok(false) => conn.state.peer_done(),
                Err(_) => broken = true,
            }
        }
        if broken {
            self.close(event.token, dispatch);
            turn.closed += 1;
            return turn;
        }
        turn.add(self.drive(event.token, dispatch));
        turn
    }

    /// Push one connection as far as it will go, then fix up its interest.
    fn drive(&mut self, token: Token, dispatch: &impl Dispatch) -> Turn {
        let mut turn = Turn::default();
        let poller = &self.poller;
        let postbox = &self.postbox;
        let Some(conn) = self.conns.get_mut(token) else {
            turn.stale += 1;
            return turn;
        };

        let mut finished = false;
        while !finished {
            let before = conn.state.buffered();
            match conn.state.step() {
                Step::Closed => finished = true,
                Step::Submit(request) => {
                    dispatch.submit(postbox.job(conn.id, token, request));
                    turn.submitted += 1;
                }
                Step::Open => {}
            }
            if conn.state.flush(&mut conn.socket).is_err() {
                finished = true;
            }
            if conn.state.is_finished() {
                finished = true;
            }
            if finished
                || conn.state.is_awaiting()
                || conn.state.wants_write()
                || conn.state.buffered() == before
            {
                break;
            }
        }

        if !finished {
            let wanted = if conn.state.wants_write() {
                Interest::both()
            } else {
                Interest::READABLE
            };
            if wanted != conn.interest {
                match poller.reregister(&conn.socket, token, wanted) {
                    Ok(()) => conn.interest = wanted,
                    Err(_) => finished = true,
                }
            }
        }

        if finished {
            self.close(token, dispatch);
            turn.closed += 1;
        }
        turn
    }

    fn close(&mut self, token: Token, dispatch: &impl Dispatch) {
        if let Some(conn) = self.conns.remove(token) {
            let _ = self.poller.deregister(&conn.socket);
            dispatch.closed(conn.id);
        }
    }

    /// Close every connection this worker owns.
    pub fn shutdown(&mut self, dispatch: &impl Dispatch) {
        let tokens: Vec<Token> = self.conns.iter_mut().map(|c| c.token).collect();
        for t in tokens {
            self.close(t, dispatch);
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
