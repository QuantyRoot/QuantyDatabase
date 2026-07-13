# QuantyDB Design Decisions

Short ADRs. Newest at the bottom. If a decision gets reversed, strike it
through and add a new entry, never silently edit history. (Fitting, for this
project.)

## ADR-001: Rust

Memory safety in a pager/B-tree is worth a lot, the ecosystem for this niche
is strong (criterion, proptest, cargo-fuzz), and a WASM build stays possible.
Go was the alternative; GC pauses and weaker control over layout decided it.

## ADR-002: Copy-on-write storage instead of update-in-place + WAL

The flagship features (time travel, branching, snapshots, lock-free readers)
are structural consequences of COW. With update-in-place they would each be a
separate subsystem. Cost: write amplification and a GC we must get right.
Accepted. Dual meta pages (LMDB style) give crash safety without a WAL for
v1; group commit mitigates fsync cost later.

## ADR-003: Single writer in v1

Concurrent writers are an optimization, not a correctness feature. SQLite
serves enormous workloads single-writer. Multi-writer MVCC waits until there
is a benchmark suite that can prove it helps and a crash harness that can
prove it is safe.

## ADR-004: Own query language first, SQL as a second front end

QQL is the native interface and the reason the typed ORM can be clean. But
both front ends lower to the same logical plan, so SQL support is a parser,
not a fork of the engine. SQLite compatibility means "pragmatic subset plus
a real importer", explicitly not bug-for-bug compatibility.

## ADR-005: Parse the SQLite file format ourselves for import

No dependency on the SQLite C library. The format is stable and documented,
parsing it is a bounded task, and it doubles as a great test of our reader
discipline (fuzzing hostile files).

## ADR-006: Own binary protocol for server mode, Postgres wire later

The native protocol stays small, versioned and debuggable. Postgres wire is
an adoption feature and will be built as a translation layer when the engine
deserves the traffic, not before.

## ADR-007: No mmap by default

pread + userspace page cache behind a Storage trait. mmap is a backend
option, not the foundation (SIGBUS on truncation/IO errors is miserable to
handle correctly, and the trait keeps WASM/memory backends possible).

## ADR-008: Dependency budget

quanty-core: crc32c, blake3, parking_lot, nothing else without an ADR.
Everything above core can use tokio/serde/etc. as needed. Reason: the core
must stay auditable, portable and fast to compile.

## ADR-009: Honesty rule for public claims

No benchmark numbers, compatibility claims or feature checkmarks in public
docs unless they are reproducible from the repo. The README describes built
things as built and planned things as planned.

## ADR-010: Space reclamation waits for retention (phase 3)

Resolved in phase 3. Deletes now merge underfull nodes with a neighbor when the pair fits one page, and mark-and-sweep GC reclaims space. See ADR-011 for how branch pointers are stored.

Phase 1 files only grow: replaced and deleted pages are never reused, and
deletes unlink emptied nodes but do not rebalance underfull ones.

This is deliberate. Reusing a page is only safe once no retained commit can
reference it, and the machinery that knows that (retention policy, commit
DAG, GC) is phase 3 work. Reuse without it would silently corrupt the time
travel and branching features the whole design exists for. Delete
rebalancing is bundled into the same phase because merging nodes is another
producer of dead pages.

Until then: correctness first, `VACUUM`-style compaction and the free list
land together with GC. The format already reserves the free list root in
the meta and the free list page type, so this changes nothing on disk.

## ADR-011: Branch pointers live outside the versioned trees

Branch heads and the current-branch pointer are stored in a small refs tree
whose root sits in the meta page, not under the catalog root. Versioning the
pointers by the commits they point at is circular: a commit would have to
contain its own head, and two branches would each carry a stale copy of the
other. Git keeps refs outside the object store for the same reason. The cost
is that a branch operation is a second small tree write; the benefit is that
history stays a clean immutable DAG and branch creation is O(1). A database
that never branches has no refs tree at all, so the feature is free until
used.

## ADR-012: No format migrations before 1.0

Pre-1.0, a file written by an older format version is rejected with a clear
message rather than migrated in place. Migration code is a lasting
maintenance burden and a class of subtle bugs, and it is not worth carrying
while the format still changes with most phases. The file format is still the
contract within a version: the same version reads and writes compatibly, and
the version number is bumped whenever the layout changes. This decision will
be revisited before any 1.0 release, at which point forward migration becomes
a supported feature.

## ADR-013: The MSRV claim covers the test suite, not just the build

`rust-version = "1.75"` said the project works on 1.75, but CI only ran
`cargo build --workspace` on that toolchain. Dev dependencies are compiled
for tests, not for plain builds, so the tempfile dependency tree was free to
drift onto newer language editions unnoticed; by the time this was caught,
cargo 1.75 could no longer even parse the getrandom manifest in the lock
file. The green check proved less than it appeared to, which collides with
ADR-009.

Decision: the MSRV claim means `cargo test --workspace` passes on 1.75, and
CI enforces exactly that, with `--locked` so the committed lock file is the
thing the badge certifies (cargo 1.75 has no MSRV-aware dependency
resolution). Consequence: dev dependencies now count against the MSRV. The
only one we had was tempfile, used for temp directories in eleven places;
that is a thirty line helper in tests/common now, and the workspace has zero
external dev dependencies. New ones are welcome when they are worth carrying
under this rule.

The crash, heavy and fuzz jobs stay on stable. They exist to catch storage
bugs, not toolchain drift, and one pinned job is enough for that.

## ADR-014: The SQL dialect borrows the engine's semantics

Phase 4 adds SQL as a second front end. The tempting promise is "sqlite
compatible"; the honest one is "sqlite flavored". A dialect that quietly
behaves almost like sqlite is a trap for exactly the queries where it
matters, so the line is drawn the other way around: the SQL parser lowers
onto the same AST as QQL, the engine's semantics apply unchanged, and every
place where SQL tradition would disagree is either refused at parse time or
documented in docs/SQL.md.

The concrete calls, all guarded by golden tests:

- Null comparisons. The engine's `= null` holds where SQL's `= NULL` never
  matches. Comparisons written against the NULL literal are parse errors
  pointing at IS NULL / IS NOT NULL, which lower onto the engine's
  null-safe operators and mean exactly what the SQL forms promise. IS as a
  general null-safe comparison works like sqlite's.
- Names match exactly, in the case they were written. sqlite matches
  case-insensitively; a case-insensitive catalog is a bigger change than a
  documented rule, and one exact spelling keeps diffs honest. Quoted names
  must still have identifier shape, because everything in the catalog has
  to render back into QQL, the canonical language; the fuzzer holds the two
  front ends to that with parse(pretty(lowered)) == lowered.
- Foreign keys parse and are not enforced, which is sqlite's own default.
  WITHOUT ROWID and STRICT parse and change nothing because they describe
  properties every table here has anyway.
- The lossy type mappings (NUMERIC on float, the date family on text)
  follow sqlite affinity in spirit and are spelled out in the docs instead
  of being discovered.

Unsupported SQL is refused with an error naming the missing piece, never
parsed into something subtly different. That includes joins and
transactions until their slices of this phase land.

## ADR-015: Joins live in the AST, and probes are only shortcuts

Phase 4's second slice adds joins. Two decisions shaped it.

Joins belong to the shared AST, not to SQL alone. The canonical language is
QQL and the fuzzer holds both front ends to parse(pretty(lowered)) ==
lowered, so anything the SQL front end can build must be expressible in QQL.
QQL therefore gets join syntax too: `get t join u on ...` and `left join`.
The `on` condition is a normal expression, and column references grew a
qualified form (`u.id`) that both languages parse the same way. There are
no table aliases yet, because `as` collides with `as of` and self-joins are
the only thing aliases would unlock right now; a table joined to itself is a
named error rather than a guess.

Joins are left-deep and evaluated in written order: the base table scans,
each `on` may reference the tables joined so far, `where` runs once over the
fully joined row, then ordering, limit and projection. `where` after the
join, with no predicate pushdown in this version, is the boring correct
default; pushing a filter below a join is an optimization that has to prove
it preserves left-join semantics, and that proof is not worth writing before
there is a benchmark asking for it.

The strategy layer is a pure accelerator. A join step may probe the right
table by primary key (`KeyProbe`) or by secondary index (`IndexProbe`)
instead of scanning it (`NestedLoop`), but the full `on` condition is still
evaluated on every candidate the probe returns. So a probe can only skip
rows that could not have matched; it can never add or drop a result row
compared to the nested loop. To keep that guarantee airtight, a probe is
only planned when the probe value's type cannot fail to coerce to the right
column's type (equal types, or int widening to float); every other case
falls back to the nested loop rather than risk a coercion error the scan
would not have hit. The join model test checks this the hard way: the same
data goes into three right-table shapes that force the three strategies, and
all three must return the same multiset as a brute-force reference join, on
thousands of randomized inputs.

## ADR-016: An open transaction is a replayed statement list, for now

Phase 4's third slice adds `begin` / `commit` / `rollback` across
statements. The obvious implementation is to hold a core `WriteTx` open in
the session for the transaction's lifetime. That does not typecheck, and
the reason is worth writing down: a `WriteTx` borrows the `Db`, so a
session holding both is self-referential, and `Db::gc` needs `&mut self`,
which an outstanding borrow would forbid. Working around that with interior
mutability or a self-referential crate would put unsafe or a dependency
underneath the most safety-critical code in the project, to save work in a
path nobody has benchmarked yet.

So the transaction is its statement list. An open transaction buffers the
mutating statements it accepted, in order, and its effect is defined as
that list applied to one write transaction at `commit`. To read or explain
mid-transaction, the list is replayed into a throwaway write transaction
that is then dropped, so a read sees exactly what `commit` would produce so
far and nothing sticks. `rollback` drops the list. Crash safety is
inherited rather than added: an open transaction has not touched the disk
at all, so a process killed with one open leaves the database exactly as it
was before `begin`, and `commit` is a single core commit and therefore
already atomic. The txn crash harness kills a thousand times inside open
transactions and demands whole transaction groups back, never a partial
one.

The cost is honest and quadratic: the n-th statement of a transaction
replays n-1 statements before it. That is fine for the transaction sizes
this thing has today and unacceptable for a bulk load inside one
transaction, so the replacement is already scoped: buffer a write set
(owned key/value overlay plus catalog overlay) instead of statements, read
through the overlay onto a base snapshot, and apply the overlay in one
write transaction at commit. That version needs the executor to run against
an overlay view rather than a `WriteTx` directly, which is a real change to
`Run`, and it should land with a benchmark that shows the replay hurting,
not before.

A mutation that fails inside a transaction is not buffered and does not
close the transaction: it is validated by the same replay that would run
it, so a rejected statement simply never joins the list. Branch and history
statements own their commits and are refused inside a transaction rather
than silently reordered around it.
