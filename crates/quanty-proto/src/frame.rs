//! Frame headers and the handshake.
//!
//! See docs/PROTOCOL.md. A frame is a one byte message type, a four byte
//! little endian body length, and the body. The handshake sits before all
//! framing and is fixed for all time, because it is how the version gets
//! agreed and so cannot itself be negotiable.

use crate::codec::{Reader, Writer};
use crate::error::{ProtoError, Result};
use crate::limits::MAX_BODY;

/// Bytes on the wire before the body: type plus length.
pub const HEADER_LEN: usize = 5;

/// The protocol version this build speaks.
pub const VERSION: u16 = 1;

/// The six bytes a client leads with.
pub const MAGIC: &[u8; 6] = b"QUANTY";

/// Nine bytes from the client. Fixed forever.
pub const CLIENT_HELLO_LEN: usize = 9;
/// Four bytes back from the server. Fixed forever.
pub const SERVER_HELLO_LEN: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// The five bytes in front of every body.
pub struct FrameHeader {
    /// Which message follows.
    pub msg_type: u8,
    /// How many bytes of body follow, already checked against `MAX_BODY`.
    pub body_len: usize,
}

impl FrameHeader {
    /// Serialize the header.
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut out = [0u8; HEADER_LEN];
        out[0] = self.msg_type;
        out[1..5].copy_from_slice(&(self.body_len as u32).to_le_bytes());
        out
    }

    /// Decode a header and refuse anything over the cap.
    ///
    /// The caller reads exactly `body_len` bytes next, so this returning
    /// `Ok` is what licenses that allocation.
    pub fn decode(bytes: &[u8; HEADER_LEN]) -> Result<Self> {
        let body_len = u32::from_le_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]) as u64;
        if body_len > MAX_BODY as u64 {
            return Err(ProtoError::TooLarge {
                declared: body_len,
                limit: MAX_BODY as u64,
            });
        }
        Ok(FrameHeader {
            msg_type: bytes[0],
            body_len: body_len as usize,
        })
    }
}

/// Why a server turned a client away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Refusal {
    /// The server no longer speaks that version.
    VersionTooOld = 0x01,
    /// The server does not speak that version yet.
    VersionTooNew = 0x02,
    /// The first six bytes were not ours.
    BadMagic = 0x03,
}

impl Refusal {
    /// Map a byte from the wire back to a reason.
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            0x01 => Refusal::VersionTooOld,
            0x02 => Refusal::VersionTooNew,
            0x03 => Refusal::BadMagic,
            _ => return None,
        })
    }
}

/// What the client sends first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// What the client sends first.
pub struct ClientHello {
    /// Highest version the client can speak.
    pub version: u16,
}

impl ClientHello {
    /// Serialize the nine byte hello.
    pub fn encode(&self) -> [u8; CLIENT_HELLO_LEN] {
        let mut out = [0u8; CLIENT_HELLO_LEN];
        out[0..6].copy_from_slice(MAGIC);
        out[6..8].copy_from_slice(&self.version.to_le_bytes());
        out[8] = 0;
        out
    }

    /// Parse a nine byte hello.
    pub fn decode(bytes: &[u8; CLIENT_HELLO_LEN]) -> Result<Self> {
        let mut r = Reader::new(bytes);
        let magic = r.raw(MAGIC.len())?;
        if magic != MAGIC.as_slice() {
            return Err(ProtoError::BadMagic);
        }
        let version = r.u16()?;
        if r.u8()? != 0 {
            return Err(ProtoError::Malformed("reserved byte"));
        }
        Ok(ClientHello { version })
    }
}

/// What the server sends back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerHello {
    /// Both sides will speak `version`.
    Accepted {
        /// The agreed version, never above what the client asked for.
        version: u16,
    },
    /// The connection is about to close.
    Refused(Refusal),
}

impl ServerHello {
    /// Serialize the four byte reply.
    pub fn encode(&self) -> [u8; SERVER_HELLO_LEN] {
        let mut out = [0u8; SERVER_HELLO_LEN];
        match self {
            ServerHello::Accepted { version } => {
                out[0] = 0x01;
                out[1..3].copy_from_slice(&version.to_le_bytes());
            }
            ServerHello::Refused(r) => {
                out[0] = 0x00;
                out[3] = *r as u8;
            }
        }
        out
    }

    /// Parse a four byte reply.
    pub fn decode(bytes: &[u8; SERVER_HELLO_LEN]) -> Result<Self> {
        match bytes[0] {
            0x01 => Ok(ServerHello::Accepted {
                version: u16::from_le_bytes([bytes[1], bytes[2]]),
            }),
            0x00 => Refusal::from_u8(bytes[3])
                .map(ServerHello::Refused)
                .ok_or(ProtoError::Malformed("refusal reason")),
            _ => Err(ProtoError::Malformed("handshake status")),
        }
    }
}

/// Decide what to answer a hello with.
///
/// Split out from any I/O so the version rule is testable on its own. The
/// agreed version is never above what the client asked for, which is what
/// lets a new server keep speaking to an old client.
pub fn negotiate(hello: ClientHello) -> ServerHello {
    if hello.version == 0 {
        ServerHello::Refused(Refusal::VersionTooOld)
    } else if hello.version > VERSION {
        ServerHello::Refused(Refusal::VersionTooNew)
    } else {
        ServerHello::Accepted {
            version: hello.version.min(VERSION),
        }
    }
}

/// Build a complete frame: header followed by body.
pub fn frame(msg_type: u8, body: &[u8]) -> Result<Vec<u8>> {
    if body.len() > MAX_BODY {
        return Err(ProtoError::TooLarge {
            declared: body.len() as u64,
            limit: MAX_BODY as u64,
        });
    }
    let mut w = Writer::with_capacity(HEADER_LEN + body.len());
    w.u8(msg_type);
    w.u32(body.len() as u32);
    let mut out = w.finish()?;
    out.extend_from_slice(body);
    Ok(out)
}
