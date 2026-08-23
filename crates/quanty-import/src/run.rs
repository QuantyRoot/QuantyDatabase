//! The second pass: writing what the first pass decided.
//!
//! Everything that could be a judgement call was made in `plan`, so this
//! module has no opinions left to have. It creates the tables the plan
//! names, reads the source rows, converts each value to the type the plan
//! chose, and writes them. If it finds something the plan did not predict,
//! that is a disagreement between the two passes rather than a decision to
//! make here, and it stops and says so.
//!
//! Rows go in batches, each of which is one statement and therefore one
//! transaction. That bounds the memory an import needs regardless of how
//! large the source is, and it means a failure part way through leaves a
//! partial database rather than an empty one. That is the deliberate trade:
//! the plan pass exists precisely so that the failures worth knowing about
//! are known before a single row is written, and an import writes into a
//! fresh database that can simply be discarded.

use std::fmt;

use quanty_core::{Storage, Value};
use quanty_exec::{ExecError, Session};
use quanty_ql::ast::{ColumnDef, Expr, Statement, TableDef, TypeName};
use quanty_sqlite::{MappedCell, Reader, RowLayout, Source, SqliteError, SqliteValue};

use crate::plan::{ColumnPlan, ImportPlan, TablePlan, ValueSource};

/// Rows per statement.
///
/// Large enough that the per statement overhead disappears, small enough
/// that one batch of rows is a bounded amount of memory whatever the source
/// looks like.
const BATCH: usize = 256;

/// The largest integer a double can hold without losing anything.
const EXACT_INTEGER_LIMIT: i64 = 1 << 53;

#[derive(Debug)]
pub enum ImportError {
    Source(SqliteError),
    Write(ExecError),
    /// The data did not match the plan. Names the row so it can be looked
    /// at, because this means one of the two passes is wrong.
    Unplanned {
        table: String,
        column: String,
        rowid: Option<i64>,
        reason: String,
    },
}

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ImportError::Source(e) => write!(f, "reading the source: {e}"),
            ImportError::Write(e) => write!(f, "writing the database: {e}"),
            ImportError::Unplanned {
                table,
                column,
                rowid,
                reason,
            } => {
                write!(f, "{table}.{column}")?;
                if let Some(rowid) = rowid {
                    write!(f, " (row {rowid})")?;
                }
                write!(f, ": {reason}")
            }
        }
    }
}

impl std::error::Error for ImportError {}

impl From<SqliteError> for ImportError {
    fn from(e: SqliteError) -> Self {
        ImportError::Source(e)
    }
}

impl From<ExecError> for ImportError {
    fn from(e: ExecError) -> Self {
        ImportError::Write(e)
    }
}

/// What the import actually did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub tables: Vec<TableReport>,
}

impl Report {
    pub fn rows(&self) -> u64 {
        self.tables.iter().map(|t| t.rows).sum()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableReport {
    pub source_name: String,
    pub name: String,
    /// Rows written, which is checked against the count the plan predicted.
    pub rows: u64,
    pub indexes: Vec<String>,
}

/// Execute `plan` against `session`, reading rows from `reader`.
pub fn execute<S: Source, T: Storage>(
    reader: &Reader<S>,
    plan: &ImportPlan,
    session: &mut Session<T>,
) -> Result<Report, ImportError> {
    let mut tables = Vec::new();
    for table in &plan.tables {
        tables.push(import_table(reader, table, session)?);
    }
    Ok(Report { tables })
}

fn import_table<S: Source, T: Storage>(
    reader: &Reader<S>,
    table: &TablePlan,
    session: &mut Session<T>,
) -> Result<TableReport, ImportError> {
    session.execute_ast(&Statement::TableDef(TableDef {
        name: table.name.clone(),
        columns: table
            .columns
            .iter()
            .map(|column| ColumnDef {
                name: column.name.clone(),
                ty: column.ty,
                nullable: column.nullable,
                key: column.key,
                index: column.indexed,
                // SQLite has no full text column attribute to carry over,
                // and guessing which imported text wants an inverted index
                // is not the importer's call.
                text: false,
                default: column.default.clone(),
            })
            .collect(),
    }))?;

    // the layout is what turns record positions into declared columns, and
    // it is read from the source's own create statement rather than guessed
    let object = reader
        .schema()?
        .object(&table.source_name)
        .cloned()
        .ok_or_else(|| ImportError::Unplanned {
            table: table.source_name.clone(),
            column: String::new(),
            rowid: None,
            reason: "the plan names a table the schema does not have".to_string(),
        })?;
    let def = object.table_def()?;
    let layout = RowLayout::new(&def);

    let mut written = 0u64;
    let mut batch: Vec<Vec<(String, Expr)>> = Vec::with_capacity(BATCH);
    for row in reader.rows(table.root_page)? {
        let row = row?;
        let mut values = Vec::with_capacity(table.columns.len());
        for column in &table.columns {
            let value = value_for(table, column, &row, &layout)?;
            values.push((column.name.clone(), Expr::Literal(value)));
        }
        batch.push(values);

        if batch.len() == BATCH {
            written += flush(session, &table.name, &mut batch)?;
        }
    }
    written += flush(session, &table.name, &mut batch)?;

    if written != table.rows {
        return Err(ImportError::Unplanned {
            table: table.source_name.clone(),
            column: String::new(),
            rowid: None,
            reason: format!(
                "the plan counted {} rows and the import wrote {written}",
                table.rows
            ),
        });
    }

    Ok(TableReport {
        source_name: table.source_name.clone(),
        name: table.name.clone(),
        rows: written,
        indexes: table
            .columns
            .iter()
            .filter(|c| c.indexed)
            .map(|c| c.name.clone())
            .collect(),
    })
}

fn flush<T: Storage>(
    session: &mut Session<T>,
    table: &str,
    batch: &mut Vec<Vec<(String, Expr)>>,
) -> Result<u64, ImportError> {
    if batch.is_empty() {
        return Ok(0);
    }
    let rows = batch.len() as u64;
    session.execute_ast(&Statement::Put {
        table: table.to_string(),
        rows: std::mem::take(batch),
    })?;
    batch.reserve(BATCH);
    Ok(rows)
}

/// One column of one row, converted to the type the plan chose.
fn value_for(
    table: &TablePlan,
    column: &ColumnPlan,
    row: &quanty_sqlite::Row,
    layout: &RowLayout,
) -> Result<Value, ImportError> {
    let unplanned = |reason: String| ImportError::Unplanned {
        table: table.source_name.clone(),
        column: column.source_name.clone(),
        rowid: row.rowid,
        reason,
    };

    let stored = match column.source {
        ValueSource::Rowid => {
            return match row.rowid {
                Some(rowid) => Ok(Value::Int(rowid)),
                None => Err(unplanned(
                    "the plan uses the rowid as a key, but this row has none".to_string(),
                )),
            }
        }
        ValueSource::Declared(index) => layout.cell(row, index),
    };

    match stored {
        MappedCell::Rowid(rowid) => Ok(Value::Int(rowid)),
        // a record that ends early leaves its trailing columns to the
        // declared default, which the plan already turned into ours
        MappedCell::Missing => Ok(column.default.clone().unwrap_or(Value::Null)),
        MappedCell::Virtual => Err(unplanned(
            "the plan reads a virtual generated column, which holds no data".to_string(),
        )),
        MappedCell::Value(value) => convert(value, column.ty).ok_or_else(|| {
            unplanned(format!(
                "the plan chose {} for this column and the row holds {}",
                crate::plan::type_name(column.ty),
                value.type_name()
            ))
        }),
    }
}

/// Convert a stored value to the planned type, or `None` when it does not
/// belong there.
///
/// The widening rules are ADR-019's, applied here rather than decided here.
/// Two of them are worth spelling out because they are lossy in a way the
/// report has to be able to describe:
///
/// An integer becomes a float when the column mixes both, which is exact up
/// to 2^53 and not beyond, so past that this stops instead.
///
/// A number becomes text when the column mixes a number with text. Integers
/// render as decimal. Floats render as the shortest string that reads back
/// as the same double, which is not always what sqlite's own printf would
/// produce, but it is the rendering that does not lose anything.
fn convert(value: &SqliteValue, ty: TypeName) -> Option<Value> {
    Some(match (value, ty) {
        (SqliteValue::Null, _) => Value::Null,

        (SqliteValue::Integer(n), TypeName::Int) => Value::Int(*n),
        (SqliteValue::Integer(n), TypeName::Float) => {
            if n.unsigned_abs() > EXACT_INTEGER_LIMIT as u64 {
                return None;
            }
            Value::Float(*n as f64)
        }
        (SqliteValue::Integer(n), TypeName::Text) => Value::Text(n.to_string()),
        (SqliteValue::Integer(n), TypeName::Bytes) => Value::Bytes(n.to_string().into_bytes()),

        (SqliteValue::Real(f), TypeName::Float) => Value::Float(*f),
        (SqliteValue::Real(f), TypeName::Text) => Value::Text(render_float(*f)),
        (SqliteValue::Real(f), TypeName::Bytes) => Value::Bytes(render_float(*f).into_bytes()),

        (SqliteValue::Text(t), TypeName::Text) => Value::Text(t.clone()),
        (SqliteValue::Text(t), TypeName::Bytes) => Value::Bytes(t.clone().into_bytes()),

        (SqliteValue::Blob(b), TypeName::Bytes) => Value::Bytes(b.clone()),

        _ => return None,
    })
}

/// A float as text, in the shortest form that reads back as itself.
///
/// The special values have no literal in our language, and a column that
/// holds one has a bigger problem than its formatting, so they are named in
/// full rather than rendered as something that would parse.
fn render_float(f: f64) -> String {
    if f.is_nan() {
        "NaN".to_string()
    } else if f.is_infinite() {
        if f.is_sign_negative() {
            "-Infinity".to_string()
        } else {
            "Infinity".to_string()
        }
    } else {
        format!("{f}")
    }
}
