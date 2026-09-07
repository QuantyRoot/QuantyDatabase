//! Asking a process to stop, rather than killing it.
//!
//! The default action for SIGINT, SIGTERM and SIGHUP is to end the process
//! where it stands. For a database that means every shutdown is a crash:
//! the recovery path is exercised, which is why the crash harness exists,
//! but connections are dropped mid-answer, the executor thread never joins
//! and the file lock is released by the kernel rather than by us. It
//! recovers, and recovering is not the same as closing.
//!
//! So the three of them set a flag instead. `SIGHUP` is in the list
//! because nothing here has a configuration to reload, which is the only
//! other thing it traditionally means; when something does, it comes back
//! out of this list and gets its own handler.
//!
//! **The flag is polled, not delivered.** `signal` on both platforms
//! installs a handler with restart semantics, so a blocking call is
//! resumed rather than interrupted. A loop that waits without a timeout
//! will not notice this; every loop in this workspace waits with one.
//!
//! The handler does exactly one relaxed store. That is the whole of it on
//! purpose: almost nothing else is safe to call between two instructions
//! of a thread that was not expecting to be interrupted, and a handler
//! that allocates or takes a lock is a deadlock waiting for the wrong
//! moment.

#![allow(unsafe_code)]

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

/// Written by the handler, read by whoever runs the loop.
static QUIT: AtomicBool = AtomicBool::new(false);

/// The controlling terminal went away.
const SIGHUP: i32 = 1;
/// Someone pressed Ctrl-C.
const SIGINT: i32 = 2;
/// Someone, usually an init system, asked for a shutdown.
const SIGTERM: i32 = 15;

/// `signal` answers `(void (*)(int)) -1` when it refuses.
const SIG_ERR: isize = -1;

extern "C" {
    fn signal(sig: i32, handler: extern "C" fn(i32)) -> isize;
}

extern "C" fn note_quit(_sig: i32) {
    QUIT.store(true, Ordering::Relaxed);
}

/// Turn the three shutdown signals into a flag.
///
/// Call this once, early. Calling it again is harmless and installs the
/// same handler over itself.
pub fn listen_for_quit() -> io::Result<()> {
    for sig in [SIGINT, SIGTERM, SIGHUP] {
        // SAFETY: `note_quit` is an `extern "C"` function with exactly the
        // signature the C library expects, it has static lifetime, and its
        // body is a single relaxed store.
        let previous = unsafe { signal(sig, note_quit) };
        if previous == SIG_ERR {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Whether a shutdown has been asked for since the process started.
pub fn quit_requested() -> bool {
    QUIT.load(Ordering::Relaxed)
}
