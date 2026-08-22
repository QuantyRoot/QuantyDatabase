<div align="center">

```
 ██████╗ ██╗   ██╗ █████╗ ███╗   ██╗████████╗██╗   ██╗
██╔═══██╗██║   ██║██╔══██╗████╗  ██║╚══██╔══╝╚██╗ ██╔╝
██║   ██║██║   ██║███████║██╔██╗ ██║   ██║    ╚████╔╝
██║▄▄ ██║██║   ██║██╔══██║██║╚██╗██║   ██║     ╚██╔╝
╚██████╔╝╚██████╔╝██║  ██║██║ ╚████║   ██║      ██║
 ╚══▀▀═╝  ╚═════╝ ╚═╝  ╚═╝╚═╝  ╚═══╝   ╚═╝      ╚═╝
```

**One database that reshapes itself into whatever you need :3**

[![CI](https://github.com/QuantyRoot/QuantyDatabase/actions/workflows/ci.yml/badge.svg)](https://github.com/QuantyRoot/QuantyDatabase/actions/workflows/ci.yml)
![Status](https://img.shields.io/badge/status-pre--alpha-orange)
[![Rust](https://img.shields.io/badge/Rust-1.75+-B7410E?logo=rust)](https://www.rust-lang.org)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Made by Elchi](https://img.shields.io/badge/made%20by-Elchi-8A2BE2)](https://github.com/Elchi-dev)

</div>

---

## Overview

Every project follows the same arc. You start with SQLite because it's
simple. You outgrow it and migrate to Postgres. Then you bolt on Redis for
caching, Elasticsearch for search, and S3 for assets. Now you run five
systems with five configs, five backup strategies and five ways to lose
data at 3am.

Quanty is my attempt to break that arc: a single storage engine that adapts
to the job instead of making you migrate between databases.

- Need an embedded, zero-config, single-file db? That's the default.
- Need a server that handles thousands of connections? Same file, same
  engine, run `quanty serve`.
- Need to store assets, search full text, keep history? Also the same
  engine. No sidecar systems.

> Quanty is in early development. The storage core, both query front ends
> and the network server work today; the rest of this README describes
> where the project is going. See [ROADMAP.md](docs/ROADMAP.md) for what is
> actually finished.

**Written by one person, funded by nobody, and it depends on nothing.** The
lock file holds this workspace and not one package besides. No company
behind it, no sponsors, no roadmap written by someone else. If that sounds
like a constraint, it is: the checksum, the locks, the epoll layer, the
SHA-256 and the wire protocol are all written out here rather than pulled
in, and each of those choices is argued and costed in
[DECISIONS.md](docs/DECISIONS.md).

---

## The trick

Most databases overwrite data in place and treat history as a problem.
Quanty is copy-on-write all the way down: every commit is immutable, and
the database is a chain of commits, like a git repo for your data.

That one decision makes the headline features structural instead of
bolted on:

```
                     +------------------ quanty ------------------+
  your app  <-----> |  embedded API  |  server mode  |  sqlite sql |
                     +--------------------+------------------------+
                     |     query layer (QQL + SQL front ends)      |
                     +--------------------+------------------------+
                     |  copy-on-write core: commits, snapshots,    |
                     |  branches, MVCC, blobs, indexes             |
                     +---------------------------------------------+
```

- **Time travel.** `get users as of 42` or `as of time <unix_ms>`. Query any
  past state of your database. The "oh no, I just broke prod" command.
- **Branching.** Fork your entire database in milliseconds, test a risky
  migration on the branch, merge or throw it away. Copy-on-write means a
  branch costs nothing until it diverges.
- **Instant snapshots.** Fork the db per test, run, discard. No fixtures,
  no cleanup scripts.
- **Lock-free readers.** Readers pin a commit and never block writers.
  Thousands of concurrent readers are the easy case, not the hard one.

---

## Quick look

QQL and the Rust block are built. The CLI block after them is the target
API: branching from the shell goes through `quanty run` today, so consider
that last part a preview and not documentation.

Schema and queries in QQL, Quanty's native language:

```
table users {
  id:    int  @key
  name:  text @index
  score: int = 0
}

get users where score > 100 order by score desc limit 10
set users where id = 1 { score += 5 }
get users as of 42 where name = "elchi"
```

Embedded in Rust. This part is built, and ADR-030 fixes the surface:

```rust
use quanty::Database;

let mut db = Database::open("app.qdb")?;

// a transaction is a borrow, so it cannot outlive the database and
// cannot be left open: the closure returning decides commit or rollback
db.transaction(|tx| tx.execute(r#"put users { id: 1, name: "elchi" }"#))?;

// the SQL front end understands your existing sqlite queries too
let rows = db.query_sql("SELECT name FROM users WHERE id = 1")?;
for row in &rows {
    println!("{}", row[0]);
}
```

Branching from the CLI:

```sh
quanty branch app.qdb risky-migration
quanty exec app.qdb --branch risky-migration "..."
quanty merge app.qdb risky-migration     # or just delete the branch
```

---

## Planned features

### Core
- Single-file, embedded, zero config, ACID with real crash recovery
- Copy-on-write storage with snapshots, branches and `as of` queries
- Configurable history retention (keep everything, keep 7 days, keep heads)
- Order-preserving typed keys, secondary indexes, `explain` from day one

### Server
- `quanty serve` turns any db file into a network database
- An event loop written on epoll directly, no async runtime
- Small versioned binary protocol, token auth, `quanty connect` to speak it

### SQLite compatibility
- Direct `.sqlite` import, no SQLite dependency, we read the format ourselves
- A pragmatic SQLite-flavored SQL front end alongside QQL, so typical app
  queries run unchanged

### Assets and search
- Content-addressed blob store with chunking and dedup for large files
- Streaming reads/writes, so a 1 GiB asset doesn't need 1 GiB of RAM
- Transactional full-text search, no external search cluster
- Cold data tiering to S3-compatible buckets, planned after the core

### The adaptive part
- Built-in stats: Quanty watches your workload and tells you what it sees
- Index suggestions, and opt-in automatic indexes
- Workload-aware defaults instead of a wall of tuning knobs

---

## Non-goals

Keeping this list is half the battle:

- Not a distributed consensus system. No raft cluster, no multi-region
  story. One node, done properly.
- Not bug-for-bug SQLite or Postgres compatible. Compatibility means your
  everyday queries work, not that every edge case matches.
- Not an ORM for every language on day one. Rust first, others when the
  engine deserves them.

---

## Status

Pre-alpha, and further along than that sounds. What works today:

- the storage core: pager, copy-on-write B-tree, transactions, snapshots of
  any commit, branches and `as of` queries
- QQL and a SQLite-flavored SQL front end, both lowering to the same plans,
  with inner and left joins and multi-statement transactions
- a `.sqlite` importer that reads the format directly, with no SQLite
  library underneath it, and a command line tool around it
- a network server: `quanty serve` runs the same engine over a versioned
  binary protocol, with token authentication, and `quanty connect` is the
  client for it

```sh
quanty import app.sqlite app.qdb
quanty run app.qdb "get users { name } where score > 100"

quanty serve app.qdb --tokens tokens.txt
quanty connect 127.0.0.1:7878 "get users { name }" --token <token>
```

The server is one event loop per worker on epoll, with the connection
parked rather than blocked while a statement runs, and statements that
arrive together share one commit and one fsync. Readers do not queue behind
someone else's open transaction. What that costs and what it buys is
measured in [ADR-028](docs/DECISIONS.md), not asserted.

The importer reads what SQLite writes, including tables without rowids,
uncheckpointed write-ahead logs, text in utf-16 and columns added by a
later `alter table`. See [IMPORT.md](docs/IMPORT.md) for what it decides on
your behalf and what it tells you about afterwards.

The test bar: property tests against a model, four fuzzers, golden query
suites, an index consistency checker, row for row verification of an
imported database against SQLite's own output, two crash harnesses that
kill -9 the process mid-write a thousand times each per CI run, and a soak
that runs many connections against the server at once and checks the
promises that have to hold whichever way a race went.

No dependencies. Not "few": the lock file holds eleven packages and all
eleven are this workspace, so `cargo build` fetches nothing, there is no
supply chain to audit, and the whole suite runs on a toolchain from 2023.
What that costs is measured and written down in
[ADR-020](docs/DECISIONS.md) rather than waved at.

### By the numbers

| | |
|---|---|
| Rust, source | 21210 lines across 11 crates |
| Rust, tests | 11704 lines, 373 test functions |
| Design notes | 2816 lines, 29 decision records |
| Dependencies | 0 |
| People | 1 |
| Funding | none |
| CI per push | 11 jobs, 4 fuzzers, 2000 kill -9s, a 10 minute server soak |

More than one line of test for every two lines of code, and every one of
them runs on every push.

The acceptance runs behind phase 4, each an hour on one core:

| | |
|---|---|
| SQL parser | 4244088000 inputs parsed, no panic, no broken roundtrip |
| SQLite reader | 1831872 damaged files read, 12 billion cells, no panic |
| chinook import | 15607 rows, every value compared against the source |

The phase 5 acceptance run, on two cores of a Ryzen 5 5600G with the
client pinned to other cores and ten thousand idle connections open the
whole time:

| | |
|---|---|
| Duration | 30 minutes |
| Statements | 1800064 answered, **0 failed** |
| Rate | 1000/s, nine reads per write, each write fsynced |
| Latency | 123 us mean, 7.6 ms worst |
| Idle connections | 10000 held, 0 refused, 10000 still open at the end |
| Memory | under 10 MB resident, flat: **under 1 kB per connection** |
| Descriptors | 10042, flat all half hour, every one returned |

That run is the criterion for the whole server design and not a box to
tick: [ADR-023](docs/DECISIONS.md) says a red one means the design was
wrong and gets rewritten, so it is worth knowing it is green.

Against SQLite, both engines driven through their own command line tool,
on one development core:

| | quanty vs sqlite |
|---|---|
| Open a database | **0.91x**, faster |
| 5000 lookups by key | **0.94x**, faster |
| 20 full scans of 5000 rows | 1.91x slower |
| Bulk insert, 5000 rows in one transaction | 5.28x slower |

The write path is the one that is behind, it is behind for reasons that are
measured rather than guessed, and it has moved: bulk insert was 12.82x and
key lookup was 1.44x before the descent stopped decoding branch nodes it
only walks through. `docs/ROADMAP.md` has the profile and what is left.

Smaller numbers, on one development core with the client competing for it,
so a floor rather than a result:

| | |
|---|---|
| Reads over the network | 20000 QPS, 0 errors, 45 us mean |
| Writes over the network | 9357 statements/s, all committed, all fsynced |
| Commit, in process | 292 us alone, 22 us when 512 share one |

The last row is why group commit exists, and the reasoning from measurement
to decision is [ADR-028](docs/DECISIONS.md).

Progress lives in [ROADMAP.md](docs/ROADMAP.md). Design notes live in
[ARCHITECTURE.md](docs/ARCHITECTURE.md) and [DECISIONS.md](docs/DECISIONS.md)
if you want to see how the sausage is made. If you want to help, or just to
know what the rules are before you open anything,
[CONTRIBUTING.md](CONTRIBUTING.md) says both.

The hundredth commit has a page of its own in [HUNDRED.md](HUNDRED.md):
the bugs worth remembering, the ideas that lost, and what one person with
no dependencies and no funding has to show for a hundred commits.

Star the repo if you want to follow along. :3

---

## License

MIT. See [LICENSE](LICENSE).
