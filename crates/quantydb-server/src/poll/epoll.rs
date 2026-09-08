//! The readiness layer on epoll.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use super::{flag, Event, Interest, Token, Waker, WAKE_TOKEN};
use crate::sys;

/// Added to every registration, whatever the caller asked for.
const ALWAYS: u32 = sys::EPOLLRDHUP;

/// Turn an interest into the kernel's bits.
fn wanted(interest: Interest) -> u32 {
    let mut bits = ALWAYS;
    if interest.wants_read() {
        bits |= sys::EPOLLIN;
    }
    if interest.wants_write() {
        bits |= sys::EPOLLOUT;
    }
    bits
}

/// Turn the kernel's bits into this crate's.
///
/// `EPOLLHUP` is both halves at once: the peer is gone for reading and the
/// connection is finished, which is why it sets two flags here.
fn reported(events: u32) -> u32 {
    let mut bits = 0;
    if events & sys::EPOLLIN != 0 {
        bits |= flag::READABLE;
    }
    if events & sys::EPOLLOUT != 0 {
        bits |= flag::WRITABLE;
    }
    if events & sys::EPOLLRDHUP != 0 {
        bits |= flag::READ_CLOSED;
    }
    if events & sys::EPOLLHUP != 0 {
        bits |= flag::READ_CLOSED | flag::ERROR;
    }
    if events & sys::EPOLLERR != 0 {
        bits |= flag::ERROR;
    }
    bits
}

/// Watches descriptors for one worker thread.
pub struct Poller {
    epoll: OwnedFd,
    wake: OwnedFd,
    /// Reused across calls. Allocating a wait buffer per iteration would be
    /// a page fault per turn of the loop for nothing.
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
                events: sys::EPOLLIN,
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
                events: wanted(interest),
                data: token.0,
            },
        )
    }

    /// Start watching a listening socket shared by every worker.
    pub fn register_listener(
        &self,
        fd: &impl AsRawFd,
        token: Token,
        shared: bool,
    ) -> io::Result<()> {
        let mut events = sys::EPOLLIN;
        if shared {
            events |= sys::EPOLLEXCLUSIVE;
        }
        sys::ctl(
            self.epoll.as_raw_fd(),
            sys::EPOLL_CTL_ADD,
            fd.as_raw_fd(),
            sys::EpollEvent {
                events,
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
                events: wanted(interest),
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
        Waker::from_raw(self.wake.as_raw_fd())
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
            f(Event::new(token, reported(e.events())));
        }
        if woken {
            // Drain first, then report: a reply posted after the drain
            // leaves the eventfd readable and earns another turn.
            sys::drain(self.wake.as_raw_fd())?;
            f(Event::new(WAKE_TOKEN, flag::READABLE));
        }
        Ok(n)
    }
}

/// Post a wakeup to the eventfd a poller is watching.
pub(super) fn wake(fd: RawFd) -> io::Result<()> {
    sys::notify(fd)
}
