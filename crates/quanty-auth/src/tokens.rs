//! The token file: what it looks like, and how a revocation takes effect.

use std::collections::HashSet;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::sha256::{from_hex, sha256, to_hex};

/// Why a token file could not be used.
#[derive(Debug)]
pub enum TokensError {
    /// The file could not be read.
    Io(io::Error),
    /// A line was not a hash and a label.
    Malformed {
        /// One based, so it matches an editor.
        line: usize,
        /// What is wrong with it.
        detail: &'static str,
    },
}

impl fmt::Display for TokensError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokensError::Io(e) => write!(f, "{e}"),
            TokensError::Malformed { line, detail } => write!(f, "line {line}: {detail}"),
        }
    }
}

impl std::error::Error for TokensError {}

/// The tokens a server accepts, and where they came from.
///
/// A failure to reload is deliberately not fatal and deliberately not
/// ignored: the last good set stays in force and the caller is told. The
/// safe direction for a broken credential file is to keep refusing what it
/// was already refusing, not to fall open and not to fall over.
#[derive(Debug)]
pub struct Tokens {
    path: PathBuf,
    accepted: HashSet<[u8; 32]>,
    seen: Option<SystemTime>,
}

impl Tokens {
    /// Read a token file.
    pub fn load(path: impl Into<PathBuf>) -> Result<Tokens, TokensError> {
        let path = path.into();
        let mut tokens = Tokens {
            path,
            accepted: HashSet::new(),
            seen: None,
        };
        tokens.read()?;
        Ok(tokens)
    }

    /// Whether this token is one of them.
    pub fn accepts(&self, token: &[u8]) -> bool {
        self.accepted.contains(&sha256(token))
    }

    /// How many tokens are in force.
    pub fn len(&self) -> usize {
        self.accepted.len()
    }

    /// Whether the file left nothing that can authenticate.
    pub fn is_empty(&self) -> bool {
        self.accepted.is_empty()
    }

    /// Where they came from.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Re-read the file if it changed, so revoking a token does not need a
    /// restart. Returns whether anything was re-read.
    ///
    /// A file whose modification time went backwards counts as changed:
    /// restoring an older file is exactly the case where reading it again
    /// matters most.
    pub fn reload_if_changed(&mut self) -> Result<bool, TokensError> {
        let now = modified(&self.path);
        if now == self.seen && now.is_some() {
            return Ok(false);
        }
        self.read()?;
        Ok(true)
    }

    fn read(&mut self) -> Result<(), TokensError> {
        let stamp = modified(&self.path);
        let text = fs::read_to_string(&self.path).map_err(TokensError::Io)?;
        let mut accepted = HashSet::new();
        for (index, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let mut parts = line.split_whitespace();
            let hash = parts.next().unwrap_or_default();
            let digest = from_hex(hash).ok_or(TokensError::Malformed {
                line: index + 1,
                detail: "expected sixty four hex characters",
            })?;
            if parts.next().is_none() {
                return Err(TokensError::Malformed {
                    line: index + 1,
                    detail: "a hash needs a label after it",
                });
            }
            accepted.insert(digest);
        }
        self.accepted = accepted;
        self.seen = stamp;
        Ok(())
    }
}

fn modified(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// A fresh token, and the line that lets a server accept it.
///
/// The token is never stored anywhere by this function. Whoever calls it
/// has the only copy and has to hand it to its owner.
pub fn mint(label: &str) -> io::Result<(String, String)> {
    let raw = read_random()?;
    let token = to_hex(&raw);
    let line = format!("{} {}", to_hex(&sha256(token.as_bytes())), label);
    Ok((token, line))
}

fn read_random() -> io::Result<[u8; 32]> {
    use std::io::Read;
    let mut out = [0u8; 32];
    fs::File::open("/dev/urandom")?.read_exact(&mut out)?;
    Ok(out)
}
