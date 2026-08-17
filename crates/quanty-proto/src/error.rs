//! Errors for the wire protocol.

use std::fmt;

/// Result of any codec operation.
pub type Result<T> = std::result::Result<T, ProtoError>;

/// A failure decoding or encoding protocol bytes.
#[derive(Debug, Clone, PartialEq)]
pub enum ProtoError {
    /// The buffer ended before the value being read did.
    Truncated {
        /// Bytes the read wanted.
        needed: usize,
        /// Bytes actually left.
        available: usize,
    },
    /// A declared length exceeds what the protocol allows, or what the
    TooLarge {
        /// What the wire claimed.
        declared: u64,
        /// What was permitted.
        limit: u64,
    },
    /// A declared element count exceeds the protocol's cap for that
    TooManyElements {
        /// What the wire claimed.
        declared: u64,
        /// What the protocol permits.
        limit: u64,
    },
    /// A tag or message type byte that this version does not define.
    UnknownTag(u8),
    /// A field held a value outside its permitted set, e.g. a bool that was
    Malformed(&'static str),
    /// Text that claimed to be UTF-8 and was not.
    BadUtf8,
    /// The handshake magic did not match.
    BadMagic,
    /// The peer asked for a protocol version this build does not speak.
    UnsupportedVersion(u16),
    /// Bytes remained in a body after its message was fully decoded.
    TrailingBytes(usize),
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtoError::Truncated { needed, available } => {
                write!(f, "truncated: needed {needed} bytes, had {available}")
            }
            ProtoError::TooLarge { declared, limit } => {
                write!(f, "declared length {declared} exceeds limit {limit}")
            }
            ProtoError::TooManyElements { declared, limit } => {
                write!(f, "declared {declared} elements, protocol allows {limit}")
            }
            ProtoError::UnknownTag(t) => write!(f, "unknown tag 0x{t:02x}"),
            ProtoError::Malformed(what) => write!(f, "malformed {what}"),
            ProtoError::BadUtf8 => write!(f, "invalid utf-8 in text"),
            ProtoError::BadMagic => write!(f, "not a quanty client"),
            ProtoError::UnsupportedVersion(v) => write!(f, "unsupported protocol version {v}"),
            ProtoError::TrailingBytes(n) => write!(f, "{n} unread bytes at end of body"),
        }
    }
}

impl std::error::Error for ProtoError {}

/// The `u16` sent inside an `Error` message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum ErrorCode {
    /// Frame or encoding was malformed.
    Protocol = 0x0001,
    /// The requested protocol version is not spoken here.
    UnsupportedVersion = 0x0002,
    /// A statement arrived before a successful `Auth`.
    NotAuthenticated = 0x0003,
    /// The token was rejected.
    AuthFailed = 0x0004,
    /// The statement did not parse.
    Parse = 0x0005,
    /// The statement parsed and failed while running.
    Execution = 0x0006,
    /// The single writer (ADR-003) refused the statement rather than
    WriteQueue = 0x0007,
    /// The server is closing and will accept no more statements.
    ShuttingDown = 0x0008,
}

impl ErrorCode {
    /// The number that travels.
    pub fn as_u16(self) -> u16 {
        self as u16
    }

    /// Map a number from the wire back to a code.
    pub fn from_u16(v: u16) -> Option<Self> {
        Some(match v {
            0x0001 => ErrorCode::Protocol,
            0x0002 => ErrorCode::UnsupportedVersion,
            0x0003 => ErrorCode::NotAuthenticated,
            0x0004 => ErrorCode::AuthFailed,
            0x0005 => ErrorCode::Parse,
            0x0006 => ErrorCode::Execution,
            0x0007 => ErrorCode::WriteQueue,
            0x0008 => ErrorCode::ShuttingDown,
            _ => return None,
        })
    }
}
