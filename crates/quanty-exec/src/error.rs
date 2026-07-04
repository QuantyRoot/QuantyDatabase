//! Execution errors.

use std::fmt;

use quanty_ql::ParseError;

#[derive(Debug)]
pub enum ExecError {
    /// The statement did not parse.
    Parse(ParseError),
    /// The statement parsed but cannot be planned (unknown table, unknown
    /// column, bad schema).
    Plan(String),
    /// The statement failed while running (type error, overflow, duplicate
    /// key, missing value).
    Exec(String),
    /// The storage layer failed underneath us.
    Storage(quanty_core::Error),
}

impl ExecError {
    pub(crate) fn plan(message: impl Into<String>) -> Self {
        ExecError::Plan(message.into())
    }

    pub(crate) fn exec(message: impl Into<String>) -> Self {
        ExecError::Exec(message.into())
    }
}

impl fmt::Display for ExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExecError::Parse(e) => write!(f, "{e}"),
            ExecError::Plan(m) | ExecError::Exec(m) => write!(f, "{m}"),
            ExecError::Storage(e) => write!(f, "storage error: {e}"),
        }
    }
}

impl std::error::Error for ExecError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ExecError::Parse(e) => Some(e),
            ExecError::Storage(e) => Some(e),
            _ => None,
        }
    }
}

impl From<ParseError> for ExecError {
    fn from(e: ParseError) -> Self {
        ExecError::Parse(e)
    }
}

impl From<quanty_core::Error> for ExecError {
    fn from(e: quanty_core::Error) -> Self {
        ExecError::Storage(e)
    }
}
