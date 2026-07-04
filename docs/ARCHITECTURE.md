# QuantyDB Architecture

Internal design doc. This is the source of truth for how Quanty is built.
If code and this doc disagree, fix one of them.

## Design principles

1. **One core, many personalities.** There is exactly one storage engine.
   Embedded mode, server mode and SQLite compat are thin layers on top of it.
   No feature gets its own storage path.
2. **Time travel is not a feature, it is the storage model.** Every commit is
   immutable. Snapshots, branching and AS OF queries fall out of the design
   instead of being bolted on.
3. **Correct first, fast second, clever third.** Every layer ships with a
   boring reference implementation and a test suite before it gets optimized.
4. **The file format is the contract.** Versioned, documented, forward
   compatible. Breaking the format after v1 requires a migration path.

## Layer diagram

```
 +---------------------------------------------------------------+
 |  frontends                                                    |
 |  embedded API (Rust crate) | server (own protocol) | CLI      |
 |  SQLite dialect + .sqlite importer                            |
 +-------------------------------+-------------------------------+
 |  query layer                                                  |
 |  QQL lexer/parser -> logical plan -> physical plan -> executor|
 +-------------------------------+-------------------------------+
 |  catalog                                                      |
 |  schemas, tables, indexes, branches (stored as system tables) |
 +-------------------------------+-------------------------------+
 |  storage core                                                 |
 |  pager | COW B-tree | commit log | MVCC | free list | blobs   |
 +---------------------------------------------------------------+
```

## Storage core

### File layout

Single file, page based. Default page size 4096 bytes, set at creation time
and stored in the header.

```
page 0:  meta page A   (header, format version, root of commit tree, ...)
page 1:  meta page B   (identical role, ping-pong with A)
page 2+: data pages    (btree nodes, overflow, free list, blob chunks)
```

Dual meta pages, LMDB style. A commit fsyncs all new data pages, then writes
the meta page with the higher transaction id to the *other* slot and fsyncs
again. Recovery after a crash: read both metas, pick the one with the highest
txid and a valid checksum. No WAL needed for v1. This gives us crash safety
with two fsyncs per commit and zero recovery time.

Every page starts with a small header:

```
offset  size  field
0       4     checksum (crc32c of the rest of the page)
4       1     page type (meta, branch node, leaf, overflow, freelist, blob)
5       1     flags
6       2     entry count / used bytes
8       8     lsn of the commit that wrote this page
```

### Copy-on-write B-tree

Writes never modify a page in place. A write transaction copies the path from
leaf to root, modifies the copies, and the commit installs a new root pointer.

Consequences we exploit:

- A snapshot is a root pointer plus a commit id. Zero copy cost.
- A branch is a named snapshot that accepts new commits. Also zero cost.
- Readers never block writers and never take locks. A reader pins a commit
  and reads a frozen tree.
- Old commits stay readable until garbage collected, which is what makes
  AS OF queries work.

Node layout: slotted pages, keys and values length prefixed. Values larger
than ~1/4 page spill to overflow page chains. Keys are compared as raw bytes;
the encoding layer (see below) guarantees that byte order equals logical
order.

### Commits and the commit DAG

Each commit record stores:

```
commit id (u64, monotonic)
parent commit id(s)        (two parents after a merge)
root page of the data tree
root page of the catalog tree
wall clock timestamp
optional message / tag
```

Commits form a DAG exactly like git. Branch heads are stored in a small
system table. `AS OF <time>` resolves to the newest commit on the current
branch with timestamp <= time. `AS OF #<commit>` pins an exact commit.

### MVCC and concurrency

v1: single writer, many readers. One write transaction at a time per
database, enforced with a file lock in embedded mode and a mutex in server
mode. Readers are lock free (they pin a commit). This is the LMDB/SQLite
model and it is enough for a long time.

v2 (later): optimistic multi-writer per branch. Writers build against a base
commit, at commit time we check for page-level conflicts and retry losers.
Do not build this before the benchmark suite exists.

### Space reclamation

Old pages become garbage when no retained commit references them. Retention
policy per database:

- `keep = all` (full time travel, file grows)
- `keep = <duration>` (e.g. 7d, the default)
- `keep = heads` (only branch heads, minimal size, SQLite-like behavior)

GC walks commits outside the retention window, moves their exclusively owned
pages to the free list once retention allows it (phase 3, ADR-010). The free
list is itself a small tree of page ranges. GC
runs incrementally on commit or via explicit `quanty gc`.

### Key encoding

All indexed values are encoded with an order-preserving encoding so the
B-tree only ever compares bytes:

- ints: flipped sign bit, big endian
- floats: IEEE 754 with sign-dependent bit flip
- text: UTF-8 bytes, 0x00 escaped, 0x00 0x00 terminator
- composite keys: concatenation of the above

This is a well known technique (FoundationDB tuple layer, MyRocks). Write it
once, property test it hard (encode then compare == compare originals).

### Blob store

Values above a threshold (default 64 KiB) leave the row and become blobs:

- chunked (default 1 MiB chunks), each chunk hashed with BLAKE3
- content addressed: identical chunks are stored once (free dedup)
- rows store a blob descriptor (total size, chunk hash list)
- chunks live in dedicated blob pages in the same file for v1
- S3/bucket tiering moves cold chunks out later; the descriptor does not
  change, only the chunk location table does

## Query layer

### QQL

Own language. Design goals: readable, typed, no surprises. Grammar lives in
`docs/QQL.md` once the parser exists. Rough shape:

```
table users {
  id:    int  @key
  name:  text @index
  score: int = 0
}

get users where score > 100 order by score desc limit 10
set users where id = 1 { score += 5 }
```

Hand written recursive descent lexer/parser. No parser generators; error
messages matter more than grammar convenience.

### SQL dialect

A second parser front end that accepts a pragmatic SQLite-flavored SQL subset
and lowers to the same logical plan as QQL. Target list for v1: CREATE TABLE,
DROP TABLE, INSERT, SELECT with WHERE/ORDER BY/LIMIT/JOIN (inner + left),
UPDATE, DELETE, CREATE INDEX, transactions. Everything else returns a clear
"not supported yet" error, never a wrong result.

### Planner and executor

- Logical plan: relational algebra nodes (scan, filter, project, join, sort,
  limit, aggregate).
- Physical plan v1: rule based. Use an index when a filter matches an index
  prefix, otherwise full scan. Nested loop join, index nested loop when
  possible. Sort is external merge sort when the set exceeds a memory budget.
- Executor v1: pull-based iterator model (volcano). Vectorized batches are a
  v2 optimization, the iterator interface should already pass row batches to
  make that transition cheap.
- Every physical plan is explainable from day one: `explain <query>` ships in
  the same milestone as the planner.

### Catalog

System tables live in the same B-tree keyspace under a reserved prefix:
`__quanty/tables`, `__quanty/indexes`, `__quanty/branches`, `__quanty/meta`.
The catalog is versioned with the data because it lives in the same commit
tree. Schema changes are therefore branchable and time travelable for free.

## Server mode

- tokio, one task per connection, shared engine handle
- own binary protocol: length prefixed frames, request id, msgpack payloads
  (simple, debuggable, versioned handshake). Postgres wire protocol is a
  separate future front end, not the native protocol.
- auth: per-database tokens for v1, users/roles later
- connection scaling target: 10k mostly idle connections on a small VPS
  without falling over. Readers scale naturally (lock free), writers queue.

## SQLite compatibility

Two independent pieces, do not mix them up:

1. **Importer.** Read the SQLite file format directly (it is documented and
   stable), convert tables, indexes and data into a Quanty file.
   `quanty import app.sqlite -o app.qdb`. No SQLite library dependency, we
   parse the format ourselves.
2. **Dialect.** The SQL front end above. Goal is "your typical app queries
   run unchanged", not bug-for-bug compatibility.

## Testing strategy

This project lives or dies on trust in the storage layer.

- **Model testing:** property test the B-tree against `std::collections::BTreeMap`
  with random operation sequences, including reopen-from-disk between ops.
- **Crash testing:** a harness that runs workloads in a child process,
  SIGKILLs it at random points (including mid-fsync via failpoints), reopens
  the file and verifies invariants. Run thousands of iterations in CI.
- **Encoding tests:** property test order preservation of the key encoding.
- **SQL tests:** sqllogictest-style golden files for the SQL subset.
- **Fuzzing:** cargo-fuzz targets for the QQL parser, the SQL parser and the
  file format reader (a corrupted file must produce an error, never UB).
- **Benchmarks:** criterion micro benches plus a macro bench (bulk load,
  point reads, range scans, mixed workload) tracked over time. Compare
  against SQLite and redb honestly and publish numbers only when they are
  reproducible.

## Workspace layout

```
quanty/
  Cargo.toml            (workspace)
  crates/
    quanty-core/        pager, btree, commits, mvcc, blobs, encoding
    quanty-ql/          QQL + SQL front ends (pure syntax)
    quanty-exec/        catalog, planner, executor
    quanty/             public embedded API, re-exports, the crate users add
    quanty-derive/      ORM derive macros
    quanty-server/      tokio server, protocol
    quanty-cli/         quanty binary (repl, import, branch, gc, serve)
  docs/
  tests/                cross-crate integration + crash harness
```

Dependency budget for quanty-core: as close to zero as possible. crc32c,
blake3, maybe parking_lot. No tokio, no serde in the core. The core must
compile to WASM later, keep it std-only and io-abstracted (a `Storage` trait
over file/mmap/memory backends from day one).

## Performance notes (for later, do not gold plate early)

- mmap vs pread: start with pread + a small userspace page cache (clock
  eviction). mmap is a backend behind the Storage trait, not the default.
- group commit in server mode: batch concurrent write txns into one fsync.
- bloom filters per leaf range and prefix compression in nodes: v2.
- the adaptive story (auto index suggestions, hot/cold tiering, layout
  switching) needs a stats collector first. Ship `quanty stats` early, make
  decisions from real numbers.
