//! Where a worker sends work it will not do itself, and how the answer
//! finds its way back.
//!
//! ADR-024 parks a connection rather than blocking its worker. Two things
//! have to travel for that: the request, which goes out through `Dispatch`,
//! and the answer, which comes back through an `Outbox` and a wakeup. This
//! module holds both and knows nothing about what answers them, so the
//! reactor keeps compiling without an execution engine behind it.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use quanty_proto::{ClientMessage, ServerMessage};

use crate::poll::{Token, Waker};

/// Names one connection for as long as it is open, across every worker.
///
/// Distinct from `Token`, which names a slot in one worker's registry and
/// is handed out again after the connection in it closes. Whatever holds
/// per-connection state needs the name that is never reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnId(u64);

impl ConnId {
    /// The next unused name.
    pub fn next() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        ConnId(NEXT.fetch_add(1, Ordering::Relaxed))
    }

    /// The number behind the name, for logs and tests.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// A request and the way back to the connection that asked it.
///
/// Holding a job is how the executor makes a connection wait: it can sit in
/// a queue for as long as ADR-024's deadlines allow before being answered.
pub struct Job {
    /// Which connection asked.
    pub id: ConnId,
    /// What it asked.
    pub request: ClientMessage,
    back: Arc<Outbox>,
    token: Token,
}

impl Job {
    /// Answer the request and wake the worker that owns the connection.
    ///
    /// Fails only if the wakeup itself fails; a connection that closed in
    /// the meantime is not an error, its answer is dropped on arrival.
    pub fn answer(self, messages: Vec<ServerMessage>) -> io::Result<()> {
        self.back.push(Reply {
            token: self.token,
            id: self.id,
            messages,
        })
    }
}

/// One answer on its way back to a worker.
pub struct Reply {
    /// The slot to deliver to, checked against reuse on arrival.
    pub token: Token,
    /// The connection the answer was computed for.
    pub id: ConnId,
    /// What to send.
    pub messages: Vec<ServerMessage>,
}

/// Answers waiting for a worker to pick them up.
pub struct Outbox {
    queue: Mutex<Vec<Reply>>,
    waker: Waker,
}

impl Outbox {
    /// An outbox that wakes the worker behind `waker`.
    pub fn new(waker: Waker) -> Arc<Self> {
        Arc::new(Outbox {
            queue: Mutex::new(Vec::new()),
            waker,
        })
    }

    fn push(&self, reply: Reply) -> io::Result<()> {
        self.lock().push(reply);
        self.waker.wake()
    }

    /// Take everything queued.
    pub fn take(&self) -> Vec<Reply> {
        std::mem::take(&mut *self.lock())
    }

    /// A poisoned queue is taken anyway. The panic that poisoned it cannot
    /// have left a half-written `Reply` here, because pushing one is a move
    /// into a `Vec` and nothing in between can fail.
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<Reply>> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Whoever answers requests.
pub trait Dispatch {
    /// Hand off one request. The answer comes back through its outbox.
    fn submit(&self, job: Job);

    /// Note that a connection is gone, so its state can be dropped and any
    /// transaction it held rolled back.
    fn closed(&self, id: ConnId);
}

/// Builds jobs for one worker's connections.
pub(crate) struct Postbox {
    outbox: Arc<Outbox>,
}

impl Postbox {
    pub(crate) fn new(outbox: Arc<Outbox>) -> Self {
        Postbox { outbox }
    }

    pub(crate) fn job(&self, id: ConnId, token: Token, request: ClientMessage) -> Job {
        Job {
            id,
            request,
            back: Arc::clone(&self.outbox),
            token,
        }
    }
}

/// A dispatcher that answers nothing.
///
/// What the idle half of the acceptance criterion needs: connections are
/// accepted and held, and no statement is ever answered.
pub struct Idle;

impl Dispatch for Idle {
    fn submit(&self, _job: Job) {}

    fn closed(&self, _id: ConnId) {}
}
