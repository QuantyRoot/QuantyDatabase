# Changelog

All notable changes to QuantyDB are recorded here.

The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html) with
the pre-1.0 rule that a minor version may break anything.

Entries before this file existed were reconstructed from the annotated
tags and the commit history, which is why the older ones are shorter:
they say what the release was, not everything it touched. The one line
that runs past the usual 79 columns is a comparison link, which cannot be
wrapped.

## [Unreleased]

### Added

- An embedded crate. `quanty` is what a Rust application depends on, with
  a concrete `Database`, transactions as a borrow, and statements as text
  (ADR-030).
- `#[derive(Row)]` in `quanty-derive`, mapping a struct with named fields
  to a table. No `syn` and no `quote`: the macro walks the token stream
  (ADR-031).
- A content addressed blob store with deduplication, and an `asset` column
  that holds a descriptor rather than the bytes. A gigabyte streams in and
  out holding 16 MiB resident, and a second copy of a chunk costs two
  pages (ADR-033, ADR-034).
- `gc blobs`, which collects chunks no row names by walking the rows
  rather than trusting a reference count.
- Full text search: `@text` columns, `match` and `phrase`, prefix terms
  with a trailing star, disjunctions read as a union of posting lists, and
  BM25 ranking scored while the postings are read. 502x a brute force scan
  over a search mix of a hundred thousand documents, and the index stays
  consistent through a thousand `kill -9`s (ADR-036).
- `index docs.body text` and `drop index`, so a text index can be built
  over a table that already has rows and taken away again.
- `show stats`, `show suggestions`, and the shell verbs `branch`,
  `branches`, `switch`, `merge`, `log`, `stats` and `gc`.
- Index suggestions: the columns a scan narrowed on without an index,
  worst first by rows walked. Following one took a benchmark from 7.6s to
  45ms.

### Changed

- The tokenizer treats a word as a run of whatever Unicode calls letters
  and digits, lowercased the same way. It was ASCII, which did not merely
  fail to stem but shredded any text with an accent in it. An index built
  by the old tokenizer has to be dropped and built again.
- The minimum supported Rust version moved from 1.75 to 1.89, for
  `File::try_lock` (ADR-035).
- The catalog gained versions 2 and 3, for the `asset` column type and for
  a column's text index. A table definition is written at the lowest
  version that can express it, so a table that gained neither stays
  readable by everything that came before.
- SHA-256 moved from `quanty-auth` into `quanty-core`, since a hash used
  for content addressing belongs with the storage and two copies of one
  would be two databases.

### Fixed

- Two writers on one file could each believe they were alone, and the
  second silently replaced a commit that had been acknowledged. Opening
  for writing now takes an exclusive advisory lock, and `commit` refuses
  with `WriterRaced` where the lock cannot help. Readers take no lock, so
  many of them alongside one writer is still the model (ADR-035).
- A ranked search skipped the rest of its condition, so
  `match "x" and id > 3` returned the rows the filter should have removed.
- macOS and Windows builds. The non-Linux paths had never been compiled,
  because every CI job ran on Linux, and had rotted: an attribute above
  the wrong line took the server crate out of those builds entirely, and
  the tool ended up with two definitions of `serve`. CI now builds and
  tests on all three.

### Documentation

- ARCHITECTURE was checked claim by claim against the code for the first
  time. Fifteen statements were not true, most of them plans that were
  never built, and one was a safety promise nothing kept.

## [0.3.0] - 2026-08-21

### Added

- A network server for the same engine: `quanty serve` and
  `quanty connect`. An event loop per worker written on epoll directly,
  connections parked rather than blocked, one executor thread owning the
  session, group commit, and token authentication stored beside the
  database rather than in it.

Ten thousand idle connections and a thousand mixed statements a second
for thirty minutes, 1800064 of them, none failed, under ten megabytes
resident. The server survives `kill -9` mid-write with every acknowledged
row still present. The write path became 2.25 times faster, and a key
lookup is faster than SQLite's.

## [0.2.0] - 2026-08-15

### Added

- A SQL front end lowering onto the same plans as QQL, joins, and
  multi-statement transactions.
- A SQLite importer that reads the file format directly.
- A command line tool around all of it.

### Removed

- The last dependency. The workspace uses nothing outside the standard
  library (ADR-020).

## [0.1.1] - 2026-07-05

### Added

- Branches, time travel and garbage collection: `branch`, `switch`,
  `merge`, `as of`, and a `gc` that frees the pages no retained commit
  points at.
- The MSRV job runs the whole test suite rather than only building, so
  the claim covers what the code does and not only that it compiles
  (ADR-013).

### Removed

- `tempfile`, replaced by a temp directory helper in the tests.

## [0.1.0] - 2026-07-04

### Added

- The storage core: a copy-on-write B-tree, commits and snapshots, with
  a crash harness that kills the process mid-write and checks that every
  acknowledged commit is still there.
- CI from the first day: fmt, clippy, tests, the crash harness, an MSRV
  job and a parser fuzzer.

[Unreleased]: https://github.com/QuantyRoot/QuantyDatabase/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/QuantyRoot/QuantyDatabase/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/QuantyRoot/QuantyDatabase/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/QuantyRoot/QuantyDatabase/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/QuantyRoot/QuantyDatabase/releases/tag/v0.1.0
