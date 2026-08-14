//! The schema table.
//!
//! Every SQLite database keeps its own schema in an ordinary table rooted at
//! page 1, with five columns: the kind of object, its name, the name of the
//! table it belongs to, the page its b-tree starts on, and the statement
//! that created it. Reading it is how anything else in the file is found.
//!
//! Three things about it are not obvious and each one has already produced
//! a wrong assumption in this crate's tests:
//!
//! An index SQLite created on its own, for a primary key or a unique
//! constraint, has no statement of its own and stores NULL there. Anything
//! that expects text in that column falls over on the first database with a
//! composite primary key in it.
//!
//! Views and triggers have no b-tree, so their root page is 0. That is not
//! a page number that happens to be unusable, it is the absence of one, and
//! it is worth a different type rather than a magic value.
//!
//! Objects whose name begins with `sqlite_` belong to SQLite itself.
//! `sqlite_sequence` holds autoincrement counters, `sqlite_stat1` holds
//! planner statistics, and `sqlite_autoindex_*` are the indexes above.
//! Importing them would mean importing another engine's bookkeeping, so
//! they are flagged here and skipped by the importer.

use crate::error::{Result, SqliteError};
use crate::record::SqliteValue;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjectKind {
    Table,
    Index,
    View,
    Trigger,
}

impl ObjectKind {
    fn parse(text: &str) -> Result<ObjectKind> {
        Ok(match text {
            "table" => ObjectKind::Table,
            "index" => ObjectKind::Index,
            "view" => ObjectKind::View,
            "trigger" => ObjectKind::Trigger,
            other => {
                return Err(SqliteError::malformed(
                    1,
                    format!("the schema holds an object of unknown kind {other:?}"),
                ))
            }
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            ObjectKind::Table => "table",
            ObjectKind::Index => "index",
            ObjectKind::View => "view",
            ObjectKind::Trigger => "trigger",
        }
    }
}

/// One row of the schema table.
#[derive(Debug, Clone, PartialEq)]
pub struct SchemaObject {
    pub kind: ObjectKind,
    pub name: String,
    /// The table this belongs to. For a table it is its own name; for an
    /// index, view or trigger it is what the object is defined over.
    pub table_name: String,
    /// Where this object's b-tree starts. None for views and triggers,
    /// which have no b-tree at all.
    pub root_page: Option<u32>,
    /// The statement that created this object. None for indexes SQLite
    /// created for a primary key or a unique constraint.
    pub sql: Option<String>,
}

impl SchemaObject {
    /// Whether this belongs to SQLite's own bookkeeping rather than to the
    /// user's data.
    pub fn is_internal(&self) -> bool {
        let name = self.name.as_bytes();
        name.len() >= 7 && name[..7].eq_ignore_ascii_case(b"sqlite_")
    }
}

/// Everything the database says about itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Schema {
    objects: Vec<SchemaObject>,
}

impl Schema {
    /// Build the schema from the rows of the schema table.
    ///
    /// `page_count` is used to reject root pages that point outside the
    /// file, which is worth catching here rather than halfway through an
    /// import.
    pub(crate) fn from_rows(
        rows: impl IntoIterator<Item = Vec<SqliteValue>>,
        page_count: u32,
    ) -> Result<Schema> {
        let mut objects = Vec::new();
        for values in rows {
            if values.len() != 5 {
                return Err(SqliteError::malformed(
                    1,
                    format!(
                        "a schema row has {} columns, the schema table has five",
                        values.len()
                    ),
                ));
            }
            let kind = match &values[0] {
                SqliteValue::Text(t) => ObjectKind::parse(t)?,
                other => {
                    return Err(SqliteError::malformed(
                        1,
                        format!("a schema row's kind is {}, not text", other.type_name()),
                    ))
                }
            };
            let name = text_column(&values[1], "name")?;
            let table_name = text_column(&values[2], "tbl_name")?;

            let root_page = match &values[3] {
                // views and triggers have no b-tree; both spellings of that
                // appear in the wild
                SqliteValue::Null => None,
                SqliteValue::Integer(0) => None,
                SqliteValue::Integer(n) if *n > 0 && *n <= page_count as i64 => Some(*n as u32),
                other => {
                    return Err(SqliteError::malformed(
                        1,
                        format!(
                            "{name} has root page {other:?}, outside the file's 1..={page_count}"
                        ),
                    ))
                }
            };
            if root_page.is_none() && matches!(kind, ObjectKind::Table | ObjectKind::Index) {
                return Err(SqliteError::malformed(
                    1,
                    format!("the {} {name} has no root page", kind.as_str()),
                ));
            }

            let sql = match &values[4] {
                SqliteValue::Null => None,
                SqliteValue::Text(t) => Some(t.clone()),
                other => {
                    return Err(SqliteError::malformed(
                        1,
                        format!("{name} has a {} statement, not text", other.type_name()),
                    ))
                }
            };

            objects.push(SchemaObject {
                kind,
                name,
                table_name,
                root_page,
                sql,
            });
        }
        Ok(Schema { objects })
    }

    pub fn objects(&self) -> &[SchemaObject] {
        &self.objects
    }

    /// Every table, SQLite's own bookkeeping included. Callers that mean
    /// the user's data want `user_tables`.
    pub fn tables(&self) -> impl Iterator<Item = &SchemaObject> {
        self.objects.iter().filter(|o| o.kind == ObjectKind::Table)
    }

    /// Every table that holds the user's own data.
    pub fn user_tables(&self) -> impl Iterator<Item = &SchemaObject> {
        self.tables().filter(|o| !o.is_internal())
    }

    /// Every index in the schema, the ones SQLite made for itself included.
    /// Those have no statement of their own, which is how they are told
    /// apart.
    pub fn indexes(&self) -> impl Iterator<Item = &SchemaObject> {
        self.objects.iter().filter(|o| o.kind == ObjectKind::Index)
    }

    /// Look an object up by name. SQLite compares identifiers without
    /// regard to ASCII case, so `track` finds `Track`.
    pub fn object(&self, name: &str) -> Option<&SchemaObject> {
        self.objects
            .iter()
            .find(|o| o.name.eq_ignore_ascii_case(name))
    }

    /// The indexes defined over `table`, in the order the schema lists them.
    pub fn indexes_for(&self, table: &str) -> impl Iterator<Item = &SchemaObject> {
        let table = table.to_string();
        self.objects.iter().filter(move |o| {
            o.kind == ObjectKind::Index && o.table_name.eq_ignore_ascii_case(&table)
        })
    }
}

fn text_column(value: &SqliteValue, column: &str) -> Result<String> {
    match value {
        SqliteValue::Text(t) => Ok(t.clone()),
        other => Err(SqliteError::malformed(
            1,
            format!("a schema row's {column} is {}, not text", other.type_name()),
        )),
    }
}

impl SchemaObject {
    /// Parse this object's create statement into the shape of the table it
    /// makes.
    ///
    /// Fails for anything that is not a table, and for a table whose
    /// statement SQLite kept but this parser cannot read.
    pub fn table_def(&self) -> Result<crate::ddl::TableDef> {
        if self.kind != ObjectKind::Table {
            return Err(SqliteError::unsupported(format!(
                "{} is a {}, not a table",
                self.name,
                self.kind.as_str()
            )));
        }
        match &self.sql {
            Some(sql) => crate::ddl::parse_create_table(sql),
            None => Err(SqliteError::malformed(
                1,
                format!("the table {} has no create statement", self.name),
            )),
        }
    }
}
