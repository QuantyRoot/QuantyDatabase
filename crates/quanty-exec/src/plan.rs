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
use quanty_ql::ast::{BinaryOp, Direction, Expr};
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
fn as_column_eq_literal(expr: &Expr) -> Option<(&str, &Value)> {
    let Expr::Binary(l, BinaryOp::Eq, r) = expr else {
        return None;
    };
    match (l.as_ref(), r.as_ref()) {
        (Expr::Column(c), Expr::Literal(v)) | (Expr::Literal(v), Expr::Column(c)) => {
            Some((c.as_str(), v))
        }
        _ => None,
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
            if let Some(pos) = table.column_position(col) {
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
            if let Some(pos) = table.column_position(col) {
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
    pub child: Option<Box<ExplainNode>>,
}

impl ExplainNode {
    pub fn leaf(label: String) -> Self {
        ExplainNode { label, child: None }
    }

    pub fn over(label: String, child: ExplainNode) -> Self {
        ExplainNode {
            label,
            child: Some(Box::new(child)),
        }
    }

    pub fn render(&self) -> Vec<String> {
        let mut out = Vec::new();
        let mut node = Some(self);
        let mut depth = 0;
        while let Some(n) = node {
            out.push(format!("{}{}", "  ".repeat(depth), n.label));
            node = n.child.as_deref();
            depth += 1;
        }
        out
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
    order: &Option<(String, Direction)>,
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
