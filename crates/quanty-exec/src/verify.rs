//! Index consistency checking.
//!
//! Rebuilds the expected entry set of every secondary index from the table
//! rows and compares it, byte for byte, against what is actually stored.
//! Anything missing, anything extra, anything with a non-empty value is a
//! failure. This is the tool the phase 2 acceptance runs after random
//! workloads, and later the backbone of a `quanty check` command.

use std::collections::{BTreeMap, BTreeSet};

use quanty_core::Storage;

use crate::catalog::{self, Table};
use crate::error::ExecError;
use crate::exec::{
    corpus_key, decode_row, index_entry_key, index_prefix, key_successor, row_pk, table_prefix,
    Session,
};
use crate::text;
use quanty_core::Value;

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

        // text indexes carry values, so they are rebuilt as key to value
        // and compared on both. A posting whose positions drifted is a
        // wrong answer, not a missing entry, and only comparing keys
        // would call that fine.
        for (pos, col) in table.columns.iter().enumerate() {
            let Some(text_id) = col.text_index_id else {
                continue;
            };
            let mut expected: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
            let (mut docs, mut total) = (0u64, 0u64);
            for item in tx.scan(Some(&tprefix), tend.as_deref())? {
                let (_, bytes) = item?;
                let values = decode_row(table, &bytes)?;
                let pk = row_pk(table, &values);
                let Value::Text(s) = &values[pos] else {
                    continue;
                };
                for (term, positions) in text::postings(s) {
                    expected.insert(
                        index_entry_key(text_id, &Value::Text(term), &pk),
                        text::encode_positions(&positions),
                    );
                }
                let length = text::length(s);
                expected.insert(
                    index_entry_key(text_id, &Value::Int(0), &pk),
                    length.to_le_bytes().to_vec(),
                );
                docs += 1;
                total += length as u64;
            }
            if docs > 0 {
                let mut counters = docs.to_le_bytes().to_vec();
                counters.extend_from_slice(&total.to_le_bytes());
                expected.insert(corpus_key(text_id), counters);
            }

            let mut actual: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
            let iprefix = index_prefix(text_id);
            let iend = key_successor(&iprefix);
            for item in tx.scan(Some(&iprefix), iend.as_deref())? {
                let (key, value) = item?;
                actual.insert(key, value);
            }

            let where_ = format!("text index {}.{}", table.name, col.name);
            for (key, want) in &expected {
                match actual.get(key) {
                    None => problems.push(format!("{where_}: missing entry {key:02x?}")),
                    Some(got) if got != want => problems.push(format!(
                        "{where_}: entry {key:02x?} holds {got:02x?}, expected {want:02x?}"
                    )),
                    Some(_) => {}
                }
            }
            for key in actual.keys() {
                if !expected.contains_key(key) {
                    problems.push(format!("{where_}: stray entry {key:02x?}"));
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
