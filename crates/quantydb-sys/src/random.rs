//! Bytes nobody can guess.
//!
//! A token is a credential, so this has to be the operating system's
//! generator and not a hash of the clock. There is no portable way to ask
//! for one from the standard library, which is why it lives down here
//! with the other syscalls.

#![allow(unsafe_code)]

use std::io;

/// Fill `out` with cryptographically random bytes.
pub fn fill(out: &mut [u8]) -> io::Result<()> {
    imp::fill(out)
}

/// Thirty-two random bytes, the size everything here wants.
pub fn bytes32() -> io::Result<[u8; 32]> {
    let mut out = [0u8; 32];
    fill(&mut out)?;
    Ok(out)
}

#[cfg(unix)]
mod imp {
    use std::fs::File;
    use std::io::{self, Read};

    /// `/dev/urandom` rather than `getrandom`, because it needs no
    /// syscall number and behaves the same once the pool is up, which it
    /// is by the time any process of ours runs.
    pub fn fill(out: &mut [u8]) -> io::Result<()> {
        File::open("/dev/urandom")?.read_exact(out)
    }
}

#[cfg(windows)]
mod imp {
    use std::io;

    /// `BCRYPT_USE_SYSTEM_PREFERRED_RNG`, so no algorithm handle is
    /// needed and the system's own generator answers.
    const USE_SYSTEM_PREFERRED_RNG: u32 = 0x0000_0002;

    // bcrypt.lib is not among the libraries the standard library links,
    // so the symbol has to be asked for by name or the linker will not
    // find it. That was a red Windows job, not a guess.
    #[link(name = "bcrypt")]
    extern "system" {
        fn BCryptGenRandom(
            algorithm: *mut core::ffi::c_void,
            buffer: *mut u8,
            count: u32,
            flags: u32,
        ) -> i32;
    }

    pub fn fill(out: &mut [u8]) -> io::Result<()> {
        let len = u32::try_from(out.len())
            .map_err(|_| io::Error::other("asked for more random bytes than fit in a u32"))?;
        // Safety: the pointer and length describe `out`, and the call
        // writes at most `len` bytes into it.
        let status = unsafe {
            BCryptGenRandom(
                core::ptr::null_mut(),
                out.as_mut_ptr(),
                len,
                USE_SYSTEM_PREFERRED_RNG,
            )
        };
        if status < 0 {
            return Err(io::Error::other(format!(
                "BCryptGenRandom refused with status {status:#x}"
            )));
        }
        Ok(())
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use std::io;

    /// Refused rather than faked. A token from a generator nobody has
    /// checked is worse than no token at all.
    pub fn fill(_out: &mut [u8]) -> io::Result<()> {
        Err(io::Error::other(
            "no random source is known for this platform",
        ))
    }
}
