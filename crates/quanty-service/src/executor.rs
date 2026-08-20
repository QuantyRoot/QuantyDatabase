//! The thread that owns the session, and the queue in front of it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use quanty_auth::Tokens;
use quanty_core::Storage;
use quanty_exec::{Parked, Session};
use quanty_proto::{ClientMessage, ErrorCode, ServerMessage};
use quanty_ql::ast::Statement;
use quanty_server::{ConnId, Dispatch, Job};

use crate::answer::{answer, failed, parse, ready, Kind, Parsed};

/// How often the token file is looked at again, so a revoked token stops
/// working without the server being restarted.
const TOKEN_RELOAD: Duration = Duration::from_secs(1);

/// How long a statement waits for the writer before it is refused.
pub const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a connection may hold an open transaction in silence.
///
/// It was ten seconds while an open transaction stalled reads too, which
/// made one forgotten `begin` a server-wide stall. Reads go past now
/// (ADR-029), so this only bounds how long writers wait, and thirty
/// seconds is the ordinary answer to that.
pub const IDLE_IN_TXN: Duration = Duration::from_secs(30);

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
    ///
    /// Without `tokens` the server requires no authentication, which is the
    /// case docs/PROTOCOL.md already describes and the reason `quanty serve`
    /// belongs on a loopback address until one is given.
    pub fn spawn<S>(session: Session<S>, deadlines: Deadlines, tokens: Option<Tokens>) -> Executor
    where
        S: Storage + Send + 'static,
    {
        let (tx, rx) = mpsc::channel();
        let stats = Arc::new(Stats::default());
        let mine = Arc::clone(&stats);
        let thread = thread::spawn(move || State::new(session, deadlines, tokens, mine).run(rx));
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
    /// Whether it has shown a token this server accepts.
    authenticated: bool,
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
    /// The tokens this server accepts, if it requires any.
    tokens: Option<Tokens>,
    /// When the token file was last looked at again.
    checked: Instant,
    stats: Arc<Stats>,
}

impl<S: Storage> State<S> {
    fn new(
        session: Session<S>,
        deadlines: Deadlines,
        tokens: Option<Tokens>,
        stats: Arc<Stats>,
    ) -> Self {
        State {
            session,
            deadlines,
            conns: HashMap::new(),
            holder: None,
            ready: Vec::new(),
            blocked: Vec::new(),
            tokens,
            checked: Instant::now(),
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
        if let ClientMessage::Auth(token) = &job.request {
            let verdict = self.authenticate(job.id, token);
            let _ = job.answer(verdict);
            return;
        }
        // Nothing but `Auth` is answered before a token has been shown, so
        // an unauthenticated connection cannot parse, queue or run
        // anything.
        if !self.is_authenticated(job.id) {
            let _ = job.answer(failed(
                ErrorCode::NotAuthenticated,
                "this server requires a token; send Auth first",
            ));
            return;
        }
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
        if self.runnable_now(&pending) {
            self.ready.push(pending);
        } else {
            self.blocked.push(pending);
        }
    }

    /// Check a token and remember the answer for this connection.
    ///
    /// The file is re-read when it has changed, so a revoked token stops
    /// working within a second rather than at the next restart. A file that
    /// has become unreadable or malformed leaves the last good set in
    /// force: falling open would be worse and falling over would be worse
    /// still.
    fn authenticate(&mut self, id: ConnId, token: &[u8]) -> Vec<ServerMessage> {
        if self.tokens.is_none() {
            self.conns.entry(id).or_default().authenticated = true;
            return ready();
        }
        if self.checked.elapsed() >= TOKEN_RELOAD {
            self.checked = Instant::now();
            if let Some(tokens) = self.tokens.as_mut() {
                let _ = tokens.reload_if_changed();
            }
        }
        let accepted = self
            .tokens
            .as_ref()
            .map(|t| t.accepts(token))
            .unwrap_or(false);
        if accepted {
            self.conns.entry(id).or_default().authenticated = true;
            ready()
        } else {
            // No detail: which part of a rejected token was wrong is
            // exactly what an attacker would like to be told.
            failed(ErrorCode::AuthFailed, "the token was not accepted")
        }
    }

    fn is_authenticated(&self, id: ConnId) -> bool {
        self.tokens.is_none()
            || self
                .conns
                .get(&id)
                .map(|c| c.authenticated)
                .unwrap_or(false)
    }

    /// Whether this can run while things are as they are.
    ///
    /// A connection holding a transaction blocks the writers behind it,
    /// which is ADR-024, but not the readers: a read commits nothing, so it
    /// cannot invalidate the parked batch, and it sees the committed head
    /// rather than anything the holder has pending. A request that never
    /// reaches the engine, an `Auth` or one that did not parse, has nothing
    /// to wait for either.
    fn runnable_now(&self, pending: &Pending) -> bool {
        match self.holder {
            None => true,
            Some((holder, _)) => holder == pending.job.id || !Self::touches_data(pending),
        }
    }

    fn touches_data(pending: &Pending) -> bool {
        match &pending.work {
            Either::Run(parsed) => parsed.kind != Kind::Reads,
            Either::Say(_) => false,
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
            // Whatever is at the front and cannot run yet goes back to
            // waiting: a `begin` earlier in this turn may have taken the
            // queue since this was queued as ready.
            if !self.runnable_now(&self.ready[0]) {
                let pending = self.ready.remove(0);
                self.blocked.push(pending);
                continue;
            }
            let batch = self.next_batch();
            if batch <= 1 {
                let pending = self.ready.remove(0);
                self.alone(pending);
            } else {
                let group: Vec<Pending> = self.ready.drain(..batch).collect();
                self.grouped(group);
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
        // A batch opens a transaction of its own, which cannot happen while
        // a connection already holds one.
        if self.holder.is_some() {
            return 0;
        }
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

        // Only this connection's own transaction moves the holder. A read
        // that ran past someone else's must not clear it, or the writers
        // waiting behind it would be let go and the parked batch would be
        // invalidated by the first of them to commit.
        let open = parked.is_open();
        self.conns.entry(id).or_default().parked = parked;
        if open {
            self.holder = Some((id, Instant::now()));
        } else if matches!(self.holder, Some((holder, _)) if holder == id) {
            self.holder = None;
        }

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
