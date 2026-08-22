//! QuantyDB, embedded in your process.
//!
//! ```no_run
//! # fn main() -> Result<(), quanty::Error> {
//! let mut db = quanty::Database::create("app.qdb")?;
//! db.execute("table users { id: int @key, name: text }")?;
//! db.transaction(|tx| {
//!     tx.execute("put users { id: 1, name: \"ada\" }")?;
//!     tx.execute("put users { id: 2, name: \"grace\" }")
//! })?;
//! for row in db.query("get users { name }")?.rows() {
//!     println!("{}", row[0]);
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # What this crate promises
//!
//! Everything exported here is semver-stable from 0.4 on: breaking it
//! takes a minor version and a CHANGELOG entry. Nothing from the internal
//! crates is re-exported, so no internal type reaches your signatures.
//!
//! Not covered by that promise: [`Value`] and [`Outcome`] are
//! `#[non_exhaustive]` and may gain variants; the on-disk format carries
//! its own version and its own compatibility rules; and the extension API
//! of ADR-018 is explicitly unstable before 1.0.
//!
//! # Rows as structs
//!
//! ```no_run
//! # fn main() -> Result<(), quanty::Error> {
//! #[derive(quanty::Row)]
//! #[quanty(table = "users")]
//! struct User {
//!     id: i64,
//!     name: String,
//! }
//!
//! let mut db = quanty::Database::open("app.qdb")?;
//! db.insert(&User { id: 1, name: "ada".into() })?;
//! let users: Vec<User> = db.query_as("get users { id, name }")?;
//! # Ok(())
//! # }
//! ```
//!
//! Values written this way travel as a parsed statement rather than as
//! text, so nothing has to be quoted or escaped. See ADR-030 for the
//! surface and ADR-031 for what the derive covers.

#![forbid(unsafe_code)]

mod row;

use std::fmt;
use std::path::Path;

use quanty_core::{FileStorage, MemStorage};
use quanty_exec::{ExecError, Output, Session};
use quanty_ql::ast::{Expr, Statement};

pub use quanty_derive::Row;
pub use row::{FromValue, IntoValue, Row};

#[doc(hidden)]
pub use row::__private;

/// A database, open and owned by this process.
///
/// Owning it is what makes `gc` safe to expose: every method that can
/// move a page takes `&mut self`, so the borrow checker proves no read is
/// outstanding while it runs. Reaching around an open transaction to run
/// one is therefore not an error at runtime, it is not a program:
///
/// ```compile_fail,E0499
/// let mut db = quanty::Database::in_memory().unwrap();
/// db.transaction(|tx| {
///     tx.execute("put t { id: 1 }")?;
///     db.gc(2)?; // second mutable borrow of `db`
///     Ok(())
/// }).unwrap();
/// ```
pub struct Database {
    backend: Backend,
}

/// Two storage backends behind one concrete type, so `Database` carries no
/// type parameter into an embedder's signatures.
enum Backend {
    File(Session<FileStorage>),
    Mem(Session<MemStorage>),
}

/// Runs `$m` on whichever backend is open.
macro_rules! on_backend {
    ($self:expr, $s:ident => $body:expr) => {
        match &mut $self.backend {
            Backend::File($s) => $body,
            Backend::Mem($s) => $body,
        }
    };
    (ref $self:expr, $s:ident => $body:expr) => {
        match &$self.backend {
            Backend::File($s) => $body,
            Backend::Mem($s) => $body,
        }
    };
}

impl Database {
    /// Create a database at `path`. Fails if a file is already there.
    pub fn create(path: impl AsRef<Path>) -> Result<Self> {
        let db = quanty_core::Db::create_file(path).map_err(Error::from_storage)?;
        Ok(Database {
            backend: Backend::File(Session::new(db)),
        })
    }

    /// Open an existing database at `path`, recovering it if the last
    /// process died mid-commit.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let db = quanty_core::Db::open_file(path).map_err(Error::from_storage)?;
        Ok(Database {
            backend: Backend::File(Session::new(db)),
        })
    }

    /// A database that never touches the disk. Useful for tests.
    pub fn in_memory() -> Result<Self> {
        let db = quanty_core::Db::in_memory().map_err(Error::from_storage)?;
        Ok(Database {
            backend: Backend::Mem(Session::new(db)),
        })
    }

    /// Run one QQL statement.
    pub fn execute(&mut self, source: &str) -> Result<Outcome> {
        on_backend!(self, s => s.execute(source))
            .map(Outcome::from)
            .map_err(Error::from)
    }

    /// Run one SQL statement, in the dialect of `docs/SQL.md`.
    pub fn execute_sql(&mut self, source: &str) -> Result<Outcome> {
        on_backend!(self, s => s.execute_sql(source))
            .map(Outcome::from)
            .map_err(Error::from)
    }

    /// Run one statement that is expected to return rows.
    ///
    /// Fails with [`ErrorKind::Exec`] if it returned something else, which
    /// is the common case of asking `execute` for rows and unwrapping.
    pub fn query(&mut self, source: &str) -> Result<Rows> {
        self.execute(source)?.into_rows()
    }

    /// Run one SQL statement that is expected to return rows.
    pub fn query_sql(&mut self, source: &str) -> Result<Rows> {
        self.execute_sql(source)?.into_rows()
    }

    /// Run one statement and turn its rows into structs.
    pub fn query_as<T: Row>(&mut self, source: &str) -> Result<Vec<T>> {
        self.query(source)?.into_typed()
    }

    /// Insert one row.
    pub fn insert<T: Row>(&mut self, row: &T) -> Result<()> {
        self.insert_all(std::slice::from_ref(row)).map(|_| ())
    }

    /// Insert several rows as one statement.
    ///
    /// The values travel as a parsed statement rather than as text, so
    /// nothing here has to quote or escape anything (ADR-031).
    pub fn insert_all<T: Row>(&mut self, rows: &[T]) -> Result<u64> {
        if rows.is_empty() {
            return Ok(0);
        }
        let put = Statement::Put {
            table: T::TABLE.to_string(),
            rows: rows
                .iter()
                .map(|row| {
                    row.to_values()
                        .into_iter()
                        .map(|(column, value)| {
                            (column.to_string(), Expr::Literal(into_core(value)))
                        })
                        .collect()
                })
                .collect(),
        };
        on_backend!(self, s => s.execute_ast(&put)).map_err(Error::from)?;
        Ok(rows.len() as u64)
    }

    /// Run `f` inside a transaction, committing if it returns `Ok` and
    /// rolling back if it returns `Err`.
    ///
    /// The [`Transaction`] borrows the database, so it cannot outlive it
    /// and cannot be held across anything that moves a page.
    pub fn transaction<T, F>(&mut self, f: F) -> Result<T>
    where
        F: FnOnce(&mut Transaction<'_>) -> Result<T>,
    {
        self.execute("begin")?;
        let mut tx = Transaction { db: self };
        match f(&mut tx) {
            Ok(value) => {
                self.execute("commit")?;
                Ok(value)
            }
            Err(e) => {
                // The rollback is best effort: reporting its failure would
                // hide the error that caused it.
                let _ = self.execute("rollback");
                Err(e)
            }
        }
    }

    /// Drop history, keeping the newest `keep` commits reachable, and free
    /// the pages nothing refers to any more.
    ///
    /// Refuses inside a transaction, because it commits on its own.
    pub fn gc(&mut self, keep: usize) -> Result<GcReport> {
        let report = on_backend!(self, s => s.gc(keep)).map_err(Error::from)?;
        Ok(GcReport {
            pruned_commits: report.pruned_commits,
            freed_pages: report.freed_pages,
            page_count: report.page_count,
        })
    }

    /// The branch this database is on.
    pub fn branch(&self) -> String {
        on_backend!(ref self, s => s.db().current_branch())
    }

    /// The commit id at the head of the current branch.
    pub fn head(&self) -> u64 {
        on_backend!(ref self, s => s.db().head_commit())
    }

    /// Whether a transaction is open.
    pub fn in_transaction(&self) -> bool {
        on_backend!(ref self, s => s.in_txn())
    }
}

impl fmt::Debug for Database {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Database")
            .field("branch", &self.branch())
            .field("head", &self.head())
            .finish()
    }
}

/// An open transaction, borrowed from the [`Database`] for the length of a
/// [`Database::transaction`] closure.
pub struct Transaction<'a> {
    db: &'a mut Database,
}

impl Transaction<'_> {
    /// Run one QQL statement inside the transaction.
    pub fn execute(&mut self, source: &str) -> Result<Outcome> {
        self.db.execute(source)
    }

    /// Run one SQL statement inside the transaction.
    pub fn execute_sql(&mut self, source: &str) -> Result<Outcome> {
        self.db.execute_sql(source)
    }

    /// Run one statement inside the transaction, expecting rows.
    pub fn query(&mut self, source: &str) -> Result<Rows> {
        self.db.query(source)
    }

    /// Run one SQL statement inside the transaction, expecting rows.
    pub fn query_sql(&mut self, source: &str) -> Result<Rows> {
        self.db.query_sql(source)
    }

    /// Run one statement inside the transaction, expecting structs.
    pub fn query_as<T: Row>(&mut self, source: &str) -> Result<Vec<T>> {
        self.db.query_as(source)
    }

    /// Insert one row inside the transaction.
    pub fn insert<T: Row>(&mut self, row: &T) -> Result<()> {
        self.db.insert(row)
    }

    /// Insert several rows inside the transaction, as one statement.
    pub fn insert_all<T: Row>(&mut self, rows: &[T]) -> Result<u64> {
        self.db.insert_all(rows)
    }
}

impl fmt::Debug for Transaction<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Transaction").finish_non_exhaustive()
    }
}

/// What one [`Database::gc`] run did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GcReport {
    /// Commits that fell out of retention this run.
    pub pruned_commits: u64,
    /// Pages handed back to the free list for later commits to reuse.
    pub freed_pages: u64,
    /// Pages in the database. `gc` reuses rather than shrinks, so this is
    /// what stops the file growing, not what makes it smaller.
    pub page_count: u64,
}

/// What a statement produced.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Outcome {
    /// It did its work and had nothing to report.
    Done,
    /// It touched `count` rows. `verb` is the past tense of what it did.
    Affected { verb: String, count: u64 },
    /// It returned rows.
    Rows(Rows),
    /// It reported in prose, one line at a time.
    Lines(Vec<String>),
}

impl Outcome {
    /// The rows, or an error naming what came back instead.
    pub fn into_rows(self) -> Result<Rows> {
        match self {
            Outcome::Rows(rows) => Ok(rows),
            other => Err(Error {
                kind: ErrorKind::Exec,
                message: format!("statement returned {}, not rows", other.describe()),
            }),
        }
    }

    fn describe(&self) -> &'static str {
        match self {
            Outcome::Done => "no result",
            Outcome::Affected { .. } => "a row count",
            Outcome::Rows(_) => "rows",
            Outcome::Lines(_) => "text",
        }
    }
}

impl From<Output> for Outcome {
    fn from(output: Output) -> Self {
        match output {
            Output::Ok => Outcome::Done,
            Output::Count { verb, n } => Outcome::Affected {
                verb: verb.to_string(),
                count: n,
            },
            Output::Rows { columns, rows } => Outcome::Rows(Rows {
                columns,
                rows: rows
                    .into_iter()
                    .map(|row| row.into_iter().map(Value::from).collect())
                    .collect(),
            }),
            Output::Lines(lines) => Outcome::Lines(lines),
        }
    }
}

/// Rows and the names of their columns.
///
/// Names are qualified as `table.column` when the statement read from more
/// than one table, and bare otherwise.
#[derive(Debug, Clone, PartialEq)]
pub struct Rows {
    columns: Vec<String>,
    rows: Vec<Vec<Value>>,
}

impl Rows {
    /// The column names, in the order the values arrive.
    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    /// The rows.
    pub fn rows(&self) -> &[Vec<Value>] {
        &self.rows
    }

    /// How many rows came back.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether no rows came back.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The position of `name` among the columns, matching the qualified
    /// name first and the bare one after it.
    pub fn column(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| c == name)
            .or_else(|| self.columns.iter().position(|c| suffix_matches(c, name)))
    }

    /// Take the rows, dropping the column names.
    pub fn into_rows(self) -> Vec<Vec<Value>> {
        self.rows
    }

    /// Turn the rows into structs.
    ///
    /// Each of `T::COLUMNS` is resolved to a position once, so a struct
    /// whose fields are in a different order than the query asked for
    /// still works, and a missing column fails before any row is built.
    pub fn into_typed<T: Row>(self) -> Result<Vec<T>> {
        let mut positions = Vec::with_capacity(T::COLUMNS.len());
        for want in T::COLUMNS {
            match self.column(want) {
                Some(i) => positions.push(i),
                None => {
                    return Err(Error {
                        kind: ErrorKind::Exec,
                        message: format!(
                            "no column {want} in this result; it has {}",
                            self.columns.join(", ")
                        ),
                    })
                }
            }
        }
        self.rows
            .into_iter()
            .map(|mut row| {
                let values = positions
                    .iter()
                    .map(|&i| std::mem::replace(&mut row[i], Value::Null))
                    .collect();
                T::from_values(values)
            })
            .collect()
    }
}

impl<'a> IntoIterator for &'a Rows {
    type Item = &'a Vec<Value>;
    type IntoIter = std::slice::Iter<'a, Vec<Value>>;

    fn into_iter(self) -> Self::IntoIter {
        self.rows.iter()
    }
}

impl IntoIterator for Rows {
    type Item = Vec<Value>;
    type IntoIter = std::vec::IntoIter<Vec<Value>>;

    fn into_iter(self) -> Self::IntoIter {
        self.rows.into_iter()
    }
}

/// Back to the engine's value type. The reverse conversion is a move
/// rather than a copy, because the variants line up one to one.
fn into_core(v: Value) -> quanty_core::Value {
    match v {
        Value::Null => quanty_core::Value::Null,
        Value::Bool(b) => quanty_core::Value::Bool(b),
        Value::Int(i) => quanty_core::Value::Int(i),
        Value::Float(f) => quanty_core::Value::Float(f),
        Value::Text(t) => quanty_core::Value::Text(t),
        Value::Bytes(b) => quanty_core::Value::Bytes(b),
    }
}

/// Whether `qualified` is `something.name`.
fn suffix_matches(qualified: &str, name: &str) -> bool {
    match qualified.rfind('.') {
        Some(dot) => &qualified[dot + 1..] == name,
        None => false,
    }
}

/// One value in a row.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
}

impl From<quanty_core::Value> for Value {
    fn from(v: quanty_core::Value) -> Self {
        match v {
            quanty_core::Value::Null => Value::Null,
            quanty_core::Value::Bool(b) => Value::Bool(b),
            quanty_core::Value::Int(i) => Value::Int(i),
            quanty_core::Value::Float(f) => Value::Float(f),
            quanty_core::Value::Text(t) => Value::Text(t),
            quanty_core::Value::Bytes(b) => Value::Bytes(b),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(x) => write!(f, "{x}"),
            Value::Text(t) => write!(f, "{t}"),
            Value::Bytes(b) => {
                for byte in b {
                    write!(f, "{byte:02x}")?;
                }
                Ok(())
            }
        }
    }
}

/// The result of anything in this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// What went wrong, in the words the database used.
#[derive(Debug, Clone, PartialEq)]
pub struct Error {
    kind: ErrorKind,
    message: String,
}

impl Error {
    /// Which layer refused.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    fn from_storage(e: quanty_core::Error) -> Self {
        Error {
            kind: ErrorKind::Storage,
            message: e.to_string(),
        }
    }
}

/// Which layer an [`Error`] came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The statement did not parse.
    Parse,
    /// It parsed but could not be planned: unknown table or column, or a
    /// schema that does not allow it.
    Plan,
    /// It ran and failed: type error, overflow, duplicate key.
    Exec,
    /// The storage layer failed underneath.
    Storage,
}

impl From<ExecError> for Error {
    fn from(e: ExecError) -> Self {
        let kind = match &e {
            ExecError::Parse(_) => ErrorKind::Parse,
            ExecError::Plan(_) => ErrorKind::Plan,
            ExecError::Exec(_) => ErrorKind::Exec,
            ExecError::Storage(_) => ErrorKind::Storage,
        };
        Error {
            kind,
            message: e.to_string(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}
