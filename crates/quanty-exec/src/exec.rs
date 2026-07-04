//! Statement execution.
//!
//! One statement = one storage transaction. Errors abort the whole
//! statement (the transaction is dropped, nothing sticks). Read-only
//! statements never commit, so they never burn commit ids.
//!
//! Physical layout of the logical layer, all in the data tree:
//!
//! - row:   key `(table_id, pk values...)`, value = tuple-encoded columns
//! - index: key `(index_id, column value, pk values...)`, value empty
//!
//! Tuple encoding is order preserving and prefix friendly, so table scans
//! and index probes are plain range scans.

use quanty_core::{decode_key, encode_key, Db, Storage, Value, WriteTx};
use quanty_ql::ast::{Direction, Expr, Get, Statement};

use crate::catalog::{self, Table};
use crate::error::ExecError;
use crate::plan::{self, Access, AccessPlan, ExplainNode};
use crate::value_ops::{self, NoScope, Scope};

/// Fetched rows: `(row key, decoded column values)` pairs.
type Fetched = Vec<(Vec<u8>, Vec<Value>)>;

pub struct Session<S: Storage> {
    db: Db<S>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    Ok,
    Count { verb: &'static str, n: u64 },
    Rows(Vec<Vec<Value>>),
    Lines(Vec<String>),
}

impl Output {
    pub fn render(&self) -> String {
        match self {
            Output::Ok => "ok".to_string(),
            Output::Count { verb, n } => format!("{verb} {n}"),
            Output::Rows(rows) => {
                let lines: Vec<String> = rows
                    .iter()
                    .map(|row| {
                        row.iter()
                            .map(value_ops::render)
                            .collect::<Vec<_>>()
                            .join("|")
                    })
                    .collect();
                lines.join("\n")
            }
            Output::Lines(lines) => lines.join("\n"),
        }
    }
}

impl<S: Storage> Session<S> {
    pub fn new(db: Db<S>) -> Self {
        Session { db }
    }

    pub fn db(&self) -> &Db<S> {
        &self.db
    }

    /// Parse and execute one statement.
    pub fn execute(&self, source: &str) -> Result<Output, ExecError> {
        let stmt = quanty_ql::parse(source)?;
        let tx = self.db.begin();
        let mut run = Run { tx, mutated: false };
        let output = run.statement(&stmt)?;
        if run.mutated {
            run.tx.commit()?;
        }
        Ok(output)
    }
}

/// One statement's execution state.
struct Run<'db, S: Storage> {
    tx: WriteTx<'db, S>,
    mutated: bool,
}

impl<S: Storage> Run<'_, S> {
    fn statement(&mut self, stmt: &Statement) -> Result<Output, ExecError> {
        match stmt {
            Statement::TableDef(def) => self.create_table(def),
            Statement::DropTable { name } => self.drop_table(name),
            Statement::Put { table, rows } => self.put(table, rows),
            Statement::Get(get) => self.get(get),
            Statement::Set {
                table,
                filter,
                assigns,
            } => self.set(table, filter.as_ref(), assigns),
            Statement::Del { table, filter } => self.del(table, filter.as_ref()),
            Statement::IndexDef { table, column } => self.create_index(table, column),
            Statement::ShowTables => self.show_tables(),
            Statement::Explain(inner) => self.explain(inner),
        }
    }

    // -----------------------------------------------------------------
    // catalog plumbing
    // -----------------------------------------------------------------

    fn load_table(&self, name: &str) -> Result<Table, ExecError> {
        match self.tx.catalog_get(&catalog::table_key(name))? {
            Some(bytes) => Table::deserialize(&bytes),
            None => Err(ExecError::plan(format!("no table named '{name}'"))),
        }
    }

    fn store_table(&mut self, table: &Table) -> Result<(), ExecError> {
        self.tx
            .catalog_put(&catalog::table_key(&table.name), &table.serialize())?;
        self.mutated = true;
        Ok(())
    }

    fn alloc_id(&mut self) -> Result<u64, ExecError> {
        let next = match self.tx.catalog_get(&catalog::seq_key())? {
            Some(bytes) => u64::from_le_bytes(
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| ExecError::exec("broken id counter"))?,
            ),
            None => 1,
        };
        self.tx
            .catalog_put(&catalog::seq_key(), &(next + 1).to_le_bytes())?;
        self.mutated = true;
        Ok(next)
    }

    // -----------------------------------------------------------------
    // DDL
    // -----------------------------------------------------------------

    fn create_table(&mut self, def: &quanty_ql::ast::TableDef) -> Result<Output, ExecError> {
        if self
            .tx
            .catalog_get(&catalog::table_key(&def.name))?
            .is_some()
        {
            return Err(ExecError::plan(format!(
                "table '{}' already exists",
                def.name
            )));
        }
        let id = self.alloc_id()?;
        let mut table = Table::from_ast(def, id)?;
        for i in 0..table.columns.len() {
            if def.columns[i].index {
                table.columns[i].index_id = Some(self.alloc_id()?);
            }
        }
        self.store_table(&table)?;
        Ok(Output::Ok)
    }

    fn drop_table(&mut self, name: &str) -> Result<Output, ExecError> {
        let table = self.load_table(name)?;
        self.delete_range(&table_prefix(table.id))?;
        for col in &table.columns {
            if let Some(index_id) = col.index_id {
                self.delete_range(&index_prefix(index_id))?;
            }
        }
        self.tx.catalog_delete(&catalog::table_key(name))?;
        self.mutated = true;
        Ok(Output::Ok)
    }

    fn create_index(&mut self, table_name: &str, column: &str) -> Result<Output, ExecError> {
        let mut table = self.load_table(table_name)?;
        let pos = table.column_position(column).ok_or_else(|| {
            ExecError::plan(format!("table '{table_name}' has no column '{column}'"))
        })?;
        if table.columns[pos].index_id.is_some() {
            return Err(ExecError::plan(format!(
                "'{table_name}.{column}' is already indexed"
            )));
        }
        let index_id = self.alloc_id()?;
        table.columns[pos].index_id = Some(index_id);

        // backfill from existing rows
        let rows = self.fetch(
            &table,
            &AccessPlan {
                access: Access::SeqScan,
                residual: None,
            },
        )?;
        for (_, values) in &rows {
            let entry = index_entry_key(index_id, &values[pos], &row_pk(&table, values));
            self.tx.put(&entry, &[])?;
        }
        self.store_table(&table)?;
        Ok(Output::Ok)
    }

    fn show_tables(&self) -> Result<Output, ExecError> {
        let prefix = catalog::tables_prefix();
        let end = key_successor(&prefix);
        let mut names = Vec::new();
        for item in self.tx.catalog_scan(Some(&prefix), end.as_deref())? {
            let (_, bytes) = item?;
            names.push(Table::deserialize(&bytes)?.name);
        }
        Ok(Output::Lines(names))
    }

    // -----------------------------------------------------------------
    // DML
    // -----------------------------------------------------------------

    fn put(&mut self, table_name: &str, rows: &[Vec<(String, Expr)>]) -> Result<Output, ExecError> {
        let table = self.load_table(table_name)?;
        let mut count = 0u64;
        for row in rows {
            let values = self.build_row(&table, row)?;
            let key = row_key(&table, &row_pk(&table, &values));
            if self.tx.get(&key)?.is_some() {
                let pk = row_pk(&table, &values);
                let rendered: Vec<String> = pk.iter().map(quanty_ql::pretty::literal).collect();
                return Err(ExecError::exec(format!(
                    "duplicate key ({}) in table '{table_name}'",
                    rendered.join(", ")
                )));
            }
            self.tx.put(&key, &encode_key(&values))?;
            for (pos, col) in table.columns.iter().enumerate() {
                if let Some(index_id) = col.index_id {
                    let entry = index_entry_key(index_id, &values[pos], &row_pk(&table, &values));
                    self.tx.put(&entry, &[])?;
                }
            }
            self.mutated = true;
            count += 1;
        }
        Ok(Output::Count {
            verb: "put",
            n: count,
        })
    }

    /// Assemble a full row from the fields of a `put`, applying defaults
    /// and coercions, rejecting unknowns and holes.
    fn build_row(&self, table: &Table, fields: &[(String, Expr)]) -> Result<Vec<Value>, ExecError> {
        let mut values: Vec<Option<Value>> = vec![None; table.columns.len()];
        for (name, expr) in fields {
            let pos = table.column_position(name).ok_or_else(|| {
                ExecError::plan(format!("table '{}' has no column '{name}'", table.name))
            })?;
            if values[pos].is_some() {
                return Err(ExecError::plan(format!(
                    "column '{name}' appears twice in this row"
                )));
            }
            let col = &table.columns[pos];
            let value = value_ops::eval(expr, &NoScope)?;
            values[pos] = Some(
                value_ops::coerce(value, col.ty, col.nullable)
                    .map_err(|e| ExecError::exec(format!("column '{name}': {e}")))?,
            );
        }
        let mut out = Vec::with_capacity(table.columns.len());
        for (pos, col) in table.columns.iter().enumerate() {
            match values[pos].take() {
                Some(v) => out.push(v),
                None => match (&col.default, col.nullable) {
                    (Some(d), _) => out.push(d.clone()),
                    (None, true) => out.push(Value::Null),
                    (None, false) => {
                        return Err(ExecError::exec(format!(
                            "column '{}' is missing and has no default",
                            col.name
                        )))
                    }
                },
            }
        }
        Ok(out)
    }

    fn get(&self, get: &Get) -> Result<Output, ExecError> {
        let table = self.load_table(&get.table)?;
        if let Some(filter) = &get.filter {
            validate_columns(&table, filter)?;
        }
        let plan = plan::plan_access(&table, get.filter.as_ref())?;
        let mut rows: Vec<Vec<Value>> = self
            .fetch(&table, &plan)?
            .into_iter()
            .map(|(_, v)| v)
            .collect();

        if let Some((col, dir)) = &get.order {
            let pos = table.column_position(col).ok_or_else(|| {
                ExecError::plan(format!("cannot order by unknown column '{col}'"))
            })?;
            rows.sort_by(|a, b| {
                let ord = value_ops::sort_cmp(&a[pos], &b[pos]);
                match dir {
                    Direction::Asc => ord,
                    Direction::Desc => ord.reverse(),
                }
            });
        }
        if let Some(limit) = get.limit {
            rows.truncate(limit as usize);
        }
        if let Some(cols) = &get.projection {
            let positions: Vec<usize> = cols
                .iter()
                .map(|c| {
                    table.column_position(c).ok_or_else(|| {
                        ExecError::plan(format!("table '{}' has no column '{c}'", table.name))
                    })
                })
                .collect::<Result<_, _>>()?;
            rows = rows
                .into_iter()
                .map(|row| positions.iter().map(|&p| row[p].clone()).collect())
                .collect();
        }
        Ok(Output::Rows(rows))
    }

    fn set(
        &mut self,
        table_name: &str,
        filter: Option<&Expr>,
        assigns: &[quanty_ql::ast::Assign],
    ) -> Result<Output, ExecError> {
        let table = self.load_table(table_name)?;
        if let Some(f) = filter {
            validate_columns(&table, f)?;
        }
        let key_positions = table.key_positions();
        let mut targets = Vec::with_capacity(assigns.len());
        for assign in assigns {
            let pos = table.column_position(&assign.column).ok_or_else(|| {
                ExecError::plan(format!(
                    "table '{table_name}' has no column '{}'",
                    assign.column
                ))
            })?;
            if key_positions.contains(&pos) {
                return Err(ExecError::plan(format!(
                    "cannot set key column '{}' (comes with row moves in phase 3)",
                    assign.column
                )));
            }
            validate_columns(&table, &assign.expr)?;
            targets.push(pos);
        }

        let plan = plan::plan_access(&table, filter)?;
        let matches = self.fetch(&table, &plan)?;
        let mut count = 0u64;
        for (key, old) in matches {
            let mut new = old.clone();
            // every assignment sees the row as it was before the set
            let scope = RowScope {
                table: &table,
                values: &old,
            };
            for (assign, &pos) in assigns.iter().zip(&targets) {
                let col = &table.columns[pos];
                let value = value_ops::eval(&assign.expr, &scope)?;
                new[pos] = value_ops::coerce(value, col.ty, col.nullable)
                    .map_err(|e| ExecError::exec(format!("column '{}': {e}", assign.column)))?;
            }
            self.tx.put(&key, &encode_key(&new))?;
            let pk = row_pk(&table, &old);
            for (pos, col) in table.columns.iter().enumerate() {
                if let Some(index_id) = col.index_id {
                    if !value_ops::values_equal(&old[pos], &new[pos]).unwrap_or(false)
                        || value_ops::type_of(&old[pos]) != value_ops::type_of(&new[pos])
                    {
                        self.tx.delete(&index_entry_key(index_id, &old[pos], &pk))?;
                        self.tx
                            .put(&index_entry_key(index_id, &new[pos], &pk), &[])?;
                    }
                }
            }
            self.mutated = true;
            count += 1;
        }
        Ok(Output::Count {
            verb: "set",
            n: count,
        })
    }

    fn del(&mut self, table_name: &str, filter: Option<&Expr>) -> Result<Output, ExecError> {
        let table = self.load_table(table_name)?;
        if let Some(f) = filter {
            validate_columns(&table, f)?;
        }
        let plan = plan::plan_access(&table, filter)?;
        let matches = self.fetch(&table, &plan)?;
        let mut count = 0u64;
        for (key, values) in matches {
            self.tx.delete(&key)?;
            let pk = row_pk(&table, &values);
            for (pos, col) in table.columns.iter().enumerate() {
                if let Some(index_id) = col.index_id {
                    self.tx
                        .delete(&index_entry_key(index_id, &values[pos], &pk))?;
                }
            }
            self.mutated = true;
            count += 1;
        }
        Ok(Output::Count {
            verb: "del",
            n: count,
        })
    }

    // -----------------------------------------------------------------
    // row access
    // -----------------------------------------------------------------

    /// Fetch `(row_key, values)` pairs for an access plan, residual filter
    /// already applied.
    fn fetch(&self, table: &Table, plan: &AccessPlan) -> Result<Fetched, ExecError> {
        let mut out = Vec::new();
        match &plan.access {
            Access::KeyLookup { key_values } => {
                let key = row_key(table, key_values);
                if let Some(bytes) = self.tx.get(&key)? {
                    out.push((key, decode_row(table, &bytes)?));
                }
            }
            Access::IndexScan {
                value, index_id, ..
            } => {
                let prefix = {
                    let mut parts = vec![Value::Int(*index_id as i64)];
                    parts.push(value.clone());
                    encode_key(&parts)
                };
                let end = key_successor(&prefix);
                for item in self.tx.scan(Some(&prefix), end.as_deref())? {
                    let (entry_key, _) = item?;
                    let decoded = decode_key(&entry_key)
                        .map_err(|_| ExecError::exec("index entry does not decode"))?;
                    // (index_id, value, pk...)
                    if decoded.len() < 3 {
                        return Err(ExecError::exec("index entry is too short, this is a bug"));
                    }
                    let pk = &decoded[2..];
                    let key = row_key(table, pk);
                    let bytes = self.tx.get(&key)?.ok_or_else(|| {
                        ExecError::exec("index points at a missing row, this is a bug")
                    })?;
                    out.push((key, decode_row(table, &bytes)?));
                }
            }
            Access::SeqScan => {
                let prefix = table_prefix(table.id);
                let end = key_successor(&prefix);
                for item in self.tx.scan(Some(&prefix), end.as_deref())? {
                    let (key, bytes) = item?;
                    out.push((key, decode_row(table, &bytes)?));
                }
            }
        }
        if let Some(residual) = &plan.residual {
            let mut filtered = Vec::with_capacity(out.len());
            for (key, values) in out {
                let scope = RowScope {
                    table,
                    values: &values,
                };
                if value_ops::as_condition(value_ops::eval(residual, &scope)?)? {
                    filtered.push((key, values));
                }
            }
            out = filtered;
        }
        Ok(out)
    }

    fn delete_range(&mut self, prefix: &[u8]) -> Result<(), ExecError> {
        let end = key_successor(prefix);
        let keys: Vec<Vec<u8>> = self
            .tx
            .scan(Some(prefix), end.as_deref())?
            .map(|item| item.map(|(k, _)| k))
            .collect::<Result<_, _>>()?;
        for key in keys {
            self.tx.delete(&key)?;
            self.mutated = true;
        }
        Ok(())
    }

    // -----------------------------------------------------------------
    // explain
    // -----------------------------------------------------------------

    fn explain(&self, inner: &Statement) -> Result<Output, ExecError> {
        let node = match inner {
            Statement::Get(get) => {
                let table = self.load_table(&get.table)?;
                if let Some(f) = &get.filter {
                    validate_columns(&table, f)?;
                }
                let plan = plan::plan_access(&table, get.filter.as_ref())?;
                plan::explain_get(&table, &plan, &get.order, get.limit)
            }
            Statement::Set { table, filter, .. } => {
                let table = self.load_table(table)?;
                if let Some(f) = filter {
                    validate_columns(&table, f)?;
                }
                let plan = plan::plan_access(&table, filter.as_ref())?;
                ExplainNode::over(
                    format!("Update {}", table.name),
                    plan::explain_access(&table, &plan),
                )
            }
            Statement::Del { table, filter } => {
                let table = self.load_table(table)?;
                if let Some(f) = filter {
                    validate_columns(&table, f)?;
                }
                let plan = plan::plan_access(&table, filter.as_ref())?;
                ExplainNode::over(
                    format!("Delete {}", table.name),
                    plan::explain_access(&table, &plan),
                )
            }
            Statement::Put { table, rows } => {
                ExplainNode::leaf(format!("Insert {table} ({} rows)", rows.len()))
            }
            _ => {
                return Err(ExecError::plan(
                    "explain wants get, put, set or del".to_string(),
                ))
            }
        };
        Ok(Output::Lines(node.render()))
    }
}

// ---------------------------------------------------------------------------
// keys and rows
// ---------------------------------------------------------------------------

pub(crate) fn table_prefix(table_id: u64) -> Vec<u8> {
    encode_key(&[Value::Int(table_id as i64)])
}

pub(crate) fn index_prefix(index_id: u64) -> Vec<u8> {
    encode_key(&[Value::Int(index_id as i64)])
}

pub(crate) fn row_key(table: &Table, pk: &[Value]) -> Vec<u8> {
    let mut parts = Vec::with_capacity(1 + pk.len());
    parts.push(Value::Int(table.id as i64));
    parts.extend(pk.iter().cloned());
    encode_key(&parts)
}

pub(crate) fn row_pk(table: &Table, values: &[Value]) -> Vec<Value> {
    table
        .key_positions()
        .iter()
        .map(|&p| values[p].clone())
        .collect()
}

pub(crate) fn index_entry_key(index_id: u64, value: &Value, pk: &[Value]) -> Vec<u8> {
    let mut parts = Vec::with_capacity(2 + pk.len());
    parts.push(Value::Int(index_id as i64));
    parts.push(value.clone());
    parts.extend(pk.iter().cloned());
    encode_key(&parts)
}

pub(crate) fn decode_row(table: &Table, bytes: &[u8]) -> Result<Vec<Value>, ExecError> {
    let values = decode_key(bytes).map_err(|_| ExecError::exec("stored row does not decode"))?;
    if values.len() != table.columns.len() {
        return Err(ExecError::exec(format!(
            "row has {} values but table '{}' has {} columns",
            values.len(),
            table.name,
            table.columns.len()
        )));
    }
    Ok(values)
}

/// The smallest byte string greater than every extension of `prefix`, for
/// use as an exclusive range end. `None` means unbounded (all 0xFF).
pub(crate) fn key_successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut out = prefix.to_vec();
    while let Some(&last) = out.last() {
        if last == 0xFF {
            out.pop();
        } else {
            *out.last_mut().expect("non-empty") += 1;
            return Some(out);
        }
    }
    None
}

struct RowScope<'a> {
    table: &'a Table,
    values: &'a [Value],
}

impl Scope for RowScope<'_> {
    fn column(&self, name: &str) -> Result<Value, ExecError> {
        match self.table.column_position(name) {
            Some(pos) => Ok(self.values[pos].clone()),
            None => Err(ExecError::plan(format!(
                "table '{}' has no column '{name}'",
                self.table.name
            ))),
        }
    }
}

/// Every column reference in the expression must exist, checked up front
/// so an empty table still reports the typo.
fn validate_columns(table: &Table, expr: &Expr) -> Result<(), ExecError> {
    match expr {
        Expr::Literal(_) => Ok(()),
        Expr::Column(name) => match table.column_position(name) {
            Some(_) => Ok(()),
            None => Err(ExecError::plan(format!(
                "table '{}' has no column '{name}'",
                table.name
            ))),
        },
        Expr::Unary(_, inner) => validate_columns(table, inner),
        Expr::Binary(l, _, r) => {
            validate_columns(table, l)?;
            validate_columns(table, r)
        }
    }
}
