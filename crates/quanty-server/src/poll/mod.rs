//! The reactor's readiness layer, in safe Rust.
//!
//! The types below are the surface: a worker loop is written against them
//! and never learns which kernel answered. Only `Poller` differs per
//! platform, and each backend translates the kernel's flags into the ones
//! named here rather than letting epoll's numbers leak through the
//! interface, which is what they used to do.

use std::io;
use std::os::fd::RawFd;

#[cfg(target_os = "linux")]
mod epoll;
#[cfg(target_os = "linux")]
use epoll as backend;

#[cfg(target_os = "macos")]
mod kqueue;
#[cfg(target_os = "macos")]
use kqueue as backend;

pub use backend::Poller;

/// Readiness, as this crate names it. Neither kernel's numbering.
pub(crate) mod flag {
    /// Data to read, or a connection to accept.
    pub const READABLE: u32 = 1 << 0;
    /// Room to write.
    pub const WRITABLE: u32 = 1 << 1;
    /// The peer closed its writing half.
    pub const READ_CLOSED: u32 = 1 << 2;
    /// The connection failed or hung up.
    pub const ERROR: u32 = 1 << 3;
}

/// What a caller wants to hear about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interest(u32);

impl Interest {
    /// There is something to read, or a connection to accept.
    pub const READABLE: Interest = Interest(flag::READABLE);
    /// There is room to write.
    pub const WRITABLE: Interest = Interest(flag::WRITABLE);

    /// Both.
    pub fn both() -> Interest {
        Interest(Interest::READABLE.0 | Interest::WRITABLE.0)
    }

    /// Add another interest.
    pub fn and(self, other: Interest) -> Interest {
        Interest(self.0 | other.0)
    }

    /// Whether reading was asked for.
    pub(crate) fn wants_read(self) -> bool {
        self.0 & flag::READABLE != 0
    }

    /// Whether writing was asked for.
    pub(crate) fn wants_write(self) -> bool {
        self.0 & flag::WRITABLE != 0
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

/// Reserved for the registration that wakes a parked worker.
pub const WAKE_TOKEN: Token = Token(u64::MAX);

/// One descriptor that is ready, and why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Event {
    /// Whose registration this was.
    pub token: Token,
    flags: u32,
}

impl Event {
    /// Build an event from translated flags. Backends only.
    pub(crate) fn new(token: Token, flags: u32) -> Event {
        Event { token, flags }
    }

    /// Whether there is data, a pending connection, or a clean close.
    pub fn is_readable(&self) -> bool {
        self.flags & (flag::READABLE | flag::READ_CLOSED) != 0
    }

    /// Whether there is room to write.
    pub fn is_writable(&self) -> bool {
        self.flags & flag::WRITABLE != 0
    }

    /// Whether the peer closed its writing half.
    pub fn is_read_closed(&self) -> bool {
        self.flags & flag::READ_CLOSED != 0
    }

    /// Whether the connection failed or hung up.
    pub fn is_error(&self) -> bool {
        self.flags & flag::ERROR != 0
    }
}

/// A handle another thread can use to wake a parked worker.
///
/// What the descriptor is differs: on Linux it is the eventfd the poller
/// watches, on macOS the kqueue itself, because a user filter is
/// triggered through the queue that holds it rather than through a
/// descriptor of its own.
#[derive(Debug, Clone, Copy)]
pub struct Waker {
    fd: RawFd,
}

impl Waker {
    /// Build a waker over whichever descriptor the backend triggers
    /// through. Backends only.
    pub(crate) fn from_raw(fd: RawFd) -> Waker {
        Waker { fd }
    }

    /// Wake the worker.
    pub fn wake(&self) -> io::Result<()> {
        backend::wake(self.fd)
    }
}
