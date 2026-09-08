//! Listening sockets that several workers can bind to the same port.

use std::io;
use std::net::{SocketAddr, TcpListener};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};

use crate::sys;

/// Pending connections the kernel will queue before refusing.
const BACKLOG: i32 = 4096;

/// The first two bytes of a `sockaddr`, which are not the same two bytes
/// on both kernels.
///
/// Linux opens with a 16-bit family. The BSDs spend the first byte on the
/// structure's own length and leave the family one byte wide. Writing
/// Linux's layout on Darwin puts the family where the kernel reads a
/// length, and it compiles either way, which is why this is a function
/// with a test under it rather than a line inside `encode`.
#[cfg(target_os = "linux")]
fn header(family: u16, len: u8) -> [u8; 2] {
    let _ = len;
    family.to_ne_bytes()
}

/// See the Linux version.
#[cfg(target_os = "macos")]
fn header(family: u16, len: u8) -> [u8; 2] {
    [len, family as u8]
}

/// Encode a socket address the way the kernel expects it.
fn encode(addr: SocketAddr) -> Vec<u8> {
    let mut out = Vec::with_capacity(28);
    match addr {
        SocketAddr::V4(v4) => {
            out.extend_from_slice(&header(sys::AF_INET, 16));
            out.extend_from_slice(&v4.port().to_be_bytes());
            out.extend_from_slice(&v4.ip().octets());
            out.extend_from_slice(&[0u8; 8]);
        }
        SocketAddr::V6(v6) => {
            out.extend_from_slice(&header(sys::AF_INET6, 28));
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
/// **On Linux.** The kernel spreads incoming connections across every
/// socket bound this way by hashing the four-tuple, which is what a
/// shared listener plus EPOLLEXCLUSIVE does not do. See ADR-025.
///
/// **Not on Darwin.** The call succeeds and the sockets bind, and then
/// the listener that bound last receives every connection: two hundred
/// connections across four listeners measured 0 / 0 / 0 / 200. A server
/// built on this shape there has one worker wearing four coats. Use the
/// shared listener instead, which kqueue watches without complaint. See
/// ADR-038.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The sizes the kernel expects, which are the same on both, and the
    /// header, which is not.
    #[test]
    fn encoded_addresses_have_the_platform_layout() {
        let v4 = encode("127.0.0.1:8080".parse().expect("v4"));
        assert_eq!(v4.len(), 16, "sockaddr_in is sixteen bytes");
        let v6 = encode("[::1]:8080".parse().expect("v6"));
        assert_eq!(v6.len(), 28, "sockaddr_in6 is twenty-eight bytes");

        if cfg!(target_os = "macos") {
            assert_eq!(v4[0], 16, "BSD leads with the structure length");
            assert_eq!(v4[1], sys::AF_INET as u8, "then one byte of family");
            assert_eq!(v6[0], 28, "and the v6 length is not the v4 one");
            assert_eq!(v6[1], sys::AF_INET6 as u8, "AF_INET6 is 30 here");
        } else {
            assert_eq!(
                &v4[..2],
                &sys::AF_INET.to_ne_bytes(),
                "Linux leads with a two byte family and no length"
            );
            assert_eq!(&v6[..2], &sys::AF_INET6.to_ne_bytes());
        }
    }

    /// The port is the one field that is big endian whatever the host is.
    #[test]
    fn the_port_is_network_order() {
        let v4 = encode("127.0.0.1:8080".parse().expect("v4"));
        assert_eq!(&v4[2..4], &8080u16.to_be_bytes());
        assert_eq!(&v4[4..8], &[127, 0, 0, 1], "then the address");
    }
}
