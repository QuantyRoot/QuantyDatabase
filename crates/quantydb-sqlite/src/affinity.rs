//! Type affinity: what a column's declared type means.
//!
//! A declared type in SQLite does not constrain what a column holds. It
//! sets an affinity, which is a preference applied when a value goes in:
//! text that looks like a number goes into an integer affinity column as a
//! number, text that does not stays text. So the declaration is neither a
//! guarantee nor decoration, and both readings get an import wrong.
//!
//! The rules are five, applied in order, against the declared type
//! uppercased. The order is the whole trick, and it produces results that
//! look like bugs and are not:
//!
//! 1. contains `INT`               -> INTEGER
//! 2. contains `CHAR`, `CLOB`, `TEXT` -> TEXT
//! 3. contains `BLOB`, or no type at all -> BLOB
//! 4. contains `REAL`, `FLOA`, `DOUB` -> REAL
//! 5. otherwise                    -> NUMERIC
//!
//! `FLOATING POINT` has integer affinity, because `POINT` contains `INT`
//! and rule 1 wins before rule 4 is ever reached. `CHARINT` is an integer
//! for the same reason. `STRING` is numeric, because it contains none of
//! the words. These are not our jokes to fix; every reader has to agree
//! with them or it reads different data than SQLite does.
//!
//! One consequence matters more than all the others put together. In a
//! column with real affinity, SQLite stores a whole numbered value as an
//! integer to save space, and turns it back into a float on the way out
//! using the affinity. So `1.0` and `2.5` in one real column are physically
//! an integer and a float. Judging such a column by its storage classes
//! alone makes almost every real column in the world look mixed, which is
//! why the declared type is not optional information here.

use crate::record::SqliteValue;

/// SQLite's five storage classes: what a value physically is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum StorageClass {
    Null,
    Integer,
    Real,
    Text,
    Blob,
}

impl StorageClass {
    pub fn of(value: &SqliteValue) -> StorageClass {
        match value {
            SqliteValue::Null => StorageClass::Null,
            SqliteValue::Integer(_) => StorageClass::Integer,
            SqliteValue::Real(_) => StorageClass::Real,
            SqliteValue::Text(_) => StorageClass::Text,
            SqliteValue::Blob(_) => StorageClass::Blob,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            StorageClass::Null => "null",
            StorageClass::Integer => "integer",
            StorageClass::Real => "real",
            StorageClass::Text => "text",
            StorageClass::Blob => "blob",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Affinity {
    Integer,
    Text,
    Blob,
    Real,
    Numeric,
}

impl Affinity {
    /// Apply the five rules to a declared type. A column declared without a
    /// type has blob affinity, which is the one that converts nothing.
    pub fn of(declared_type: Option<&str>) -> Affinity {
        let Some(declared) = declared_type else {
            return Affinity::Blob;
        };
        let upper = declared.to_ascii_uppercase();
        if upper.contains("INT") {
            Affinity::Integer
        } else if upper.contains("CHAR") || upper.contains("CLOB") || upper.contains("TEXT") {
            Affinity::Text
        } else if upper.contains("BLOB") || upper.trim().is_empty() {
            Affinity::Blob
        } else if upper.contains("REAL") || upper.contains("FLOA") || upper.contains("DOUB") {
            Affinity::Real
        } else {
            Affinity::Numeric
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Affinity::Integer => "integer",
            Affinity::Text => "text",
            Affinity::Blob => "blob",
            Affinity::Real => "real",
            Affinity::Numeric => "numeric",
        }
    }

    /// What a stored value means once this column's affinity is applied.
    ///
    /// The only case where this differs from the physical class is an
    /// integer in a real affinity column, which is a float that SQLite
    /// wrote out compactly, and which SQLite itself reports as a real.
    pub fn logical_class(self, value: &SqliteValue) -> StorageClass {
        let stored = StorageClass::of(value);
        match (self, stored) {
            (Affinity::Real, StorageClass::Integer) => StorageClass::Real,
            _ => stored,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_five_rules_run_in_order() {
        let cases = [
            ("INTEGER", Affinity::Integer),
            ("INT", Affinity::Integer),
            ("BIGINT", Affinity::Integer),
            ("UNSIGNED BIG INT", Affinity::Integer),
            ("INT2", Affinity::Integer),
            ("VARCHAR(255)", Affinity::Text),
            ("NVARCHAR(160)", Affinity::Text),
            ("TEXT", Affinity::Text),
            ("CLOB", Affinity::Text),
            ("NCHAR(55)", Affinity::Text),
            ("BLOB", Affinity::Blob),
            ("REAL", Affinity::Real),
            ("DOUBLE PRECISION", Affinity::Real),
            ("FLOAT", Affinity::Real),
            ("NUMERIC", Affinity::Numeric),
            ("DECIMAL(10,5)", Affinity::Numeric),
            ("BOOLEAN", Affinity::Numeric),
            ("DATE", Affinity::Numeric),
            ("DATETIME", Affinity::Numeric),
            ("STRING", Affinity::Numeric),
        ];
        for (declared, expected) in cases {
            assert_eq!(Affinity::of(Some(declared)), expected, "{declared}");
            // the rules are case insensitive
            assert_eq!(
                Affinity::of(Some(&declared.to_lowercase())),
                expected,
                "{declared} in lower case"
            );
        }
    }

    #[test]
    fn the_rules_that_look_like_bugs() {
        // POINT contains INT, so rule 1 fires before rule 4 is reached
        assert_eq!(Affinity::of(Some("FLOATING POINT")), Affinity::Integer);
        assert_eq!(Affinity::of(Some("POINT")), Affinity::Integer);
        // CHARINT contains both CHAR and INT, and INT is checked first
        assert_eq!(Affinity::of(Some("CHARINT")), Affinity::Integer);
        assert_eq!(Affinity::of(Some("BLOBINT")), Affinity::Integer);
        // no type at all is blob affinity, which converts nothing
        assert_eq!(Affinity::of(None), Affinity::Blob);
        assert_eq!(Affinity::of(Some("")), Affinity::Blob);
    }

    #[test]
    fn an_integer_in_a_real_column_is_a_float() {
        let integer = SqliteValue::Integer(1);
        assert_eq!(
            Affinity::Real.logical_class(&integer),
            StorageClass::Real,
            "sqlite writes whole floats as integers in real columns"
        );
        // every other affinity takes an integer at face value
        for affinity in [
            Affinity::Integer,
            Affinity::Numeric,
            Affinity::Text,
            Affinity::Blob,
        ] {
            assert_eq!(affinity.logical_class(&integer), StorageClass::Integer);
        }
    }

    #[test]
    fn affinity_never_changes_the_other_classes() {
        let values = [
            SqliteValue::Null,
            SqliteValue::Real(2.5),
            SqliteValue::Text("x".into()),
            SqliteValue::Blob(vec![1]),
        ];
        for affinity in [
            Affinity::Integer,
            Affinity::Text,
            Affinity::Blob,
            Affinity::Real,
            Affinity::Numeric,
        ] {
            for value in &values {
                assert_eq!(
                    affinity.logical_class(value),
                    StorageClass::of(value),
                    "{affinity:?} changed {value:?}"
                );
            }
        }
    }
}
