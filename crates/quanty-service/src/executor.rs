//! The thread that owns the session, and the queue in front of it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use quanty_core::Storage;
use quanty_exec::{Parked, Session};
use quanty_proto::{ErrorCode, ServerMessage};
use quanty_ql::ast::Statement;
use quanty_server::{ConnId, Dispatch, Job};

use crate::answer::{answer, failed, parse, ready, Kind, Parsed};

/// How long a statement waits for the writer before it is refused.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a connection may hold an open transaction in silence.
///
/// Short, because in this first throw an open transaction stalls every
/// other connection, reads included. Postgres can afford to leave its
/// equivalent off by default; a server where one idle `begin` is a
/// server-wide stall cannot.
pub const IDLE_IN_TXN: Duration = Duration::from_secs(10);

/// How many statements may share one commit.
///
/// ADR-028 measured the curve: most of the win is there by a depth of 64
/// and it is flat past a few hundred. The cap exists so a burst cannot
/// build a transaction large enough to matter in memory.
pub const MAX_BATCH: usize = 256;

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

/// What the executor has done, for tests and for whoever is measuring.
///
/// Batching is invisible from outside: the same statements produce the same
/// answers whether they shared a commit or not. Without a count, a test
/// that means to exercise group commit passes just as happily when no
/// batch ever forms.
#[derive(Debug, Default)]
pub struct Stats {
    /// Commits that carried more than one statement.
    pub shared_commits: AtomicU64,
    /// Statements that went into one of those.
    pub batched: AtomicU64,
    /// The most statements one commit has carried.
    pub largest_batch: AtomicUsize,
}

/// A running executor thread.
///
/// Dropping it asks the thread to stop and waits for it, so a statement
/// already inside a commit finishes rather than being abandoned.
pub struct Executor {
    tx: Option<Sender<Work>>,
    thread: Option<JoinHandle<()>>,
    stats: Arc<Stats>,
}

impl Executor {
    /// Start a thread that owns `session` and answers what is sent to it.
    pub fn spawn<S>(session: Session<S>, deadlines: Deadlines) -> Executor
    where
        S: Storage + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        let stats = Arc::new(Stats::default());
        let mine = Arc::clone(&stats);
        let thread = thread::spawn(move || State::new(session, deadlines, mine).run(rx));
        Executor {
            tx: Some(tx),
            thread: Some(thread),
            stats,
        }
    }

    /// What it has done so far.
    pub fn stats(&self) -> Arc<Stats> {
        Arc::clone(&self.stats)
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

/// A request that has been parsed and is waiting to run.
struct Pending {
    since: Instant,
    job: Job,
    /// The statement, or the answer for a request that never reaches the
    /// engine: `Auth`, or one that did not parse.
    work: Either,
}

enum Either {
    Run(Parsed),
    Say(Vec<ServerMessage>),
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
    /// Ready to run this turn, in arrival order. The batch.
    ready: Vec<Pending>,
    /// Blocked behind the open transaction, oldest first.
    blocked: Vec<Pending>,
    stats: Arc<Stats>,
}

impl<S: Storage> State<S> {
    fn new(session: Session<S>, deadlines: Deadlines, stats: Arc<Stats>) -> Self {
        State {
            session,
            deadlines,
            conns: HashMap::new(),
            holder: None,
            ready: Vec::new(),
            blocked: Vec::new(),
            stats,
        }
    }

    fn run(mut self, rx: Receiver<Work>) {
        loop {
            let mut stop = match rx.recv_timeout(TICK) {
                Ok(work) => self.take(work),
                Err(RecvTimeoutError::Timeout) => false,
                Err(RecvTimeoutError::Disconnected) => true,
            };
            // Everything already waiting joins this turn. That is the whole
            // of group commit: the queue is deeper when the server is
            // busier, so the commit is amortized over more statements
            // exactly when that is worth most (ADR-028).
            while self.ready.len() < MAX_BATCH {
                match rx.try_recv() {
                    Ok(work) => stop |= self.take(work),
                    Err(_) => break,
                }
            }
            self.sweep();
            self.serve();
            if stop {
                break;
            }
        }
        // Statements still queued are told so instead of being dropped on
        // the floor. Transactions still parked are rolled back by going
        // out of scope, which ADR-021 makes free.
        for pending in self.ready.drain(..).chain(self.blocked.drain(..)) {
            let _ = pending.job.answer(failed(
                ErrorCode::ShuttingDown,
                "the server stopped before this statement ran",
            ));
        }
    }

    /// Returns whether the loop should stop.
    fn take(&mut self, work: Work) -> bool {
        match work {
            Work::Request(job) => {
                self.accept(job);
                false
            }
            Work::Closed(id) => {
                self.forget(id);
                false
            }
            Work::Stop => true,
        }
    }

    fn accept(&mut self, job: Job) {
        let work = match parse(&job.request) {
            Ok(Some(parsed)) => Either::Run(parsed),
            Ok(None) => Either::Say(ready()),
            Err(messages) => Either::Say(messages),
        };
        let pending = Pending {
            since: Instant::now(),
            job,
            work,
        };
        match self.holder {
            Some((holder, _)) if holder != pending.job.id => self.blocked.push(pending),
            _ => self.ready.push(pending),
        }
    }

    /// Run everything that can run, batching what may share a commit.
    fn serve(&mut self) {
        loop {
            if self.ready.is_empty() {
                self.refill();
            }
            if self.ready.is_empty() {
                return;
            }
            let batch = self.next_batch();
            if batch <= 1 {
                let pending = self.ready.remove(0);
                self.alone(pending);
            } else {
                let group: Vec<Pending> = self.ready.drain(..batch).collect();
                self.grouped(group);
            }
            // A statement that opened a transaction owns the queue now, so
            // whatever was ready behind it has to wait after all.
            if self.holder.is_some() {
                self.blocked.append(&mut self.ready);
                return;
            }
        }
    }

    /// How many statements at the front of the queue may share a commit.
    ///
    /// Only statements that run against a transaction can be batched, and
    /// only from connections that are not holding one. `begin`, `commit`
    /// and the branch statements manage their own commits, so each of them
    /// runs on its own.
    fn next_batch(&self) -> usize {
        let mut n = 0;
        for pending in &self.ready {
            let batchable = match &pending.work {
                Either::Run(parsed) => parsed.kind == Kind::Batchable,
                Either::Say(_) => false,
            };
            let clean = self
                .conns
                .get(&pending.job.id)
                .map_or(true, |c| !c.parked.is_open() && c.aborted.is_none());
            if !batchable || !clean {
                break;
            }
            n += 1;
            if n == MAX_BATCH {
                break;
            }
        }
        n
    }

    /// Several statements, one transaction, one commit.
    ///
    /// Each gets a savepoint of its own inside `Session`, so one that fails
    /// leaves the others alone. Nobody is answered until the commit
    /// succeeds: a read in the batch may have seen writes from the batch,
    /// and reporting those rows before they are durable would be a promise
    /// the server cannot keep.
    fn grouped(&mut self, group: Vec<Pending>) {
        self.stats.shared_commits.fetch_add(1, Ordering::Relaxed);
        self.stats
            .batched
            .fetch_add(group.len() as u64, Ordering::Relaxed);
        self.stats
            .largest_batch
            .fetch_max(group.len(), Ordering::Relaxed);

        if let Err(e) = self.session.execute_ast(&Statement::Begin) {
            let detail = e.to_string();
            for pending in group {
                let _ = pending
                    .job
                    .answer(failed(ErrorCode::Execution, detail.clone()));
            }
            return;
        }

        let mut answers = Vec::with_capacity(group.len());
        for pending in group {
            let messages = match &pending.work {
                Either::Run(parsed) => answer(&mut self.session, parsed),
                Either::Say(messages) => messages.clone(),
            };
            answers.push((pending.job, messages));
        }

        match self.session.execute_ast(&Statement::Commit) {
            Ok(_) => {
                for (job, messages) in answers {
                    let _ = job.answer(messages);
                }
            }
            Err(e) => {
                // They shared a commit, so they share its failure.
                let detail = format!("the shared commit failed, no statement in it ran: {e}");
                for (job, _) in answers {
                    let _ = job.answer(failed(ErrorCode::Execution, detail.clone()));
                }
            }
        }
    }

    /// One statement, with the connection's own transaction attached.
    fn alone(&mut self, pending: Pending) {
        let id = pending.job.id;
        let state = self.conns.entry(id).or_default();

        // A transaction rolled back under this connection is reported once,
        // at its next statement, rather than pushed at a client that is not
        // expecting anything. docs/PROTOCOL.md allows one reply per
        // request and nothing else.
        if let Some(reason) = state.aborted.take() {
            let _ = pending.job.answer(failed(ErrorCode::Execution, reason));
            return;
        }

        let parked = std::mem::take(&mut state.parked);
        self.session.unpark(parked);
        let messages = match &pending.work {
            Either::Run(parsed) => answer(&mut self.session, parsed),
            Either::Say(messages) => messages.clone(),
        };
        let parked = self.session.park();

        let open = parked.is_open();
        self.conns.entry(id).or_default().parked = parked;
        self.holder = if open {
            Some((id, Instant::now()))
        } else {
            None
        };

        let _ = pending.job.answer(messages);
    }

    /// Move what was blocked into the ready queue once nothing holds it.
    fn refill(&mut self) {
        if self.holder.is_none() && !self.blocked.is_empty() {
            let mut waiting = std::mem::take(&mut self.blocked);
            let keep = waiting.split_off(waiting.len().min(MAX_BATCH));
            self.ready = waiting;
            self.blocked = keep;
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
        let (keep, expired): (Vec<_>, Vec<_>) = std::mem::take(&mut self.blocked)
            .into_iter()
            .partition(|p| now.duration_since(p.since) <= busy);
        self.blocked = keep;
        for pending in expired {
            let _ = pending.job.answer(failed(
                ErrorCode::WriteQueue,
                "timed out waiting for the writer; the statement did not run",
            ));
        }
    }

    /// Drop everything held for a connection that is gone.
    fn forget(&mut self, id: ConnId) {
        self.conns.remove(&id);
        self.ready.retain(|p| p.job.id != id);
        self.blocked.retain(|p| p.job.id != id);
        if matches!(self.holder, Some((holder, _)) if holder == id) {
            self.holder = None;
        }
    }
}
