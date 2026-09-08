//! Mapping rows to structs.
//!
//! The interesting half of this file is [`Rows::into_typed`] over in
//! `lib.rs`: matching a query's columns against a struct's fields is where
//! the mistakes live, so it is written once here rather than generated
//! once per struct (ADR-031).

use crate::{Error, ErrorKind, Result, Value};

/// A struct that maps to one row of one table.
///
/// Implement this with `#[derive(Row)]` rather than by hand.
pub trait Row: Sized {
    /// The table these rows live in.
    const TABLE: &'static str;

    /// The column names, in the order [`Row::from_values`] expects them
    /// and [`Row::to_values`] yields them.
    const COLUMNS: &'static [&'static str];

    /// Build one from values already ordered as [`Row::COLUMNS`].
    fn from_values(values: Vec<Value>) -> Result<Self>;

    /// The values, in the same order, paired with their column names.
    fn to_values(&self) -> Vec<(&'static str, Value)>;
}

/// A Rust type one column value can become.
pub trait FromValue: Sized {
    /// Convert, or say what was expected and what arrived.
    fn from_value(value: Value) -> Result<Self>;
}

/// A Rust type that can become a column value.
pub trait IntoValue {
    fn into_value(self) -> Value;
}

/// Say what was expected and what arrived, without printing the value.
fn wrong_type(expected: &str, got: &Value) -> Error {
    Error {
        kind: ErrorKind::Exec,
        message: format!("expected {expected}, got {}", type_name(got)),
    }
}

fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Text(_) => "text",
        Value::Bytes(_) => "bytes",
    }
}

impl FromValue for i64 {
    fn from_value(value: Value) -> Result<Self> {
        match value {
            Value::Int(i) => Ok(i),
            other => Err(wrong_type("int", &other)),
        }
    }
}

impl IntoValue for i64 {
    fn into_value(self) -> Value {
        Value::Int(self)
    }
}

/// A narrow integer is range checked rather than truncated: a column that
/// outgrew the field is a bug worth hearing about.
macro_rules! narrow_int {
    ($t:ty) => {
        impl FromValue for $t {
            fn from_value(value: Value) -> Result<Self> {
                match value {
                    Value::Int(i) => <$t>::try_from(i).map_err(|_| Error {
                        kind: ErrorKind::Exec,
                        message: format!("{i} does not fit in {}", std::any::type_name::<$t>()),
                    }),
                    other => Err(wrong_type("int", &other)),
                }
            }
        }

        impl IntoValue for $t {
            fn into_value(self) -> Value {
                Value::Int(self as i64)
            }
        }
    };
}

narrow_int!(i32);
narrow_int!(u32);

impl FromValue for f64 {
    fn from_value(value: Value) -> Result<Self> {
        match value {
            Value::Float(f) => Ok(f),
            other => Err(wrong_type("float", &other)),
        }
    }
}

impl IntoValue for f64 {
    fn into_value(self) -> Value {
        Value::Float(self)
    }
}

impl FromValue for bool {
    fn from_value(value: Value) -> Result<Self> {
        match value {
            Value::Bool(b) => Ok(b),
            other => Err(wrong_type("bool", &other)),
        }
    }
}

impl IntoValue for bool {
    fn into_value(self) -> Value {
        Value::Bool(self)
    }
}

impl FromValue for String {
    fn from_value(value: Value) -> Result<Self> {
        match value {
            Value::Text(t) => Ok(t),
            other => Err(wrong_type("text", &other)),
        }
    }
}

impl IntoValue for String {
    fn into_value(self) -> Value {
        Value::Text(self)
    }
}

impl FromValue for Vec<u8> {
    fn from_value(value: Value) -> Result<Self> {
        match value {
            Value::Bytes(b) => Ok(b),
            other => Err(wrong_type("bytes", &other)),
        }
    }
}

impl IntoValue for Vec<u8> {
    fn into_value(self) -> Value {
        Value::Bytes(self)
    }
}

impl<T: FromValue> FromValue for Option<T> {
    fn from_value(value: Value) -> Result<Self> {
        match value {
            Value::Null => Ok(None),
            other => T::from_value(other).map(Some),
        }
    }
}

impl<T: IntoValue> IntoValue for Option<T> {
    fn into_value(self) -> Value {
        match self {
            Some(v) => v.into_value(),
            None => Value::Null,
        }
    }
}

/// Plumbing the derive emits calls to. Not part of the promise in ADR-030.
#[doc(hidden)]
pub mod __private {
    use super::{FromValue, Value};
    use crate::{Error, ErrorKind, Result};

    /// Take the next value and convert it, naming the field if either
    /// half fails.
    pub fn field<T: FromValue>(
        struct_name: &'static str,
        field: &'static str,
        value: Option<Value>,
    ) -> Result<T> {
        let value = value.ok_or_else(|| Error {
            kind: ErrorKind::Exec,
            message: format!("{struct_name}.{field}: no value for this column"),
        })?;
        T::from_value(value).map_err(|e| Error {
            kind: ErrorKind::Exec,
            message: format!("{struct_name}.{field}: {e}"),
        })
    }
}
