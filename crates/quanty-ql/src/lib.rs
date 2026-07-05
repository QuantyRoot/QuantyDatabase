//! quanty-ql: the language front ends.
//!
//! Two surfaces, one AST. QQL is the native language: lexer, recursive
//! descent parser and a canonical pretty printer (docs/QQL.md). SQL is the
//! second front end and lowers onto the same AST (docs/SQL.md). Pure
//! syntax either way: this crate knows nothing about storage or catalogs;
//! the planner and executor live in quanty-exec.

pub mod ast;
mod error;
mod lexer;
mod parser;
pub mod pretty;
pub mod sql;

pub use error::ParseError;
pub use parser::parse;
pub use pretty::pretty;
pub use sql::parse_sql;
