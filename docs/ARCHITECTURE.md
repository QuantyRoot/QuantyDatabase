# QuantyDB Architecture

Internal design doc. This is the source of truth for how Quanty is built.
If code and this doc disagree, fix one of them.

That instruction went unheeded for a while. A pass in August 2026 checked
every claim here against the code and found fifteen that were not true,
most of them plans that were never built and one, the writer lock, that
was a safety promise nothing kept. Where something is planned rather than
built this now says so in the same sentence, so that reading a paragraph
is enough to know which it is.

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
 |  schemas, tables, indexes (branch heads live in the refs tree) |
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
page 2+: data pages    (btree nodes, overflow, free list)
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
4       1     page type (meta, branch node, leaf, overflow, freelist;
              a blob slot is reserved and has never been written)
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
parent commit id           (one; merge is fast forward only, so no
                            commit has ever had two)
root page of the data tree
root page of the catalog tree
wall clock timestamp
optional message / tag
```

Commits form a DAG exactly like git. Branch heads live in a small refs tree
that sits outside the versioned data and catalog trees, so a pointer into
history is never versioned by the commits it points at (see ADR-011). `as of
time <ms>` resolves to the newest commit on the current branch at or before a
timestamp; `as of <commit>` pins an exact commit id. Garbage collection marks
what the retained commits reach and sweeps the rest into the free list for
reuse.

### MVCC and concurrency

v1: single writer, many readers. One write transaction at a time per
handle, held by a mutex in the pager. Readers are lock free (they pin a
commit). This is the LMDB/SQLite model and it is enough for a long time.

There is no file lock, so two handles on one file, in one process or two,
each believe they are the only writer. That used to lose an acknowledged
commit silently; `commit` now reads the meta slot it is about to
overwrite and refuses with `WriterRaced` instead. ADR-035 has the numbers
and says why the lock itself waits on an MSRV decision.

v2 (later): optimistic multi-writer per branch. Writers build against a base
commit, at commit time we check for page-level conflicts and retry losers.
Do not build this before the benchmark suite exists.

### Space reclamation

Old pages become garbage when no retained commit references them.
Retention is a count of commits to keep per branch, given to each run:
`gc keep <n>`, or `Session::gc(n)` from Rust. Named policies and
durations were planned here and do not exist.

GC walks commits outside the retention window, moves their exclusively
owned pages to the free list once retention allows it (phase 3, ADR-010).
The free list is itself a small tree of page ranges. It runs only when
asked, never incrementally on commit, and there is no `quanty gc`
subcommand: `quanty run <db> "gc keep 5"`.

### Key encoding

All indexed values are encoded with an order-preserving encoding so the
B-tree only ever compares bytes:

- ints: flipped sign bit, big endian
- floats: IEEE 754 with sign-dependent bit flip
- text: UTF-8 bytes, 0x00 written as 0x00 0xFF, single 0x00 terminator
- composite keys: concatenation of the above

This is a well known technique (FoundationDB tuple layer, MyRocks). Write it
once, property test it hard (encode then compare == compare originals).

### Blob store

A column declared `blob` holds a descriptor rather than the bytes. There
is no size threshold and no silent spill; ADR-034 counted what an
invisible one costs and chose against it.

- chunked (default 1 MiB chunks), each chunk hashed with SHA-256; ADR-033
  says why not BLAKE3
- content addressed: identical chunks are stored once (free dedup)
- rows store a blob descriptor (total size, chunk hash list)
- chunks are catalog entries under `("blob", hash)`, so they are versioned
  per commit and their payloads take the overflow chain the B-tree already
  has; the reference count sits at a key of its own
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

- Logical plan: relational algebra nodes (scan, filter, project, join,
  sort, limit). Aggregates were on this list and are not built.
- Physical plan v1: rule based. Use an index when a filter matches an index
  prefix, otherwise full scan. Nested loop join, index nested loop when
  possible. Sort is in memory; an external merge sort for sets past a
  memory budget was planned here and is not built.
- Executor v1: pull-based iterator model (volcano). Vectorized batches are a
  v2 optimization, the iterator interface should already pass row batches to
  make that transition cheap.
- Every physical plan is explainable from day one: `explain <query>` ships in
  the same milestone as the planner.

### Catalog

The catalog is its own tree, rooted in the commit record beside the data
tree, so it is versioned with the data and schema changes are branchable
and time travelable for free. Its keys are typed tuples in the same
encoding as everything else: `("table", name)` for a table and its
indexes, `("seq")` for the id counter, `("blob", hash)` and
`("blobrefs", hash)` for chunk bytes and their counts. The string
prefixes `__quanty/...` were planned here and appear nowhere in the code.
Branch heads are not in this tree at all; they live in the refs tree
outside the versioned trees, for the reason ADR-011 gives.

## Server mode

Built, and different from what this section first planned. The plan said
tokio and msgpack; ADR-020 had already taken the workspace to zero
dependencies, so ADR-022 chose threads, and ADR-023 overturned it again and
wrote the epoll syscalls out by hand. What exists:

- an event loop per worker on epoll, level triggered, each with its own
  listening socket via `SO_REUSEPORT` (ADR-025). A shared listener with
  `EPOLLEXCLUSIVE` was tried first and distributed 354/82/64 across three
  workers; the replacement does 155/173/172
- own binary protocol, length prefixed frames, versioned handshake, and a
  codec of its own rather than msgpack. The wire encoding deliberately does
  not share constants with the key encoding in the core, so a change to the
  file format cannot become a silent protocol change
- one executor thread owns the session; a connection's open transaction is
  parked in and out around each statement (ADR-027). A connection waiting
  on a statement is parked, not blocked, so its worker serves others
- statements that arrive together share one transaction, one write and one
  fsync, each with a savepoint of its own (ADR-028). Measured at thirteen
  times on the write path before it was built
- readers do not queue behind another connection's open transaction, since
  a read commits nothing and cannot invalidate a parked batch (ADR-029)
- auth: token hashes in a file beside the database, never inside it,
  because branching and `as of` would make "revoked" true only at the tip
  of one branch (ADR-026)
- `quanty connect` speaks the protocol, and its output is held byte for
  byte against the local path

Still open: every statement crosses the one executor thread, reads
included. They no longer stall, but they do not run in parallel either,
and whether they should is a measurement that needs more than one core.

## SQLite compatibility

Two independent pieces, do not mix them up:

1. **Importer.** Read the SQLite file format directly (it is documented and
   stable), convert tables, indexes and data into a Quanty file.
   `quanty import app.sqlite app.qdb`. No SQLite library dependency, we
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
- **Fuzzing:** four ten minute jobs over the QQL parser, the SQL parser,
  the file format reader and the wire protocol (a corrupted input must
  produce an error, never UB). They are plain `cargo test` harnesses with
  their own generators, not cargo-fuzz, which is a tool and a nightly
  toolchain this project does not take.
- **Benchmarks:** `quanty-bench`, hand written for the same reason, with
  a macro bench (bulk load, point reads, range scans, mixed workload)
  tracked over time. Compared against SQLite through both command line
  tools, and against PostgreSQL in chat. redb was named here and has
  never been measured. Publish numbers only when they are reproducible.

## Workspace layout

```
quanty/
  Cargo.toml            (workspace)
  crates/
    quanty-core/        pager, btree, commits, mvcc, blobs, encoding
    quanty-ql/          QQL + SQL front ends (pure syntax)
    quanty-exec/        catalog, planner, executor
    quanty/             public embedded API, the crate users add
    quanty-derive/      ORM derive macros
    quanty-proto/       wire protocol codec (bytes only, no I/O)
    quanty-server/      reactor, connection state machine, dispatch
    quanty-service/     the executor thread, write queue, group commit
    quanty-auth/        sha256 and the token file
    quanty-sqlite/      reader for the SQLite file format
    quanty-import/      turns a SQLite database into a QuantyDB one
    quanty-bench/       timing against SQLite, load generator, commit cost
    quanty-cli/         quanty binary (repl, import, branch, gc,
                        serve, connect, token)
  docs/
  tests/                cross-crate integration + crash harness
```

`quanty/` is built and its surface is fixed by ADR-030: concrete types
only, statements as text, transactions as a borrow, and nothing from the
internal crates re-exported, so no internal type reaches an embedder's
signatures. `quanty-derive/` sits on top of it and holds to ADR-020: it
walks the `TokenStream` it is handed, with no `syn` and no `quote`.

There is no dependency budget, because there are no dependencies. This
section used to name crc32c, blake3 and parking_lot; ADR-020 wrote out the
parts of them this project actually needed and the lock file has held
nothing but this workspace since. SHA-256 for token hashing followed the
same route later, checked against the published vectors. The core stays
std-only and io-abstracted behind a `Storage` trait over file and memory
backends, which is also what keeps a WASM build possible.

## Performance notes (for later, do not gold plate early)

- mmap vs pread: start with pread + a small userspace page cache (clock
  eviction). mmap is a backend behind the Storage trait, not the default.
- group commit in server mode: built in phase 5, batching concurrent
  write txns into one fsync.
- bloom filters per leaf range and prefix compression in nodes: v2.
- the adaptive story (auto index suggestions, hot/cold tiering, layout
  switching) needs a stats collector first. `DbStats` counts pages, head
  pages and free pages; nothing surfaces it, and `quanty stats` was named
  here and is not built. Make decisions from real numbers.
