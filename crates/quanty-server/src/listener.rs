//! Listening sockets that several workers can bind to the same port.

use std::io;
use std::net::{SocketAddr, TcpListener};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};

use crate::sys;

/// Pending connections the kernel will queue before refusing.
const BACKLOG: i32 = 4096;

/// Encode a socket address the way the kernel expects it.
fn encode(addr: SocketAddr) -> Vec<u8> {
    let mut out = Vec::with_capacity(28);
    match addr {
        SocketAddr::V4(v4) => {
            out.extend_from_slice(&sys::AF_INET.to_ne_bytes());
            out.extend_from_slice(&v4.port().to_be_bytes());
            out.extend_from_slice(&v4.ip().octets());
            out.extend_from_slice(&[0u8; 8]);
        }
        SocketAddr::V6(v6) => {
            out.extend_from_slice(&sys::AF_INET6.to_ne_bytes());
            out.extend_from_slice(&v6.port().to_be_bytes());
            out.extend_from_slice(&v6.flowinfo().to_be_bytes());
            out.extend_from_slice(&v6.ip().octets());
            out.extend_from_slice(&v6.scope_id().to_be_bytes());
        }
    }
    out
}

/// Bind a listener that shares its port with its siblings.
///
/// The kernel spreads incoming connections across every socket bound this
/// way by hashing the four-tuple, which is what a shared listener plus
/// EPOLLEXCLUSIVE does not do. See ADR-025.
pub fn bind_reuseport(addr: SocketAddr) -> io::Result<TcpListener> {
    let family = match addr {
        SocketAddr::V4(_) => sys::AF_INET,
        SocketAddr::V6(_) => sys::AF_INET6,
    };
    let fd = sys::reuseport_socket(family)?;
    sys::bind_to(fd.as_raw_fd(), &encode(addr))?;
    sys::listen_on(fd.as_raw_fd(), BACKLOG)?;
    // SAFETY: the descriptor was just created here and nothing else holds
    // it; handing it to std makes it get closed exactly once.
    #[allow(unsafe_code)]
    let listener = unsafe { TcpListener::from_raw_fd(fd.into_raw_fd()) };
    Ok(listener)
}
