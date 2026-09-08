//! The SQL front end. Pure syntax: it lowers onto the same AST as QQL and
//! knows nothing about catalogs or storage. See docs/SQL.md for the dialect
//! and ADR-014 for the semantics decisions.

mod lexer;
mod parser;

pub use parser::parse_sql;
