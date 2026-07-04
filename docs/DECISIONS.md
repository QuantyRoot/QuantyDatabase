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
