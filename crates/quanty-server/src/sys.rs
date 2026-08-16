//! The syscall boundary. The only unsafe code in this workspace.
//!
//! ADR-023 accepts unsafe here and nowhere else, so lib.rs denies it
//! globally and this module is the single `allow`. That is not decoration:
//! it means a later change cannot quietly introduce unsafe somewhere with a
//! worse blast radius without deleting a line that says what it is doing.
//!
//! Everything below is a thin translation of a C signature into a Rust one,
//! plus the two conversions every syscall needs: a negative return becomes
//! an `io::Error`, and `EINTR` becomes a retry rather than a failure. No
//! logic lives here. Nothing here allocates. The safe wrapper in `poll.rs`
//! is where behaviour begins.
//!
//! Descriptor ownership stays with the standard library. This module never
//! constructs a socket and never closes one; it takes raw descriptors that
//! something else owns and hands back raw descriptors that the caller
//! immediately wraps in `OwnedFd`. The one class of bug that would be ours
//! to make is confined to registration.

use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

/// Close the descriptor on `exec`.
///
/// Same bit as `O_CLOEXEC`. Without it a descriptor survives into any child
/// process, which is both a leak and a way for a socket to stay open after
/// the thing that was serving it is gone.
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
/// The peer closed its writing half.
///
/// Worth registering for even though `EPOLLIN` also fires on a clean close:
/// it distinguishes "the peer is done sending" from "there is data", which
/// is the difference between finishing a reply and abandoning it.
pub const EPOLLRDHUP: u32 = 0x2000;

/// The kernel's `struct epoll_event`.
///
/// **The layout differs by architecture and getting it wrong is silent.**
/// On x86_64 the struct is packed to twelve bytes so that a 32-bit process
/// sees the same shape; everywhere else it takes natural alignment and is
/// sixteen. A mismatch does not fail to compile and does not fail to run.
/// It shifts the `data` field, so every readiness event arrives attributed
/// to the wrong connection. `layout_matches_the_kernel` in tests pins the
/// size, which is the only part of this a test can reach.
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
/// this is conditional.
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
    ///
    /// A method rather than a field access because on x86_64 this struct is
    /// packed, and taking a reference to a field of a packed struct is not
    /// allowed. Copying out is the supported way and this is where it
    /// happens once instead of at every call site.
    pub fn events(&self) -> u32 {
        self.events
    }

    /// Read the opaque value.
    pub fn data(&self) -> u64 {
        self.data
    }
}

extern "C" {
    fn epoll_create1(flags: i32) -> i32;
    fn epoll_ctl(epfd: i32, op: i32, fd: i32, event: *mut EpollEvent) -> i32;
    fn epoll_wait(epfd: i32, events: *mut EpollEvent, maxevents: i32, timeout: i32) -> i32;
    fn eventfd(initval: u32, flags: i32) -> i32;
    fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
    fn write(fd: i32, buf: *const u8, count: usize) -> isize;
}

/// Create an epoll instance, owned by the returned handle.
pub fn create_epoll() -> io::Result<OwnedFd> {
    // SAFETY: no arguments are pointers, and the flag is a constant the
    // kernel defines. A negative return is an error and is not wrapped.
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
///
/// `fd` is borrowed, not taken: the caller keeps ownership and this call
/// does not extend or shorten its life.
pub fn ctl(epfd: RawFd, op: i32, fd: RawFd, mut event: EpollEvent) -> io::Result<()> {
    // SAFETY: `event` lives on this stack frame for the whole call and the
    // kernel does not retain the pointer past it. For EPOLL_CTL_DEL the
    // pointer is ignored, and passing a valid one anyway is what older
    // kernels required.
    let rc = unsafe { epoll_ctl(epfd, op, fd, &mut event) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Block until something is ready, or the timeout expires.
///
/// Returns how many entries at the front of `events` were filled.
///
/// `EINTR` is retried rather than reported. A signal arriving while the
/// worker is parked is not an error and not information; treating it as
/// either is the classic way an event loop dies on a resize or a profiler
/// attach.
pub fn wait(epfd: RawFd, events: &mut [EpollEvent], timeout_ms: i32) -> io::Result<usize> {
    // Not clamped by the kernel, so clamp here rather than truncate.
    let max = events.len().min(i32::MAX as usize) as i32;
    loop {
        // SAFETY: the pointer and length come from the same slice, which
        // outlives the call, and the kernel writes at most `max` entries.
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
    // eventfd write requires.
    let rc = unsafe { write(fd, buf.as_ptr(), buf.len()) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Drain an eventfd, clearing its count.
///
/// The descriptor is non-blocking, so an empty one reports `WouldBlock`,
/// which is a normal outcome and not an error: a worker may be woken by
/// one thread and drained before a second thread's wakeup arrives.
pub fn drain(fd: RawFd) -> io::Result<()> {
    let mut buf = [0u8; 8];
    loop {
        // SAFETY: eight bytes on this frame, which is the read size an
        // eventfd accepts.
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
    // it, and wrapping it here is what makes it get closed exactly once.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
