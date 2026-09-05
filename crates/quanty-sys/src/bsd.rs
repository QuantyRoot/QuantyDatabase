//! Darwin: kqueue, sockets and the descriptor flags epoll got for free.
//!
//! Declared rather than depended on, like `linux.rs`, and unsafe only
//! where the kernel is actually entered.
//!
//! **This is Apple's kqueue, not every BSD's.** FreeBSD 12 widened
//! `struct kevent` with a `uint64_t ext[4]` and numbers `EVFILT_USER`
//! differently, so a shared module would be two layouts wearing one name.
//! Nothing here builds FreeBSD and nothing here has run on it, so the
//! module is gated to macOS and the second layout is a later commit with
//! a machine behind it.
//!
//! Two things differ from Linux beyond the names. There is no
//! `SOCK_NONBLOCK` or `SOCK_CLOEXEC` to pass to `socket`, so both flags
//! are set afterwards with `fcntl`. And `fcntl` is variadic in C, which
//! matters: on Apple silicon a variadic argument goes on the stack while
//! a fixed one goes in a register, so declaring it with three fixed
//! arguments would put the flag somewhere the C library never looks.

#![allow(unsafe_code)]

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// A token is 64 bits and rides in `udata`, which is pointer sized.
const _: () = assert!(
    std::mem::size_of::<usize>() == 8,
    "kqueue backend assumes a 64-bit target: udata must hold a token"
);

/// Read the descriptor flags.
const F_GETFD: i32 = 1;
/// Write the descriptor flags.
const F_SETFD: i32 = 2;
/// Read the file status flags.
const F_GETFL: i32 = 3;
/// Write the file status flags.
const F_SETFL: i32 = 4;
/// Close the descriptor on `exec`.
const FD_CLOEXEC: i32 = 1;
/// Return rather than block when a call would wait.
const O_NONBLOCK: i32 = 0x0004;

/// No such registration. Deleting a filter that was never added is not a
/// failure here, because interest is expressed as a set and the caller
/// does not track which half of it the kernel currently holds.
pub const ENOENT: i32 = 2;

/// Data to read, or a connection to accept.
pub const EVFILT_READ: i16 = -1;
/// Room to write.
pub const EVFILT_WRITE: i16 = -2;
/// A filter the program triggers itself. This is the waker.
pub const EVFILT_USER: i16 = -10;

/// Add the registration, or change one that exists.
pub const EV_ADD: u16 = 0x0001;
/// Remove the registration.
pub const EV_DELETE: u16 = 0x0002;
/// Report the event once, then reset it. Used by the waker so a wakeup
/// does not repeat for ever.
pub const EV_CLEAR: u16 = 0x0020;
/// Answer every change with its own result instead of failing the batch.
pub const EV_RECEIPT: u16 = 0x0040;
/// This event carries an errno in `data` rather than readiness.
pub const EV_ERROR: u16 = 0x4000;
/// End of file: the peer closed, or the socket can no longer be written.
pub const EV_EOF: u16 = 0x8000;

/// Trigger an `EVFILT_USER` registration.
pub const NOTE_TRIGGER: u32 = 0x0100_0000;

/// The kernel's `struct kevent` as Darwin defines it.
///
/// 32 bytes: `ident` at 0, `filter` at 8, `flags` at 10, `fflags` at 12,
/// `data` at 16, `udata` at 24. FreeBSD's is 64 and is not this type.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct KEvent {
    /// The descriptor, or an identifier the program picked for a filter
    /// that has none.
    pub ident: usize,
    /// Which filter this is about.
    pub filter: i16,
    /// `EV_*` action and result flags.
    pub flags: u16,
    /// Filter specific flags, such as `NOTE_TRIGGER`.
    pub fflags: u32,
    /// Filter specific data, or an errno when `EV_ERROR` is set.
    pub data: isize,
    /// Opaque value handed back when this filter fires.
    pub udata: u64,
}

impl KEvent {
    /// An empty event, for filling a wait buffer.
    pub fn zeroed() -> Self {
        KEvent {
            ident: 0,
            filter: 0,
            flags: 0,
            fflags: 0,
            data: 0,
            udata: 0,
        }
    }

    /// A change to submit: this filter on this descriptor, with `udata`
    /// handed back when it fires.
    pub fn change(fd: RawFd, filter: i16, flags: u16, udata: u64) -> Self {
        KEvent {
            ident: fd as usize,
            filter,
            flags,
            fflags: 0,
            data: 0,
            udata,
        }
    }
}

/// The kernel's `struct timespec`.
#[repr(C)]
struct TimeSpec {
    tv_sec: i64,
    tv_nsec: i64,
}

/// Address families, Darwin values. `AF_INET6` is not Linux's 10.
pub const AF_INET: u16 = 2;
/// IPv6.
pub const AF_INET6: u16 = 30;
const SOCK_STREAM: i32 = 1;
const SOL_SOCKET: i32 = 0xffff;
const SO_REUSEADDR: i32 = 0x0004;
const SO_REUSEPORT: i32 = 0x0200;

extern "C" {
    fn kqueue() -> i32;
    fn kevent(
        kq: i32,
        changelist: *const KEvent,
        nchanges: i32,
        eventlist: *mut KEvent,
        nevents: i32,
        timeout: *const TimeSpec,
    ) -> i32;
    fn socket(domain: i32, ty: i32, protocol: i32) -> i32;
    fn setsockopt(fd: i32, level: i32, name: i32, value: *const u8, len: u32) -> i32;
    fn bind(fd: i32, addr: *const u8, len: u32) -> i32;
    fn listen(fd: i32, backlog: i32) -> i32;
    // Variadic on purpose. See the module note: a fixed third argument is
    // passed in a register on Apple silicon and read off the stack by the
    // C library, which loses the flag without failing.
    fn fcntl(fd: i32, cmd: i32, ...) -> i32;
}

/// Create a kqueue instance, owned by the returned handle.
///
/// Unlike `epoll_create1` there is no flags argument on Darwin, so
/// close-on-exec is a second call rather than a constant.
pub fn create_kqueue() -> io::Result<OwnedFd> {
    // SAFETY: no arguments, and the descriptor is handed straight to an
    // owner that will close it exactly once.
    let fd = unsafe { kqueue() };
    let fd = owned(fd)?;
    set_cloexec(fd.as_raw_fd())?;
    Ok(fd)
}

/// Apply a changelist, reporting the first change the kernel refused.
///
/// `EV_RECEIPT` is added to every change so the kernel answers each one
/// separately: without it a batch stops at the first refusal and the rest
/// are silently not applied. Errnos in `ignore` are treated as success,
/// which is how a filter that was never registered can be deleted.
///
/// The kernel writes the receipts back over `changes`, which is what the
/// buffer is for once the call has been made.
pub fn change(kq: RawFd, changes: &mut [KEvent], ignore: &[i32]) -> io::Result<()> {
    if changes.is_empty() {
        return Ok(());
    }
    for c in changes.iter_mut() {
        c.flags |= EV_RECEIPT;
    }
    let n = changes.len().min(i32::MAX as usize) as i32;
    // A receipt is already waiting, so this is a poll and not a wait.
    let now = TimeSpec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let received = loop {
        // SAFETY: both pointers come from the same slice, whose length is
        // passed alongside; the kernel is allowed to use one array for
        // changes and receipts and overwrites at most `n` entries. `now`
        // outlives the call.
        let rc = unsafe { kevent(kq, changes.as_ptr(), n, changes.as_mut_ptr(), n, &now) };
        if rc >= 0 {
            break rc as usize;
        }
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err);
    };
    for r in &changes[..received] {
        if r.flags & EV_ERROR == 0 {
            continue;
        }
        let errno = r.data as i32;
        if errno == 0 || ignore.contains(&errno) {
            continue;
        }
        return Err(io::Error::from_raw_os_error(errno));
    }
    Ok(())
}

/// Block until something is ready, or the timeout expires.
///
/// A negative timeout blocks for ever, which is a null pointer here where
/// `epoll_wait` spells it -1.
pub fn wait(kq: RawFd, events: &mut [KEvent], timeout_ms: i32) -> io::Result<usize> {
    let max = events.len().min(i32::MAX as usize) as i32;
    let spec = TimeSpec {
        tv_sec: (timeout_ms.max(0) / 1000) as i64,
        tv_nsec: (timeout_ms.max(0) % 1000) as i64 * 1_000_000,
    };
    let timeout: *const TimeSpec = if timeout_ms < 0 {
        std::ptr::null()
    } else {
        &spec
    };
    loop {
        // SAFETY: the pointer and length come from the same slice, no
        // changes are submitted, and `spec` outlives the call.
        let rc = unsafe { kevent(kq, std::ptr::null(), 0, events.as_mut_ptr(), max, timeout) };
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

fn owned(fd: i32) -> io::Result<OwnedFd> {
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the kernel just returned this descriptor, nothing else holds
    // it, and the owner closes it exactly once.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn set_cloexec(fd: RawFd) -> io::Result<()> {
    // SAFETY: no pointers; the variadic argument is an int, which is what
    // F_GETFD and F_SETFD take.
    let flags = unsafe { fcntl(fd, F_GETFD, 0) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    let rc = unsafe { fcntl(fd, F_SETFD, flags | FD_CLOEXEC) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: as in `set_cloexec`.
    let flags = unsafe { fcntl(fd, F_GETFL, 0) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    let rc = unsafe { fcntl(fd, F_SETFL, flags | O_NONBLOCK) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Create a non-blocking stream socket with the port sharing options set.
///
/// The Linux version gets both flags from `socket` itself. Darwin has no
/// such bits, so this is three calls where that is one.
pub fn reuseport_socket(family: u16) -> io::Result<OwnedFd> {
    let domain = family as i32;
    // SAFETY: no pointers; all arguments are kernel constants.
    let fd = unsafe { socket(domain, SOCK_STREAM, 0) };
    let fd = owned(fd)?;
    set_cloexec(fd.as_raw_fd())?;
    set_nonblocking(fd.as_raw_fd())?;
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
