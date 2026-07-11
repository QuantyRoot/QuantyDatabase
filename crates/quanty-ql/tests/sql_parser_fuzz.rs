//! SQL parser fuzzing, the same three attack styles as the QQL fuzzer:
//!
//! 1. random ascii-heavy byte soup
//! 2. random token soup built from the SQL vocabulary
//! 3. byte-level mutations of a corpus of valid statements
//!
//! Two invariants, checked on every single input:
//!
//! - the parser never panics; garbage comes back as `Err`, always
//! - whenever it accepts an input, the AST it lowered to must survive the
//!   QQL canonical roundtrip: parse(pretty(ast)) == ast. that is the second
//!   front end promise in one line: everything SQL can produce is a
//!   well-formed AST of the native language
//!
//! Wall clock budget via QUANTY_FUZZ_SECS (default 20). The phase 4
//! acceptance run uses a much larger budget.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use quanty_ql::{parse, parse_sql, pretty};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

const VOCAB: &[&str] = &[
    "select",
    "SELECT",
    "insert",
    "into",
    "values",
    "update",
    "set",
    "delete",
    "from",
    "where",
    "create",
    "drop",
    "table",
    "index",
    "on",
    "primary",
    "key",
    "not",
    "null",
    "default",
    "unique",
    "and",
    "or",
    "is",
    "in",
    "like",
    "between",
    "order",
    "by",
    "asc",
    "desc",
    "limit",
    "offset",
    "group",
    "having",
    "join",
    "inner",
    "left",
    "cross",
    "union",
    "distinct",
    "as",
    "explain",
    "query",
    "plan",
    "show",
    "tables",
    "begin",
    "commit",
    "rollback",
    "foreign",
    "references",
    "constraint",
    "check",
    "collate",
    "autoincrement",
    "without",
    "rowid",
    "strict",
    "true",
    "FALSE",
    "null",
    "integer",
    "INTEGER",
    "real",
    "text",
    "blob",
    "boolean",
    "varchar",
    "nvarchar",
    "numeric",
    "datetime",
    "double",
    "precision",
    "character",
    "varying",
    "users",
    "t",
    "a",
    "b",
    "c",
    "score",
    "id",
    "Album",
    "(",
    ")",
    ",",
    ";",
    ".",
    "*",
    "=",
    "==",
    "!=",
    "<>",
    "<",
    "<=",
    ">",
    ">=",
    "+",
    "-",
    "/",
    "%",
    "||",
    "0",
    "1",
    "42",
    "3.5",
    ".5",
    "5.",
    "1e9",
    "0x1f",
    "-7",
    "'x'",
    "'a b'",
    "'it''s'",
    "x'c0ffee'",
    "X'00'",
    "\"order\"",
    "[limit]",
    "`set`",
    "\"a b\"",
    "9223372036854775807",
    "--",
    "/*",
    "*/",
    "\n",
];

const CORPUS: &[&str] = &[
    "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, score INT NOT NULL DEFAULT 0, bio TEXT)",
    "CREATE TABLE [Invoice] ([InvoiceId] INTEGER NOT NULL, [Total] NUMERIC(10,2), CONSTRAINT [PK] PRIMARY KEY ([InvoiceId]), FOREIGN KEY ([InvoiceId]) REFERENCES [X] ([Y]) ON DELETE NO ACTION)",
    "CREATE TABLE pt (a INTEGER NOT NULL, b INTEGER NOT NULL, PRIMARY KEY (a, b)) WITHOUT ROWID, STRICT",
    "INSERT INTO users (id, name) VALUES (1, 'elchi'), (2, 'it''s'), (3, NULL)",
    "INSERT INTO t (a, b) VALUES (-9223372036854775807, x'00ff')",
    "SELECT * FROM users WHERE score > 10 ORDER BY score DESC LIMIT 5;",
    "SELECT name, score FROM users WHERE name <> 'elchi' AND (score >= 3 OR score < -2)",
    "SELECT users.name, cities.name FROM users JOIN cities ON users.city = cities.id",
    "SELECT * FROM users LEFT OUTER JOIN cities ON users.city = cities.id WHERE cities.rank > 1",
    "SELECT a.x, c.w FROM a INNER JOIN b ON a.x = b.y JOIN c ON b.z = c.w ORDER BY c.w DESC LIMIT 3",
    "SeLeCt \"order\", [limit], `set` FROM q WHERE a IS NOT NULL",
    "select * from t where a is b and not c = 0x1f",
    "SELECT a FROM t WHERE b || 'y' = 'xy' -- tail",
    "/* block */ SELECT a FROM t WHERE b + .5 < 5.",
    "UPDATE users SET score = score + 5, name = 'neu' WHERE id = 1",
    "DELETE FROM users WHERE score % 2 = 0",
    "CREATE INDEX idx_users_name ON users (name)",
    "DROP TABLE users",
    "show tables",
    "EXPLAIN QUERY PLAN SELECT * FROM users WHERE id = 7",
    "EXPLAIN EXPLAIN DELETE FROM t",
];

fn check(input: &str) {
    // invariant 1: never panic (a panic aborts the test run right here)
    let Ok(ast) = parse_sql(input) else { return };
    // invariant 2: the lowered AST survives the QQL canonical roundtrip
    let canon = pretty(&ast);
    match parse(&canon) {
        Ok(again) if again == ast => {}
        Ok(_) => panic!("roundtrip changed the AST\ninput: {input:?}\ncanonical: {canon:?}"),
        Err(e) => {
            panic!("canonical form does not parse: {e}\ninput: {input:?}\ncanonical: {canon:?}")
        }
    }
}

#[test]
fn fuzz_the_sql_parser() {
    let budget = Duration::from_secs(
        std::env::var("QUANTY_FUZZ_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(20),
    );
    let seed = std::env::var("QUANTY_FUZZ_SEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64
                | 1
        });
    let mut rng = Rng(seed);
    let started = Instant::now();
    let mut cases: u64 = 0;

    // the corpus itself must always parse and roundtrip
    for input in CORPUS {
        assert!(parse_sql(input).is_ok(), "corpus must parse: {input}");
        check(input);
    }

    while started.elapsed() < budget {
        for _ in 0..1000 {
            cases += 1;
            match rng.below(3) {
                // ascii-heavy byte soup
                0 => {
                    let len = rng.below(120) as usize;
                    let mut s = String::with_capacity(len);
                    for _ in 0..len {
                        let b = (rng.below(96) + 32) as u8;
                        s.push(b as char);
                    }
                    check(&s);
                }
                // token soup from the vocabulary
                1 => {
                    let len = rng.below(30) as usize;
                    let mut s = String::new();
                    for _ in 0..len {
                        s.push_str(VOCAB[rng.below(VOCAB.len() as u64) as usize]);
                        s.push(' ');
                    }
                    check(&s);
                }
                // mutate a corpus statement
                _ => {
                    let mut s = CORPUS[rng.below(CORPUS.len() as u64) as usize]
                        .as_bytes()
                        .to_vec();
                    for _ in 0..=rng.below(4) {
                        if s.is_empty() {
                            break;
                        }
                        let pos = rng.below(s.len() as u64) as usize;
                        match rng.below(3) {
                            0 => s[pos] = (rng.below(96) + 32) as u8,
                            1 => {
                                s.remove(pos);
                            }
                            _ => s.insert(pos, (rng.below(96) + 32) as u8),
                        }
                    }
                    if let Ok(s) = String::from_utf8(s) {
                        check(&s);
                    }
                }
            }
        }
    }

    println!(
        "sql parser fuzz: {cases} cases in {:.1}s (seed {seed})",
        started.elapsed().as_secs_f64()
    );
}
