//! Index consistency checking.
//!
//! Rebuilds the expected entry set of every secondary index from the table
//! rows and compares it, byte for byte, against what is actually stored.
//! Anything missing, anything extra, anything with a non-empty value is a
//! failure. This is the tool the phase 2 acceptance runs after random
//! workloads, and later the backbone of a `quanty check` command.

use std::collections::BTreeSet;

use quanty_core::Storage;

use crate::catalog::{self, Table};
use crate::error::ExecError;
use crate::exec::{
    decode_row, index_entry_key, index_prefix, key_successor, row_pk, table_prefix, Session,
};

/// Check every index of every table. Returns a human-readable report of
/// all problems found, or `Ok(())` when everything lines up.
pub fn verify_indexes<S: Storage>(session: &Session<S>) -> Result<(), ExecError> {
    let db = session.db();
    let tx = db.begin(); // consistent view of catalog + data, never committed
    let mut problems = Vec::new();

    // walk the catalog
    let prefix = catalog::tables_prefix();
    let end = key_successor(&prefix);
    let mut tables = Vec::new();
    for item in tx.catalog_scan(Some(&prefix), end.as_deref())? {
        let (_, bytes) = item?;
        tables.push(Table::deserialize(&bytes)?);
    }

    for table in &tables {
        // expected entries, rebuilt from the rows
        let mut expected: Vec<BTreeSet<Vec<u8>>> =
            table.columns.iter().map(|_| BTreeSet::new()).collect();
        let tprefix = table_prefix(table.id);
        let tend = key_successor(&tprefix);
        for item in tx.scan(Some(&tprefix), tend.as_deref())? {
            let (_, bytes) = item?;
            let values = decode_row(table, &bytes)?;
            let pk = row_pk(table, &values);
            for (pos, col) in table.columns.iter().enumerate() {
                if let Some(index_id) = col.index_id {
                    expected[pos].insert(index_entry_key(index_id, &values[pos], &pk));
                }
            }
        }

        // actual entries, straight from the tree
        for (pos, col) in table.columns.iter().enumerate() {
            let Some(index_id) = col.index_id else {
                continue;
            };
            let mut actual = BTreeSet::new();
            let iprefix = index_prefix(index_id);
            let iend = key_successor(&iprefix);
            for item in tx.scan(Some(&iprefix), iend.as_deref())? {
                let (key, value) = item?;
                if !value.is_empty() {
                    problems.push(format!(
                        "index {}.{}: entry with a non-empty value",
                        table.name, col.name
                    ));
                }
                actual.insert(key);
            }
            for missing in expected[pos].difference(&actual) {
                problems.push(format!(
                    "index {}.{}: missing entry for {missing:02x?}",
                    table.name, col.name
                ));
            }
            for stray in actual.difference(&expected[pos]) {
                problems.push(format!(
                    "index {}.{}: stray entry {stray:02x?} with no matching row",
                    table.name, col.name
                ));
            }
        }
    }

    if problems.is_empty() {
        Ok(())
    } else {
        Err(ExecError::exec(format!(
            "index verification found {} problems:\n{}",
            problems.len(),
            problems.join("\n")
        )))
    }
}
