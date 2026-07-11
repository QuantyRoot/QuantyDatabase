//! Planning: turn a filter into an access path.
//!
//! Three access paths in phase 2, tried in this order:
//!
//! 1. `KeyLookup`: the filter pins every primary key column to a literal
//! 2. `IndexScan`: some conjunct is `indexed_column = literal`
//! 3. `SeqScan`: everything else
//!
//! Conjuncts consumed by the access path disappear from the residual
//! filter; the rest is evaluated per row. `explain` prints exactly this
//! structure so the golden tests can pin planner behavior.

use quanty_core::Value;
use quanty_ql::ast::{BinaryOp, ColumnRef, Direction, Expr, Join, JoinKind, TypeName};
use quanty_ql::pretty;

use crate::catalog::Table;
use crate::error::ExecError;
use crate::value_ops;

#[derive(Debug, Clone, PartialEq)]
pub enum Access {
    /// Point lookup by full primary key. Values in key column order.
    KeyLookup { key_values: Vec<Value> },
    /// Equality scan over one secondary index.
    IndexScan {
        column: usize,
        index_id: u64,
        value: Value,
    },
    /// Walk the whole table.
    SeqScan,
}

#[derive(Debug, Clone)]
pub struct AccessPlan {
    pub access: Access,
    /// What remains of the filter after the access path consumed its part.
    pub residual: Option<Expr>,
}

/// Split an expression into its top-level `and` conjuncts.
fn conjuncts(expr: &Expr) -> Vec<&Expr> {
    match expr {
        Expr::Binary(l, BinaryOp::And, r) => {
            let mut out = conjuncts(l);
            out.extend(conjuncts(r));
            out
        }
        other => vec![other],
    }
}

/// If this conjunct is `column = literal` (either side), name them.
fn as_column_eq_literal(expr: &Expr) -> Option<(&ColumnRef, &Value)> {
    let Expr::Binary(l, BinaryOp::Eq, r) = expr else {
        return None;
    };
    match (l.as_ref(), r.as_ref()) {
        (Expr::Column(c), Expr::Literal(v)) | (Expr::Literal(v), Expr::Column(c)) => Some((c, v)),
        _ => None,
    }
}

/// Resolve a reference against one table for planning purposes. A
/// qualifier naming another table makes this a non-candidate; validation
/// has already rejected anything that cannot resolve at all.
fn position_for_plan(table: &Table, r: &ColumnRef) -> Option<usize> {
    match &r.table {
        Some(t) if t != &table.name => None,
        _ => table.column_position(&r.column),
    }
}

fn rebuild_conjunction(parts: &[&Expr]) -> Option<Expr> {
    let mut iter = parts.iter();
    let first = (*iter.next()?).clone();
    Some(iter.fold(first, |acc, e| {
        Expr::Binary(Box::new(acc), BinaryOp::And, Box::new((*e).clone()))
    }))
}

pub fn plan_access(table: &Table, filter: Option<&Expr>) -> Result<AccessPlan, ExecError> {
    let Some(filter) = filter else {
        return Ok(AccessPlan {
            access: Access::SeqScan,
            residual: None,
        });
    };
    let parts = conjuncts(filter);

    // 1. full primary key pinned by equality?
    let key_positions = table.key_positions();
    let mut key_values: Vec<Option<(&Value, usize)>> = vec![None; key_positions.len()];
    for (part_idx, part) in parts.iter().enumerate() {
        if let Some((col, value)) = as_column_eq_literal(part) {
            if let Some(pos) = position_for_plan(table, col) {
                if let Some(slot) = key_positions.iter().position(|&k| k == pos) {
                    if key_values[slot].is_none() && !matches!(value, Value::Null) {
                        // coercion keeps int literals usable against float keys
                        key_values[slot] = Some((value, part_idx));
                    }
                }
            }
        }
    }
    if key_values.iter().all(Option::is_some) {
        let consumed: Vec<usize> = key_values.iter().map(|s| s.unwrap().1).collect();
        let mut values = Vec::with_capacity(key_positions.len());
        for (slot, &kpos) in key_positions.iter().enumerate() {
            let col = &table.columns[kpos];
            let raw = key_values[slot].unwrap().0.clone();
            values.push(
                value_ops::coerce(raw, col.ty, false)
                    .map_err(|e| ExecError::plan(format!("key column '{}': {e}", col.name)))?,
            );
        }
        let residual: Vec<&Expr> = parts
            .iter()
            .enumerate()
            .filter(|(i, _)| !consumed.contains(i))
            .map(|(_, e)| *e)
            .collect();
        return Ok(AccessPlan {
            access: Access::KeyLookup { key_values: values },
            residual: rebuild_conjunction(&residual),
        });
    }

    // 2. an indexed column pinned by equality?
    for (part_idx, part) in parts.iter().enumerate() {
        if let Some((col, value)) = as_column_eq_literal(part) {
            if let Some(pos) = position_for_plan(table, col) {
                if let Some(index_id) = table.columns[pos].index_id {
                    let coerced = value_ops::coerce(
                        value.clone(),
                        table.columns[pos].ty,
                        true, // null is a legal index probe: col = null finds nulls
                    )
                    .map_err(|e| ExecError::plan(format!("column '{col}': {e}")))?;
                    let residual: Vec<&Expr> = parts
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| *i != part_idx)
                        .map(|(_, e)| *e)
                        .collect();
                    return Ok(AccessPlan {
                        access: Access::IndexScan {
                            column: pos,
                            index_id,
                            value: coerced,
                        },
                        residual: rebuild_conjunction(&residual),
                    });
                }
            }
        }
    }

    // 3. walk everything
    Ok(AccessPlan {
        access: Access::SeqScan,
        residual: Some(filter.clone()),
    })
}

// ---------------------------------------------------------------------------
// explain rendering
// ---------------------------------------------------------------------------

pub struct ExplainNode {
    pub label: String,
    pub children: Vec<ExplainNode>,
}

impl ExplainNode {
    pub fn leaf(label: String) -> Self {
        ExplainNode {
            label,
            children: Vec::new(),
        }
    }

    pub fn over(label: String, child: ExplainNode) -> Self {
        ExplainNode {
            label,
            children: vec![child],
        }
    }

    pub fn render(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.render_into(0, &mut out);
        out
    }

    fn render_into(&self, depth: usize, out: &mut Vec<String>) {
        out.push(format!("{}{}", "  ".repeat(depth), self.label));
        for child in &self.children {
            child.render_into(depth + 1, out);
        }
    }
}

pub fn explain_access(table: &Table, plan: &AccessPlan) -> ExplainNode {
    let access = match &plan.access {
        Access::KeyLookup { key_values } => {
            let keys = table.key_positions();
            let parts: Vec<String> = keys
                .iter()
                .zip(key_values)
                .map(|(&pos, v)| format!("{} = {}", table.columns[pos].name, pretty::literal(v)))
                .collect();
            ExplainNode::leaf(format!("KeyLookup {} ({})", table.name, parts.join(", ")))
        }
        Access::IndexScan { column, value, .. } => ExplainNode::leaf(format!(
            "IndexScan {} via {} = {}",
            table.name,
            table.columns[*column].name,
            pretty::literal(value)
        )),
        Access::SeqScan => ExplainNode::leaf(format!("SeqScan {}", table.name)),
    };
    match &plan.residual {
        Some(expr) => ExplainNode::over(format!("Filter {}", pretty::expr(expr)), access),
        None => access,
    }
}

pub fn explain_get(
    table: &Table,
    plan: &AccessPlan,
    order: &Option<(ColumnRef, Direction)>,
    limit: Option<u64>,
) -> ExplainNode {
    let mut node = explain_access(table, plan);
    if let Some((col, dir)) = order {
        let dir = match dir {
            Direction::Asc => "asc",
            Direction::Desc => "desc",
        };
        node = ExplainNode::over(format!("Sort {col} {dir}"), node);
    }
    if let Some(n) = limit {
        node = ExplainNode::over(format!("Limit {n}"), node);
    }
    node
}

// ---------------------------------------------------------------------------
// join planning
// ---------------------------------------------------------------------------

/// How one join step runs. Every strategy is only an accelerator: the
/// executor evaluates the full `on` condition on every candidate row, so a
/// probe can never change results, only skip rows that could not match.
#[derive(Debug, Clone, PartialEq)]
pub enum JoinStrategy {
    /// Materialize the right table once, test every pair.
    NestedLoop,
    /// Probe the right table's single-column primary key with the value of
    /// a left column, per left row.
    KeyProbe { left: ColumnRef },
    /// Probe a secondary index on the right table with the value of a left
    /// column, per left row.
    IndexProbe {
        left: ColumnRef,
        column: usize,
        index_id: u64,
    },
}

/// Can a value of the left column's type probe the right column without a
/// coercion that could fail? Equal types always; int probes float because
/// widening never fails. Everything else (float into int, cross-kind) runs
/// as a nested loop so probe execution has no failure mode the plain
/// comparison lacks.
fn probe_compatible(left: TypeName, right: TypeName) -> bool {
    left == right || (left == TypeName::Int && right == TypeName::Float)
}

/// Does this reference resolve to a column of `table`, given that `others`
/// are the other tables in scope? Validation has already guaranteed every
/// reference resolves unambiguously somewhere, so this only has to decide
/// "here or elsewhere".
fn resolves_to(r: &ColumnRef, table: &Table, others: &[&Table]) -> Option<usize> {
    match &r.table {
        Some(t) => {
            if t == &table.name {
                table.column_position(&r.column)
            } else {
                None
            }
        }
        None => {
            if others
                .iter()
                .any(|o| o.column_position(&r.column).is_some())
            {
                None
            } else {
                table.column_position(&r.column)
            }
        }
    }
}

/// Pick a strategy for joining `right` onto the tables in `left`. Looks
/// for an `on` conjunct of the shape `left_column = right_column` (either
/// side) with probe-compatible types; the right column must be the whole
/// primary key or carry a secondary index. Key probes win over index
/// probes, first match wins otherwise.
pub fn plan_join(left: &[&Table], right: &Table, on: &Expr) -> JoinStrategy {
    let mut index_probe = None;
    for part in conjuncts(on) {
        let Expr::Binary(l, BinaryOp::Eq, r) = part else {
            continue;
        };
        let (Expr::Column(a), Expr::Column(b)) = (l.as_ref(), r.as_ref()) else {
            continue;
        };
        // one side must be a right column, the other a left column
        let sides = [(a, b), (b, a)];
        for (right_ref, left_ref) in sides {
            let Some(right_pos) = resolves_to(right_ref, right, left) else {
                continue;
            };
            let Some(left_pos) = left
                .iter()
                .find_map(|t| resolves_to(left_ref, t, &[right]).map(|p| (t, p)))
            else {
                continue;
            };
            let left_ty = left_pos.0.columns[left_pos.1].ty;
            let right_col = &right.columns[right_pos];
            if !probe_compatible(left_ty, right_col.ty) {
                continue;
            }
            let key = right.key_positions();
            if key.len() == 1 && key[0] == right_pos {
                return JoinStrategy::KeyProbe {
                    left: left_ref.clone(),
                };
            }
            if let Some(index_id) = right_col.index_id {
                index_probe.get_or_insert(JoinStrategy::IndexProbe {
                    left: left_ref.clone(),
                    column: right_pos,
                    index_id,
                });
            }
        }
    }
    index_probe.unwrap_or(JoinStrategy::NestedLoop)
}

/// The explain leaf for the right side of one join step.
pub fn explain_join_probe(right: &Table, strategy: &JoinStrategy) -> ExplainNode {
    match strategy {
        JoinStrategy::NestedLoop => ExplainNode::leaf(format!("SeqScan {}", right.name)),
        JoinStrategy::KeyProbe { .. } => {
            let key = right.key_positions();
            ExplainNode::leaf(format!(
                "KeyProbe {} ({})",
                right.name, right.columns[key[0]].name
            ))
        }
        JoinStrategy::IndexProbe { column, .. } => ExplainNode::leaf(format!(
            "IndexProbe {} via {}",
            right.name, right.columns[*column].name
        )),
    }
}

/// The label for one join step; the children are the left plan and the
/// probe leaf.
pub fn join_label(join: &Join, strategy: &JoinStrategy) -> String {
    let kind = match join.kind {
        JoinKind::Inner => "inner",
        JoinKind::Left => "left",
    };
    let name = match strategy {
        JoinStrategy::NestedLoop => "NestedLoopJoin",
        JoinStrategy::KeyProbe { .. } | JoinStrategy::IndexProbe { .. } => "IndexNestedLoopJoin",
    };
    format!("{name} {kind} on {}", pretty::expr(&join.on))
}
