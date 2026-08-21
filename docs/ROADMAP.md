# QuantyDB Roadmap

Internal. Phases are ordered by dependency, not by hype. A phase is done when
its acceptance criteria pass in CI, not when the code exists.

Rule of thumb: nothing from a later phase starts while an earlier phase has
red acceptance criteria. Exceptions need a written reason in DECISIONS.md.

## Phase 0: Foundation

Pager, file format, dual meta pages, page cache, Storage trait
(file/memory backends), checksums, the crash harness itself.

Acceptance:
- [x] create db, write pages, reopen, read back, checksums verified
- [x] crash harness runs a raw page workload, 1000 SIGKILL iterations,
      zero corrupted reopens
- [x] corrupted file (bit flips via test helper) is rejected with an error,
      never a panic or garbage data
- [x] file format documented in docs/FORMAT.md

## Phase 1: COW B-tree + transactions

B-tree with insert/get/delete/range scan, overflow pages, commit records,
snapshots of any commit, single-writer transactions, order-preserving key
encoding. Space reclamation (free list, delete rebalancing) moved to phase 3
where retention makes it safe, see ADR-010.

Acceptance:
- [x] property test vs BTreeMap model, 10k random op sequences incl. reopens
      (12.5k sequences ran green: 10k in memory, 2.5k on disk with reopens)
- [x] key encoding property test: byte order == logical order, all types
- [x] open an old commit id and read the tree as it was
- [x] crash harness on the transactional API: committed data survives,
      uncommitted data fully disappears, 1000 iterations
- [x] 1M key bulk load and full scan without pathological memory use
      (28.7s load, 0.18s scan, release build)

## Phase 2: Rows, schema, QQL core

Catalog, typed rows, QQL lexer/parser/planner/executor for: table definition,
insert, get with filters, order by, limit, update, delete. Secondary indexes
kept in sync. `explain`.

Acceptance:
- [x] 150+ golden test cases (sqllogictest style) green
      (215 cases across 10 scripts)
- [x] parser fuzzing 1h without crash
      (61 minutes, 2.3 billion cases, seeds logged, plus the roundtrip
      invariant parse(pretty(ast)) == ast on every accepted input)
- [x] secondary index consistency check tool passes after random workloads
      (verify_indexes, 500 random sequences, and the checker is itself
      tested against deliberate damage)
- [x] explain shows index usage vs full scan correctly
      (KeyLookup / IndexScan / SeqScan choices pinned by golden tests)

## Phase 3: Time travel + branches

AS OF (timestamp and commit id), named branches, create/switch/delete,
fast-forward merge, `quanty log`, retention policies, GC (mark and sweep;
an incremental variant can come later if pauses ever matter).

Acceptance:
- [x] AS OF returns historically correct results in golden tests
- [x] branch, write divergent data on two branches, read both correctly
- [x] fast-forward merge works, non-ff merge is cleanly rejected for now
- [x] GC reclaims space (file stops growing under keep=heads churn workload)
      and never touches a retained commit (verified by the crash harness
      running GC in the kill window)

## Phase 4: SQL dialect + SQLite import

SQL front end (subset per ARCHITECTURE.md) lowering to the same plans as
QQL, inner and left joins in the planner and executor, multi-statement
transactions, a .sqlite importer reading the SQLite format directly, and a
minimal quanty-cli around it.

Acceptance:
- [x] the SQL golden suite runs the same logical cases as the QQL suite
- [x] SQL parser fuzzing 1h without crash, same bar as the QQL parser
      (4244088000 inputs, seed 1786811633224126431; a third of them
      mutations of valid statements, the rest byte and token soup, each
      parsed and, where it parsed, put through the canonical roundtrip)
- [x] inner and left joins return results identical to a brute force
      reference on randomized workloads; explain pins the join strategy
      (nested loop vs index nested loop) in golden tests
- [x] begin/commit/rollback across statements, and the crash harness kills
      inside an open transaction: committed data survives, the open
      transaction vanishes without a trace
- [x] import of a real-world SQLite db (chinook, checked into the repo)
      with schema + data verified row by row against the source: all 15607
      rows compared value for value after the import, against a reader that
      is itself checked against sqlite's own digests
- [x] the SQLite reader rejects corrupted and hostile files with errors,
      never panics or wrong data (fuzzed, same bar as our own format
      reader: 1831872 files in 1h, seed 1786811631166197537, reaching 232
      million pages and 12 billion cells)
- [x] unsupported SQL returns a clear error, wrong results count as P0
      bugs: 33 constructs the dialect does not have are each refused by
      name, with a position, in a golden test that also checks the
      supported subset still parses

Three things the reader refuses today rather than reads. Each refusal is a
loud error naming the reason, and each is a stopping point rather than a
decision, in this order:

All three are now read rather than refused, and what is left of the list is
the record of why each was hard:

- WITHOUT ROWID tables live in an index b-tree, and the record reorders the
  columns: the primary key columns come first in key order, then the rest in
  declared order. Nothing in the bytes says which is which, so this needed
  the create-table parser first.
- WAL mode keeps the newest committed data in a `-wal` file until a
  checkpoint. The log is read on top of the main file now, and a wal mode
  database is refused only when nothing accounts for its log, rather than on
  the strength of the header flag, which said nothing useful on its own.
- UTF-16 text, in both byte orders. Unpaired surrogates come back as an
  error rather than a replacement character, because almost-right text is
  the failure that survives an import.

## Phase 5: Server mode

This line used to say "tokio server". ADR-020 had already taken the
workspace to zero dependencies and nobody reconciled the two; ADR-022 did,
and chose threads over a runtime. ADR-023 then overturned its own reasoning
and wrote the epoll syscalls out by hand after all, which is the honest
version of what happened and is why both records are still here.

Binary protocol, an epoll event loop per worker, auth tokens, `quanty
serve` and `quanty connect`, single writer queueing, group commit.

- [x] versioned binary protocol and codec (`quanty-proto`, ADR-023)
- [x] the reactor: epoll, one listener per worker, connections parked
      rather than blocked (`quanty-server`, ADR-025)
- [x] the executor: one thread owns the session, transactions park per
      connection (`quanty-service`, ADR-027)
- [x] group commit, measured into existence rather than assumed (ADR-028)
- [x] readers do not queue behind someone else's transaction (ADR-029)
- [x] token auth, stored beside the database and never in it (ADR-026)
- [x] `quanty connect`, held to the same output as the local path
- [ ] concurrent readers: they no longer stall, but they still serialize
      over one thread. Whether they should run in parallel is the open
      question in ADR-029 and it needs a machine with more than one core

Acceptance:
- [x] 10k idle connections + 1k active mixed QPS on a 2 vCPU box, stable
      for 30 min, no fd/memory leaks. Met 2026-08-21: 1800064 statements,
      none failed, descriptors and memory flat, numbers in ACCEPTANCE.md
- [x] kill -9 the server under write load, reopen, zero corruption, and
      every acknowledged write still present. 300 kills per CI run
- [x] protocol versioned handshake, old client vs new server errors
      cleanly, tested against a running process rather than the codec

## Phase 6: Blobs + assets

Content-addressed chunked blob store, dedup, streaming read/write API,
inline threshold config.

Acceptance:
- [ ] store/retrieve 1 GiB asset with constant memory
- [ ] identical files stored twice use ~1x space (dedup verified)
- [ ] blob GC integrates with commit GC without dangling chunks (checker)

## Phase 7: Search

Inverted index for full text (tokenizer, positions, BM25 ranking) maintained
transactionally with the data.

Acceptance:
- [ ] indexed search returns identical results to a brute force scan on a
      test corpus, but >100x faster at 100k docs
- [ ] index stays consistent under the crash harness

## Phase 8: The adaptive layer

Stats collector, `quanty stats`, index suggestions, auto index (opt-in),
hot/cold blob tiering to buckets (S3 API), workload-aware defaults.

Acceptance:
- [ ] suggestions demonstrably improve a benchmark workload when applied
- [ ] tiering round-trips blobs bit-perfectly, survives network failpoints

## Phase 9: Extension API

Third party code running inside the database: scalar functions the query
language can call, hooks that see commits, tables backed by something other
than our storage. Rust only, registered on a builder at build time, no
loadable objects (ADR-018). Two prerequisites are the reason this sits last
rather than the size of the job: the public embedded crate has to exist, and
QQL needs call syntax, which is a language change through both front ends
and both fuzz corpora.

Candidates in order of how far they reach into the engine: scalar functions,
then commit hooks and a change feed, then tables not backed by our storage.
Each one gets scoped when it is scheduled, not now.

Acceptance:
- [ ] a registered scalar function parses, pretty prints and round trips in
      both front ends, with extension supplied names in both fuzz corpora
- [ ] the planner never builds a probe from a condition containing an
      extension call, proven by a model test in the shape of join_model
- [ ] an extension that errors or panics fails the statement cleanly:
      nothing partial is written and the session stays usable
- [ ] verify_indexes and the crash harness stay green under randomized
      workloads that call extension code
- [ ] docs/EXTENSIONS.md documents the surface and states plainly that it is
      unstable before 1.0

## Unscheduled and blocking: the public embedded crate

The workspace layout in ARCHITECTURE.md has carried two crates since the
beginning that no phase ever builds: `quanty/`, the public embedded API that
a Rust application would add as a dependency, and `quanty-derive/`, the ORM
derive macros over it. Neither exists. What an embedder needs is reachable
today only by depending on `quanty-exec` and `quanty-core` directly, which
are internal crates carrying no stability promise.

This is written down because it blocks phase 9, not because it is scheduled.
ADR-018 names the public embedded crate as one of the two prerequisites for
the extension API, next to call syntax in QQL, and it is also what an
in-process Rust user needs once phase 5 hands every other language a
protocol. It has been mentioned in passing more than once and never given a
phase of its own, which is how a gap stays invisible until something waits
on it.

It is not a new capability. The engine underneath is finished. The work is
choosing a surface and then keeping it, which is why it deserves a phase
rather than a corner of one.

Open, and to be decided rather than assumed here:
- where it belongs in the order. That is a question about who is waiting:
  phase 5 has a named person behind it and this has nobody yet.
- how much the first version promises. ADR-018 declares the extension
  surface explicitly unstable before 1.0; whether the embedded surface makes
  the same statement is a separate call.
- whether `quanty-derive` ships with it or follows later. A derive macro
  means a proc-macro crate, and writing one without `syn` and `quote` is a
  dependency question under ADR-020, not a detail.

## Unscheduled and blocking: fetching things over the network

`quanty update` is meant to pull releases from GitHub, and that is a
product decision, not an open question. What is open is how, because
GitHub serves nothing over plain HTTP and answers a request for it with a
redirect to HTTPS.

That leaves the workspace one problem it has never had before: TLS. Writing
it out, the way the checksum and the epoll layer and SHA-256 were written
out, is five to fifteen thousand lines, which is more than the storage core
and the query layer put together. It is also the one area where code that
is wrong and clever looks exactly like code that is right: a timing leak in
a signature check passes every test that exists.

Two things make it smaller than it sounds. Only the client half is needed.
And confidentiality is worth almost nothing here, because a public release
artifact is public: what matters is that the bytes are the ones that were
published, which is a signature over the artifact rather than a secure
channel to fetch it. That is how apt has worked for twenty years.

So it belongs in a phase of its own, with an ADR, and with interoperability
against the real GitHub as the acceptance criterion rather than against a
server written by the same hand.

## Known problems, measured

Things the benchmark suite found, kept here with their numbers rather than
in somebody's memory. Each is a defect with a known cause, not a decision.

**Bulk loading in a transaction, fixed.** A statement inside an open
transaction used to replay every statement before it, which was quadratic:
5000 rows in one transaction took 2.95 seconds against 0.15 as separate
transactions. An open transaction is now a suspended write batch (ADR-021)
and the same load takes 0.13 seconds, which is faster than no transaction,
as it should be. What is left is a 12.7x gap to sqlite on that workload,
where sqlite does it in 10 milliseconds and we do it in 129 with a single
fsync on both sides. That is CPU and memory work on our side, and it is the
next thing to measure.

**Reading is close, writing is not.** With reads timed on their own,
against a database loaded beforehand, the picture separates cleanly:

| what | quanty | sqlite | ratio |
|---|---|---|---|
| open a database, do nothing | 1.1 ms | 1.2 ms | 0.92x |
| 5000 lookups by key | 44 ms | 31 ms | 1.44x |
| 20 full scans of 5000 rows | 43 ms | 24 ms | 1.75x |
| 5000 lookups by secondary index | 77 ms | 36 ms | 2.13x |
| 5000 rows, one commit per batch | 145 ms | 36 ms | 4.09x |
| 5000 rows, one transaction | 130 ms | 10 ms | 12.82x |

So reads are within a factor of two and opening is a hair faster. The gap
is writing, and it is clearest where fsync does not hide it: 130
milliseconds against 10 for the same 5000 rows.

**Where the write time goes, measured rather than guessed.** Of those 130
milliseconds, parsing the script is 4.7, and the secondary index is about a
third (86 ms without it, 133 with). Row width barely matters: the same load
with a narrower row takes the same time. What is left is the per-insert
cost in the b-tree, roughly 16 microseconds a row, and the reason is in
`insert_rec`: every single insert reads its leaf, decodes the whole node
into vectors, inserts one entry, and encodes the whole node back, at every
level of the tree. A leaf holding a hundred entries is therefore decoded
and re-encoded a hundred times while it fills.

**Profiled on 2026-08-21, and the profile moved the plan.** `perf` over the
bulk load, user space only:

| | |
|---|---|
| `Node::decode` | 7.4% |
| `malloc` | 6.4% |
| `free` | 2.0% |
| `crc32c` | 2.7% |
| `Node::encode_into` | 2.1% |

Decoding costs three and a half times what encoding costs, and the largest
single item beside it is the allocator. The reason is in the type:
`Node::Leaf` holds `Vec<(Vec<u8>, ValueRef)>`, so every key is its own heap
allocation, made and freed on every insert. A leaf holding 150 entries
therefore does 150 mallocs and 150 frees per inserted row.

So the first thing to do is not the caching that this section used to
propose. It is to stop allocating per key: a key that fits in a couple of
dozen bytes should live inline in the entry rather than behind a pointer.
Ours are encoded integers of about ten bytes, so nearly all of them would.
That is local to the node type, needs no invalidation rule and cannot lose
data, which the alternatives cannot both claim.

Two measured steps already taken. Encoding now writes straight into the
page instead of into a fresh page-sized buffer that was then copied over
it, which removed an allocation, a zeroing and a full page memcpy per
insert at every level of the tree: 13.5 to 12.5 microseconds a row at a
thousand rows, 19.1 to 18.5 at twenty thousand. Three to seven percent,
which is worth having and is not the answer.

After the key allocations, the two ideas this section began with are still
there and still ranked by effort: keep decoded nodes for the pages on the
current insert path, or edit the encoded page in place, which is what
sqlite does. The first is a cache with an invalidation rule and the second
is a change to the write path, and in a storage engine both can lose data
in ways a test suite does not notice. Neither should start before the
allocator work says what is left.

## Later / unscheduled

Live query subscriptions, Postgres wire protocol, vector index, real merge
with conflict resolution, multi-writer MVCC, WASM build, TS codegen,
time-series helpers, queue primitives, dashboard.

These are on the vision list, not the roadmap. They get scheduled when the
phases above are green, one at a time.
