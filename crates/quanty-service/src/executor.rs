//! The thread that owns the session, and the queue in front of it.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use quanty_core::Storage;
use quanty_exec::{Parked, Session};
use quanty_proto::ErrorCode;
use quanty_server::{ConnId, Dispatch, Job};

use crate::answer::{answer, failed};

/// How long a statement waits for the writer before it is refused.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a connection may hold an open transaction in silence.
///
/// Short, because in this first throw an open transaction stalls every
/// other connection, reads included. Postgres can afford to leave its
/// equivalent off by default; a server where one idle `begin` is a
/// server-wide stall cannot.
pub const IDLE_IN_TXN: Duration = Duration::from_secs(10);

/// How often the loop wakes on its own to check the deadlines.
const TICK: Duration = Duration::from_millis(50);

/// The two deadlines of ADR-024.
#[derive(Debug, Clone, Copy)]
pub struct Deadlines {
    /// Waiting for the writer. Expiring means `0x0007` and a retry.
    pub busy: Duration,
    /// Holding an open transaction without sending. Expiring rolls it back.
    pub idle_in_txn: Duration,
}

impl Default for Deadlines {
    fn default() -> Self {
        Deadlines {
            busy: BUSY_TIMEOUT,
            idle_in_txn: IDLE_IN_TXN,
        }
    }
}

/// A running executor thread.
///
/// Dropping it asks the thread to stop and waits for it, so a statement
/// already inside a commit finishes rather than being abandoned.
pub struct Executor {
    tx: Option<Sender<Work>>,
    thread: Option<JoinHandle<()>>,
}

impl Executor {
    /// Start a thread that owns `session` and answers what is sent to it.
    pub fn spawn<S>(session: Session<S>, deadlines: Deadlines) -> Executor
    where
        S: Storage + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        let thread = thread::spawn(move || State::new(session, deadlines).run(rx));
        Executor {
            tx: Some(tx),
            thread: Some(thread),
        }
    }

    /// A handle workers can submit to. Cheap to clone, one per worker.
    pub fn handle(&self) -> Handle {
        Handle {
            tx: self.tx.clone().expect("sender is dropped only in Drop"),
        }
    }
}

impl Drop for Executor {
    fn drop(&mut self) {
        // Stop is sent rather than relying on the last sender going away:
        // a worker that still holds a handle would otherwise keep the loop
        // alive and turn this join into a hang.
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Work::Stop);
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// What a worker sends to the executor.
#[derive(Clone)]
pub struct Handle {
    tx: Sender<Work>,
}

impl Dispatch for Handle {
    fn submit(&self, job: Job) {
        if let Err(e) = self.tx.send(Work::Request(job)) {
            // The executor is gone. Say so rather than leaving a
            // connection parked on an answer that will never come.
            let Work::Request(job) = e.0 else {
                return;
            };
            let _ = job.answer(failed(
                ErrorCode::ShuttingDown,
                "the server is no longer executing statements",
            ));
        }
    }

    fn closed(&self, id: ConnId) {
        let _ = self.tx.send(Work::Closed(id));
    }
}

enum Work {
    Request(Job),
    Closed(ConnId),
    Stop,
}

/// What the executor keeps for one connection.
#[derive(Default)]
struct ConnState {
    /// Its transaction while another connection is running.
    parked: Parked,
    /// Why its transaction is gone, to be reported at its next statement.
    aborted: Option<&'static str>,
}

struct State<S: Storage> {
    session: Session<S>,
    deadlines: Deadlines,
    conns: HashMap<ConnId, ConnState>,
    /// The connection whose transaction is open, and when it last spoke.
    holder: Option<(ConnId, Instant)>,
    /// Statements waiting for the holder to finish, oldest first.
    queue: Vec<(Instant, Job)>,
}

impl<S: Storage> State<S> {
    fn new(session: Session<S>, deadlines: Deadlines) -> Self {
        State {
            session,
            deadlines,
            conns: HashMap::new(),
            holder: None,
            queue: Vec::new(),
        }
    }

    fn run(mut self, rx: Receiver<Work>) {
        loop {
            match rx.recv_timeout(TICK) {
                Ok(Work::Request(job)) => self.accept(job),
                Ok(Work::Closed(id)) => self.forget(id),
                Ok(Work::Stop) => break,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            self.sweep();
            self.drain();
        }
        // Statements still queued are told so instead of being dropped on
        // the floor. Transactions still parked are rolled back by going
        // out of scope, which ADR-021 makes free.
        for (_, job) in std::mem::take(&mut self.queue) {
            let _ = job.answer(failed(
                ErrorCode::ShuttingDown,
                "the server stopped before this statement ran",
            ));
        }
    }

    /// Run a statement now, or put it behind the open transaction.
    fn accept(&mut self, job: Job) {
        match self.holder {
            Some((holder, _)) if holder != job.id => self.queue.push((Instant::now(), job)),
            _ => self.execute(job),
        }
    }

    fn execute(&mut self, job: Job) {
        let id = job.id;
        let state = self.conns.entry(id).or_default();

        // A transaction rolled back under this connection is reported once,
        // at its next statement, rather than pushed at a client that is not
        // expecting anything. docs/PROTOCOL.md allows one reply per
        // request and nothing else.
        if let Some(reason) = state.aborted.take() {
            let _ = job.answer(failed(ErrorCode::Execution, reason));
            return;
        }

        let parked = std::mem::take(&mut state.parked);
        self.session.unpark(parked);
        let messages = answer(&mut self.session, &job.request);
        let parked = self.session.park();

        let open = parked.is_open();
        self.conns.entry(id).or_default().parked = parked;
        self.holder = if open {
            Some((id, Instant::now()))
        } else {
            None
        };

        let _ = job.answer(messages);
    }

    /// Everything that can run now, in arrival order.
    fn drain(&mut self) {
        while self.holder.is_none() && !self.queue.is_empty() {
            let (_, job) = self.queue.remove(0);
            self.execute(job);
        }
    }

    /// Enforce both deadlines.
    fn sweep(&mut self) {
        let now = Instant::now();

        if let Some((id, since)) = self.holder {
            if now.duration_since(since) > self.deadlines.idle_in_txn {
                // Dropping the parked transaction is the rollback.
                let state = self.conns.entry(id).or_default();
                state.parked = Parked::default();
                state.aborted = Some(
                    "the open transaction was rolled back after the \
                     idle-in-transaction deadline expired",
                );
                self.holder = None;
            }
        }

        if self.holder.is_none() {
            return;
        }
        let busy = self.deadlines.busy;
        let (keep, expired): (Vec<_>, Vec<_>) = std::mem::take(&mut self.queue)
            .into_iter()
            .partition(|(since, _)| now.duration_since(*since) <= busy);
        self.queue = keep;
        for (_, job) in expired {
            let _ = job.answer(failed(
                ErrorCode::WriteQueue,
                "timed out waiting for the writer; the statement did not run",
            ));
        }
    }

    /// Drop everything held for a connection that is gone.
    fn forget(&mut self, id: ConnId) {
        self.conns.remove(&id);
        self.queue.retain(|(_, job)| job.id != id);
        if matches!(self.holder, Some((holder, _)) if holder == id) {
            self.holder = None;
        }
    }
}
