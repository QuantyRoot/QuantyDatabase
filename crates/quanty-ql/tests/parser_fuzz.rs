//! Parser fuzzing.
//!
//! Three attack styles, all seeded and reproducible:
//!
//! 1. random ascii-heavy byte soup
//! 2. random token soup built from the QQL vocabulary
//! 3. byte-level mutations of a corpus of valid statements
//!
//! Two invariants, checked on every single input:
//!
//! - the parser never panics; garbage comes back as `Err`, always
//! - whenever it accepts an input, the canonical pretty print must parse
//!   again to the exact same AST (parse . pretty . parse == parse)
//!
//! Wall clock budget via QUANTY_FUZZ_SECS (default 20). The phase 2
//! acceptance run uses a much larger budget.

use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use quanty_ql::{parse, pretty};

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
    "table",
    "get",
    "put",
    "set",
    "del",
    "drop",
    "index",
    "show",
    "tables",
    "explain",
    "where",
    "join",
    "left",
    "on",
    "order",
    "by",
    "asc",
    "desc",
    "limit",
    "as",
    "of",
    "time",
    "branch",
    "branches",
    "switch",
    "merge",
    "log",
    "gc",
    "keep",
    "at",
    "and",
    "or",
    "not",
    "true",
    "false",
    "null",
    "int",
    "float",
    "text",
    "bytes",
    "bool",
    "key",
    "null",
    "users",
    "t",
    "a",
    "b",
    "c",
    "score",
    "id",
    "{",
    "}",
    "(",
    ")",
    ",",
    ":",
    ".",
    "@",
    "=",
    "!=",
    "<",
    "<=",
    ">",
    ">=",
    "+",
    "-",
    "*",
    "/",
    "%",
    "+=",
    "-=",
    "*=",
    "/=",
    "0",
    "1",
    "42",
    "3.5",
    "1e9",
    "-7",
    "\"x\"",
    "\"a b\"",
    "x\"c0ffee\"",
    "\"\\n\"",
    "\"\u{0}\"",
    "9223372036854775807",
    "#",
    "\n",
];

const CORPUS: &[&str] = &[
    "table users { id: int @key, name: text @index, score: int = 0 }",
    "table t { a: int @key, b: float @null = 1.5, c: bytes @index, d: bool = true }",
    "get users where score > 100 order by score desc limit 10",
    "get users { name, score } where name != \"elchi\" and (score >= 3 or score < -2)",
    "set users where id = 1 { score += 5, name = \"neu\" }",
    "set t { a = a * 2 + 1, b /= 4 }",
    "del users where not (score % 2 = 0)",
    "put users { id: 1, name: \"a\" }, { id: 2, name: \"b\" }",
    "put t { a: -9223372036854775808, b: 1e308, c: x\"00ff\", d: null }",
    "index users.name",
    "drop table users",
    "show tables",
    "get users as of 42 where score > 1 order by score limit 3",
    "get users { name } as of time 1700000000000",
    "get users join cities on users.city = cities.id",
    "get users left join cities on users.city = cities.id where cities.rank > 1",
    "get users join cities on users.city = cities.id { users.name, cities.name } order by cities.name desc limit 5",
    "get a join b on a.x = b.y join c on b.z = c.w { a.x, c.w }",
    "get users join cities on users.city = cities.id and cities.rank = 1 as of 4",
    "branch experiment",
    "branch fix at 17",
    "switch experiment",
    "merge experiment",
    "drop branch experiment",
    "show branches",
    "log",
    "gc keep 10",
    "explain get users where id = 7",
    "explain explain del t",
];

fn check(input: &str) {
    // invariant 1: never panic (a panic aborts the test run right here)
    let Ok(ast) = parse(input) else { return };
    // invariant 2: canonical form roundtrips to the same AST
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
fn fuzz_the_parser() {
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

    // the corpus itself must always be green
    for input in CORPUS {
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
                        let b = match rng.below(10) {
                            0..=6 => (rng.below(95) + 32) as u8, // printable
                            7 => b'\n',
                            8 => b'"',
                            _ => (rng.below(256)) as u8,
                        };
                        s.push(char::from(b.min(0x7F)));
                    }
                    check(&s);
                }
                // token soup
                1 => {
                    let len = rng.below(40) as usize + 1;
                    let mut s = String::new();
                    for _ in 0..len {
                        s.push_str(VOCAB[rng.below(VOCAB.len() as u64) as usize]);
                        if rng.below(4) != 0 {
                            s.push(' ');
                        }
                    }
                    check(&s);
                }
                // corpus mutation
                _ => {
                    let base = CORPUS[rng.below(CORPUS.len() as u64) as usize];
                    let mut bytes = base.as_bytes().to_vec();
                    for _ in 0..=rng.below(4) {
                        if bytes.is_empty() {
                            break;
                        }
                        let pos = rng.below(bytes.len() as u64) as usize;
                        match rng.below(3) {
                            0 => bytes[pos] = (rng.below(126 - 32) + 32) as u8,
                            1 => {
                                bytes.remove(pos);
                            }
                            _ => bytes.insert(pos, (rng.below(126 - 32) + 32) as u8),
                        }
                    }
                    // mutations can break utf-8 boundaries of multi-byte
                    // chars; lossy conversion mirrors what any caller with
                    // a &str could actually hand the parser
                    check(&String::from_utf8_lossy(&bytes));
                }
            }
        }
    }

    println!(
        "parser fuzz: {cases} cases in {:.1?} (seed {seed})",
        started.elapsed()
    );
}
