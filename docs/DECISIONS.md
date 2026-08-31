# QuantyDB Design Decisions

Short ADRs. Newest at the bottom. If a decision gets reversed, strike it
through and add a new entry, never silently edit history. (Fitting, for this
project.)

## Index

- [ADR-001](#adr-001-rust) Rust
- [ADR-002](#adr-002-copy-on-write-storage-instead-of-update-in-place--wal) Copy-on-write storage instead of update-in-place + WAL
- [ADR-003](#adr-003-single-writer-in-v1) Single writer in v1
- [ADR-004](#adr-004-own-query-language-first-sql-as-a-second-front-end) Own query language first, SQL as a second front end
- [ADR-005](#adr-005-parse-the-sqlite-file-format-ourselves-for-import) Parse the SQLite file format ourselves for import
- [ADR-006](#adr-006-own-binary-protocol-for-server-mode-postgres-wire-later) Own binary protocol for server mode, Postgres wire later
- [ADR-007](#adr-007-no-mmap-by-default) No mmap by default
- [ADR-008](#adr-008-dependency-budget) Dependency budget
- [ADR-009](#adr-009-honesty-rule-for-public-claims) Honesty rule for public claims
- [ADR-010](#adr-010-space-reclamation-waits-for-retention-phase-3) Space reclamation waits for retention (phase 3)
- [ADR-011](#adr-011-branch-pointers-live-outside-the-versioned-trees) Branch pointers live outside the versioned trees
- [ADR-012](#adr-012-no-format-migrations-before-10) No format migrations before 1.0
- [ADR-013](#adr-013-the-msrv-claim-covers-the-test-suite-not-just-the-build) The MSRV claim covers the test suite, not just the build
- [ADR-014](#adr-014-the-sql-dialect-borrows-the-engines-semantics) The SQL dialect borrows the engine's semantics
- [ADR-015](#adr-015-joins-live-in-the-ast-and-probes-are-only-shortcuts) Joins live in the AST, and probes are only shortcuts
- [ADR-016](#adr-016-an-open-transaction-is-a-replayed-statement-list-for-now) An open transaction is a replayed statement list, for now
- [ADR-017](#adr-017-not-and-and-or-cannot-be-names) `not`, `and` and `or` cannot be names
- [ADR-018](#adr-018-extensions-are-rust-code-linked-at-build-time) Extensions are Rust code linked at build time
- [ADR-019](#adr-019-the-declared-type-proposes-the-stored-data-decides) The declared type proposes, the stored data decides
- [ADR-020](#adr-020-the-core-has-no-dependencies-either) The core has no dependencies either
- [ADR-021](#adr-021-an-open-transaction-is-a-suspended-write-batch) An open transaction is a suspended write batch
- [ADR-022](#adr-022-the-server-has-no-dependencies-either) The server has no dependencies either
- [ADR-023](#adr-023-we-write-the-reactor-ourselves-so-unsafe-at-one-boundary) We write the reactor ourselves, so unsafe at one boundary
- [ADR-024](#adr-024-writes-queue-behind-one-writer-with-two-deadlines) Writes queue behind one writer, with two deadlines
- [ADR-025](#adr-025-epollexclusive-distributes-badly-so-so_reuseport-is-next) EPOLLEXCLUSIVE distributes badly, so SO_REUSEPORT is next
- [ADR-026](#adr-026-auth-tokens-live-outside-the-versioned-data) Auth tokens live outside the versioned data
- [ADR-027](#adr-027-one-executor-thread-owns-the-session-transactions-park) One executor thread owns the session, transactions park
- [ADR-028](#adr-028-group-commit-is-worth-building-and-not-for-the-fsync) Group commit is worth building, and not for the fsync
- [ADR-029](#adr-029-reads-go-past-an-open-transaction) Reads go past an open transaction

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

The number in this record is the one that was current when it was
written. The rule is what it decides, not the number: the MSRV is 1.89
since ADR-035, and CI runs `cargo test --workspace --locked` on whatever
it is.

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

*Superseded by ADR-021, which replaced the replay once the benchmark this
record asked for showed it hurting. The reasoning below is kept because it
is why the replacement looks the way it does.*

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

## ADR-017: `not`, `and` and `or` cannot be names

QQL was designed without reserved words: context decides, so a column may
be called `limit` or `order`. The parser fuzzer found the case where that
does not hold, after ten minutes of hunting in CI:

```
input:     del users where h=not*(score% 2 = 0)
canonical: del users where (h = (not * ((score % 2) = 0)))
```

Deep inside an expression, `not` is not in operator position, so the parser
reads it as a column name and the statement parses. The canonical form
parenthesizes the subexpression, which moves `not` to the front of an
expression, where it is the unary operator again. The canonical form no
longer parses, and the invariant that holds the two front ends together,
parse(pretty(ast)) == ast, is broken.

The word is contextually a keyword and contextually a name, and pretty
printing changes the context. That cannot be patched in the printer; the
name has to go. So `not`, `and` and `or` are refused as table and column
names, in both front ends, at the point where they are written rather than
left to misparse later. Everything else stays unreserved. In SQL the same
rule applies to quoted names, since quoting is what would otherwise smuggle
them past the reserved word list, and a quoted `"not"` still has to render
back into QQL. Case is significant: QQL keywords are lowercase, so `"NOT"`
remains a legal name.

The bug predates the transaction work and was never a wrong answer, only a
statement that could not be re-parsed from its own canonical form. It is
worth recording anyway, because it is the fuzz invariant earning its keep:
the property that looked like plumbing (a canonical form that survives a
round trip) is exactly what caught a real ambiguity in the grammar.

## ADR-018: Extensions are Rust code linked at build time

An outside developer asked about building on QuantyDB, which is the first
time the question has come from someone who is not us. It put an extension
API on the roadmap, and it immediately split into two things that share the
word plugin.

One is code that runs *inside* the database: a function the query language
can call, a hook that sees commits, a table backed by something other than
our own storage. That is an extension API. The other is an application that
*uses* the database, which needs the embedded crate if it is written in Rust
and the phase 5 protocol if it is not. No extension API substitutes for
either, and the two get conflated constantly, so this record starts by
separating them. The developer in question writes Java, so what he actually
needs is phase 5, and saying that plainly is more useful than handing him
something adjacent.

**Extensions are Rust, compiled into the binary that embeds Quanty**, and
registered on a builder before any statement runs. No `dlopen`, no C ABI, no
stable symbol level interface. Rust has no stable ABI, so a loadable object
interface means an `extern "C"` boundary plus unsafe code sitting directly
under the executor, which is the second most safety critical thing in the
project after the pager. ADR-016 already refused unsafe and a self
referential dependency to save work in a path nobody had benchmarked; this
is the same trade with a bigger blast radius. The price is real and worth
stating: an extension cannot be dropped next to a running binary, and
anyone who wants one has to rebuild. Keeping unsafe out of the query path
is worth that.

**Other languages get the wire protocol, not an ABI.** Java, Python and
everything else talk to the phase 5 server. A JNI bridge would need its own
memory safety story across a boundary we do not control, to reach a place
the protocol reaches anyway.

Two constraints on the surface are already fixed, before any of it is
designed, because both fall out of invariants that already exist.

QQL has no call syntax at all today: an expression is a literal, a column, a
unary operator or a binary operator. So a user defined function is a
language change before it is an API, touching `ast.rs`, the QQL parser,
`pretty.rs`, the SQL parser and both fuzz corpora, and
`parse(pretty(ast)) == ast` has to keep holding for names the engine did not
choose. ADR-017 is what that failure mode looks like when the names are
ours. With third party names it arrives with a second question attached, so
the rules for which names an extension may claim get decided together with
the syntax rather than after it.

ADR-015 says a probe is a pure accelerator, planned only where coercion
cannot fail. A call into extension code is not known to be total,
deterministic or side effect free, so the planner must refuse to build a
probe from any condition containing one, and must not reorder, skip or
repeat calls in ways a nested loop would not. Until that is enforced by a
model test in the shape of the join model test, an extension call has no
business in an `on` condition at all.

**Before 1.0 the extension surface is explicitly unstable.** Breaking
changes are allowed in minor releases and go in the changelog. Per ADR-009,
the honest position is that we do not yet have the users to know which
surface is the right one, and freezing the wrong one early is worse than
saying so.

Scheduling: phase 9. That is dependency order, not importance. It needs the
public embedded crate and the call syntax before it can start. The roadmap
allows pulling a later phase forward with a written reason, and this record
is that reason if a real extension user shows up before phase 8 is green.

## ADR-019: The declared type proposes, the stored data decides

SQLite is dynamically typed and we are not, so importing means answering a
question the source file does not answer for us: what type is this column.
The rule chosen here is that the declared type supplies the candidate, the
values that are actually stored settle it, and a disagreement is reported
rather than resolved in silence.

The goal it serves comes first, because it decides the ties: a developer
should be able to point us at an ordinary `.sqlite` file, run one command,
and get their data. Ninety-nine out of a hundred real databases, from
whatever codebase, should need nothing else. An import that stops is a
product failure, not a safety win, and every rule below was chosen with
that in mind.

**The declared type is not decoration, and it is not a guarantee either.**
A column declared `INTEGER` may hold text; SQLite's affinity only converts
where it can do so without losing anything. Two things follow, both
measured on chinook rather than assumed:

Reading the declaration alone gets it wrong. `DATETIME` has numeric
affinity and chinook stores text in every one of those columns. `NUMERIC(10,2)`
means integer-or-real, and the file holds reals. A reader that trusts the
declaration builds a number column for birth dates and falls over on the
first row.

Reading the bytes alone gets it wrong too, and worse. SQLite stores a
whole-numbered value in a column with real affinity as an integer, to save
space, and converts it back on the way out using that column's affinity. So
`1.0` and `2.5` in the same real column are physically an integer and a
float. Judging by storage class alone, nearly every real column in the
world looks like a mixed column, and nearly every import would stop. The
declared type is therefore not a hint but a necessity: it is the only thing
that says whether an integer on disk means an integer.

**So both are read.** Affinity is computed from the declared type by
SQLite's five rules, storage classes are collected from the rows, and the
column type follows from the two together.

**Mixed columns widen rather than stop.** Integer and real together become
`float`, since our float holds both, with one exception: an integer past
2^53 does not survive the trip and stops the import naming the row. Any
other mixture becomes `text`, or `bytes` once a blob is in it. This is the
rule the goal above bought: the alternative is refusing databases that
SQLite reads perfectly well, and a settings table with a `value` column
holding both numbers and strings is an ordinary thing to have, not a
corruption.

Widening has a real cost and it gets stated plainly rather than hidden: as
text, `10` sorts before `9`. That is why every import ends with a report
naming each column that was widened, what it held, and what it became, and
why `--strict` exists to turn the widening back into a refusal for anyone
who would rather fix the source. The promise is not that nothing lossy ever
happens. It is that nothing lossy happens quietly.

**A table always gets a key.** A rowid alias becomes the key. A composite
primary key becomes a composite key, which our catalog supports. Where
neither is available, or where the declared key cannot be one for us
because it holds NULL, the rowid becomes a key column of its own and the
original column is kept as ordinary data. That adds a column the source
schema did not have, which is a visible change and goes in the report, and
it is better than refusing the many real tables that have no primary key.

**Two passes, not one.** The first pass reads and decides, writing nothing;
the second writes. It costs a second read of the file, and it buys a report
of every problem at once, a minute in, instead of the first problem alone
after ten minutes of writing. It also means the schema is known before any
data is written, so nothing has to be migrated halfway through.

**What is skipped rather than refused**, each named in the report: views
and triggers, which hold no rows; SQLite's own `sqlite_` tables, which are
another engine's bookkeeping; virtual generated columns, which hold no data
in the file; and indexes we cannot express.

**Positions in a record are not column positions.** Three rules govern the
mapping, and each was verified against a file rather than recalled:

- a virtual generated column occupies no slot, while a stored one does, so
  zipping the declared columns against the stored values shifts every
  column after the first virtual one
- a rowid alias column is stored as NULL and its value is the cell's rowid
- a record may hold fewer values than the table has columns, which is what
  `alter table add column` leaves behind, and the missing trailing columns
  take their default from the declaration

**Types we do not invent.** `BOOLEAN` in SQLite is an integer column that
happens to hold 0 and 1, and may hold 2; it becomes `int`. `DATETIME` holds
whatever the application put there, usually text; it stays what it is. We
have no date type and this is not the place to pretend otherwise.

## ADR-020: The core has no dependencies either

ADR-008 gave quanty-core a budget of three: crc32c, blake3 and parking_lot.
Two of those were being used, and both are now written out instead, so the
workspace depends on nothing outside the standard library. This supersedes
the budget rather than bending it, so the reasoning belongs here.

The two were not the same job.

**parking_lot** was providing `Mutex` and `RwLock` at four call sites, with
no use of the parts that make it interesting: no `try_lock`, no upgradable
reads, no `Condvar`. What is left is what the standard library has had for
years, and since 1.62 its locks are futex based, which is where most of
parking_lot's original advantage came from. The one real difference is
poisoning, which std has and parking_lot does not, so the replacement had
to decide what to do about it. It takes the lock anyway, and `sync.rs` says
why in full: the state behind these locks is not where durability lives.
The pager's in-memory metadata is only replaced after a commit is already
on disk, and a write transaction that panics is dropped uncommitted, so a
panic cannot leave the guarded state disagreeing with the file. What can
leave the file inconsistent is a process dying mid-write, and the crash
harness covers that.

**crc32c** was a genuine trade rather than a formality, because modern x86
and aarch64 have a CRC-32C instruction and no table can match it. Keeping
the instruction would mean either unsafe intrinsics with runtime feature
detection or a dependency carrying them, and this project has already
refused unsafe for a smaller prize (ADR-016). So the question was what the
instruction is actually worth here, and it was measured rather than
guessed, both implementations side by side on one machine: 1.8 microseconds
per 4 KiB page for the slice-by-16 table against 1.0 for the instruction,
2.1 GiB/s against 3.9. Roughly twice as fast, and both far quicker than the
things they sit beside. A commit ends in an fsync costing hundreds of
microseconds. A page arriving in the cache came off a disk or through a
syscall first.

Under a microsecond a page is what this costs, and thirteen entries in the
lock file is what it buys back, since those two pulled in eleven more
between them. Each of those has its own release schedule, its own minimum
supported Rust version and its own security surface, in a project whose CI
runs the whole suite on a toolchain from 2023.

The correctness of the replacement is not a matter of confidence. While
crc32c was still present, the new implementation was checked against it
over twenty thousand inputs of every length up to nine kilobytes, and it
agreed on all of them, so existing database files stay readable: the
checksums are the same bytes as before. The published check value for
CRC-32C, 0xe3069283 over the nine ascii digits, is asserted separately, and
it pins the polynomial, the reflection and both conditioning steps at once.

What this does not change: everything above the core may still take
dependencies as it needs them, and the sqlite reader, the importer and the
command line tool happen to need none. If a benchmark ever shows the
checksum dominating something real, the answer is a measured intrinsic
behind a feature flag, not a quiet return to a dependency.

## ADR-021: An open transaction is a suspended write batch

ADR-016 chose a replay model for multi-statement transactions and named the
condition for replacing it: "it should land with a benchmark that shows the
replay hurting, not before." The benchmark now exists and it hurts. Loading
5000 rows as 50 batched inserts took 2.95 seconds inside one transaction
against 0.15 seconds as 50 separate ones, 294 times slower than sqlite
doing the same thing. A transaction was slower than no transaction, which
is backwards, and bulk loading is the main reason anyone opens one.

The cause was exactly the one ADR-016 predicted: every statement replayed
the whole buffer to validate against it, so N statements cost N squared
over two statement executions.

**An open transaction is now the write batch itself, suspended.** A batch
holds two kinds of thing: borrowed ones, the pager reference and the writer
lock, and owned ones, the base metadata and the pages written so far. Only
the borrowed half was ever the problem. `WriteBatch::suspend` puts the
borrows down and hands back the owned half, `Pager::resume` picks it up
again, and a session between statements holds nothing but data. So the
self-referential struct that ADR-016 could not typecheck is not needed, and
neither is unsafe or a dependency, which is what that record was protecting.

Two things had to be preserved rather than assumed.

**A rejected statement must leave nothing behind.** Under replay this was
free: a statement that failed simply never joined the list. Against a live
batch it is not, because a statement can write pages before it fails. So a
batch now takes a savepoint before each statement, recording the pre-image
of every page on first touch along with the roots and the allocation state,
and rolls back to it if the statement fails. One level is enough: a
statement joins the transaction whole or not at all. The golden suite
checks this directly, with a two-row insert whose second row is the wrong
type.

**A suspended batch must not build on a snapshot that has moved.** The
writer lock is released while suspended, so another writer could commit in
between. `resume` compares the base commit id against the current one and
refuses if they differ, telling the caller to roll back and retry.
Continuing would overwrite that commit with ours, which is the one outcome
worth failing loudly for.

The cost has moved rather than vanished, and the new shape is the better
one: an open transaction now holds its dirty pages in memory, so it costs
memory proportional to what it has written, where before it cost time
proportional to the square of how many statements it had run. Memory is
the resource a caller can see coming and bound by committing periodically.
`SuspendedTx::dirty_pages` reports it.

After the change, the same 5000 rows in one transaction take 129
milliseconds instead of 2950, and a transaction is now faster than no
transaction, as it should be. Against sqlite the gap on that workload is
12.7 times rather than 294, which is the next thing to look at rather than
the end of it.

## ADR-022: The server has no dependencies either

*The dependency decision below stands. Its conclusion that the server must
therefore be thread per connection was wrong and is superseded by ADR-023.*

Phase 5 was written as "binary protocol, tokio server". That line predates
ADR-020, which took the workspace to zero dependencies by writing out the
last two, and it was never reconciled. The README now advertises the zero,
so the question had to be answered before anything went into a Cargo.toml:
does the server get an exception, or does the claim stand as written.

**The claim stands as written. No tokio, no async runtime, no dependency
anywhere in the workspace.** The server is built on the standard library.

This is a product decision, not a technical one. ADR-008 always allowed
dependencies above the core, so tokio was permitted by the letter of the
rules; what it was not permitted by is the sentence on the front page. A
qualified claim ("the core has none, the server has some") is a weaker thing
to own than an unqualified one, and the unqualified one is only worth
keeping if it survives the first phase that makes it inconvenient. This is
that phase.

**The consequence is thread per connection**, because that is the only I/O
model the standard library has. `std::net` offers blocking reads and
`set_nonblocking`, but no readiness primitive: no epoll, no kqueue, no
`poll`. Getting one means either the `libc` crate, which is a dependency, or
declaring the syscalls `extern "C"` by hand, which is unsafe code sitting
under the connection handler and platform specific on top. ADR-016 refused
unsafe to save work in a path nobody had benchmarked and ADR-018 refused it
again at a wider blast radius; refusing it a third time here is consistent
rather than novel. So: one thread per connection, blocking reads, writes
serialized through a queue as ADR-003 already requires.

**The price, stated plainly, is the first acceptance criterion.** Phase 5
asks for 10k idle connections plus 1k active mixed QPS on a 2 vCPU box,
stable for 30 minutes. Thread per connection meets that or it does not, and
which one is a measurement nobody here has taken. What is known: 10k idle
threads is 10k kernel tasks, each blocked in `read`, which Linux handles far
better than the model's reputation suggests, and the stack cost is tunable
down from the 2 MiB default via `Builder::stack_size`. What is not known is
what 1k QPS of scheduling churn costs on two cores, and there are two limits
outside the process that can refuse the number outright: the open file
limit and the process limit. Neither is a code problem and both belong in
the acceptance test rather than in an assumption here.

So the acceptance test is the decision procedure. It runs early in the
phase, not at the end, because it is the thing that can invalidate this
record. If it fails on the model rather than on tuning, the fork reopens
with numbers attached, which is the only form in which ADR-016 allows a
rewrite of something this central. Per ADR-009 the honest position today is
that this is expected to hold and has not been shown to.

Two things this record does not decide. The wire format lives in
docs/PROTOCOL.md and is independent of the runtime; it would look the same
under tokio. Where auth tokens are stored and how they are revoked is still
open, and the protocol reserves room for it rather than answering it.

## ADR-023: We write the reactor ourselves, so unsafe at one boundary

ADR-022 answered "no tokio" and then drew a conclusion that does not
follow. It reasoned that the standard library has no readiness primitive,
that obtaining one means either the `libc` crate or hand-written `extern
"C"` declarations, that ADR-016 and ADR-018 both refused unsafe, and that
refusing it a third time was therefore consistent. **The third refusal was
not consistent, and this record supersedes that part of ADR-022.**

What the earlier two refused was unsafe as a *shortcut*: ADR-016 wanted a
self-referential struct to avoid restructuring a transaction, ADR-018 the
same at wider scope. In both, safe Rust could do the job and unsafe only
did it with less work, and in both the blast radius was the write path,
where a mistake is silent data corruption discovered later. Neither
property holds here. There is no safe alternative that produces the
capability at all, and the blast radius is a socket poll: the failure mode
is a connection that hangs or an fd handled twice, found by the first test
that opens two connections, not a page written wrong in a file somebody
trusts.

There is also a trust argument the earlier record missed. This workspace
already depends on unsafe for every read and every fsync; it is std's
unsafe, wrapped and audited. For files std supplies the wrapper. For
`epoll` it does not. So the question was never whether to accept unsafe at
the OS boundary, only whether to accept a wrapper we wrote or go without
the capability.

**We write it.** A reactor, a small one, with the syscalls declared by hand
and every use above the boundary in safe Rust.

The surface is fixed and short: `epoll_create1`, `epoll_ctl`, `epoll_wait`,
`eventfd` and `eventfd_write` for cross-thread wakeups, and `setsockopt` for
`SO_REUSEPORT`. Six functions and one struct layout. Socket lifetime stays
with std, which owns the descriptors through `TcpStream` and `OwnedFd`, so
we never construct or close an fd by hand and the one class of bug that
would be ours to make is confined to registration.

**The shape, and why it is less overhead than the thing we are not using.**
N worker threads, each with its own epoll instance and its own listening
socket via `SO_REUSEPORT`, each owning a disjoint set of connections. The
kernel spreads accepts across the workers, so a connection is born on the
thread that will serve it and is never handed anywhere. Nothing is shared
between workers, so there is no work stealing, no cross-thread wakeup on the
common path and no synchronization to pay for. Each connection is a small
hand-written state machine, reading a header, reading a body, executing,
writing a reply, with a partial-write buffer for the case where the socket
takes less than we offered. That is what a general async runtime builds
generically with wakers, pinned futures and dynamic dispatch, and what we
need is specific enough to write directly. Timers ride on the `epoll_wait`
timeout against a per-worker deadline heap, which is all phase 5 needs and
costs one comparison per loop.

Level-triggered first. Edge-triggered is faster and turns every missed
drain into a hang rather than a slow path, and this project does not adopt
the faster shape before a benchmark asks for it. That is ADR-016's rule and
it applies to us here.

**The price, and it is real.** `epoll` is Linux. The portable fallback is
thread per connection, which is what ADR-022 proposed as the whole design
and which survives here as the second implementation. That is not a
consolation prize: two implementations behind one interface is a
differential test, the same instrument the sqlite oracle tests already use,
and the fallback is the one whose correctness is obvious. Every protocol
level test runs against both, and a divergence is a bug in the reactor by
definition. macOS keeps working without a second hand-written binding, and
kqueue waits until somebody needs it in production rather than in principle.

The other price is that the acceptance criterion is now the reactor's to
meet rather than the thread pool's, and it is still unmeasured. The
measurement moves earlier, not later: the 10k idle plus 1k QPS test lands
with the first working listener, before the connection state machine grows
anything worth optimizing, because it is the number that decides whether
this record or its fallback is the design.

*Measured on 2026-08-21, and this record stands.* Two cores of a Ryzen 5
5600G held ten thousand idle connections and answered 1800064 mixed
statements at a thousand a second for thirty minutes, none of them failed,
with descriptors and resident memory flat throughout. Latency was 123
microseconds mean and 7.6 milliseconds worst. The fallback in ADR-022 stays
superseded, and it stays superseded for a reason that is now a number
rather than an argument. The run is written down in docs/ACCEPTANCE.md
with the machine attached.

## ADR-024: Writes queue behind one writer, with two deadlines

ADR-003 gives the database one writer and ADR-021 makes an open transaction
a suspended write batch. Neither says what a server does when a second
connection wants to write while the first is mid-transaction, and with a
reactor the naive answer is unavailable: a worker thread cannot block, it
has other connections on it.

**Writes go to a queue served by one writer thread. A connection waiting on
that queue is parked, not blocked, and its worker moves on.** The reply is
sent when the write completes, which is what the "one request in flight"
rule in docs/PROTOCOL.md already lets us do: the client is not expecting
anything else on that connection meanwhile.

**Group commit falls out of this rather than being added to it.** The writer
thread drains everything queued, applies it in one write batch and fsyncs
once, then answers all of them. Under load the queue is deeper and the
fsync is amortized over more statements, which is the behaviour worth
having: the system gets more efficient exactly when it is busiest. The
1k QPS in the acceptance criterion is a claim about that amortization more
than about anything else.

**Two deadlines, because there are two different waits and treating them
alike gets one of them wrong.**

The first is waiting for the writer. An autocommit statement queued behind
other autocommit statements waits milliseconds, and waiting is right. It
gets a generous deadline; if it expires, error `0x0007` and the client may
retry. This is SQLite's `busy_timeout` and it is the correct model for a
queue that drains on its own.

The second is different in kind and is the one that actually bites in
production: a connection that has run `begin`, holds the suspended batch,
and then goes quiet. Nothing drains. A human left a session open, or a
client crashed without closing its socket, and every writer behind it waits
on something that may never come. So: **an idle-in-transaction deadline**.
A connection holding an open transaction that sends nothing for the
configured interval has it rolled back and is told so. Postgres carries
`idle_in_transaction_session_timeout` for exactly this reason and it exists
because the failure is common, not because it is clever. ADR-021 makes
rolling back cheap for us, since a suspended batch has touched no disk.

Two consequences worth stating rather than discovering.

Ordering between connections is not promised. The writer applies in queue
order, so two clients writing on two connections have whatever order the
queue gave them. A client that needs an order has one connection or one
transaction. This was already true and is now written down.

The queue is a fairness surface. FIFO is the starting point because it is
the one whose behaviour is predictable under overload; anything cleverer
waits for a benchmark showing FIFO hurting, per ADR-016.

## ADR-025: EPOLLEXCLUSIVE distributes badly, so SO_REUSEPORT is next

ADR-023 named `SO_REUSEPORT` for spreading accepts across workers. It was
built with `EPOLLEXCLUSIVE` instead: every worker watches one shared
listener and the kernel wakes exactly one of them per connection. That
costs no syscalls we did not already have, where `SO_REUSEPORT` needs
`socket`, `setsockopt`, `bind` and `listen` written by hand along with a
`sockaddr`, and ADR-016 says not to buy the harder shape before something
asks for it.

Something asked. Five hundred connections against three workers landed
354 / 82 / 64. The kernel wakes the first waiter on the queue and that is
usually the same one, so a shared listener does not balance, it favours.

This matters because of what the acceptance criterion measures. Ten
thousand connections spread 70/20/10 across workers on two cores is not a
two core test; it is a one core test with two idle helpers, and it would
fail for a reason that has nothing to do with the reactor.

So `SO_REUSEPORT` is now justified by a measurement rather than by
preference. It landed in the same session: five hundred connections across
the same three workers went 155 / 173 / 172, because the kernel hashes the
four-tuple instead of waking whoever was first in the queue.

The price is four more functions at the boundary ADR-023 opened, `socket`,
`setsockopt`, `bind` and `listen`, plus a `sockaddr` encoded by hand. That
takes it from six to ten. The descriptor still goes straight to
`TcpListener::from_raw_fd`, so ownership and closing stay with the standard
library and the new code is a bind sequence and nothing else.

`EPOLLEXCLUSIVE` stays for the case of a listener genuinely shared, which
the differential test against the thread fallback will want.

## ADR-026: Auth tokens live outside the versioned data

The protocol carries an opaque token and docs/PROTOCOL.md deliberately does
not say where it is kept. Phase 5 has to answer that, and the branching and
time travel this database is built on rule out the obvious answer.

**Credentials are not versioned.** Put token hashes in an ordinary table and
"revoked" becomes true only at the tip of one branch. ADR-005 gives every
reader `as of <commit>`, so a revoked hash stays readable in history, and
ADR-009 gives anyone a branch from before the revocation where the token
still works. Those two features are the point of the product, so the
credential store is what has to move.

**So tokens live in a file beside the database, not inside it.** One line
per token: a hash and a label, ASCII, in the style of `authorized_keys`.
The server reads it at startup and again when its mtime changes. Revoking a
token is deleting a line, it takes effect without a running server, and it
cannot be undone by switching branches or reading history.

The cost is a second place where durability matters, which ADR-002 would
otherwise argue against. It is worth it here because the failure modes are
the ones we want: a truncated or unreadable token file means no client
authenticates, which is the safe direction, and it means no write path from
client statements into the credential store at all.

**The token is hashed, not encrypted, and not run through a slow KDF.** A
token is generated with full entropy rather than chosen by a human, so
there is no guessing attack for a work factor to slow down. ADR-020 keeps
dependencies out of the workspace, so the hash is one we write against
published test vectors, and that is a slice of its own.

**Built.** `quanty serve --tokens <file>` requires a token; without the
flag nothing changes for anyone. `quanty token <label>` mints one and
prints it once, together with the line to append. The file is looked at
again once a second, so deleting a line shuts the door on a running
server. A file that has become unreadable or malformed leaves the last
good set in force: falling open is worse, and falling over is worse still.

The file's own permissions are part of the credential. A token file that
anyone can write to is a way in rather than a store, since anyone can add
their own line, so it is refused outright and refused again on every
reload. A file that anyone can read only earns a note at startup: it holds
hashes of full entropy tokens, which are not worth reading, but it is
rarely what someone meant.

Two things it does not promise, written down rather than discovered. The
check happens when `Auth` arrives, so revoking a token shuts out new
connections and does not cut off one that is already talking. And a
refused token is not a ban: the connection stays open and may try again,
because the alternative is a server that a typo can lock you out of.

SHA-256 is written out in `quanty-auth` because ADR-020 keeps dependencies
out. It is checked against the published vectors, the million byte one
included, and against a separate implementation at every length around the
block and padding boundaries, which is the only way to cover inputs the
standard does not publish.

**Without a token file the server requires no authentication.** `Auth` is
answered with `Ready` without the token being looked at, which is exactly
the "a server that does not require it" case docs/PROTOCOL.md already
describes. That is a real configuration and not a placeholder, but it is
the reason `quanty serve` belongs on a loopback address until this is done.

## ADR-027: One executor thread owns the session, transactions park

ADR-024 puts a queue in front of one writer. What it does not say is how
the engine is held, and the code answers that more narrowly than expected.

**A shared database was tried and rejected.** The natural shape is
`Arc<Db>` with a session per connection. `Db::gc` takes `&mut self` on
purpose: outstanding snapshots borrow the database, so the borrow checker
proves reader quiescence before a page is reused. Behind an `Arc` that
proof is unavailable and `gc` becomes unreachable, and `gc` is a statement
in the language. The compile-time proof is worth more than the shape.

**So there is one session, and the per-connection part of it parks.** A
session is a database plus at most one open transaction, and only the
second half belongs to a connection. ADR-021 already made a suspended
transaction a value that has touched no disk, so moving it in and out
around each statement costs nothing and dropping it is a rollback. The
executor thread owns the session and a table of parked transactions.

**Everything serializes, reads included, and that is the price.** (The
stall this describes is removed by ADR-029; the serialization is not.) A
connection holding an open transaction makes every other connection wait,
not only the writers ADR-024 was thinking about. This is measured, not
assumed: a read issued while another connection has a transaction open does
not return until that transaction closes. Two consequences follow.

The idle-in-transaction deadline is short, ten seconds rather than the off
by default Postgres can afford, because here one forgotten `begin` is a
server-wide stall rather than one blocked writer.

And the thing to measure next is named: whether a read can bypass the queue
and run against a snapshot while a transaction is parked. It is safe in
principle, since a snapshot commits nothing and cannot invalidate the
parked batch, but it needs the executor to tell a read from a write from
the plan rather than from the text, and ADR-016 wants the number first.

**Group commit is deferred for the same reason.** The shape ADR-024 asked
for is here, one thread draining a queue, and batching several statements
into one write and one fsync is a change to one function. It is also an
optimization, and there is no measurement of what fsync costs here yet.

## ADR-028: Group commit is worth building, and not for the fsync

ADR-024 deferred group commit and ADR-027 deferred it again, both times for
the same reason: ADR-016 wants the number first. `quanty-commit-cost` is
that measurement. Batching k statements into one transaction is the same
arithmetic as group commit at queue depth k, so the curve over k is the
ceiling. On the development container, one core, overlay filesystem:

```
fsync                          155 us
batch     per statement    statements/s
    1          291.6 us            3429
    2          152.7 us            6549
    8           67.1 us           14902
   64           28.1 us           35648
  512           21.6 us           46355
```

**Thirteen times, so it gets built.** Per statement falls from 292 us to
22 us, and most of the win is already there at a depth of 64, which is a
queue depth an ordinary load reaches.

**But the fsync is only half of what is being amortized.** The per commit
overhead is about 270 us and the fsync is 155 us of it. The remaining
115 us is the rest of the commit path, the copy on write page copying and
the meta and commit records. ADR-024 described group commit as amortizing
the fsync; that is true and it is not the whole reason. On a machine where
fsync is cheaper than it is here, or on one where this container is lying
about reaching a disk, there is still a commit path to amortize, so the
conclusion does not depend on the number the container is least trusted to
report.

**The write amplification is the finding nobody asked for.** Two thousand
statements at depth one leave 23 MB on disk, twelve kilobytes each, because
every commit copies the path from root to leaf. Eight thousand at depth 512
leave under a megabyte, a hundred bytes each. That is a hundredfold, and it
lands on the disk and on garbage collection rather than on latency, which
is why the throughput number alone would have understated this.

**The machinery it needs is free.** Statements batched together must fail
independently, so each needs a savepoint around it, and a savepoint that
cost what it replaced would sink the idea. Measured below the statement
layer, twenty thousand writes in one transaction cost 17.61 us each plain
and 18.02 us each with a savepoint around every one. Two percent, against
270 us saved per commit.

The numbers above describe this container and are the shape of the answer,
not its size. The acceptance machine gets its own run.

**Built, and measured again from outside.** A write load through the
server, thirty two active connections on the same core as the load
generator, answers 9357 statements per second. The in process baseline in
the table above, one commit per statement and no sockets at all, is 3429.
Doing strictly more work per statement and still going two and a half times
faster is the batching, since nothing else changed.

Two limits the implementation puts on itself. A statement that manages its
own commit, `begin`, `commit` and the branch statements, runs alone,
because inside a transaction they either refuse or mean something else. And
nobody in a batch is answered until the commit succeeds: a read batched
with a write may have seen that write, so reporting its rows before the
shared commit is durable would promise something the server could still
take back.

## ADR-029: Reads go past an open transaction

Partially supersedes ADR-027, which recorded that every statement waits
behind an open transaction and named this as the thing to fix.

**A read cannot hurt a parked transaction.** ADR-021 suspends a write batch
by releasing the writer lock and keeping the pending pages in memory, and
resuming fails only if another writer has committed since. A read commits
nothing, so it cannot be that writer. It reads the committed head, which is
what it would have read had it waited, so nothing about the answer changes
either.

**Making it wait was the expensive half of ADR-027.** One connection that
runs `begin` and stops talking stalled every other connection on the
server, reads included, until the idle-in-transaction deadline expired.
That is why the deadline was set to ten seconds. With reads going past it
bounds only how long writers wait, so it goes back to thirty.

**This is not the throughput question, and that one stays open.** Nothing
here runs in parallel: reads still cross the one executor thread, one at a
time, in arrival order. What was removed is a stall, not a serialization.
Whether readers should run on their own threads against shared snapshots is
still the open question ADR-027 named, it is still an optimization, and
ADR-016 still wants a measurement that a single core cannot produce.

**The bookkeeping this needs is easy to get wrong.** A statement that ends
without an open transaction used to mean nobody holds one. That stops being
true when a read from another connection runs while a transaction is
parked: recording it the naive way clears the holder, lets the waiting
writers go, and the first of them to commit kills the batch that was parked.
The holder only moves for the connection whose transaction it is, and there
is a test that fails without that.

**Checked together, not only apart.** Parking, batching, both deadlines
and this bypass each have a test of their own, and none of them reaches the
state space they share. `crates/quanty-service/tests/soak.rs` runs six
connections doing arbitrary things for a budget and checks four invariants
that hold whichever way a race went: every request gets exactly one
answer, the answer fits the question, waiting past the deadline is answered
rather than hung, and the rows that survive are exactly the rows that were
promised. It also asserts that it reached the contended paths at all,
because a soak that quietly never contends would report success.

**Conservative about what counts as a read.** `get`, `show tables` and
`explain` only. `log` and `show branches` write nothing either, but they
refuse to run inside a transaction, so they stay where the engine already
puts them rather than being reclassified from outside it.

## ADR-030: The embedded crate owns the database and speaks statements

README draws three doors into the product and only two of them exist.
`quanty/` is the left one: what a Rust application adds to its
`Cargo.toml`. The engine under it is finished, so this record is not
about a capability. It is about choosing a surface narrow enough to
keep, and naming what it does not cover.

**Reader quiescence needs no special handling, which was unexpected.**
ADR-027 keeps `gc` behind `&mut Db` so the borrow checker proves no
snapshot is outstanding before a page is reused, and calls that proof
worth more than an `Arc`. Reading the code rather than the record: no
caller ever holds `&mut Db`. `gc` is a statement, `Session::execute`
takes `&mut self`, and `Session` owns the database and lends out only
`&Db`. The proof therefore belongs to `execute`, not to `gc`, and any
surface that owns the database and routes mutation through `&mut self`
inherits it for free. `Database::gc` is a thin wrapper over a statement,
not a hole to be defended.

**The public type is not generic.** `Session<S: Storage>` is the shape
underneath, and lifting that parameter into the public surface would put
`S` in every signature an embedder writes and make `Storage` itself a
promise. `Database` is concrete; the two backends live in a private enum
matched once per call. A backend of one's own is exactly what ADR-018
puts behind the extension API after 1.0, so refusing it here costs
nothing that was on offer.

**Statements arrive as text.** The typed alternative means restating the
query language in Rust types, and every one of those types is a promise
made before anyone has written against it. Text is already fuzzed, has
golden files, and is what the server speaks, so the embedded and remote
doors describe work the same way. What is typed is what comes back:
`Value`, column names and rows, because parsing our own output would be
absurd. The typed front end is `quanty-derive`, it follows later, and it
will be built on this rather than beside it.

**A transaction is a borrow, not an object.** `Database::transaction`
takes a closure and hands it `&mut Transaction`, which borrows the
database mutably: it cannot outlive it, cannot be held across a `gc`,
and cannot be forgotten, because the closure returning decides commit or
rollback. `SuspendedTx` and `park` stay internal. They exist so one
executor thread can multiplex connections (ADR-027) and an embedder with
its own database has nothing to multiplex.

**0.4 promises the surface and nothing under it.** ADR-018 declares the
extension surface unstable before 1.0. This one makes the opposite
promise, because a surface that promises nothing gives no reason to
prefer it over depending on `quanty-exec` directly, which is the state
this crate exists to end. Every item `quanty` exports is semver-stable
from 0.4: breaking it needs a minor bump and a line in the annotated tag
for that release, which is where this project keeps release notes. A
CHANGELOG belongs with publishing to crates.io and neither exists yet. Not
covered, and said plainly in the crate docs: nothing from `quanty-core`
or `quanty-exec` is re-exported, so no internal type leaks into an
embedder's signatures; `Value` and `Outcome` are non-exhaustive and may
grow variants; the file format has its own version and its own rules.

**The borrow is proved by a doctest that is weaker than it looks.**
`compile_fail` in rustdoc passes when the snippet fails to build for any
reason at all; the error code after it is documentation, not a check,
which was confirmed by asserting the wrong code and watching it pass.
The snippet is therefore kept minimal, and E0499 was confirmed by hand
once. A stricter check needs a compile-test harness and nobody is
waiting on one.

**The price.** Statements are strings, so a typo is a runtime error
until the derive lands. Custom storage is unreachable. Suspended
transactions are unreachable. Additions cost more than they would to an
unstable surface, which is the point of keeping it small.

## ADR-031: The derive maps a struct to a row, and writes go through the AST

ADR-020 has been asked four times now and the answer has not moved: no
`syn`, no `quote`. A proc macro is handed a `TokenStream` and can walk it,
and a derive over a struct of named fields is a narrow enough shape to
walk by hand. This records what the derive covers and what it refuses.

**Generated writes build a statement, they do not print one.** QQL has no
parameters, and `render_value` renders text unquoted because it exists to
display a value rather than to embed one. A derive that pasted field
values into `put users { name: "..." }` would therefore be an injection
surface generated at compile time, which is a worse thing to ship than no
derive at all. `quanty-import` already writes through
`Session::execute_ast` with `Expr::Literal` holding the value, and the
derive uses the same road. The AST stays out of the public surface;
`Database::insert` takes the row and builds it internally.

**The macro emits as little as possible.** Matching a query's column
names against a struct's fields is the part that is easy to get wrong, so
it is written once, by hand, in `Rows::into_typed`, which resolves each
column to a position and hands the derive a `Vec<Value>` already in
order. What the macro generates is positional and dull: a table name, a
list of column names, one `from_value` per field, and the reverse. The
plumbing those calls need lives in a `#[doc(hidden)]` module, which is
how the stability promise of ADR-030 stays a promise about the surface
people write against rather than about everything that is reachable.

**The table name is the struct name in snake case**, overridable with
`#[quanty(table = "...")]`, and a field maps to a column of its own name,
overridable with `#[quanty(column = "...")]`. Nothing is pluralised. A
`User` maps to `user`, not to `users`, because a macro that guesses
English plurals is wrong often enough to be worse than typing the
attribute.

**Refused, with a compile error rather than a surprise:** tuple structs,
unit structs, enums, generics and lifetimes. Each of them has a sensible
meaning that would have to be chosen, and choosing it here without anyone
asking is how a surface grows things nobody wanted.

**The price.** `FromValue` is implemented for `i64`, `i32`, `u32`, `f64`,
`bool`, `String`, `Vec<u8>` and `Option<T>`, and for nothing else: a
narrow integer conversion is range checked and fails rather than wrapping,
and every type outside that list needs a hand written impl. There is no
`where` clause and no generic row. The macro's own error messages point
at the struct rather than at the offending token, because carrying spans
by hand through a hand rolled parser is real work and no user has asked
for it yet. `to_values` clones each field, so every field has to be
`Clone`; taking the row apart by value would be cheaper and would stop
the caller keeping it. And the snake case conversion splits on every
capital, so an acronym comes out as `h_t_t_p_header`, which is what
`#[quanty(table = "...")]` is for.

## ADR-032: Branch verbs on the tool, and no `--branch` on `run`

Phase 3 finished branching two phases ago, and it has been reachable ever
since as `quanty run db.qdb "branch x"`. README drew `quanty branch` and
`quanty merge` from the start and no phase ever built them, which is the
same kind of gap ADR-030 closed on the library side, one shell wide.

**The verbs build a statement, they do not print one.** `quanty tables`
set the precedent by passing the text `show tables` to the parser, and
five more of those would be five more places where a name gets glued into
a string. `branch`, `branches`, `switch`, `merge` and `log` construct the
AST and call `execute_ast`, the same road the derive takes for the same
reason (ADR-031). Nothing is quoted, and a bad branch name meets
`refs::validate_name` and its actual message rather than a parse error
about an unexpected token.

**`--sql` does not reach them.** These statements belong to the tool, not
to the user, so the flags the user typed are not the flags they run under.
Reading `branch x` as SQL could only ever be a mistake.

**Deleting a branch stays in `run`.** Every verb this tool has is one
word, and `drop branch` is two. Inventing `drop-branch`, or overloading
`drop` so that it means a branch here and a table in QQL, buys one saved
word on a rare and destructive operation. `quanty run db.qdb "drop branch
x"` says what it does.

**`--branch` is refused rather than faked.** README sketched running a
statement against a branch without moving to it. There is no engine
support for that: a write always lands on the current branch, and
`switch_branch` writes the refs tree, so the flag would have to switch,
run, and switch back. That is three commits where the user asked for one,
it is visible to every other reader and to a server holding the same
file, and a kill between the first and the third leaves the database on a
branch nobody chose. The flag names itself in the error so that someone
who read the old README learns why rather than learning that it is
unknown. Doing it properly means a per-session branch in the engine,
which is a real feature and belongs to whoever needs it.

## ADR-033: Blobs are content addressed chunks in the catalog tree

Phase 6 asks for three things: a gigabyte in and out at constant memory,
the same file twice costing space once, and a blob collector that leaves
nothing dangling. Reading the pager first changed what the first one
means.

**A write batch holds every dirty page in memory until it commits.**
`WriteBatch::dirty` is a `BTreeMap<PageId, Box<[u8]>>`, so a gigabyte
written inside one transaction is a gigabyte of resident memory, and no
API shape hides that. **A blob write is therefore many commits, not one.**
Chunks go in first, in commits of their own, and the row that points at
them lands last. A kill anywhere before that last commit leaves chunks
that nothing references, which is garbage rather than corruption, and is
exactly what the collector below is for. The alternative, spilling a
batch to disk, is a change to the commit protocol and would be paid for by
every write in the database to help the rarest one.

**Chunks live in the catalog tree under a reserved prefix.** Catalog keys
are already typed tuples, `("table", name)` and `("seq")`, so `("blob",
hash)` sits beside them and needs no new root and no format bump. Two
things fall out for free: chunk payloads larger than a page take the
overflow chain the B-tree already has, and chunks are versioned per commit
like everything else, so a snapshot of an old commit sees the chunks that
commit could see. `PageType::Blob` stays the unused slot it has always
been.

**Reachability is not enough, so chunks are counted.** The existing
collector frees pages nothing points at, and a chunk nobody references is
still an entry in a live tree, so it would sit there forever. Each chunk
carries a reference count: writing one that is already there stores
nothing, the descriptor commit raises the count, and dropping a descriptor
lowers it and deletes the entry at zero. Branches need no special case:
each lineage has its own catalog tree, so it has its own counts.

**Collecting what a dead run left behind is unresolved, and the obvious
answer is wrong.** A chunk with a count of zero looks collectible, and
sweeping those was the first plan written here. It races: a blob write
spans commits by the paragraph above, so between the commit that stores a
chunk and the commit that names it the count is legitimately zero, and a
sweep in that window deletes data a descriptor is about to point at. The
alternatives each cost something real. A reachability pass over every
descriptor is exact, is O(rows), and needs the schema, so it does not
belong in the storage layer. An uploader that holds its own reference
moves the problem to whoever dies holding one. The counts land now
because they are needed whichever way that goes and are testable on
their own; the sweep does not, and orphaned chunks occupy space until
somebody decides which it is.

**Resolved: the sweep walks rows, and two other changes took the race
away.** `gc blobs` collects every chunk named by an asset column in the
current head and deletes the rest. It is the reachability pass this
record called exact and O(rows) and put outside the storage layer, and
that is where it went: `WriteTx::sweep_chunks` takes the reachable set
and knows nothing about what a column means, while the executor, which
does, builds it.

The race is gone for two reasons that were not true when the paragraph
above was written. Rows hold references now (ADR-034), so a chunk no
committed row names is garbage by definition rather than possibly
in-flight. And two handles can no longer interleave their commits at all:
the guard from ADR-035 fails one of them, so a sweep cannot slide between
the commit that stores a chunk and the commit that names it. A caller
holding a descriptor it has not stored is holding uncommitted state, and
finds out at its next `retain_chunk`, which refuses rather than storing a
row that points at nothing. That was run, not reasoned about.

Only the current head is walked. Older commits keep their own catalog
tree, so a swept chunk is still readable through `as of`, and the cost
stays linear in the rows that exist now rather than in history.

**SHA-256, not BLAKE3.** ARCHITECTURE named BLAKE3 back when the
dependency question was still open. There is a hand written SHA-256 in
this repository already, checked against the published vectors, and a
second hash function is a second thing to get right. It moves from
`quanty-auth` into `quanty-core`, which is where a trust anchor belongs,
and auth depends on core for it rather than keeping a copy: two
implementations of a hash that drift are two different databases.

**Fixed chunks of 1 MiB, not content defined.** The acceptance criterion
is that the same file twice costs space once, and fixed chunks give that.
Rolling hash chunking would also dedup a file with a byte inserted at the
front, which fixed chunks do not, and that is the price. It is a change
of one function later, not of the format, because the descriptor lists
hashes either way.

**The count lives at its own key, which the acceptance test taught.** The
first layout here put the count in front of the bytes, on the theory that
a small prefix is cheap to change. A B-tree replaces a value whole, so
every change to a count copied the payload's entire overflow chain:
storing a 200 kB chunk a second time cost 52 pages, which is the chunk
again, and the dedup criterion failed on its first run. The bytes now sit
at `("blob", hash)`, written once and never rewritten, and the count at
`("blobrefs", hash)`, eight bytes on its own. The second copy costs two
pages.

**Measured.** SHA-256 runs at 231 MiB/s here and `write_blob` at 222, so
the write path is hash bound rather than disk bound, as expected. A
gigabyte goes in in about 11 seconds and comes back in about 2, holding
16 MiB resident either way. Committing every 400 chunks instead of every
8 takes that to 417 MiB, which is the paragraph at the top of this record
in numbers.

**The price.** SHA-256 is slower per byte than BLAKE3 and is on the write
path of every chunk. A blob write is not
atomic with the row that names it, so a torn write leaves collectible
chunks and a reader never sees a half blob. Shifted content does not
dedup. And the reference count is a write, so storing a chunk that
already exists still costs a commit.

## ADR-034: A blob is a column type, not a value that appears everywhere

ARCHITECTURE says a value over a threshold leaves the row and becomes a
blob, which reads as though the spill should be invisible: put a hundred
megabytes in a `bytes` column and the engine quietly chunks it. That is
one of two designs and it is the expensive one.

**What the invisible version costs, counted rather than guessed.** A
descriptor has to be told apart from the bytes it stands for, so `Value`
gains a variant. There are twenty exhaustive matches on `Value` across
seven crates: the wire protocol, both parsers, the pretty printer, the
importer, the public crate. Every one of them would need an answer to a
question nobody has asked, starting with what a blob looks like on the
wire and what `quanty run` prints for one. It also throws away the
streaming from ADR-033: if a blob is a value, reading the row means
reassembling it, and a gigabyte column is a gigabyte in memory on every
`get`.

**A column knows its own type.** The catalog already carries one per
column, so a column declared `blob` stores a descriptor and every other
column does not, with no tag, no new variant, no format bump, and no
question about what the wire does with something it has never seen. The
descriptor travels as the bytes it already is, and the schema is what
says how to read them. Rows stay small whatever the payload weighs, and
the payload is reached with `read_blob`, which is the API that already
holds one chunk at a time.

**The price, and it is a real one.** It is not transparent. Storing a
large value means declaring the column `blob` and handing over a reader
rather than a `Vec<u8>`, so an existing `bytes` column does not become
cheap by growing. Two ways to hold bytes now exist and a user has to pick,
which is exactly the kind of choice a database is supposed to make for
people. The trade is that the choice is visible in the schema, where it
can be read, rather than being a threshold constant that silently
changes how a table behaves the day a value crosses it.

**ARCHITECTURE is wrong on this point and now says so.** The threshold
sentence predates the streaming API and the count of what a variant
costs.

**The type is called `asset`, because `blob` is already taken.** This
record said "a column declared `blob`" and the SQL front end has mapped
`BLOB` to `bytes` since the dialect existed, which is what SQLite means
by it and what the importer relies on. Declaring a QQL `blob` on top of
that would give one word two meanings inside one database: `CREATE TABLE
t (d BLOB)` a column of plain bytes, `table t { d: blob }` a chunked one,
with `show tables` printing whichever the front end happened to be. The
alternative, pointing SQL's `BLOB` at the chunked type, is worse: SQLite
blobs are usually small, and every imported column would be chunked for
nothing.

`asset` is not a new coinage. The roadmap phase has been called "Blobs +
assets" and its criterion has said "1 GiB asset" since before any of this
was built, so the vocabulary already exists and only the type name was
missing. SQL keeps `BLOB` meaning `bytes` and gains no way to declare a
chunked column, which costs nothing anyone has: SQL compatibility is
about running an existing application's queries, and no existing query
declares storage this database invented.

## ADR-035: One writer is a claim this file could not back

ARCHITECTURE said a single writer is "enforced with a file lock in
embedded mode and a mutex in server mode". Half of that was true. There
is no file lock anywhere in this workspace; the writer mutex lives in the
`Pager`, so it is per handle, and two handles on one file each believe
they are alone.

**What that did, run rather than reasoned about.** Two handles on one
file, A commits a row, B commits another:

```
B's commit returned Ok(1), the same txid A had just been given
key 1: gone
key 2: there
```

`commit` derives its txid from the meta it cached at `begin`, and writes
the meta into slot `txid % 2`. Both handles computed 1, both wrote slot
1, and the second replaced a commit that had already been acknowledged.
No error, no corruption a checksum would catch, just a row that was
promised and is not there.

**A guard, because a refusal beats a lie.** `commit` now reads the meta
slot it is about to overwrite before it writes anything. Commits
alternate slots, so any other writer's first commit lands in exactly that
slot, and every later one leaves something higher in one slot or the
other; one read catches all of it. A raced commit returns `WriterRaced`
and changes nothing, and the caller can reopen and try again.

**It was reported as costing 9%, and that number was noise.** Two
thousand durable commits measured 266us without the guard and 290 with,
one run each, and that difference drove a decision. Measured properly,
seven rounds of each in one process, a single configuration spreads from
302us to 493us: the variance is six times the effect. The median even
came out lower with the guard than without, which is impossible for pure
added work and is the tell.

What is true is that the guard is one page read on a path dominated by
an fsync, and that on this single core container it cannot be told apart
from the noise. What it costs belongs on the Ryzen, with the rest of the
measurements that need more than one core.

**The lock itself is not mine to decide.** `File::try_lock` needs a newer
MSRV than 1.75, which is this project's and has a CI job named after it.
A real advisory lock would make the guard's 9% unnecessary, since a lock
is paid once at open rather than once per commit.

The version was written here as "1.89 or later" from memory, then
"measured" as 1.85, then measured properly as 1.89. The middle step is
worth keeping, because the probe that produced it was
`let _ = f.try_lock();`, which compiles against any return type and so
proved that the name existed and nothing more. 1.85 has a `try_lock` that
returns `Result<bool, io::Error>`; the `Result<(), TryLockError>` this
code matches on arrives in 1.89. Clippy's `incompatible_msrv` caught it,
which is the lint doing the job a careless measurement did not.

So: 1.75 to 1.89. The workspace needed no code change to compile there,
but the toolchain brought lints with it, and CONTRIBUTING says moving one
means reading its new lints rather than silencing them. Fourteen of
them, all mechanical: `chunks_exact` with a constant size becomes
`as_chunks`, `x % n == 0` becomes `is_multiple_of`, `map_or(true, ..)`
becomes `is_none_or`. Four sit in the hand written CRC and SHA-256, whose
published vectors are what makes such a rewrite checkable, and they still
pass. One turned up two byte identical copies of a hex decoder in the two
lexers, now one.

The alternatives are worse. `libc` is a dependency and ADR-020 has
answered that four times. Raw syscalls mean unsafe in the storage core,
in the one crate where that is least welcome.

**Resolved: the MSRV moved to 1.89 and the lock exists.**
`FileStorage::open` takes an exclusive advisory lock and a second writer
is refused with `AlreadyOpen`. The operating system drops it when the
process ends, killed or not, so a crash leaves no database unopenable.

Readers take no lock, because many readers alongside one writer is the
model and always was; a shared lock would only conflict with the
writer's. `Db::open_file_unlocked` is that path, and the tool asks
`Statement::writes()` before it opens, so `quanty run db "get users"`
still answers while a server holds the file and `put` is refused.

**The guard stays, and skipping it when the lock is held would be a
bug.** That was built, and then thought about: a locked writer and an
unlocked reader coexist by design, nothing stops a reader writing, and a
locked handle that skipped the check would overwrite such a commit in
exactly the silence the check exists to end. It runs on every commit.

**The price of the guard.** Nine percent on an unbatched durable commit,
and it detects rather than prevents: two writers still race, the loser
just learns about it instead of winning silently. A crash between the
read and the meta write leaves the file exactly as the loser found it,
which is the same guarantee as any other commit that never happened.

## ADR-036: A text index is a secondary index whose value is a term

Phase 7 wants an inverted index with positions and BM25, consistent under
the crash harness. Reading the index code first: a secondary index entry
is `(index_id, value, ...pk)` in the data tree, written and deleted
alongside the row that produced it. A posting is the same shape with the
term where the value goes. One row makes one entry per distinct term
instead of one entry, and the entry carries its positions instead of
being empty.

**That is the whole storage design, and it is deliberate.** ARCHITECTURE
says no feature gets its own storage path, and search is the feature most
likely to want one. Reusing the entry key means postings are versioned,
branched, time travelled and collected by machinery that already exists
and is already under the crash harness, and it means the second
acceptance criterion is mostly inherited rather than built.

**Statistics live in the same index under an integer namespace.** BM25
needs the document frequency of a term, the length of each document and
the corpus average. Keys are typed tuples and the encoding orders types
before values, integers before text, so `(index_id, 0, ...pk)` holds a
document's length and `(index_id, 1)` holds the corpus counters, both
sorting ahead of every term without a prefix anyone has to reserve.
Document frequency is not stored. It was going to sit at `(index_id,
term)`, and writing the maintenance made the reason to drop it obvious:
document frequency is the number of postings a term has, and scoring
reads every one of them anyway, because BM25 has to score every document
that contains the term. A stored counter would be a second truth that can
drift from the first and buys nothing. What cannot be derived from one
term's postings is the corpus size and the average document length, and
those are what the counters at `(index_id, 1)` are for.

**A word is a run of letters and digits, and Unicode decides which those
are.** The first version of this was ASCII, on the reasoning that
anything more is a table nobody had written. That was wrong twice over.
The standard library already carries the tables, so `is_alphanumeric` and
`to_lowercase` cost no dependency at all; and what ASCII did was not
merely unhelpful but destructive, since a German corpus came out as
fragments. A word with an umlaut tokenized into the pieces around it, so
searching such a corpus found nothing and looked like it worked.

What is still not done, and each for the reason the old paragraph gave:
stemming, stop words, folding accents away, and segmenting languages that
put no spaces between words. Case is Unicode's business and the library
knows it; folding an accent away is a judgement about a language that
somebody has to write down and keep. A word written with an accent is a
different word from the same one without.

The tokenizer is one function behind one call, so replacing it is a
change to one file and a reindex, which `drop index` and `index ... text`
now make an ordinary thing to do rather than a migration.

**Ranking is BM25 with the usual constants**, k1 = 1.2 and b = 0.75,
because a phase that has to beat a brute force scan by a hundredfold
should spend its budget on the index rather than on inventing a scoring
function. A `match` returns its answers best first; an explicit
`order by` overrules that, and ties fall back to the primary key so the
same query gives the same answer twice.

**Scoring happens while the postings are read, and the document length
comes off the row.** The first is why document frequency is not stored: a
term's is the length of its list, and the walk that intersects the lists
has it. The second was measured rather than assumed. Reading each
document's length from `(index_id, 0, pk)` costs one point lookup per
result, which is invisible on a selective query and ruinous on a broad
one: a query matching eighty thousand of a hundred thousand documents
went to three times the cost of the scan it exists to beat. The row is
fetched anyway and the column index is in the plan, so the length is
counted from the text there instead. That took the search mix from 275x
back to 461x, which is where it was before ranking existed.

**A phrase is a second operator, not a quoting convention inside the
first.** `where body phrase "quick brown"` rather than quotes nested in
the query string: QQL is a typed language rather than a search box, and
an operator says what it means without asking the lexer to carry two
levels of quoting. It is a plain binary operator like `match`, so a
column without `@text` evaluates it row by row and the brute force stays
the same predicate rather than a second implementation written to agree.

Its terms keep the order they were written in, repeats and all, and that
was a bug before it was a rule: the planner reused the term list built
for `match`, which sorts and deduplicates because holding every word is a
question that does not care in what order it is asked. A phrase does
care, and `"quick brown"` was being looked up as `brown quick`.

**A phrase is scored as one term of its own**, occurring as often as it
occurs and held by as many documents as hold it, which is why scoring
needs a second pass: its document frequency is not known until every
candidate has been checked. Summing its words would rank a document that
holds them scattered above one that holds them together, which is the
opposite of what was asked for.

**The length ended up in the posting, and the entry at
`(index_id, 0, pk)` is gone.** It was kept for a top-k that scores
without materialising every row, and building that showed the entry was
the wrong shape for it: reading a length per candidate is a point lookup
per candidate, which is the cost that was measured and removed once
already. Every posting now carries its document's length in front of the
positions, so scoring a candidate needs nothing but the posting the walk
already read.

That is a second copy of a fact, which this record argued against for
document frequency. The difference is that it was measured rather than
assumed, and that `verify_indexes` rebuilds postings from the rows, so a
copy that drifts is caught rather than believed. It costs four bytes per
term of a document.

**A disjunction is a union of groups, not a second access path.** The
planner splits a condition on `and` and narrows on one conjunct, so
`a or b` has no conjunct to narrow on and used to read every row. The
text access now holds a list of groups rather than one term list: a
document has to hold every term of a group and is answered by any one
group. One group is a plain `match` and is unchanged, which is why this
generalised the access instead of adding another.

Every leaf has to name the same column, since one access reads one index,
and a mixture falls back to a scan. So does any group with no words,
because `match ""` matches everything and one such group makes the whole
disjunction match everything. Measured on a hundred thousand documents:
229x to 328x for selective disjunctions against the scan they replace,
and parity when both terms are common enough to answer ninety thousand
rows, which is the same wall every other query meets.

Scoring sums BM25 over the query terms a document actually holds. For one
`match` that is all of them and nothing changes; for a union it is the
classic answer to what `or` should rank higher, and a document holding
both terms outranks one holding either. A lone `phrase` keeps its own
rule, since it is one term of its own.

**A prefix term is a range of the same index.** Postings sort by term, so
every word starting with `quick` sits between `quick` and the first word
that does not, and `match "quick*"` is one scan rather than a lookup per
expansion. A document reached through several of those words is still one
answer: the postings are merged, frequencies add and positions join, so
it is scored once and ranked above a document reached through one.

The star is read before tokenizing, because the tokenizer sees it as
punctuation. The query is cut on whitespace, a chunk ending in a star
marks its last word as a prefix, and a star anywhere else stays a
separator: `qu*ick` is two words. That rule needed a test that could tell
the two readings apart, and the first one could not, because both
readings answered the same on the documents it had.

**A phrase refuses a prefix rather than ignoring it.** Reading the star
as punctuation would answer `phrase "quick brown*"` with the exact
phrase, which is a wrong answer nobody asked for. Both the index and the
scan refuse, so they agree about the refusal too.

**A limit travels into the access, but only when nothing after it can
drop a row.** A residual predicate can, and truncating before it runs
answers `limit 10` with fewer than ten, so the limit is not pushed down
then. Measured on a hundred thousand documents: `limit 10` over a query
matching eighty-two thousand of them went from 721ms to 71ms, and the
same query without a limit from 752ms to 493ms, because scoring no longer
reads a row it will not return.

**Tried and taken back out: peeking before intersecting.** Reading a
bounded prefix of every posting list first, to find a rare term without
reading the common ones out, and then probing the rest with point
lookups. It measured worse: a three term query with one rare term went
from 8.6ms to 10.4ms, and a two term query with no hits from 0.3ms to
1.5ms. A sequential walk of a posting list beats a btree descent per
survivor, which is not what I expected and is why it was measured.

**The price.** A row with a text column now writes one index entry per
distinct term in it, so an insert into an indexed table is no longer a
fixed amount of work; a thousand word document is a thousand entries.
Deleting is symmetric and pays the same. Reindexing is the only migration
path for a tokenizer change, and nothing automates it yet.
