//! Who is allowed to talk to the server, and how that is taken away again.
//!
//! ADR-026 decides the shape: token hashes live in a file beside the
//! database, never in it. Branching and `as of` would otherwise make
//! "revoked" true only at the tip of one branch, and history would keep
//! handing back the hash of a token that was supposed to be gone.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

mod tokens;

pub use quanty_core::{from_hex, sha256, to_hex};
pub use tokens::{mint, Tokens, TokensError};
