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

Binary protocol, tokio server, auth tokens, quanty-cli `serve` and remote
repl, concurrent readers with single writer queueing, group commit.

Acceptance:
- [ ] 10k idle connections + 1k active mixed QPS on a 2 vCPU box, stable
      for 30 min, no fd/memory leaks
- [ ] kill -9 the server under write load, reopen, zero corruption
- [ ] protocol versioned handshake, old client vs new server errors cleanly

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

**Everything durable is about 2.5x sqlite.** Insert, point lookup, scan and
indexed lookup all land between 2.4 and 2.7 times sqlite's time on the CI
runner, with no optimisation work done anywhere. That is a starting point
rather than a problem, and it is written down so the next measurement has
something to move against.

## Later / unscheduled

Live query subscriptions, Postgres wire protocol, vector index, real merge
with conflict resolution, multi-writer MVCC, WASM build, TS codegen,
time-series helpers, queue primitives, dashboard.

These are on the vision list, not the roadmap. They get scheduled when the
phases above are green, one at a time.
