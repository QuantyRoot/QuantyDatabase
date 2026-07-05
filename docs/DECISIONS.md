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
