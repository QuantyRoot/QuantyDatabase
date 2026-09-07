//! Linux: epoll, sockets and eventfd.
//!
//! Declared rather than depended on, and unsafe only where the kernel is
//! actually entered.

#![allow(unsafe_code)]

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// Close the descriptor on `exec`.
const CLOEXEC: i32 = 0o2000000;

/// Do not block on an `eventfd` read that would have nothing to return.
const EFD_NONBLOCK: i32 = 0o4000;

/// Add a descriptor to the interest list.
pub const EPOLL_CTL_ADD: i32 = 1;
/// Remove a descriptor from the interest list.
pub const EPOLL_CTL_DEL: i32 = 2;
/// Change the interest recorded for a descriptor.
pub const EPOLL_CTL_MOD: i32 = 3;

/// Readable, or a listener with a pending connection.
pub const EPOLLIN: u32 = 0x001;
/// Writable.
pub const EPOLLOUT: u32 = 0x004;
/// An error condition. Always reported, never registered for.
pub const EPOLLERR: u32 = 0x008;
/// Hang up. Always reported, never registered for.
pub const EPOLLHUP: u32 = 0x010;
/// Wake exactly one waiter when a shared descriptor becomes ready.
pub const EPOLLEXCLUSIVE: u32 = 1 << 28;

/// The peer closed its writing half.
pub const EPOLLRDHUP: u32 = 0x2000;

/// The kernel's `struct epoll_event`.
#[cfg(target_arch = "x86_64")]
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct EpollEvent {
    /// Bitmask of `EPOLL*` flags.
    pub events: u32,
    /// Opaque value handed back when this descriptor is ready.
    pub data: u64,
}

/// The kernel's `struct epoll_event`. See the x86_64 definition for why
#[cfg(not(target_arch = "x86_64"))]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct EpollEvent {
    /// Bitmask of `EPOLL*` flags.
    pub events: u32,
    /// Opaque value handed back when this descriptor is ready.
    pub data: u64,
}

impl EpollEvent {
    /// An empty event, for filling a wait buffer.
    pub fn zeroed() -> Self {
        EpollEvent { events: 0, data: 0 }
    }

    /// Read the flags.
    pub fn events(&self) -> u32 {
        self.events
    }

    /// Read the opaque value.
    pub fn data(&self) -> u64 {
        self.data
    }
}

/// Address families and socket options, Linux values.
pub const AF_INET: u16 = 2;
/// IPv6.
pub const AF_INET6: u16 = 10;
const SOCK_STREAM: i32 = 1;
const SOCK_NONBLOCK: i32 = 0o4000;
const SOL_SOCKET: i32 = 1;
const SO_REUSEADDR: i32 = 2;
const SO_REUSEPORT: i32 = 15;

extern "C" {
    fn epoll_create1(flags: i32) -> i32;
    fn socket(domain: i32, ty: i32, protocol: i32) -> i32;
    fn setsockopt(fd: i32, level: i32, name: i32, value: *const u8, len: u32) -> i32;
    fn bind(fd: i32, addr: *const u8, len: u32) -> i32;
    fn listen(fd: i32, backlog: i32) -> i32;
    fn epoll_ctl(epfd: i32, op: i32, fd: i32, event: *mut EpollEvent) -> i32;
    fn epoll_wait(epfd: i32, events: *mut EpollEvent, maxevents: i32, timeout: i32) -> i32;
    fn eventfd(initval: u32, flags: i32) -> i32;
    fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
    fn write(fd: i32, buf: *const u8, count: usize) -> isize;
}

/// Create an epoll instance, owned by the returned handle.
pub fn create_epoll() -> io::Result<OwnedFd> {
    // SAFETY: no arguments are pointers, and the flag is a constant the
    let fd = unsafe { epoll_create1(CLOEXEC) };
    owned(fd)
}

/// Create an eventfd for waking a worker from another thread.
pub fn create_eventfd() -> io::Result<OwnedFd> {
    // SAFETY: as above; both flags are kernel constants.
    let fd = unsafe { eventfd(0, CLOEXEC | EFD_NONBLOCK) };
    owned(fd)
}

/// Add, modify or remove a descriptor's registration.
pub fn ctl(epfd: RawFd, op: i32, fd: RawFd, mut event: EpollEvent) -> io::Result<()> {
    // SAFETY: `event` lives on this stack frame for the whole call and the
    let rc = unsafe { epoll_ctl(epfd, op, fd, &mut event) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Block until something is ready, or the timeout expires.
pub fn wait(epfd: RawFd, events: &mut [EpollEvent], timeout_ms: i32) -> io::Result<usize> {
    let max = events.len().min(i32::MAX as usize) as i32;
    loop {
        // SAFETY: the pointer and length come from the same slice, which
        let rc = unsafe { epoll_wait(epfd, events.as_mut_ptr(), max, timeout_ms) };
        if rc >= 0 {
            return Ok(rc as usize);
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err);
    }
}

/// Post a wakeup to an eventfd.
pub fn notify(fd: RawFd) -> io::Result<()> {
    let one: u64 = 1;
    let buf = one.to_ne_bytes();
    // SAFETY: the buffer is eight bytes on this frame and eight is what an
    let rc = unsafe { write(fd, buf.as_ptr(), buf.len()) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Drain an eventfd, clearing its count.
pub fn drain(fd: RawFd) -> io::Result<()> {
    let mut buf = [0u8; 8];
    loop {
        // SAFETY: eight bytes on this frame, which is the read size an
        let rc = unsafe { read(fd, buf.as_mut_ptr(), buf.len()) };
        if rc >= 0 {
            continue;
        }
        let err = io::Error::last_os_error();
        return match err.kind() {
            io::ErrorKind::WouldBlock => Ok(()),
            io::ErrorKind::Interrupted => continue,
            _ => Err(err),
        };
    }
}

fn owned(fd: i32) -> io::Result<OwnedFd> {
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the kernel just returned this descriptor, nothing else holds
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Create a non-blocking stream socket with the port sharing options set.
pub fn reuseport_socket(family: u16) -> io::Result<OwnedFd> {
    let domain = family as i32;
    // SAFETY: no pointers; all arguments are kernel constants.
    let fd = unsafe { socket(domain, SOCK_STREAM | CLOEXEC | SOCK_NONBLOCK, 0) };
    let fd = owned(fd)?;
    set_flag(fd.as_raw_fd(), SO_REUSEADDR)?;
    set_flag(fd.as_raw_fd(), SO_REUSEPORT)?;
    Ok(fd)
}

fn set_flag(fd: RawFd, name: i32) -> io::Result<()> {
    let on: i32 = 1;
    let bytes = on.to_ne_bytes();
    // SAFETY: four bytes on this frame, which is the width these options take.
    let rc = unsafe { setsockopt(fd, SOL_SOCKET, name, bytes.as_ptr(), bytes.len() as u32) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Bind an already-configured socket to an encoded address.
pub fn bind_to(fd: RawFd, addr: &[u8]) -> io::Result<()> {
    // SAFETY: the slice outlives the call and its length is passed alongside.
    let rc = unsafe { bind(fd, addr.as_ptr(), addr.len() as u32) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Start accepting.
pub fn listen_on(fd: RawFd, backlog: i32) -> io::Result<()> {
    // SAFETY: no pointers.
    let rc = unsafe { listen(fd, backlog) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
