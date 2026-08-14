//! Which stored value belongs to which declared column, and what a column
//! actually holds.
//!
//! These two things live together because the second cannot be done without
//! the first. Counting what is in a column means knowing which value in the
//! record is that column, and the record does not line up with the column
//! list. Three rules pull them apart, and each was checked against a file
//! written by SQLite rather than recalled:
//!
//! A virtual generated column occupies no slot in the record, while a
//! stored one does. In `(a, v virtual, s stored)` the record holds two
//! values, `a` and `s`. Zipping the declared columns against the stored
//! values puts `s`'s value into `v`, and then every column after it, and
//! nothing about the result looks wrong.
//!
//! A column that is an alias for the rowid is stored as NULL, and its value
//! is the cell's rowid. Reading it as stored gives a column of nothing.
//!
//! A record may end early. `alter table add column` does not rewrite the
//! existing rows, so a table with four columns can hold rows with two
//! values in them, and the rest take the default from the declaration. A
//! reader that treats a short record as corruption refuses ordinary files.

use crate::affinity::{Affinity, StorageClass};
use crate::ddl::TableDef;
use crate::error::Result;
use crate::record::SqliteValue;
use crate::schema::SchemaObject;
use crate::source::Source;
use crate::tree::Row;
use crate::Reader;

/// Where a declared column's value comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// Position in the record.
    Stored(usize),
    /// Position in the record, which holds NULL; the value is the rowid.
    RowidAlias(usize),
    /// Computed on demand by SQLite and absent from the file.
    Virtual,
}

/// A column's value in one row, once the record has been lined up with the
/// declaration.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell<'a> {
    /// The value as stored.
    Value(&'a SqliteValue),
    /// The row's rowid, standing in for a column that aliases it.
    Rowid(i64),
    /// The record ended before this column, so its default applies.
    Missing,
    /// A virtual generated column: the file holds nothing for it.
    Virtual,
}

/// Lines a table's records up with its declared columns.
///
/// Built once per table and used for every row, because the layout is a
/// property of the declaration rather than of any particular row.
pub struct RowLayout {
    slots: Vec<Slot>,
}

impl RowLayout {
    pub fn new(def: &TableDef) -> RowLayout {
        if def.without_rowid {
            return RowLayout::keyed(def);
        }
        let alias = def.rowid_alias().map(|c| c.name.clone());
        let mut slots = Vec::with_capacity(def.columns.len());
        let mut position = 0usize;
        for column in &def.columns {
            if column.generated.is_virtual() {
                slots.push(Slot::Virtual);
                continue;
            }
            let is_alias = alias
                .as_deref()
                .is_some_and(|a| a.eq_ignore_ascii_case(&column.name));
            slots.push(if is_alias {
                Slot::RowidAlias(position)
            } else {
                Slot::Stored(position)
            });
            position += 1;
        }
        RowLayout { slots }
    }

    /// The layout of a without rowid table, where the record is permuted.
    ///
    /// Such a table is stored as an index b-tree keyed by its primary key,
    /// and the entry holds the key columns first, in key order, then the
    /// remaining columns in declared order. Nothing in the bytes says which
    /// arrangement is in force, so this is read off the declaration and is
    /// the whole reason the create statement has to be parsed before such a
    /// table can be read at all.
    ///
    /// A key column named in the primary key but missing from the column
    /// list cannot happen in a database sqlite accepted, and if it does the
    /// column simply takes the next position, which keeps the mapping
    /// total rather than panicking on a file we did not write.
    fn keyed(def: &TableDef) -> RowLayout {
        let mut order: Vec<usize> = Vec::with_capacity(def.columns.len());
        for key in &def.primary_key {
            if let Some(index) = def.column_index(&key.name) {
                if !order.contains(&index) {
                    order.push(index);
                }
            }
        }
        for index in 0..def.columns.len() {
            if !order.contains(&index) {
                order.push(index);
            }
        }

        // walk the record order, handing out positions, then put the slots
        // back into declared order for the caller
        let mut slots = vec![Slot::Virtual; def.columns.len()];
        let mut position = 0usize;
        for index in order {
            if def.columns[index].generated.is_virtual() {
                continue;
            }
            slots[index] = Slot::Stored(position);
            position += 1;
        }
        RowLayout { slots }
    }

    /// How many columns the declaration has, virtual ones included.
    pub fn declared_columns(&self) -> usize {
        self.slots.len()
    }

    /// How many values a complete record of this table holds.
    pub fn stored_columns(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| !matches!(s, Slot::Virtual))
            .count()
    }

    /// The value of declared column `index` in `row`.
    ///
    /// Takes the unified row, so the same layout serves both kinds of
    /// table: one call site, one set of rules, and no way for the two paths
    /// to drift apart.
    pub fn cell<'a>(&self, row: &'a Row, index: usize) -> Cell<'a> {
        match self.slots.get(index) {
            None => Cell::Missing,
            Some(Slot::Virtual) => Cell::Virtual,
            Some(Slot::RowidAlias(at)) => match row.rowid {
                // the slot exists and holds NULL; the value is the rowid.
                // if a file ever stored something else there, the rowid is
                // still the authority, because that is what sqlite indexes
                // and what other tables reference.
                Some(rowid) => Cell::Rowid(rowid),
                // a row without a rowid cannot have a column aliasing one,
                // so this is a file disagreeing with itself. report what is
                // actually stored rather than inventing a number.
                None => match row.values.get(*at) {
                    Some(value) => Cell::Value(value),
                    None => Cell::Missing,
                },
            },
            Some(Slot::Stored(at)) => match row.values.get(*at) {
                Some(value) => Cell::Value(value),
                None => Cell::Missing,
            },
        }
    }
}

/// What one column turned out to hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSurvey {
    pub name: String,
    pub declared_type: Option<String>,
    pub affinity: Affinity,
    /// Rows per logical storage class, affinity already applied.
    pub nulls: u64,
    pub integers: u64,
    pub reals: u64,
    pub texts: u64,
    pub blobs: u64,
    /// Rows whose record ended before this column, which take its default.
    pub missing: u64,
    /// The largest integer seen, by magnitude. A column that mixes integers
    /// and reals can only become a float if every integer in it survives
    /// the trip, and past 2^53 they stop doing that.
    pub largest_integer: u64,
    pub is_rowid_alias: bool,
    pub is_virtual: bool,
}

impl ColumnSurvey {
    /// The classes actually present, ignoring nulls, which are a question
    /// about nullability rather than about type.
    pub fn classes(&self) -> Vec<StorageClass> {
        let mut present = Vec::new();
        for (count, class) in [
            (self.integers, StorageClass::Integer),
            (self.reals, StorageClass::Real),
            (self.texts, StorageClass::Text),
            (self.blobs, StorageClass::Blob),
        ] {
            if count > 0 {
                present.push(class);
            }
        }
        present
    }

    pub fn has_nulls(&self) -> bool {
        self.nulls > 0 || self.missing > 0
    }
}

/// What a whole table turned out to hold.
#[derive(Debug, Clone, PartialEq)]
pub struct TableSurvey {
    pub name: String,
    pub rows: u64,
    pub columns: Vec<ColumnSurvey>,
    pub primary_key: Vec<String>,
    pub without_rowid: bool,
    /// The largest rowid in the table, which matters when the rowid has to
    /// become a key column of its own.
    pub largest_rowid: i64,
}

impl<S: Source> Reader<S> {
    /// Read every row of a table and report what each column holds.
    ///
    /// This is the first of the import's two passes: it writes nothing and
    /// its whole purpose is to let the second pass know the shape of what
    /// it is about to write, and to let a caller see every problem at once
    /// rather than the first one after ten minutes of work.
    pub fn survey_table(&self, object: &SchemaObject) -> Result<TableSurvey> {
        let def = object.table_def()?;
        let layout = RowLayout::new(&def);

        let mut columns: Vec<ColumnSurvey> = def
            .columns
            .iter()
            .map(|column| ColumnSurvey {
                name: column.name.clone(),
                declared_type: column.declared_type.clone(),
                affinity: Affinity::of(column.declared_type.as_deref()),
                nulls: 0,
                integers: 0,
                reals: 0,
                texts: 0,
                blobs: 0,
                missing: 0,
                largest_integer: 0,
                is_rowid_alias: def
                    .rowid_alias()
                    .is_some_and(|c| c.name.eq_ignore_ascii_case(&column.name)),
                is_virtual: column.generated.is_virtual(),
            })
            .collect();

        let mut rows = 0u64;
        let mut largest_rowid = 0i64;

        let root = object.root_page.ok_or_else(|| {
            crate::error::SqliteError::malformed(
                1,
                format!("the table {} has no root page", object.name),
            )
        })?;

        // rows() picks the walk from the root page, so a without rowid
        // table is surveyed the same way as any other
        for row in self.rows(root)? {
            let row = row?;
            rows += 1;
            largest_rowid = largest_rowid.max(row.rowid.unwrap_or(0));

            for (index, survey) in columns.iter_mut().enumerate() {
                match layout.cell(&row, index) {
                    Cell::Virtual => {}
                    Cell::Missing => survey.missing += 1,
                    Cell::Rowid(rowid) => {
                        survey.integers += 1;
                        survey.largest_integer = survey.largest_integer.max(rowid.unsigned_abs());
                    }
                    Cell::Value(value) => {
                        if let SqliteValue::Integer(n) = value {
                            survey.largest_integer = survey.largest_integer.max(n.unsigned_abs());
                        }
                        match survey.affinity.logical_class(value) {
                            StorageClass::Null => survey.nulls += 1,
                            StorageClass::Integer => survey.integers += 1,
                            StorageClass::Real => survey.reals += 1,
                            StorageClass::Text => survey.texts += 1,
                            StorageClass::Blob => survey.blobs += 1,
                        }
                    }
                }
            }
        }

        Ok(TableSurvey {
            name: def.name.clone(),
            rows,
            columns,
            primary_key: def.primary_key.iter().map(|k| k.name.clone()).collect(),
            without_rowid: def.without_rowid,
            largest_rowid,
        })
    }
}
