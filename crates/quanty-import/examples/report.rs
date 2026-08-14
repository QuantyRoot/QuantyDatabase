//! Print what a SQLite file would become, without writing anything.
//!
//! ```text
//! cargo run -p quanty-import --example report -- some.sqlite [--strict]
//! ```
//!
//! This is the dry run, and until the command line tool exists it is also
//! the fastest way to point the importer at a real database and see what it
//! makes of it.

use quanty_import::{plan, Options};
use quanty_sqlite::{FileSource, Reader};

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(path) = args.next() else {
        eprintln!("usage: report <file.sqlite> [--strict]");
        std::process::exit(2);
    };
    let strict = args.any(|a| a == "--strict");

    let reader = match FileSource::open(&path).and_then(Reader::open) {
        Ok(reader) => reader,
        Err(e) => {
            eprintln!("{path}: {e}");
            std::process::exit(1);
        }
    };
    let plan = match plan(&reader, &Options { strict }) {
        Ok(plan) => plan,
        Err(e) => {
            eprintln!("{path}: {e}");
            std::process::exit(1);
        }
    };

    print!("{}", plan.report());
    println!(
        "\n{} tables, {} rows, {}",
        plan.tables.len(),
        plan.rows(),
        if plan.is_runnable() {
            "ready to import".to_string()
        } else {
            format!("{} problems", plan.problems.len())
        }
    );
    if !plan.is_runnable() {
        std::process::exit(1);
    }
}
