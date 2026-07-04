//! quanty-ql: the QQL front end.
//!
//! Lexer, recursive descent parser, AST and a canonical pretty printer.
//! Pure syntax: this crate knows nothing about storage or catalogs; the
//! planner and executor live in quanty-exec. Grammar and semantics are
//! documented in docs/QQL.md.

pub mod ast;
mod error;
mod lexer;
mod parser;
pub mod pretty;

pub use error::ParseError;
pub use parser::parse;
pub use pretty::pretty;
