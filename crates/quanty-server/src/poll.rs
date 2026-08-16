//! The reactor's readiness layer, in safe Rust.
//!
//! One `Poller` per worker thread. It owns an epoll instance and an
//! eventfd, watches a set of descriptors it does not own, and reports which
//! are ready. It does no I/O on those descriptors and knows nothing about
//! the protocol; the connection state machine sits above it.
//!
//! **Level-triggered, deliberately.** Edge-triggered epoll reports a
//! descriptor once per transition, so a handler that stops reading before
//! the socket is drained never hears about the rest and the connection
//! hangs until the peer gives up. Level-triggered keeps reporting while
//! data remains, so the same mistake costs an extra loop iteration instead
//! of a stall. ADR-023 takes the shape whose failure mode is slow over the
//! one whose failure mode is silent, and revisits it when a benchmark asks.
//! `unread_data_is_reported_again` in tests pins this, so a later switch
//! has to be deliberate.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use crate::sys;

/// What a caller wants to hear about.
///
/// Error and hangup are not here because they are not requestable: epoll
/// reports them whether or not anyone asked, so offering them as a choice
/// would be a lie about what the registration does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interest(u32);

/// Added to every registration, whatever the caller asked for.
///
/// `EPOLLRDHUP` says the peer closed its writing half, and unlike
/// `EPOLLERR` and `EPOLLHUP` the kernel only reports it if it was
/// requested. `EPOLLHUP` itself arrives only once *both* directions are
/// shut, so a connection registered for writing alone is told nothing at
/// all when its peer disappears: it stays writable forever and the reactor
/// holds a socket nobody is on the other end of. At ten thousand
/// connections that is not a leak that shows up in testing, it is a slow
/// one that shows up in a week.
///
/// So it is not offered as a choice. Every registration gets it, because
/// there is no state a connection can be in where not knowing is useful,
/// and a flag that must be remembered is a flag that will be forgotten.
const ALWAYS: u32 = sys::EPOLLRDHUP;

impl Interest {
    /// There is something to read, or a connection to accept.
    pub const READABLE: Interest = Interest(sys::EPOLLIN);
    /// There is room to write.
    pub const WRITABLE: Interest = Interest(sys::EPOLLOUT);

    /// Both.
    pub fn both() -> Interest {
        Interest(Interest::READABLE.0 | Interest::WRITABLE.0)
    }

    /// Add another interest.
    pub fn and(self, other: Interest) -> Interest {
        Interest(self.0 | other.0)
    }
}

/// Identifies a registration when its descriptor becomes ready.
///
/// The worker chooses these and uses them to find the connection. A plain
/// integer rather than a pointer: a pointer in the kernel's hands outlives
/// nothing safely, and an index into a slab can be validated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Token(pub u64);

/// Reserved for the eventfd that wakes a parked worker.
///
/// Not a magic number scattered through the loop: a worker comparing
/// against this is saying "another thread has something for me", and that
/// deserves a name.
pub const WAKE_TOKEN: Token = Token(u64::MAX);

/// One descriptor that is ready, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Event {
    /// Whose registration this was.
    pub token: Token,
    flags: u32,
}

impl Event {
    /// Whether there is data, a pending connection, or a clean close.
    pub fn is_readable(&self) -> bool {
        self.flags & (sys::EPOLLIN | sys::EPOLLRDHUP) != 0
    }

    /// Whether there is room to write.
    pub fn is_writable(&self) -> bool {
        self.flags & sys::EPOLLOUT != 0
    }

    /// Whether the peer closed its writing half.
    pub fn is_read_closed(&self) -> bool {
        self.flags & (sys::EPOLLRDHUP | sys::EPOLLHUP) != 0
    }

    /// Whether the connection failed or hung up.
    ///
    /// Reported without being requested, so a loop that ignores it spins on
    /// a dead socket forever. Every caller must handle it.
    pub fn is_error(&self) -> bool {
        self.flags & (sys::EPOLLERR | sys::EPOLLHUP) != 0
    }
}

/// Watches descriptors for one worker thread.
pub struct Poller {
    epoll: OwnedFd,
    wake: OwnedFd,
    /// Reused across calls. Allocating a wait buffer per iteration would be
    /// an allocation per event loop turn, which is the one place in a
    /// reactor where that cost is paid ten thousand times a second.
    buf: Vec<sys::EpollEvent>,
}

impl Poller {
    /// Create a poller that will report at most `capacity` events per wait.
    ///
    /// The capacity is not a limit on watched descriptors, only on how many
    /// are reported at once; the rest are reported on the next turn, which
    /// under level-triggered semantics loses nothing.
    pub fn new(capacity: usize) -> io::Result<Self> {
        let epoll = sys::create_epoll()?;
        let wake = sys::create_eventfd()?;
        let poller = Poller {
            epoll,
            wake,
            buf: vec![sys::EpollEvent::zeroed(); capacity.max(1)],
        };
        sys::ctl(
            poller.epoll.as_raw_fd(),
            sys::EPOLL_CTL_ADD,
            poller.wake.as_raw_fd(),
            sys::EpollEvent {
                events: Interest::READABLE.0,
                data: WAKE_TOKEN.0,
            },
        )?;
        Ok(poller)
    }

    /// Start watching a descriptor.
    ///
    /// The caller keeps ownership. Deregistering before the descriptor
    /// closes is the caller's job; closing it while registered removes it
    /// from the interest list automatically, but only if no other
    /// descriptor was duplicated from it, which is why this API does not
    /// rely on that.
    pub fn register(&self, fd: &impl AsRawFd, token: Token, interest: Interest) -> io::Result<()> {
        sys::ctl(
            self.epoll.as_raw_fd(),
            sys::EPOLL_CTL_ADD,
            fd.as_raw_fd(),
            sys::EpollEvent {
                events: interest.0 | ALWAYS,
                data: token.0,
            },
        )
    }

    /// Change what a descriptor is watched for.
    ///
    /// Peer-close stays registered whatever is passed, for the reason given
    /// on `ALWAYS`: the moment a connection switches to writing is exactly
    /// when losing it would matter.
    pub fn reregister(
        &self,
        fd: &impl AsRawFd,
        token: Token,
        interest: Interest,
    ) -> io::Result<()> {
        sys::ctl(
            self.epoll.as_raw_fd(),
            sys::EPOLL_CTL_MOD,
            fd.as_raw_fd(),
            sys::EpollEvent {
                events: interest.0 | ALWAYS,
                data: token.0,
            },
        )
    }

    /// Stop watching a descriptor.
    pub fn deregister(&self, fd: &impl AsRawFd) -> io::Result<()> {
        sys::ctl(
            self.epoll.as_raw_fd(),
            sys::EPOLL_CTL_DEL,
            fd.as_raw_fd(),
            sys::EpollEvent::zeroed(),
        )
    }

    /// Wake this poller from another thread.
    ///
    /// Cheap and idempotent: several wakeups before the worker runs collapse
    /// into one, which is the behaviour a write queue wants when it finishes
    /// several statements at once.
    pub fn waker(&self) -> Waker {
        Waker {
            fd: self.wake.as_raw_fd(),
        }
    }

    /// Block until something is ready, then call `f` for each event.
    ///
    /// A callback rather than a returned slice because the events live in a
    /// buffer this struct owns, and handing out a borrow of it would stop
    /// the caller from touching the poller while handling them, which is
    /// exactly what a handler needs to do.
    ///
    /// `timeout_ms` of -1 waits forever.
    pub fn poll(&mut self, timeout_ms: i32, mut f: impl FnMut(Event)) -> io::Result<usize> {
        let n = sys::wait(self.epoll.as_raw_fd(), &mut self.buf, timeout_ms)?;
        let mut woken = false;
        for e in &self.buf[..n] {
            let token = Token(e.data());
            if token == WAKE_TOKEN {
                woken = true;
                continue;
            }
            f(Event {
                token,
                flags: e.events(),
            });
        }
        if woken {
            // Drain after dispatching, so a wakeup posted while these
            // events were being handled is not swallowed: the count goes to
            // zero here and any later post makes the next wait return
            // immediately.
            sys::drain(self.wake.as_raw_fd())?;
        }
        Ok(n)
    }
}

/// A handle another thread can use to wake a parked worker.
#[derive(Debug, Clone, Copy)]
pub struct Waker {
    fd: RawFd,
}

impl Waker {
    /// Wake the worker.
    ///
    /// Borrows the poller's descriptor, so a `Waker` must not outlive the
    /// `Poller` it came from. In the server this holds by construction: the
    /// worker owns its poller for the whole life of the process and hands
    /// wakers to the write queue, which is shut down first.
    pub fn wake(&self) -> io::Result<()> {
        sys::notify(self.fd)
    }
}

// SAFETY note, not an unsafe impl: `Waker` is `Send` and `Sync`
// automatically because `RawFd` is, and posting to an eventfd from several
// threads at once is defined by the kernel to be atomic. No lock is needed
// and none is taken.
