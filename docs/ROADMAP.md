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
and an `asset` column that holds a descriptor. There is no inline
threshold: ADR-034 counted what an invisible spill would cost and chose a
declared column type instead.

Acceptance:
- [x] store/retrieve 1 GiB asset with constant memory: 16 MiB resident on
      one core and 65 on the four core runner, against a gigabyte of
      payload, measured from `/proc/self/statm` by a watcher thread in the
      heavy suite. The spread is allocator retention, not the database
- [x] identical files stored twice use ~1x space (dedup verified): the
      second copy of a 200 kB chunk costs two pages
- [x] blob GC integrates with commit GC without dangling chunks (checker):
      an `asset` column claims its chunks on insert and gives them back on
      delete, on overwrite and when the table is dropped; `gc blobs`
      collects what a run that died mid upload left behind, by walking the
      rows rather than trusting a count; and `check_blobs` reports the
      store sound after every one of those. ADR-033 records why the race
      that blocked this is gone.

## Phase 7: Search

Inverted index for full text (tokenizer, positions, BM25 ranking)
maintained transactionally with the data. ADR-036 fixes the shape: a
posting is a secondary index entry whose value is a term, so postings
inherit versioning, branching and the crash harness rather than getting a
storage path of their own.

Complete: the tokenizer, postings kept in step with the rows, `match`
over them, and BM25 ranking scored while the postings are read.

Acceptance:
- [x] indexed search returns identical results to a brute force scan on a
      test corpus, but >100x faster at 100k docs: 461x over a search mix,
      on 100k documents of 15 words drawn Zipf over a 5000 word
      vocabulary. `match` is a plain binary operator, so the brute force
      is the same predicate on a column without `@text` rather than a
      second implementation written to agree.

      The heavy test prints the whole curve rather than one number,
      because the number depends on how much a query matches: 4245x for a
      word nothing contains, 191x at 187 hits, 128x at 215, and 1x for
      the corpus's most common word at 82607 hits, where the index is
      slower than the scan because ranking has to score every one of
      them. Nothing can beat a scan at producing eighty thousand rows,
      and a benchmark that hid that would be measuring the corpus
- [x] index stays consistent under the crash harness: the harness table
      now carries a `@text` column, and recovery is followed by
      `verify_indexes`, which rebuilds every posting from the surviving
      rows and compares keys and values. 1000 kills, 999 of them with
      acknowledged transactions, all clean

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

## Done: the public embedded crate, and the derive that follows it

The workspace layout in ARCHITECTURE.md carried two crates since the
beginning that no phase ever built: `quanty/`, the public embedded API that
a Rust application adds as a dependency, and `quanty-derive/`, the ORM
derive macros over it. Both exist now; ADR-030 fixes the surface and
ADR-031 fixes what the derive covers.

- [x] `quanty/` exists, depends only on the internal crates, and re-exports
      none of them, so no internal type reaches an embedder's signatures.
- [x] `Database` carries no type parameter, and `gc` stays behind `&mut`:
      reaching around an open transaction to run one does not compile.
- [x] A closure transaction commits on `Ok`, rolls back on `Err`, and
      leaves no transaction open either way.
- [x] `quanty-derive/`, without `syn` and without `quote`: a struct of
      named fields maps to a table, and generated writes go through the
      statement AST rather than through generated text (ADR-031).

What follows was written before any of it was built and is kept because the
reasoning is what the surface was chosen against.

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
- whether `quanty-derive` ships with it or follows later. It follows
  later: the two are separable and only one of them is on phase 9's path.

**Decided: no exception, so no `syn` and no `quote`.** ADR-020 has now
been asked three times, for tokio, for a TLS client and for this, and the
answer is the same each time. It is workable rather than pleasant: a proc
macro is handed a `TokenStream` by the compiler and can walk it directly,
and a derive over a struct of named fields with a handful of attributes is
a narrow enough shape to walk by hand. It is real work and it is the reason
the derive follows the embedded crate rather than arriving with it.

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

**That was tried on the same day and it does not go where it looked.** An
inline key and value type, the same twenty four bytes as a `Vec<u8>` so
nothing got fatter, measured on both paths:

| | before | inline | |
|---|---|---|---|
| `WriteTx::put`, 20000 per txn | 14.6 us | **7.7 us** | 1.9x faster |
| `put` statements, 20000 per txn | 19.5 us | 20.4 us | 5% slower |

Both reproduce across runs and both reproduce on reverting, so neither is
noise. The embedded write path nearly halves. The statement path, which is
what the server, the shell and QQL all use, gets slightly worse: the
fifteen byte inline buffer has to be zeroed before the copy, and a row
value longer than that allocates anyway, so the statement path pays the
zeroing and keeps the allocation.

The change was reverted, because a storage core does not take a change
whose net effect is unclear. What it leaves behind is worth more than it
was:

**The statement path is not dominated by the b-tree.** A raw `put` into a
tree of the same size cost 7.7 microseconds and a `put` statement cost
19.5. This section had said for weeks that bulk insert is slow because of
`insert_rec`, and that was true of the embedded API and mostly false of the
statement path.

**Profiling the statement path found it in one line.** A profile of a
write-only run had `btree::get` in it, which should not appear in a
benchmark that only writes. It came from `Exec::put`:

```
if self.tx.get(&key)?.is_some() { ... duplicate }
self.tx.put(&key, &encode_key(&values))?;
```

Two full descents per row, both decoding every node on the path, and the
first one redundant: the binary search inside the insert already knows
whether the key was there. `WriteTx::put_unique` now decides on the way
down and reports it. A descent writes nothing until it comes back up, so a
duplicate found at the leaf returns without touching anything.

| | before | after |
|---|---|---|
| 50000 `put` statements, one transaction | 18.7 us/row | **11.1 us/row** |

**1.7x on the path the server, the shell and QQL all use.** A value large
enough to overflow keeps the old route, because overflow pages are written
before the descent and a duplicate found afterwards would leave them behind
unreferenced.

**And then the descent stopped decoding what it only walks through.** The
profile still had `Node::decode` and the allocator on top, and the reason
was that following a branch materialized it: a `Vec` for every key on the
page, allocated and dropped again, at every level of every descent, to
follow one pointer. `Node::branch_child` walks the cells in the page bytes
instead. It scans where the decoded version binary searched, which is a
hundred and fifty short memcmps against a hundred and fifty allocations.

The insert descent now decodes a branch only when the recursion comes back
with something that changes it, and in a bulk load it usually does not,
because a leaf keeps its page after the first touch. The read descent never
decodes a branch at all.

| | before | after |
|---|---|---|
| 50000 `put` statements, one transaction | 18.7 us/row | **8.3 us/row** |

**Against sqlite, both through their own command line tools:**

| | before | after |
|---|---|---|
| bulk insert, 5000 rows in one transaction | 12.82x slower | **5.28x** |
| 5000 lookups by key | 1.44x slower | **0.94x, faster than sqlite** |
| 20 full scans of 5000 rows | 1.91x slower | 1.91x |
| 5000 lookups through a secondary index | 1.64x slower | 1.64x |
| durable insert, one commit each | 1.62x slower | 1.62x |

Two and a quarter times on the statement write path, and the key lookup
crossed over. Nothing else moved, which fits: scans decode leaves and that
is real work, and a durable insert is dominated by the fsync.

What is left above the storage layer is parsing, planning, the catalog and
row encoding. Scans and the secondary index are the two rows in the table
above that have not moved at all, and neither has been profiled.

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

A per-session branch belongs here too. A write always lands on the branch
the database is switched to, so running a statement somewhere else means
switching there and back, which ADR-032 refused to hide behind a flag.
Making it real means the branch becomes part of a session rather than of
the file, and that touches the server as much as the tool.

These are on the vision list, not the roadmap. They get scheduled when the
phases above are green, one at a time.
