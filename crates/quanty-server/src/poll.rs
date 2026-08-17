//! The reactor's readiness layer, in safe Rust.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use crate::sys;

/// What a caller wants to hear about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interest(u32);

/// Added to every registration, whatever the caller asked for.
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
/// Low 32 bits are a slot index, high 32 a generation. A slot reused after
/// a close would otherwise receive events queued for its predecessor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Token(pub u64);

impl Token {
    /// Build a token from a slot and its generation.
    pub fn new(index: u32, generation: u32) -> Token {
        Token(((generation as u64) << 32) | index as u64)
    }

    /// The slot this refers to.
    pub fn index(self) -> u32 {
        self.0 as u32
    }

    /// How many times that slot had been handed out.
    pub fn generation(self) -> u32 {
        (self.0 >> 32) as u32
    }
}

/// Reserved for the eventfd that wakes a parked worker.
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
    pub fn is_error(&self) -> bool {
        self.flags & (sys::EPOLLERR | sys::EPOLLHUP) != 0
    }
}

/// Watches descriptors for one worker thread.
pub struct Poller {
    epoll: OwnedFd,
    wake: OwnedFd,
    /// Reused across calls. Allocating a wait buffer per iteration would be
    buf: Vec<sys::EpollEvent>,
}

impl Poller {
    /// Create a poller that will report at most `capacity` events per wait.
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

    /// Start watching a listening socket shared by every worker.
    pub fn register_listener(&self, fd: &impl AsRawFd, token: Token) -> io::Result<()> {
        sys::ctl(
            self.epoll.as_raw_fd(),
            sys::EPOLL_CTL_ADD,
            fd.as_raw_fd(),
            sys::EpollEvent {
                events: sys::EPOLLIN | sys::EPOLLEXCLUSIVE,
                data: token.0,
            },
        )
    }

    /// Change what a descriptor is watched for.
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
    pub fn waker(&self) -> Waker {
        Waker {
            fd: self.wake.as_raw_fd(),
        }
    }

    /// Block until something is ready, then call `f` for each event.
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
    pub fn wake(&self) -> io::Result<()> {
        sys::notify(self.fd)
    }
}
