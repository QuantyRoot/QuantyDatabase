//! quanty-import: turning a SQLite database into a QuantyDB one.
//!
//! The import runs in two passes (ADR-019). The first reads the whole
//! source and decides what it becomes, writing nothing; the second executes
//! that decision. This crate holds both, and the first one is useful on its
//! own: it is what `--dry-run` prints.
//!
//! The goal it is built to is that an ordinary `.sqlite` file, from
//! whatever codebase, imports with one command and no further work. So
//! where a choice has to be made it is made rather than refused, and every
//! choice lands in a report the developer can read afterwards. `--strict`
//! turns those choices back into refusals.

mod default;
mod name;
mod plan;

pub use plan::{plan, ColumnPlan, ImportPlan, Note, Options, Problem, TablePlan, ValueSource};
