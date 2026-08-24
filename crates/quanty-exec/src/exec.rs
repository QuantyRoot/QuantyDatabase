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

use quanty_core::{decode_key, encode_key, Db, GcReport, Storage, SuspendedTx, Value, WriteTx};
use quanty_ql::ast::{AsOf, ColumnRef, Direction, Expr, Get, Statement, TypeName};

use crate::catalog::{self, Table};
use crate::error::ExecError;
use crate::plan::{self, Access, AccessPlan, ExplainNode};
use crate::text;
use crate::value_ops::{self, NoScope, Scope};
use quanty_core::Snapshot;

/// Why a branch or history statement will not run inside a transaction.
/// Shared so the statement and [`Session::gc`] cannot drift apart.
const NOT_IN_TXN: &str = "branch and history statements cannot run inside a \
                          transaction; commit or rollback first";

/// Fetched rows: `(row key, decoded column values)` pairs.
type Fetched = Vec<(Vec<u8>, Vec<Value>)>;

/// Raw key/value pairs from a scan, before row decoding.
type RawRows = Vec<(Vec<u8>, Vec<u8>)>;

/// An open explicit transaction.
struct Open {
    tx: SuspendedTx,
    /// Whether anything in it actually wrote, so that an empty transaction
    /// commits nothing.
    mutated: bool,
}

/// A connection's transaction, parked while another connection runs.
///
/// ADR-021 makes a suspended transaction a value that has touched no disk,
/// so parking one is a move and dropping one is a rollback. The server
/// needs this because one executor thread runs the statements of many
/// connections and each of them keeps its own `begin`.
#[derive(Default)]
pub struct Parked(Option<Open>);

impl Parked {
    /// Whether a transaction is parked here.
    pub fn is_open(&self) -> bool {
        self.0.is_some()
    }
}

pub struct Session<S: Storage> {
    db: Db<S>,
    /// The open `begin` transaction, suspended between statements, or
    /// `None` in autocommit mode. It holds the writes made so far in
    /// memory and has touched nothing on disk, so a process killed with one
    /// open leaves the database exactly as it was before `begin`.
    txn: Option<Open>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Output {
    Ok,
    Count {
        verb: &'static str,
        n: u64,
    },
    Rows {
        /// One name per column, in the order the values arrive. Qualified
        /// as `table.column` when a statement reads from more than one
        /// table, bare otherwise.
        columns: Vec<String>,
        /// The rows themselves.
        rows: Vec<Vec<Value>>,
    },
    Lines(Vec<String>),
}

impl Output {
    pub fn render(&self) -> String {
        match self {
            Output::Ok => "ok".to_string(),
            Output::Count { verb, n } => format!("{verb} {n}"),
            Output::Rows { rows, .. } => {
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
        Session { db, txn: None }
    }

    pub fn db(&self) -> &Db<S> {
        &self.db
    }

    /// Whether a transaction is open on this session.
    pub fn in_txn(&self) -> bool {
        self.txn.is_some()
    }

    /// Take the open transaction out so another connection can use the
    /// session. Nothing is committed and nothing is written.
    pub fn park(&mut self) -> Parked {
        Parked(self.txn.take())
    }

    /// Put a parked transaction back before running the next statement of
    /// the connection it belongs to.
    pub fn unpark(&mut self, parked: Parked) {
        self.txn = parked.0;
    }

    /// Parse and execute one QQL statement. One statement is one
    /// transaction: it commits fully or leaves no trace.
    pub fn execute(&mut self, source: &str) -> Result<Output, ExecError> {
        let stmt = quanty_ql::parse(source)?;
        self.run_parsed(&stmt)
    }

    /// Parse and execute one SQL statement. The SQL front end lowers onto
    /// the same AST, so everything downstream of the parser is shared:
    /// same planner, same executor, same transaction rule.
    pub fn execute_sql(&mut self, source: &str) -> Result<Output, ExecError> {
        let stmt = quanty_ql::parse_sql(source)?;
        self.run_parsed(&stmt)
    }

    /// Execute a statement that is already built.
    ///
    /// Both front ends lower onto this AST, so this is the same door they
    /// use with the parser taken off the front. It exists for callers that
    /// construct statements instead of writing them: the sqlite import
    /// writes a row per record, and rendering every value into text only to
    /// parse it straight back would be work whose only product is the
    /// chance of a formatting bug.
    pub fn execute_ast(&mut self, stmt: &Statement) -> Result<Output, ExecError> {
        self.run_parsed(stmt)
    }

    /// Drop history, keeping the newest `keep` commits, and free the pages
    /// nothing refers to any more.
    ///
    /// `&mut self` is the reader-quiescence proof `Db::gc` asks for: the
    /// session owns the database and lends out only `&Db`, so no snapshot
    /// can be outstanding here (ADR-027, ADR-030).
    pub fn gc(&mut self, keep: usize) -> Result<GcReport, ExecError> {
        if self.txn.is_some() {
            return Err(ExecError::exec(NOT_IN_TXN));
        }
        Ok(self.db.gc(keep)?)
    }

    fn run_parsed(&mut self, stmt: &Statement) -> Result<Output, ExecError> {
        match stmt {
            // transaction control drives the session's txn state
            Statement::Begin => {
                if self.txn.is_some() {
                    return Err(ExecError::exec(
                        "a transaction is already open; commit or rollback first",
                    ));
                }
                self.txn = Some(Open {
                    tx: self.db.begin().suspend(),
                    mutated: false,
                });
                Ok(Output::Ok)
            }
            Statement::Commit => {
                let Some(open) = self.txn.take() else {
                    return Err(ExecError::exec("no transaction is open"));
                };
                // a transaction that wrote nothing commits nothing and
                // burns no commit id
                if open.mutated {
                    self.db.resume(open.tx)?.commit()?;
                }
                Ok(Output::Ok)
            }
            Statement::Rollback => {
                if self.txn.take().is_none() {
                    return Err(ExecError::exec("no transaction is open"));
                }
                Ok(Output::Ok)
            }
            // while a transaction is open every other statement is buffered
            // or served from a replay, never committed on its own
            _ if self.txn.is_some() => self.run_in_txn(stmt),
            // branch and history statements manage their own commits at
            // the database level instead of running inside a write tx
            Statement::Branch { name, at } => {
                self.db.create_branch(name, *at)?;
                Ok(Output::Ok)
            }
            Statement::Switch { name } => {
                self.db.switch_branch(name)?;
                Ok(Output::Lines(vec![format!("switched to {name}")]))
            }
            Statement::Merge { name } => {
                let head = self.db.merge_ff(name)?;
                Ok(Output::Lines(vec![format!(
                    "merged {name}, head is now commit {head}"
                )]))
            }
            Statement::DropBranch { name } => {
                self.db.drop_branch(name)?;
                Ok(Output::Ok)
            }
            Statement::ShowStats => {
                let stats = self.db.stats()?;
                let page_size = self.db.page_size() as u64;
                Ok(Output::Lines(vec![
                    format!("page size {page_size}"),
                    format!("pages {}", stats.page_count),
                    format!("head pages {}", stats.head_pages),
                    format!("free pages {}", stats.free_pages),
                    format!("bytes {}", stats.page_count * page_size),
                ]))
            }
            Statement::ShowBranches => {
                let current = self.db.current_branch();
                let lines = self
                    .db
                    .branches()?
                    .into_iter()
                    .map(|(name, r)| {
                        let marker = if name == current { "*" } else { " " };
                        format!("{marker} {name} @{}", r.head_id)
                    })
                    .collect();
                Ok(Output::Lines(lines))
            }
            Statement::Log => {
                let lines = self
                    .db
                    .log()?
                    .into_iter()
                    .map(|c| format!("commit {} parent {}", c.commit_id, c.parent_id))
                    .collect();
                Ok(Output::Lines(lines))
            }
            Statement::Gc { keep } => {
                let report = self.gc(*keep as usize)?;
                Ok(Output::Lines(vec![format!(
                    "gc pruned {} commits, freed {} pages",
                    report.pruned_commits, report.freed_pages
                )]))
            }
            // reads from history resolve a snapshot and never open a tx
            Statement::Get(get) if get.as_of.is_some() => self.read_as_of(get),
            _ => {
                let tx = self.db.begin();
                let mut run = Run { tx, mutated: false };
                let output = run.statement(stmt)?;
                if run.mutated {
                    run.tx.commit()?;
                }
                Ok(output)
            }
        }
    }

    /// A historical read: resolve the snapshot and run against it. Never
    /// opens a write transaction and, inside an explicit transaction,
    /// ignores the pending writes because `as of` asks for committed
    /// history by definition.
    fn read_as_of(&self, get: &Get) -> Result<Output, ExecError> {
        let snap = match get.as_of.expect("caller checked as_of is some") {
            AsOf::Commit(id) => self.db.snapshot_at(id)?,
            AsOf::Time(t) => self.db.snapshot_at_time(t)?,
        };
        run_get(&snap, get)
    }

    /// Statement handling while an explicit transaction is open.
    ///
    /// The model is replay: the transaction is exactly the ordered list of
    /// its mutating statements, and its effect is that list applied to a
    /// single write transaction, atomically, at `commit`. To read or
    /// validate mid-transaction, that list is replayed into a throwaway
    /// write transaction and then discarded, so a read sees precisely what
    /// `commit` would produce so far, and nothing sticks until `commit`.
    /// This is the boring, obviously correct version; a write-set overlay
    /// that avoids the replay is the planned optimization (ADR-016).
    fn run_in_txn(&mut self, stmt: &Statement) -> Result<Output, ExecError> {
        match stmt {
            // database-level statements own their commits and cannot be
            // part of a data transaction; make the user close it first
            Statement::Branch { .. }
            | Statement::Switch { .. }
            | Statement::Merge { .. }
            | Statement::DropBranch { .. }
            | Statement::ShowBranches
            | Statement::ShowStats
            | Statement::Log
            | Statement::Gc { .. } => Err(ExecError::exec(NOT_IN_TXN)),
            // history reads are independent of the pending writes
            Statement::Get(get) if get.as_of.is_some() => self.read_as_of(get),
            Statement::Begin | Statement::Commit | Statement::Rollback => {
                unreachable!("transaction control is handled in run_parsed")
            }
            // everything else runs against the transaction's own writes
            _ => self.in_open_txn(stmt),
        }
    }

    /// Run one statement on the open transaction.
    ///
    /// The transaction is resumed, the statement runs against the writes
    /// made so far, and the transaction is suspended again. Nothing is
    /// replayed, so N statements cost what their writes cost rather than
    /// N squared (ADR-021).
    ///
    /// A statement that fails is undone completely and does not close the
    /// transaction, which is the promise ADR-016 made and the reason for
    /// the savepoint: the writes of a rejected statement must not be
    /// visible to the next one, and must not reach a later commit.
    fn in_open_txn(&mut self, stmt: &Statement) -> Result<Output, ExecError> {
        let open = self.txn.take().expect("transaction is open");
        let mutating = !matches!(
            stmt,
            Statement::Get(_) | Statement::ShowTables | Statement::Explain(_)
        );

        // a resume that fails takes the transaction with it, which is the
        // rollback: nothing was on disk, so there is nothing to undo. the
        // session is back in autocommit mode and the error says why.
        let mut tx = self.db.resume(open.tx)?;
        let roots = tx.roots();
        tx.savepoint();

        let mut run = Run { tx, mutated: false };
        let result = run.statement(stmt);
        let mut tx = run.tx;
        let wrote = run.mutated;

        match &result {
            Ok(_) => tx.release_savepoint(),
            Err(_) => tx.rollback_savepoint(roots),
        }
        self.txn = Some(Open {
            tx: tx.suspend(),
            mutated: open.mutated || (result.is_ok() && mutating && wrote),
        });
        result
    }
}

/// A point in time to read from: the open transaction (branch head plus
/// the statement's own writes) or a historical snapshot. Reads go through
/// this so `get` behaves identically against both, including reading the
/// table definition of that point in time; schema changes travel through
/// history like everything else.
///
/// Scans collect eagerly: the two underlying iterator types differ, and
/// every result set passes through sorting and projection anyway.
trait View {
    fn view_get(&self, key: &[u8]) -> quanty_core::Result<Option<Vec<u8>>>;
    fn view_scan(&self, start: Option<&[u8]>, end: Option<&[u8]>) -> Result<RawRows, ExecError>;
    fn view_catalog_get(&self, key: &[u8]) -> quanty_core::Result<Option<Vec<u8>>>;
}

impl<S: Storage> View for WriteTx<'_, S> {
    fn view_get(&self, key: &[u8]) -> quanty_core::Result<Option<Vec<u8>>> {
        self.get(key)
    }
    fn view_scan(&self, start: Option<&[u8]>, end: Option<&[u8]>) -> Result<RawRows, ExecError> {
        let mut out = Vec::new();
        for item in self.scan(start, end)? {
            out.push(item?);
        }
        Ok(out)
    }
    fn view_catalog_get(&self, key: &[u8]) -> quanty_core::Result<Option<Vec<u8>>> {
        self.catalog_get(key)
    }
}

impl<S: Storage> View for Snapshot<'_, S> {
    fn view_get(&self, key: &[u8]) -> quanty_core::Result<Option<Vec<u8>>> {
        self.get(key)
    }
    fn view_scan(&self, start: Option<&[u8]>, end: Option<&[u8]>) -> Result<RawRows, ExecError> {
        let mut out = Vec::new();
        for item in self.scan(start, end)? {
            out.push(item?);
        }
        Ok(out)
    }
    fn view_catalog_get(&self, key: &[u8]) -> quanty_core::Result<Option<Vec<u8>>> {
        self.catalog_get(key)
    }
}

fn load_table_from<V: View>(view: &V, name: &str) -> Result<Table, ExecError> {
    match view.view_catalog_get(&catalog::table_key(name))? {
        Some(bytes) => Table::deserialize(&bytes),
        None => Err(ExecError::plan(format!("no table named '{name}'"))),
    }
}

/// The whole read pipeline: plan, fetch, filter, order, limit, project.
fn run_get<V: View>(view: &V, get: &Get) -> Result<Output, ExecError> {
    if !get.joins.is_empty() {
        return run_join_get(view, get);
    }
    let table = load_table_from(view, &get.table)?;
    if let Some(filter) = &get.filter {
        validate_columns(&table, filter)?;
    }
    let plan = plan::plan_access(&table, get.filter.as_ref())?;

    // A search returns its answers best first unless asked otherwise.
    // Nothing else here has an opinion about order, but a match does: the
    // scores are what the index was read for.
    let mut rows: Vec<Vec<Value>> = match (&plan.access, &get.order) {
        (
            Access::TextMatch {
                index_id,
                column,
                terms,
            },
            None,
        ) => {
            let (fetched, scores) = text_match(view, &table, *index_id, *column, terms)?;
            let mut ranked: Vec<(f64, Vec<Value>)> = scores
                .into_iter()
                .zip(fetched.into_iter().map(|(_, v)| v))
                .collect();
            // Descending by score, and by primary key where scores tie, so
            // the same query gives the same order twice.
            ranked.sort_by(|a, b| {
                b.0.partial_cmp(&a.0)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| {
                        // The encoded key, because Value is not Ord: floats
                        // have no total order and the encoding gives one.
                        row_key(&table, &row_pk(&table, &a.1))
                            .cmp(&row_key(&table, &row_pk(&table, &b.1)))
                    })
            });
            ranked.into_iter().map(|(_, v)| v).collect()
        }
        _ => fetch_rows(view, &table, &plan)?
            .into_iter()
            .map(|(_, v)| v)
            .collect(),
    };

    if let Some((col, dir)) = &get.order {
        if let Some(t) = &col.table {
            if t != &table.name {
                return Err(ExecError::plan(format!(
                    "no table named '{t}' in this statement"
                )));
            }
        }
        let pos = table.column_position(&col.column).ok_or_else(|| {
            ExecError::plan(format!("cannot order by unknown column '{}'", col.column))
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
    let mut columns: Vec<String> = table.columns.iter().map(|c| c.name.clone()).collect();
    if let Some(cols) = &get.projection {
        let positions: Vec<usize> = cols
            .iter()
            .map(|c| position_in(&table, c))
            .collect::<Result<_, _>>()?;
        rows = rows
            .into_iter()
            .map(|row| positions.iter().map(|&p| row[p].clone()).collect())
            .collect();
        columns = positions.iter().map(|&p| columns[p].clone()).collect();
    }
    Ok(Output::Rows { columns, rows })
}

/// One document that has a term: its row key and how often the term
/// occurs in it.
type Posting = (Vec<u8>, u32);

/// Intersect the posting lists and score what survives.
///
/// Scoring happens here rather than after, because the numbers BM25 needs
/// are the ones this walk already has: a term's document frequency is the
/// length of its posting list, and its frequency in a document is the
/// length of that posting's position list. Reading them again afterwards
/// would double the cost of the only expensive part (ADR-036).
fn text_match<V: View>(
    view: &V,
    table: &Table,
    index_id: u64,
    column: usize,
    terms: &[String],
) -> Result<(Fetched, Vec<f64>), ExecError> {
    // Per term: every document that has it, in row key order, with how
    // often it occurs there.
    let mut lists: Vec<Vec<Posting>> = Vec::with_capacity(terms.len());
    for term in terms {
        let prefix = encode_key(&[Value::Int(index_id as i64), Value::Text(term.clone())]);
        let end = key_successor(&prefix);
        let mut hits = Vec::new();
        for (entry_key, positions) in view.view_scan(Some(&prefix), end.as_deref())? {
            let decoded =
                decode_key(&entry_key).map_err(|_| ExecError::exec("posting does not decode"))?;
            if decoded.len() < 3 {
                return Err(ExecError::exec("posting is too short, this is a bug"));
            }
            let tf = text::decode_positions(&positions)
                .ok_or_else(|| ExecError::exec("posting positions do not decode"))?
                .len() as u32;
            hits.push((row_key(table, &decoded[2..]), tf));
        }
        // A term nothing contains empties the intersection.
        if hits.is_empty() {
            return Ok((Vec::new(), Vec::new()));
        }
        lists.push(hits);
    }

    // Document frequency is the list length, taken before the lists are
    // reordered or consumed.
    let document_frequency: Vec<usize> = lists.iter().map(|l| l.len()).collect();
    let (docs, total_length) = match view.view_get(&corpus_key(index_id))? {
        Some(bytes) => decode_corpus(&bytes)?,
        None => (0, 0),
    };
    let average_length = if docs == 0 {
        1.0
    } else {
        total_length as f64 / docs as f64
    };

    let mut order: Vec<usize> = (0..lists.len()).collect();
    order.sort_by_key(|&i| lists[i].len());
    let seed = order[0];

    let mut fetched = Vec::new();
    let mut scores = Vec::new();
    'row: for (key, tf) in &lists[seed] {
        let mut frequencies = vec![(seed, *tf)];
        for &i in &order[1..] {
            match lists[i].binary_search_by(|(k, _)| k.cmp(key)) {
                Ok(at) => frequencies.push((i, lists[i][at].1)),
                Err(_) => continue 'row,
            }
        }

        let Some(bytes) = view.view_get(key)? else {
            continue;
        };
        let row = decode_row(table, &bytes)?;

        // The length comes from the row that was fetched anyway, not from
        // the entry at (index_id, 0, pk). Measured: one point lookup per
        // result put a query matching eighty thousand documents at three
        // times the cost of the scan it is meant to beat.
        let length = match &row[column] {
            Value::Text(s) => text::length(s) as f64,
            _ => average_length,
        };
        let score: f64 = frequencies
            .iter()
            .map(|&(i, tf)| bm25(tf, document_frequency[i], docs, length, average_length))
            .sum();

        fetched.push((key.clone(), row));
        scores.push(score);
    }
    Ok((fetched, scores))
}

/// One term's contribution to a document's score.
///
/// Okapi BM25 with the usual constants. A phase that has to beat a scan
/// by a hundredfold should spend its budget on the index rather than on
/// inventing a scoring function (ADR-036).
fn bm25(tf: u32, document_frequency: usize, docs: u64, length: f64, average_length: f64) -> f64 {
    const K1: f64 = 1.2;
    const B: f64 = 0.75;
    let n = docs as f64;
    let df = document_frequency as f64;
    let idf = (1.0 + (n - df + 0.5) / (df + 0.5)).ln();
    let tf = tf as f64;
    idf * (tf * (K1 + 1.0)) / (tf + K1 * (1.0 - B + B * length / average_length))
}

fn fetch_rows<V: View>(view: &V, table: &Table, plan: &AccessPlan) -> Result<Fetched, ExecError> {
    let mut out = Vec::new();
    match &plan.access {
        Access::KeyLookup { key_values } => {
            let key = row_key(table, key_values);
            if let Some(bytes) = view.view_get(&key)? {
                out.push((key, decode_row(table, &bytes)?));
            }
        }
        Access::TextMatch {
            index_id,
            column,
            terms,
        } => {
            let (rows, _) = text_match(view, table, *index_id, *column, terms)?;
            out.extend(rows);
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
            for (entry_key, _) in view.view_scan(Some(&prefix), end.as_deref())? {
                let decoded = decode_key(&entry_key)
                    .map_err(|_| ExecError::exec("index entry does not decode"))?;
                // (index_id, value, pk...)
                if decoded.len() < 3 {
                    return Err(ExecError::exec("index entry is too short, this is a bug"));
                }
                let pk = &decoded[2..];
                let key = row_key(table, pk);
                let bytes = view.view_get(&key)?.ok_or_else(|| {
                    ExecError::exec("index points at a missing row, this is a bug")
                })?;
                out.push((key, decode_row(table, &bytes)?));
            }
        }
        Access::SeqScan => {
            let prefix = table_prefix(table.id);
            let end = key_successor(&prefix);
            for (key, bytes) in view.view_scan(Some(&prefix), end.as_deref())? {
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

// ---------------------------------------------------------------------------
// joins
// ---------------------------------------------------------------------------

/// The tables of a join statement, in from-clause order, with the offset
/// of each table's columns inside the combined row.
pub(crate) struct Bound {
    tables: Vec<Table>,
    offsets: Vec<usize>,
}

impl Bound {
    fn new(tables: Vec<Table>) -> Result<Bound, ExecError> {
        for (i, a) in tables.iter().enumerate() {
            if tables[..i].iter().any(|b| b.name == a.name) {
                return Err(ExecError::plan(format!(
                    "table '{}' appears twice in this statement; table aliases are not supported yet",
                    a.name
                )));
            }
        }
        let mut offsets = Vec::with_capacity(tables.len());
        let mut width = 0;
        for t in &tables {
            offsets.push(width);
            width += t.columns.len();
        }
        Ok(Bound { tables, offsets })
    }

    /// Resolve a reference against the first `upto` tables. Unqualified
    /// names must be unambiguous within that scope.
    /// Every column of every table, qualified, in value order.
    ///
    /// Qualified even for one table, because a join and a single table read
    /// go through different functions and only the join can arrive here.
    fn column_names(&self) -> Vec<String> {
        self.tables
            .iter()
            .flat_map(|t| {
                t.columns
                    .iter()
                    .map(move |c| format!("{}.{}", t.name, c.name))
            })
            .collect()
    }

    fn resolve_within(&self, r: &ColumnRef, upto: usize) -> Result<usize, ExecError> {
        match &r.table {
            Some(t) => {
                let Some(i) = self.tables[..upto].iter().position(|tab| &tab.name == t) else {
                    return Err(ExecError::plan(format!(
                        "no table named '{t}' in this statement"
                    )));
                };
                match self.tables[i].column_position(&r.column) {
                    Some(pos) => Ok(self.offsets[i] + pos),
                    None => Err(ExecError::plan(format!(
                        "table '{t}' has no column '{}'",
                        r.column
                    ))),
                }
            }
            None => {
                let mut hits = Vec::new();
                for (i, t) in self.tables[..upto].iter().enumerate() {
                    if let Some(pos) = t.column_position(&r.column) {
                        hits.push((i, pos));
                    }
                }
                match hits.as_slice() {
                    [] => Err(ExecError::plan(format!(
                        "no table in this statement has a column '{}'",
                        r.column
                    ))),
                    [(i, pos)] => Ok(self.offsets[*i] + pos),
                    many => {
                        let spellings: Vec<String> = many
                            .iter()
                            .map(|(i, _)| format!("{}.{}", self.tables[*i].name, r.column))
                            .collect();
                        Err(ExecError::plan(format!(
                            "column '{}' is ambiguous here; qualify it ({})",
                            r.column,
                            spellings.join(" or ")
                        )))
                    }
                }
            }
        }
    }

    fn validate_within(&self, expr: &Expr, upto: usize) -> Result<(), ExecError> {
        match expr {
            Expr::Literal(_) => Ok(()),
            Expr::Column(r) => self.resolve_within(r, upto).map(|_| ()),
            Expr::Unary(_, inner) => self.validate_within(inner, upto),
            Expr::Binary(l, _, r) => {
                self.validate_within(l, upto)?;
                self.validate_within(r, upto)
            }
        }
    }
}

/// A combined row over the first `upto` tables of a bind.
struct BoundScope<'a> {
    bound: &'a Bound,
    upto: usize,
    values: &'a [Value],
}

impl Scope for BoundScope<'_> {
    fn column(&self, r: &ColumnRef) -> Result<Value, ExecError> {
        Ok(self.values[self.bound.resolve_within(r, self.upto)?].clone())
    }
}

/// Load and bind everything a join statement needs; shared between
/// execution and explain so both see the identical plan.
fn bind_join_get<V: View>(
    view: &V,
    get: &Get,
) -> Result<(Bound, Vec<plan::JoinStrategy>), ExecError> {
    let mut tables = vec![load_table_from(view, &get.table)?];
    for j in &get.joins {
        tables.push(load_table_from(view, &j.table)?);
    }
    let bound = Bound::new(tables)?;
    // each on-condition sees the tables joined so far plus its own
    for (i, join) in get.joins.iter().enumerate() {
        bound.validate_within(&join.on, i + 2)?;
    }
    if let Some(filter) = &get.filter {
        bound.validate_within(filter, bound.tables.len())?;
    }
    let mut strategies = Vec::with_capacity(get.joins.len());
    for (i, join) in get.joins.iter().enumerate() {
        let left: Vec<&Table> = bound.tables[..=i].iter().collect();
        strategies.push(plan::plan_join(&left, &bound.tables[i + 1], &join.on));
    }
    Ok((bound, strategies))
}

fn run_join_get<V: View>(view: &V, get: &Get) -> Result<Output, ExecError> {
    let (bound, strategies) = bind_join_get(view, get)?;

    // base table: full scan; the filter runs after all joins
    let seq = AccessPlan {
        access: Access::SeqScan,
        residual: None,
    };
    let mut rows: Vec<Vec<Value>> = fetch_rows(view, &bound.tables[0], &seq)?
        .into_iter()
        .map(|(_, v)| v)
        .collect();

    for (i, (join, strategy)) in get.joins.iter().zip(&strategies).enumerate() {
        rows = join_step(view, rows, &bound, i, join, strategy)?;
    }

    if let Some(filter) = &get.filter {
        let mut kept = Vec::with_capacity(rows.len());
        for row in rows {
            let scope = BoundScope {
                bound: &bound,
                upto: bound.tables.len(),
                values: &row,
            };
            if value_ops::as_condition(value_ops::eval(filter, &scope)?)? {
                kept.push(row);
            }
        }
        rows = kept;
    }

    if let Some((col, dir)) = &get.order {
        let pos = bound.resolve_within(col, bound.tables.len())?;
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
    let mut columns = bound.column_names();
    if let Some(cols) = &get.projection {
        let positions: Vec<usize> = cols
            .iter()
            .map(|c| bound.resolve_within(c, bound.tables.len()))
            .collect::<Result<_, _>>()?;
        rows = rows
            .into_iter()
            .map(|row| positions.iter().map(|&p| row[p].clone()).collect())
            .collect();
        columns = positions.iter().map(|&p| columns[p].clone()).collect();
    }
    Ok(Output::Rows { columns, rows })
}

/// One join step. The strategy only decides which right rows become
/// candidates; the full on-condition runs on every candidate, so every
/// strategy returns exactly what the nested loop would.
fn join_step<V: View>(
    view: &V,
    left_rows: Vec<Vec<Value>>,
    bound: &Bound,
    step: usize,
    join: &quanty_ql::ast::Join,
    strategy: &plan::JoinStrategy,
) -> Result<Vec<Vec<Value>>, ExecError> {
    let right = &bound.tables[step + 1];
    let right_width = right.columns.len();
    let upto = step + 2;
    let mut out = Vec::new();

    // the nested loop materializes the right side once
    let materialized = if matches!(strategy, plan::JoinStrategy::NestedLoop) {
        let seq = AccessPlan {
            access: Access::SeqScan,
            residual: None,
        };
        Some(
            fetch_rows(view, right, &seq)?
                .into_iter()
                .map(|(_, v)| v)
                .collect::<Vec<_>>(),
        )
    } else {
        None
    };

    for left in left_rows {
        let candidates: Vec<Vec<Value>> = match strategy {
            plan::JoinStrategy::NestedLoop => materialized.clone().expect("materialized above"),
            plan::JoinStrategy::KeyProbe { left: probe } => {
                let value = BoundScope {
                    bound,
                    upto: step + 1,
                    values: &left,
                }
                .column(probe)?;
                if matches!(value, Value::Null) {
                    // key columns are never null; nothing can match
                    Vec::new()
                } else {
                    let key = right.key_positions()[0];
                    let coerced = value_ops::coerce(value, right.columns[key].ty, false)
                        .expect("plan_join only probes with compatible types");
                    let plan = AccessPlan {
                        access: Access::KeyLookup {
                            key_values: vec![coerced],
                        },
                        residual: None,
                    };
                    fetch_rows(view, right, &plan)?
                        .into_iter()
                        .map(|(_, v)| v)
                        .collect()
                }
            }
            plan::JoinStrategy::IndexProbe {
                left: probe,
                column,
                index_id,
            } => {
                let value = BoundScope {
                    bound,
                    upto: step + 1,
                    values: &left,
                }
                .column(probe)?;
                // null probes a null index entry, matching the engine's
                // null = null rule; coercion cannot fail (see plan_join)
                let coerced = value_ops::coerce(value, right.columns[*column].ty, true)
                    .expect("plan_join only probes with compatible types");
                let plan = AccessPlan {
                    access: Access::IndexScan {
                        column: *column,
                        index_id: *index_id,
                        value: coerced,
                    },
                    residual: None,
                };
                fetch_rows(view, right, &plan)?
                    .into_iter()
                    .map(|(_, v)| v)
                    .collect()
            }
        };

        let mut matched = false;
        for r in candidates {
            let mut combined = left.clone();
            combined.extend(r);
            let scope = BoundScope {
                bound,
                upto,
                values: &combined,
            };
            if value_ops::as_condition(value_ops::eval(&join.on, &scope)?)? {
                out.push(combined);
                matched = true;
            }
        }
        if join.kind == quanty_ql::ast::JoinKind::Left && !matched {
            let mut padded = left;
            padded.resize(padded.len() + right_width, Value::Null);
            out.push(padded);
        }
    }
    Ok(out)
}

/// One statement's execution state.
struct Run<'db, S: Storage> {
    tx: WriteTx<'db, S>,
    mutated: bool,
}

impl<S: Storage> Run<'_, S> {
    /// Take or give back one reference per chunk a descriptor names.
    ///
    /// Repeats count: a file whose middle chunk equals its first names it
    /// twice, and the two calls have to be symmetric or the count drifts.
    fn move_asset_refs(&mut self, value: &Value, delta: i64) -> Result<(), ExecError> {
        let bytes = match value {
            Value::Bytes(b) => b,
            _ => return Ok(()),
        };
        let blob = quanty_core::BlobRef::decode(bytes)?;
        for hash in &blob.chunks {
            if delta > 0 {
                self.tx.retain_chunk(hash)?;
            } else {
                self.tx.release_chunk(hash)?;
            }
        }
        Ok(())
    }

    /// Add or remove one row's postings for one text-indexed column.
    ///
    /// Symmetric on purpose: the same walk produces the same keys either
    /// way, so a delete cannot miss an entry an insert made.
    fn move_postings(
        &mut self,
        index_id: u64,
        pk: &[Value],
        value: &Value,
        adding: bool,
    ) -> Result<(), ExecError> {
        let text = match value {
            Value::Text(s) => s.as_str(),
            // A null column is not a document. It contributes no postings
            // and is not counted in the corpus, so avgdl stays the average
            // over documents that exist.
            _ => return Ok(()),
        };

        for (term, positions) in text::postings(text) {
            let key = index_entry_key(index_id, &Value::Text(term), pk);
            if adding {
                self.tx.put(&key, &text::encode_positions(&positions))?;
            } else {
                self.tx.delete(&key)?;
            }
        }

        // The per-document length. Scoring reads it off the row it
        // fetches anyway, so nothing reads this today; it stays because
        // the next thing search wants is a top-k that scores without
        // materialising every row, and then the length has to be in the
        // index. verify_indexes rebuilds it from the rows, so it cannot
        // quietly drift while it waits.
        let length = text::length(text);
        let len_key = index_entry_key(index_id, &Value::Int(0), pk);
        if adding {
            self.tx.put(&len_key, &length.to_le_bytes())?;
        } else {
            self.tx.delete(&len_key)?;
        }
        self.move_corpus(index_id, length, adding)
    }

    /// Keep the two numbers BM25 cannot get from a posting list: how many
    /// documents there are and how long they are on average.
    fn move_corpus(&mut self, index_id: u64, length: u32, adding: bool) -> Result<(), ExecError> {
        let key = corpus_key(index_id);
        let (mut docs, mut total) = match self.tx.get(&key)? {
            Some(bytes) => decode_corpus(&bytes)?,
            None => (0u64, 0u64),
        };
        if adding {
            docs += 1;
            total += length as u64;
        } else {
            docs = docs.saturating_sub(1);
            total = total.saturating_sub(length as u64);
        }
        if docs == 0 {
            self.tx.delete(&key)?;
        } else {
            let mut bytes = docs.to_le_bytes().to_vec();
            bytes.extend_from_slice(&total.to_le_bytes());
            self.tx.put(&key, &bytes)?;
        }
        Ok(())
    }

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
            Statement::GcBlobs => self.gc_blobs(),
            Statement::Explain(inner) => self.explain(inner),
            // branch and history statements never reach here: Session::execute
            // routes them before any write transaction opens
            Statement::Branch { .. }
            | Statement::Switch { .. }
            | Statement::Merge { .. }
            | Statement::DropBranch { .. }
            | Statement::ShowBranches
            | Statement::ShowStats
            | Statement::Log
            | Statement::Gc { .. }
            | Statement::Begin
            | Statement::Commit
            | Statement::Rollback => Err(ExecError::exec(
                "control statement reached the write path, this is a bug",
            )),
        }
    }

    // -----------------------------------------------------------------
    // catalog plumbing
    // -----------------------------------------------------------------

    fn load_table(&self, name: &str) -> Result<Table, ExecError> {
        load_table_from(&self.tx, name)
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
            if def.columns[i].text {
                if table.columns[i].ty != TypeName::Text {
                    return Err(ExecError::plan(format!(
                        "'@text' wants a text column, and '{}' is {}",
                        table.columns[i].name,
                        table.columns[i].ty.as_str()
                    )));
                }
                table.columns[i].text_index_id = Some(self.alloc_id()?);
            }
        }
        self.store_table(&table)?;
        Ok(Output::Ok)
    }

    fn drop_table(&mut self, name: &str) -> Result<Output, ExecError> {
        let table = self.load_table(name)?;

        // delete_range wipes the prefix without looking at what is in it,
        // so a table with asset columns has to give its chunks back first
        // or they are held by rows that no longer exist.
        let asset_columns: Vec<usize> = table
            .columns
            .iter()
            .enumerate()
            .filter(|(_, c)| c.ty == TypeName::Asset)
            .map(|(pos, _)| pos)
            .collect();
        if !asset_columns.is_empty() {
            let rows = self.fetch(
                &table,
                &AccessPlan {
                    access: Access::SeqScan,
                    residual: None,
                },
            )?;
            for (_, values) in &rows {
                for &pos in &asset_columns {
                    self.move_asset_refs(&values[pos], -1)?;
                }
            }
        }

        self.delete_range(&table_prefix(table.id))?;
        for col in &table.columns {
            if let Some(index_id) = col.index_id {
                self.delete_range(&index_prefix(index_id))?;
            }
            // Postings, lengths and the corpus counters all live under the
            // text index's own id, so one range takes the lot.
            if let Some(text_id) = col.text_index_id {
                self.delete_range(&index_prefix(text_id))?;
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

    /// Drop every chunk no row in the current head names.
    ///
    /// This is what closes the criterion ADR-033 left open. The storage
    /// layer cannot do it because only the schema says which columns hold
    /// a descriptor; here that is known, so reachability is exact rather
    /// than a guess about reference counts.
    ///
    /// Only the current head is walked. Older commits keep their own
    /// catalog tree, so a chunk removed here is still readable through
    /// `as of`, and the walk stays linear in the rows that exist now.
    fn gc_blobs(&mut self) -> Result<Output, ExecError> {
        let prefix = catalog::tables_prefix();
        let end = key_successor(&prefix);
        let mut tables = Vec::new();
        for item in self.tx.catalog_scan(Some(&prefix), end.as_deref())? {
            let (_, bytes) = item?;
            tables.push(Table::deserialize(&bytes)?);
        }

        let mut reachable = std::collections::HashSet::new();
        for table in &tables {
            let positions: Vec<usize> = table
                .columns
                .iter()
                .enumerate()
                .filter(|(_, c)| c.ty == TypeName::Asset)
                .map(|(pos, _)| pos)
                .collect();
            if positions.is_empty() {
                continue;
            }
            let rows = self.fetch(
                table,
                &AccessPlan {
                    access: Access::SeqScan,
                    residual: None,
                },
            )?;
            for (_, values) in &rows {
                for &pos in &positions {
                    if let Value::Bytes(d) = &values[pos] {
                        for hash in quanty_core::BlobRef::decode(d)?.chunks {
                            reachable.insert(hash);
                        }
                    }
                }
            }
        }

        let report = self.tx.sweep_chunks(&reachable)?;
        if report.removed > 0 {
            self.mutated = true;
        }
        Ok(Output::Lines(vec![format!(
            "gc blobs dropped {} chunks holding {} bytes, kept {}",
            report.removed, report.bytes, report.kept
        )]))
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
            // One descent: the insert reports the duplicate it already
            // had to look for, rather than being asked first.
            if !self.tx.put_unique(&key, &encode_key(&values))? {
                let pk = row_pk(&table, &values);
                let rendered: Vec<String> = pk.iter().map(quanty_ql::pretty::literal).collect();
                return Err(ExecError::exec(format!(
                    "duplicate key ({}) in table '{table_name}'",
                    rendered.join(", ")
                )));
            }
            for (pos, col) in table.columns.iter().enumerate() {
                if col.ty == TypeName::Asset {
                    self.move_asset_refs(&values[pos], 1)?;
                }
                if let Some(text_id) = col.text_index_id {
                    let pk = row_pk(&table, &values);
                    self.move_postings(text_id, &pk, &values[pos], true)?;
                }
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
        // as-of reads are handled before a write tx is ever opened, in
        // `Session::execute`; inside a write statement they make no sense
        if get.as_of.is_some() {
            return Err(ExecError::plan("as of cannot run inside a write statement"));
        }
        run_get(&self.tx, get)
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
                if let Some(text_id) = col.text_index_id {
                    if old[pos] != new[pos] {
                        self.move_postings(text_id, &pk, &old[pos], false)?;
                        self.move_postings(text_id, &pk, &new[pos], true)?;
                    }
                }
                if col.ty == TypeName::Asset && old[pos] != new[pos] {
                    // new first: if the two share chunks, releasing the old
                    // one first could take a shared chunk to zero and drop
                    // bytes the new descriptor still names.
                    self.move_asset_refs(&new[pos], 1)?;
                    self.move_asset_refs(&old[pos], -1)?;
                }
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
                if col.ty == TypeName::Asset {
                    self.move_asset_refs(&values[pos], -1)?;
                }
                if let Some(text_id) = col.text_index_id {
                    self.move_postings(text_id, &pk, &values[pos], false)?;
                }
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
        fetch_rows(&self.tx, table, plan)
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
            Statement::Get(get) if !get.joins.is_empty() => {
                let (bound, strategies) = bind_join_get(&self.tx, get)?;
                let mut node = ExplainNode::leaf(format!("SeqScan {}", bound.tables[0].name));
                for (i, (join, strategy)) in get.joins.iter().zip(&strategies).enumerate() {
                    let probe = plan::explain_join_probe(&bound.tables[i + 1], strategy);
                    node = ExplainNode {
                        label: plan::join_label(join, strategy),
                        children: vec![node, probe],
                    };
                }
                if let Some(filter) = &get.filter {
                    node = ExplainNode::over(
                        format!("Filter {}", quanty_ql::pretty::expr(filter)),
                        node,
                    );
                }
                if let Some((col, dir)) = &get.order {
                    bound.resolve_within(col, bound.tables.len())?;
                    let dir = match dir {
                        Direction::Asc => "asc",
                        Direction::Desc => "desc",
                    };
                    node = ExplainNode::over(format!("Sort {col} {dir}"), node);
                }
                if let Some(n) = get.limit {
                    node = ExplainNode::over(format!("Limit {n}"), node);
                }
                node
            }
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

/// Where a text index keeps how many documents it has and how long they
/// are in total. Integers sort before text in the key encoding, so this
/// and the per-document lengths sit ahead of every term (ADR-036).
pub(crate) fn corpus_key(index_id: u64) -> Vec<u8> {
    encode_key(&[Value::Int(index_id as i64), Value::Int(1)])
}

pub(crate) fn decode_corpus(bytes: &[u8]) -> Result<(u64, u64), ExecError> {
    let bad = || ExecError::exec("text index corpus counters do not decode");
    if bytes.len() != 16 {
        return Err(bad());
    }
    let docs = u64::from_le_bytes(bytes[..8].try_into().map_err(|_| bad())?);
    let total = u64::from_le_bytes(bytes[8..].try_into().map_err(|_| bad())?);
    Ok((docs, total))
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
    fn column(&self, r: &ColumnRef) -> Result<Value, ExecError> {
        Ok(self.values[position_in(self.table, r)?].clone())
    }
}

/// Resolve a reference against a single-table statement. A qualifier must
/// name that table.
fn position_in(table: &Table, r: &ColumnRef) -> Result<usize, ExecError> {
    if let Some(t) = &r.table {
        if t != &table.name {
            return Err(ExecError::plan(format!(
                "no table named '{t}' in this statement"
            )));
        }
    }
    table.column_position(&r.column).ok_or_else(|| {
        ExecError::plan(format!(
            "table '{}' has no column '{}'",
            table.name, r.column
        ))
    })
}

/// Every column reference in the expression must exist, checked up front
/// so an empty table still reports the typo.
fn validate_columns(table: &Table, expr: &Expr) -> Result<(), ExecError> {
    match expr {
        Expr::Literal(_) => Ok(()),
        Expr::Column(r) => position_in(table, r).map(|_| ()),
        Expr::Unary(_, inner) => validate_columns(table, inner),
        Expr::Binary(l, _, r) => {
            validate_columns(table, l)?;
            validate_columns(table, r)
        }
    }
}
