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
- [ ] the SQL golden suite runs the same logical cases as the QQL suite
- [ ] SQL parser fuzzing 1h without crash, same bar as the QQL parser
- [ ] inner and left joins return results identical to a brute force
      reference on randomized workloads; explain pins the join strategy
      (nested loop vs index nested loop) in golden tests
- [ ] begin/commit/rollback across statements, and the crash harness kills
      inside an open transaction: committed data survives, the open
      transaction vanishes without a trace
- [ ] import of a real-world SQLite db (chinook, checked into the repo)
      with schema + data verified row by row against the source
- [ ] the SQLite reader rejects corrupted and hostile files with errors,
      never panics or wrong data (fuzzed, same bar as our own format
      reader)
- [ ] unsupported SQL returns a clear error, wrong results count as P0 bugs

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

## Later / unscheduled

Live query subscriptions, Postgres wire protocol, vector index, real merge
with conflict resolution, multi-writer MVCC, WASM build, TS codegen,
time-series helpers, queue primitives, dashboard.

These are on the vision list, not the roadmap. They get scheduled when the
phases above are green, one at a time.
