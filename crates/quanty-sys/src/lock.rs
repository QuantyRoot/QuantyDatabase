//! One writer per file.
//!
//! Both platforms have a lock and they do not mean the same thing, which
//! is the whole reason this is not one call to the standard library.
//!
//! On unix `flock` is advisory: it keeps two writers apart and a reader
//! never notices it. On Windows a byte range lock is mandatory, so
//! locking the file a database lives in stops anybody else reading that
//! database at all. Taking the standard library's whole-file lock there
//! turned "many readers alongside one writer" into "one process", and
//! made a second handle answer `not a quanty database`, because its reads
//! came back refused.
//!
//! So Windows locks a single byte far past anything a database will grow
//! into. Nothing ever reads or writes there, the lock still conflicts
//! with a second writer asking for the same byte, and readers are left
//! alone. SQLite does the same thing for the same reason.

#![allow(unsafe_code)]

use std::fs::File;
use std::io;

/// Why a lock could not be taken.
#[derive(Debug)]
pub enum LockError {
    /// Somebody else holds it.
    Held,
    /// The platform or the filesystem would not say. Not a reason to
    /// refuse the database, since the commit guard does not need the
    /// kernel's help.
    Unsupported(io::Error),
}

/// Take the writer lock, without waiting for it.
pub fn try_write_lock(file: &File) -> Result<(), LockError> {
    imp::try_write_lock(file)
}

#[cfg(unix)]
mod imp {
    use super::LockError;
    use std::fs::{File, TryLockError};

    pub fn try_write_lock(file: &File) -> Result<(), LockError> {
        match file.try_lock() {
            Ok(()) => Ok(()),
            Err(TryLockError::WouldBlock) => Err(LockError::Held),
            Err(TryLockError::Error(e)) => Err(LockError::Unsupported(e)),
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::LockError;
    use std::fs::File;
    use std::io;
    use std::os::windows::io::AsRawHandle;

    /// Far past any database, so nothing reads or writes the byte this
    /// locks and mandatory locking costs a reader nothing.
    const LOCK_OFFSET: u64 = 0xFFFF_FFFF_0000_0000;

    const LOCKFILE_FAIL_IMMEDIATELY: u32 = 0x0000_0001;
    const LOCKFILE_EXCLUSIVE_LOCK: u32 = 0x0000_0002;
    const ERROR_LOCK_VIOLATION: i32 = 33;

    #[repr(C)]
    struct Overlapped {
        internal: usize,
        internal_high: usize,
        offset: u32,
        offset_high: u32,
        event: *mut core::ffi::c_void,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn LockFileEx(
            handle: *mut core::ffi::c_void,
            flags: u32,
            reserved: u32,
            bytes_low: u32,
            bytes_high: u32,
            overlapped: *mut Overlapped,
        ) -> i32;
    }

    pub fn try_write_lock(file: &File) -> Result<(), LockError> {
        let mut overlapped = Overlapped {
            internal: 0,
            internal_high: 0,
            offset: LOCK_OFFSET as u32,
            offset_high: (LOCK_OFFSET >> 32) as u32,
            event: core::ptr::null_mut(),
        };
        // Safety: the handle belongs to `file` and outlives the call, and
        // the overlapped structure is owned here and passed by pointer as
        // the call expects.
        let ok = unsafe {
            LockFileEx(
                file.as_raw_handle(),
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                0,
                1,
                0,
                &mut overlapped,
            )
        };
        if ok != 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        match err.raw_os_error() {
            Some(ERROR_LOCK_VIOLATION) => Err(LockError::Held),
            _ => Err(LockError::Unsupported(err)),
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use super::LockError;
    use std::fs::File;
    use std::io;

    pub fn try_write_lock(_file: &File) -> Result<(), LockError> {
        Err(LockError::Unsupported(io::Error::other(
            "no file lock is known for this platform",
        )))
    }
}
