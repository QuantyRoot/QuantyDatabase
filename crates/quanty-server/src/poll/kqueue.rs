//! The readiness layer on kqueue.
//!
//! Three things are shaped differently from epoll and are worth naming
//! before the code says them:
//!
//! *Interest is two registrations, not one bitmask.* `EVFILT_READ` and
//! `EVFILT_WRITE` are separate entries, so changing interest means adding
//! one and deleting the other. Deleting a filter that was never added
//! answers `ENOENT`, which is expected rather than an error: the caller
//! passes the set it wants and this file makes the kernel agree with it.
//!
//! *One descriptor can produce two events per wait*, one per filter,
//! where `epoll_wait` reports each descriptor once with its bits merged.
//! Collapsing them would cost a scan of the whole result on every turn to
//! catch a case that only arises when a connection is readable and
//! writable in the same instant, so the events are passed through and the
//! worker loop, which is idempotent per token, absorbs it. ADR-038.
//!
//! *A close is heard on the read filter and nowhere else.* epoll adds
//! `EPOLLRDHUP` to every registration, so even a write-only one learns
//! that the peer went away. kqueue has no equivalent: a peer's FIN sets
//! `SS_CANTRCVMORE` and the write filter watches `SS_CANTSENDMORE`, so
//! the only way to hear it is to register the read filter, whose data
//! readiness would then have to be swallowed on every poll of a caller
//! that did not ask for it. The promise is narrowed to registrations
//! that include readable, which is every one the worker makes. ADR-038.
//!
//! *The waker is a filter rather than a descriptor.* `EVFILT_USER` with
//! `NOTE_TRIGGER` needs no eventfd and no read to drain it: `EV_CLEAR`
//! makes the kernel reset it as it is delivered, so a trigger that
//! arrives while the worker is busy sets it again and earns another turn,
//! which is the same promise the eventfd version makes by draining before
//! it reports.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use super::{flag, Event, Interest, Token, Waker, WAKE_TOKEN};
use crate::sys;

/// The identifier the user filter is registered under.
///
/// `ident` is only unique per filter, so zero here cannot collide with a
/// descriptor watched under `EVFILT_READ` or `EVFILT_WRITE`.
const WAKE_IDENT: usize = 0;

/// Deleting a filter the kernel does not hold is how interest is narrowed.
const IGNORE_MISSING: &[i32] = &[sys::ENOENT];

/// Turn the kernel's flags into this crate's, for one filter.
///
/// `EV_EOF` means different things on the two filters. On the read filter
/// it is the peer closing its writing half, which is `EPOLLRDHUP`. On the
/// write filter it is this end being unable to write ever again, which is
/// `EPOLLHUP`, and `EPOLLHUP` sets both halves.
fn reported(filter: i16, flags: u16) -> u32 {
    let eof = flags & sys::EV_EOF != 0;
    let mut bits = 0;
    if filter == sys::EVFILT_READ {
        bits |= flag::READABLE;
        if eof {
            bits |= flag::READ_CLOSED;
        }
    }
    if filter == sys::EVFILT_WRITE {
        bits |= flag::WRITABLE;
        if eof {
            bits |= flag::READ_CLOSED | flag::ERROR;
        }
    }
    if flags & sys::EV_ERROR != 0 {
        bits |= flag::ERROR;
    }
    bits
}

/// The two changes that make the kernel hold exactly `interest`.
fn changes(fd: RawFd, token: Token, interest: Interest) -> [sys::KEvent; 2] {
    let read = if interest.wants_read() {
        sys::EV_ADD
    } else {
        sys::EV_DELETE
    };
    let write = if interest.wants_write() {
        sys::EV_ADD
    } else {
        sys::EV_DELETE
    };
    [
        sys::KEvent::change(fd, sys::EVFILT_READ, read, token.0),
        sys::KEvent::change(fd, sys::EVFILT_WRITE, write, token.0),
    ]
}

/// Watches descriptors for one worker thread.
pub struct Poller {
    kq: OwnedFd,
    /// Reused across calls. Allocating a wait buffer per iteration would be
    /// a page fault per turn of the loop for nothing.
    buf: Vec<sys::KEvent>,
}

impl Poller {
    /// Create a poller that will report at most `capacity` events per wait.
    pub fn new(capacity: usize) -> io::Result<Self> {
        let kq = sys::create_kqueue()?;
        let mut wake = [sys::KEvent {
            ident: WAKE_IDENT,
            filter: sys::EVFILT_USER,
            flags: sys::EV_ADD | sys::EV_CLEAR,
            fflags: 0,
            data: 0,
            udata: WAKE_TOKEN.0,
        }];
        sys::change(kq.as_raw_fd(), &mut wake, &[])?;
        Ok(Poller {
            kq,
            buf: vec![sys::KEvent::zeroed(); capacity.max(1)],
        })
    }

    /// Start watching a descriptor.
    pub fn register(&self, fd: &impl AsRawFd, token: Token, interest: Interest) -> io::Result<()> {
        self.apply(fd.as_raw_fd(), token, interest)
    }

    /// Start watching a listening socket shared by every worker.
    ///
    /// `shared` is recorded by the interface and cannot be honoured here:
    /// kqueue has no `EPOLLEXCLUSIVE`, so every worker watching the same
    /// listener wakes and all but one find nothing to accept. That is
    /// correct and wasteful, and ADR-038 says why it is allowed to be.
    pub fn register_listener(
        &self,
        fd: &impl AsRawFd,
        token: Token,
        shared: bool,
    ) -> io::Result<()> {
        let _ = shared;
        self.apply(fd.as_raw_fd(), token, Interest::READABLE)
    }

    /// Change what a descriptor is watched for.
    pub fn reregister(
        &self,
        fd: &impl AsRawFd,
        token: Token,
        interest: Interest,
    ) -> io::Result<()> {
        self.apply(fd.as_raw_fd(), token, interest)
    }

    /// Stop watching a descriptor.
    pub fn deregister(&self, fd: &impl AsRawFd) -> io::Result<()> {
        let mut list = changes(fd.as_raw_fd(), Token(0), Interest(0));
        sys::change(self.kq.as_raw_fd(), &mut list, IGNORE_MISSING)
    }

    /// Wake this poller from another thread.
    pub fn waker(&self) -> Waker {
        Waker::from_raw(self.kq.as_raw_fd())
    }

    /// Block until something is ready, then call `f` for each event.
    pub fn poll(&mut self, timeout_ms: i32, mut f: impl FnMut(Event)) -> io::Result<usize> {
        let n = sys::wait(self.kq.as_raw_fd(), &mut self.buf, timeout_ms)?;
        let mut woken = false;
        for e in &self.buf[..n] {
            let token = Token(e.udata);
            if token == WAKE_TOKEN {
                woken = true;
                continue;
            }
            f(Event::new(token, reported(e.filter, e.flags)));
        }
        if woken {
            // Reported last, as on Linux, and already cleared by the
            // kernel: a trigger posted since then earns another turn.
            f(Event::new(WAKE_TOKEN, flag::READABLE));
        }
        Ok(n)
    }

    fn apply(&self, fd: RawFd, token: Token, interest: Interest) -> io::Result<()> {
        let mut list = changes(fd, token, interest);
        sys::change(self.kq.as_raw_fd(), &mut list, IGNORE_MISSING)
    }
}

/// Trigger the user filter a poller is watching.
pub(super) fn wake(kq: RawFd) -> io::Result<()> {
    // EV_ADD on an entry that exists updates it, which is how a trigger
    // is delivered. EV_CLEAR is repeated rather than assumed to survive:
    // the whole point of it is that a wakeup does not repeat for ever.
    let mut trigger = [sys::KEvent {
        ident: WAKE_IDENT,
        filter: sys::EVFILT_USER,
        flags: sys::EV_ADD | sys::EV_CLEAR,
        fflags: sys::NOTE_TRIGGER,
        data: 0,
        udata: WAKE_TOKEN.0,
    }];
    sys::change(kq, &mut trigger, &[])
}
