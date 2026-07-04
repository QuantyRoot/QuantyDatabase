//! Value semantics: expression evaluation, comparisons, coercion, output
//! rendering. The rules live in docs/QQL.md; the short version:
//!
//! - `=` and `!=` treat null as an ordinary value: `null = null` is true
//! - `<`, `<=`, `>`, `>=` involving null are always false
//! - arithmetic with null yields null
//! - int and float compare and mix numerically; every other type mix is an
//!   error, not a silent coercion
//! - int arithmetic is checked: overflow and division by zero are errors

use std::cmp::Ordering;

use quanty_core::Value;
use quanty_ql::ast::{BinaryOp, Expr, TypeName, UnaryOp};

use crate::error::ExecError;

/// A row scope: resolve column names to values during evaluation.
pub trait Scope {
    fn column(&self, name: &str) -> Result<Value, ExecError>;
}

/// The empty scope, for evaluating constant expressions.
pub struct NoScope;

impl Scope for NoScope {
    fn column(&self, name: &str) -> Result<Value, ExecError> {
        Err(ExecError::plan(format!("unknown column '{name}'")))
    }
}

pub fn eval(expr: &Expr, scope: &dyn Scope) -> Result<Value, ExecError> {
    match expr {
        Expr::Literal(v) => Ok(v.clone()),
        Expr::Column(name) => scope.column(name),
        Expr::Unary(UnaryOp::Neg, inner) => match eval(inner, scope)? {
            Value::Int(i) => Ok(Value::Int(
                i.checked_neg()
                    .ok_or_else(|| ExecError::exec("integer overflow"))?,
            )),
            Value::Float(f) => Ok(Value::Float(-f)),
            Value::Null => Ok(Value::Null),
            other => Err(ExecError::exec(format!(
                "cannot negate a {}",
                type_of(&other)
            ))),
        },
        Expr::Unary(UnaryOp::Not, inner) => match eval(inner, scope)? {
            Value::Bool(b) => Ok(Value::Bool(!b)),
            Value::Null => Ok(Value::Null),
            other => Err(ExecError::exec(format!(
                "'not' wants a bool, got a {}",
                type_of(&other)
            ))),
        },
        Expr::Binary(lhs, op, rhs) => eval_binary(lhs, *op, rhs, scope),
    }
}

fn eval_binary(
    lhs: &Expr,
    op: BinaryOp,
    rhs: &Expr,
    scope: &dyn Scope,
) -> Result<Value, ExecError> {
    // and/or short-circuit; null acts as "unknown but falsy enough"
    if op == BinaryOp::And || op == BinaryOp::Or {
        let l = as_condition(eval(lhs, scope)?)?;
        if op == BinaryOp::And && !l {
            return Ok(Value::Bool(false));
        }
        if op == BinaryOp::Or && l {
            return Ok(Value::Bool(true));
        }
        return Ok(Value::Bool(as_condition(eval(rhs, scope)?)?));
    }

    let l = eval(lhs, scope)?;
    let r = eval(rhs, scope)?;
    match op {
        BinaryOp::Eq => Ok(Value::Bool(values_equal(&l, &r)?)),
        BinaryOp::NotEq => Ok(Value::Bool(!values_equal(&l, &r)?)),
        BinaryOp::Lt | BinaryOp::LtEq | BinaryOp::Gt | BinaryOp::GtEq => {
            let ord = match compare(&l, &r)? {
                Some(ord) => ord,
                None => return Ok(Value::Bool(false)), // null on either side
            };
            Ok(Value::Bool(match op {
                BinaryOp::Lt => ord == Ordering::Less,
                BinaryOp::LtEq => ord != Ordering::Greater,
                BinaryOp::Gt => ord == Ordering::Greater,
                BinaryOp::GtEq => ord != Ordering::Less,
                _ => unreachable!(),
            }))
        }
        BinaryOp::Add | BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div | BinaryOp::Mod => {
            arith(l, op, r)
        }
        BinaryOp::And | BinaryOp::Or => unreachable!("handled above"),
    }
}

/// `=` semantics: null is a plain value here.
pub fn values_equal(l: &Value, r: &Value) -> Result<bool, ExecError> {
    match (l, r) {
        (Value::Null, Value::Null) => Ok(true),
        (Value::Null, _) | (_, Value::Null) => Ok(false),
        _ => Ok(compare(l, r)? == Some(Ordering::Equal)),
    }
}

/// Ordering comparison. `None` means null was involved. Type mismatches
/// (other than int/float) are errors.
pub fn compare(l: &Value, r: &Value) -> Result<Option<Ordering>, ExecError> {
    Ok(Some(match (l, r) {
        (Value::Null, _) | (_, Value::Null) => return Ok(None),
        (Value::Int(a), Value::Int(b)) => a.cmp(b),
        (Value::Float(a), Value::Float(b)) => a.total_cmp(b),
        (Value::Int(a), Value::Float(b)) => (*a as f64).total_cmp(b),
        (Value::Float(a), Value::Int(b)) => a.total_cmp(&(*b as f64)),
        (Value::Text(a), Value::Text(b)) => a.cmp(b),
        (Value::Bytes(a), Value::Bytes(b)) => a.cmp(b),
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        (a, b) => {
            return Err(ExecError::exec(format!(
                "cannot compare a {} to a {}",
                type_of(a),
                type_of(b)
            )))
        }
    }))
}

fn arith(l: Value, op: BinaryOp, r: Value) -> Result<Value, ExecError> {
    let sym = op.as_str();
    match (l, r) {
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (Value::Int(a), Value::Int(b)) => {
            let out = match op {
                BinaryOp::Add => a.checked_add(b),
                BinaryOp::Sub => a.checked_sub(b),
                BinaryOp::Mul => a.checked_mul(b),
                BinaryOp::Div => {
                    if b == 0 {
                        return Err(ExecError::exec("division by zero"));
                    }
                    a.checked_div(b)
                }
                BinaryOp::Mod => {
                    if b == 0 {
                        return Err(ExecError::exec("division by zero"));
                    }
                    a.checked_rem(b)
                }
                _ => unreachable!(),
            };
            Ok(Value::Int(
                out.ok_or_else(|| ExecError::exec("integer overflow"))?,
            ))
        }
        (a @ (Value::Int(_) | Value::Float(_)), b @ (Value::Int(_) | Value::Float(_))) => {
            let a = as_f64(&a);
            let b = as_f64(&b);
            Ok(Value::Float(match op {
                BinaryOp::Add => a + b,
                BinaryOp::Sub => a - b,
                BinaryOp::Mul => a * b,
                BinaryOp::Div => a / b,
                BinaryOp::Mod => a % b,
                _ => unreachable!(),
            }))
        }
        (Value::Text(a), Value::Text(b)) if op == BinaryOp::Add => Ok(Value::Text(a + &b)),
        (a, b) => Err(ExecError::exec(format!(
            "cannot apply {sym} to a {} and a {}",
            type_of(&a),
            type_of(&b)
        ))),
    }
}

fn as_f64(v: &Value) -> f64 {
    match v {
        Value::Int(i) => *i as f64,
        Value::Float(f) => *f,
        _ => unreachable!("checked by caller"),
    }
}

/// A filter must produce a bool; null counts as false.
pub fn as_condition(v: Value) -> Result<bool, ExecError> {
    match v {
        Value::Bool(b) => Ok(b),
        Value::Null => Ok(false),
        other => Err(ExecError::exec(format!(
            "the condition is a {}, not a bool",
            type_of(&other)
        ))),
    }
}

/// Fit a value into a column: exact type, int widening into float columns,
/// null only where allowed.
pub fn coerce(v: Value, ty: TypeName, nullable: bool) -> Result<Value, ExecError> {
    match (&v, ty) {
        (Value::Null, _) => {
            if nullable {
                Ok(Value::Null)
            } else {
                Err(ExecError::exec("null in a column that is not @null"))
            }
        }
        (Value::Int(_), TypeName::Int) => Ok(v),
        (Value::Int(i), TypeName::Float) => Ok(Value::Float(*i as f64)),
        (Value::Float(_), TypeName::Float) => Ok(v),
        (Value::Text(_), TypeName::Text) => Ok(v),
        (Value::Bytes(_), TypeName::Bytes) => Ok(v),
        (Value::Bool(_), TypeName::Bool) => Ok(v),
        _ => Err(ExecError::exec(format!(
            "a {} value does not fit into a column of type {}",
            type_of(&v),
            ty.as_str()
        ))),
    }
}

/// Sort order for `order by`: null first, then the natural order of the
/// column's type. Columns are homogeneous, so cross-type never happens.
pub fn sort_cmp(l: &Value, r: &Value) -> Ordering {
    match (l, r) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        _ => compare(l, r).ok().flatten().unwrap_or(Ordering::Equal),
    }
}

pub fn type_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Text(_) => "text",
        Value::Bytes(_) => "bytes",
    }
}

/// Render a value for query output.
pub fn render(v: &Value) -> String {
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(true) => "true".to_string(),
        Value::Bool(false) => "false".to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => format!("{f:?}"),
        Value::Text(s) => s.clone(),
        Value::Bytes(b) => {
            let hex: String = b.iter().map(|byte| format!("{byte:02x}")).collect();
            format!("x\"{hex}\"")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(src: &str) -> Result<Value, ExecError> {
        let stmt = quanty_ql::parse(&format!("get t where {src}")).unwrap();
        let quanty_ql::ast::Statement::Get(get) = stmt else {
            panic!()
        };
        eval(&get.filter.unwrap(), &NoScope)
    }

    #[test]
    fn null_rules_are_what_the_docs_say() {
        assert_eq!(v("null = null").unwrap(), Value::Bool(true));
        assert_eq!(v("null = 1").unwrap(), Value::Bool(false));
        assert_eq!(v("null != 1").unwrap(), Value::Bool(true));
        assert_eq!(v("null < 1").unwrap(), Value::Bool(false));
        assert_eq!(v("null > 1").unwrap(), Value::Bool(false));
        assert_eq!(v("(null + 1) = null").unwrap(), Value::Bool(true));
    }

    #[test]
    fn int_arithmetic_is_checked() {
        assert!(v("9223372036854775807 + 1")
            .unwrap_err()
            .to_string()
            .contains("overflow"));
        assert!(v("1 / 0")
            .unwrap_err()
            .to_string()
            .contains("division by zero"));
        assert!(v("1 % 0")
            .unwrap_err()
            .to_string()
            .contains("division by zero"));
        assert_eq!(v("7 / 2").unwrap(), Value::Int(3), "int division truncates");
    }

    #[test]
    fn mixed_numerics_and_type_errors() {
        assert_eq!(v("1 < 1.5").unwrap(), Value::Bool(true));
        assert_eq!(v("2 = 2.0").unwrap(), Value::Bool(true));
        assert_eq!(v("1 + 0.5").unwrap(), Value::Float(1.5));
        assert!(v("1 = \"1\"")
            .unwrap_err()
            .to_string()
            .contains("cannot compare"));
        assert!(v("true + 1")
            .unwrap_err()
            .to_string()
            .contains("cannot apply"));
    }

    #[test]
    fn text_concat_works() {
        assert_eq!(v("(\"a\" + \"b\") = \"ab\"").unwrap(), Value::Bool(true));
    }
}
