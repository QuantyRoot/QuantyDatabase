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
