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

> Quanty is in early development. The design is done, the core is being
> built, and nothing here is installable yet. This README describes where
> the project is going. See [ROADMAP.md](docs/ROADMAP.md) for what's
> actually finished.

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

- **Time travel.** `get users as of -2h`. Query any past state of your
  database. The "oh no, I just broke prod" command.
- **Branching.** Fork your entire database in milliseconds, test a risky
  migration on the branch, merge or throw it away. Copy-on-write means a
  branch costs nothing until it diverges.
- **Instant snapshots.** Fork the db per test, run, discard. No fixtures,
  no cleanup scripts.
- **Lock-free readers.** Readers pin a commit and never block writers.
  Thousands of concurrent readers are the easy case, not the hard one.

---

## Quick look

This is the target API, so consider it a preview and not documentation.

Schema and queries in QQL, Quanty's native language:

```
table users {
  id:    int  @key
  name:  text @index
  score: int = 0
}

get users where score > 100 order by score desc limit 10
set users where id = 1 { score += 5 }
get users as of -30m where name = "elchi"
```

Embedded in Rust:

```rust
use quanty::Quanty;

let db = Quanty::open("app.qdb")?;

db.exec(r#"insert users { id: 1, name: "elchi" }"#)?;

// the SQL front end understands your existing sqlite queries too
let rows = db.sql("SELECT name FROM users WHERE id = 1")?;
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
- Async I/O, designed for thousands of concurrent connections
- Small versioned binary protocol, token auth

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

Pre-alpha. The storage core (pager, copy-on-write B-tree, transactions,
snapshots of any commit) and the first QQL slice (tables, put/get/set/del,
secondary indexes, explain) are in and tested. The test bar: property
tests against a model, parser fuzzing, 200+ golden query tests, an index
consistency checker, and a crash harness that kill -9s the process
mid-write a thousand times per CI run.

Progress lives in [ROADMAP.md](docs/ROADMAP.md). Design notes live in
[ARCHITECTURE.md](docs/ARCHITECTURE.md) and [DECISIONS.md](docs/DECISIONS.md)
if you want to see how the sausage is made.

Star the repo if you want to follow along. :3

---

## License

MIT. See [LICENSE](LICENSE).
