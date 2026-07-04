//! quanty-exec: catalog, planner and executor for QuantyDB.
//!
//! Takes a parsed QQL statement and runs it against a quanty-core
//! database: typed rows over the data tree, table definitions in the
//! catalog tree, secondary indexes kept in sync, access path planning
//! with `explain` to prove it.

mod catalog;
mod error;
mod exec;
mod plan;
mod value_ops;
mod verify;

pub use catalog::{Column, Table};
pub use error::ExecError;
pub use exec::{Output, Session};
pub use plan::{Access, AccessPlan};
pub use verify::verify_indexes;
