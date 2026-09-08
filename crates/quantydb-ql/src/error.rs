//! Parse errors with byte positions.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub at: Option<usize>,
    pub message: String,
}

impl ParseError {
    pub fn at(at: usize, message: impl Into<String>) -> Self {
        ParseError {
            at: Some(at),
            message: message.into(),
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.at {
            Some(at) => write!(f, "parse error at byte {at}: {}", self.message),
            None => write!(f, "parse error: {}", self.message),
        }
    }
}

impl std::error::Error for ParseError {}
