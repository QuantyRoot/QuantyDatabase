//! The catalog: table definitions living in the catalog tree.
//!
//! Catalog keys use the same tuple encoding as everything else:
//!
//! - `("seq")` holds the next object id (tables and indexes share one
//!   counter, ids never repeat)
//! - `("table", name)` holds the serialized table definition
//!
//! Because the catalog tree and the data tree commit through the same meta,
//! schema changes are atomic with the data they affect, and time travel
//! sees the schema as it was.

use quanty_core::{encode_key, Value};
use quanty_ql::ast::{self, TypeName};

use crate::error::ExecError;

#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    pub id: u64,
    pub name: String,
    pub columns: Vec<Column>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub name: String,
    pub ty: TypeName,
    pub nullable: bool,
    pub key: bool,
    /// Object id of this column's secondary index, if one exists.
    pub index_id: Option<u64>,
    pub default: Option<Value>,
}

impl Table {
    /// Positions of the primary key columns, in declaration order.
    pub fn key_positions(&self) -> Vec<usize> {
        self.columns
            .iter()
            .enumerate()
            .filter(|(_, c)| c.key)
            .map(|(i, _)| i)
            .collect()
    }

    pub fn column_position(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    /// Build a validated `Table` from a parsed definition.
    pub fn from_ast(def: &ast::TableDef, id: u64) -> Result<Table, ExecError> {
        let mut columns = Vec::with_capacity(def.columns.len());
        for col in &def.columns {
            if columns.iter().any(|c: &Column| c.name == col.name) {
                return Err(ExecError::plan(format!(
                    "column '{}' is defined twice in table '{}'",
                    col.name, def.name
                )));
            }
            if col.key && col.nullable {
                return Err(ExecError::plan(format!(
                    "key column '{}' cannot be @null",
                    col.name
                )));
            }
            if let Some(default) = &col.default {
                let coerced = crate::value_ops::coerce(default.clone(), col.ty, col.nullable)
                    .map_err(|e| {
                        ExecError::plan(format!("default for column '{}': {e}", col.name))
                    })?;
                columns.push(Column {
                    name: col.name.clone(),
                    ty: col.ty,
                    nullable: col.nullable,
                    key: col.key,
                    index_id: None,
                    default: Some(coerced),
                });
            } else {
                columns.push(Column {
                    name: col.name.clone(),
                    ty: col.ty,
                    nullable: col.nullable,
                    key: col.key,
                    index_id: None,
                    default: None,
                });
            }
        }
        let table = Table {
            id,
            name: def.name.clone(),
            columns,
        };
        if table.key_positions().is_empty() {
            return Err(ExecError::plan(format!(
                "table '{}' needs at least one @key column",
                def.name
            )));
        }
        Ok(table)
    }
}

// ---------------------------------------------------------------------------
// catalog keys
// ---------------------------------------------------------------------------

pub fn table_key(name: &str) -> Vec<u8> {
    encode_key(&[Value::Text("table".into()), Value::Text(name.into())])
}

pub fn tables_prefix() -> Vec<u8> {
    encode_key(&[Value::Text("table".into())])
}

pub fn seq_key() -> Vec<u8> {
    encode_key(&[Value::Text("seq".into())])
}

// ---------------------------------------------------------------------------
// serialization
// ---------------------------------------------------------------------------

const CATALOG_VERSION: u8 = 1;

fn type_tag(ty: TypeName) -> u8 {
    match ty {
        TypeName::Int => 0,
        TypeName::Float => 1,
        TypeName::Text => 2,
        TypeName::Bytes => 3,
        TypeName::Bool => 4,
    }
}

fn type_from_tag(tag: u8) -> Option<TypeName> {
    Some(match tag {
        0 => TypeName::Int,
        1 => TypeName::Float,
        2 => TypeName::Text,
        3 => TypeName::Bytes,
        4 => TypeName::Bool,
        _ => return None,
    })
}

impl Table {
    pub fn serialize(&self) -> Vec<u8> {
        let mut out = vec![CATALOG_VERSION];
        out.extend_from_slice(&self.id.to_le_bytes());
        put_str(&mut out, &self.name);
        out.extend_from_slice(&(self.columns.len() as u16).to_le_bytes());
        for col in &self.columns {
            put_str(&mut out, &col.name);
            out.push(type_tag(col.ty));
            let mut flags = 0u8;
            if col.key {
                flags |= 1;
            }
            if col.nullable {
                flags |= 2;
            }
            out.push(flags);
            out.extend_from_slice(&col.index_id.unwrap_or(0).to_le_bytes());
            match &col.default {
                Some(v) => {
                    out.push(1);
                    let enc = encode_key(std::slice::from_ref(v));
                    out.extend_from_slice(&(enc.len() as u32).to_le_bytes());
                    out.extend_from_slice(&enc);
                }
                None => out.push(0),
            }
        }
        out
    }

    pub fn deserialize(buf: &[u8]) -> Result<Table, ExecError> {
        let bad = || ExecError::exec("catalog entry does not deserialize, this is a bug");
        let mut r = Reader { buf, at: 0 };
        if r.u8().ok_or_else(bad)? != CATALOG_VERSION {
            return Err(ExecError::exec("catalog entry has an unknown version"));
        }
        let id = r.u64().ok_or_else(bad)?;
        let name = r.str().ok_or_else(bad)?;
        let ncols = r.u16().ok_or_else(bad)? as usize;
        let mut columns = Vec::with_capacity(ncols);
        for _ in 0..ncols {
            let cname = r.str().ok_or_else(bad)?;
            let ty = type_from_tag(r.u8().ok_or_else(bad)?).ok_or_else(bad)?;
            let flags = r.u8().ok_or_else(bad)?;
            let index_id = r.u64().ok_or_else(bad)?;
            let default = match r.u8().ok_or_else(bad)? {
                0 => None,
                1 => {
                    let len = r.u32().ok_or_else(bad)? as usize;
                    let enc = r.bytes(len).ok_or_else(bad)?;
                    let mut values = quanty_core::decode_key(enc)
                        .map_err(|_| ExecError::exec("catalog default does not decode"))?;
                    if values.len() != 1 {
                        return Err(bad());
                    }
                    Some(values.remove(0))
                }
                _ => return Err(bad()),
            };
            columns.push(Column {
                name: cname,
                ty,
                nullable: flags & 2 != 0,
                key: flags & 1 != 0,
                index_id: (index_id != 0).then_some(index_id),
                default,
            });
        }
        if r.at != buf.len() {
            return Err(bad());
        }
        Ok(Table { id, name, columns })
    }
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u16).to_le_bytes());
    out.extend_from_slice(s.as_bytes());
}

struct Reader<'a> {
    buf: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        let out = self.buf.get(self.at..self.at + n)?;
        self.at += n;
        Some(out)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.bytes(1)?[0])
    }

    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.bytes(2)?.try_into().ok()?))
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.bytes(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.bytes(8)?.try_into().ok()?))
    }

    fn str(&mut self) -> Option<String> {
        let len = self.u16()? as usize;
        String::from_utf8(self.bytes(len)?.to_vec()).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_roundtrips_through_the_codec() {
        let def = quanty_ql::parse(
            "table users { id: int @key, name: text @index, score: float = 1.5, blob: bytes @null }",
        )
        .unwrap();
        let quanty_ql::ast::Statement::TableDef(def) = def else {
            panic!()
        };
        let mut table = Table::from_ast(&def, 7).unwrap();
        table.columns[1].index_id = Some(8);
        let bytes = table.serialize();
        assert_eq!(Table::deserialize(&bytes).unwrap(), table);
    }

    #[test]
    fn validation_rejects_broken_definitions() {
        for src in [
            "table t { a: int }",                      // no key
            "table t { a: int @key @null }",           // nullable key
            "table t { a: int @key, a: text }",        // duplicate column
            "table t { a: int @key, b: int = \"x\" }", // default type mismatch
        ] {
            let quanty_ql::ast::Statement::TableDef(def) = quanty_ql::parse(src).unwrap() else {
                panic!()
            };
            assert!(Table::from_ast(&def, 1).is_err(), "accepted: {src}");
        }
    }
}
