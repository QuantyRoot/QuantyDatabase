# Contributing

Thanks for looking. This is a one person project with no funding behind
it, so the most useful thing you can do is read a little before you write
a lot.

## Before anything else: the dependency rule

**The lock file holds this workspace and nothing else, and that is not
negotiable.** A patch that adds a package will be turned down however good
the package is. ADR-020 explains what that costs and why it is worth
paying; the short version is that a database people trust with their data
should be auditable by one person in an afternoon.

This has teeth. The checksum, the reader-writer locks under the storage
core, SHA-256 for token hashing, the epoll layer and the wire protocol are
all written out here because the alternative was a dependency. If your
change needs something the standard library does not have, that is a
conversation to have in an issue first, and the answer is usually "write
the part we need".

## What good looks like here

**Decisions live in ADRs, not in comments.** `docs/DECISIONS.md` has
twenty nine of them. If your change turns on a judgement call, the ADR is
where the reasoning goes, including what it costs. An ADR with no cost in
it is marketing.

**Comments stay short.** One or two lines on public items. Explaining
prose almost never; that is what the ADRs are for. A comment that argues
with the code is worse than no comment, and there is at least one in the
history of this repository that claimed a bound the code did not enforce.

**Measure before you optimize.** ADR-016 says so, and it is enforced in
practice: group commit sat unbuilt through two decision records because
nobody had measured what a commit cost. When the measurement arrived it
said thirteen times, and it also said the fsync was only half of what was
being amortized, which nobody had guessed.

**Do not guess a port.** Binding one to see which is free and then
letting it go is a race, and this server sets `SO_REUSEPORT`, so the race
is quiet: two test servers hold the same port, a test talks to the wrong
one, and it surfaces much later as a refused connection when the other
test tears its server down. It passed twenty five times on one core and
failed on the four core runner. `--listen 127.0.0.1:0` makes the kernel
choose and the server prints what it got; read that line.

**A sleep is not a synchronisation primitive.** A fixed wait encodes an
assumption about how long something takes, and a loaded machine is under no
obligation to agree. The server crash harness spent one commit killing
servers after a fixed 189 milliseconds and then reporting that the kill
proved nothing, which was true and was a fact about the CI runner rather
than about the server. Wait for the condition, with a generous ceiling, and
say which part timed out.

**A test that cannot fail is not a test.** This is the recurring lesson of
the project, written down properly in `HUNDRED.md`. If you add a test for
something invisible, break the thing on purpose and watch the test go red
before you commit it. Several tests here have a comment saying exactly
what was switched off to prove they would catch it.

**English and plain ASCII** in code, comments, commit messages and
documentation, wrapped at 79 columns in the docs.

## The gates

Every one of these has to pass before a commit, no exceptions:

```
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace -- --test-threads=8
cargo +1.75.0 test --workspace --locked -- --test-threads=8
```

`--test-threads=8` is not optional. A machine with one core runs the tests
serially and hides every concurrency bug in them; the CI runner has four
and will find them at the worst moment. If your change touches anything
concurrent, run the affected test file twenty times before you believe it.

The minimum supported Rust version is 1.75. Clippy checks it, so a method
stabilized in 1.82 fails the lint rather than the build, which is easy to
misread as a formatting problem.

If you add a crate, run `cargo update -p <name>` so the lock file knows
about it, or the MSRV job fails on `--locked`.

## CI

Eleven jobs per push. Four fuzzers at ten minutes each, two crash harnesses
that kill -9 the process a thousand times each, a ten minute soak against
the server, a benchmark against SQLite, the gates above, and the same
suite again on the minimum supported toolchain.

They take a while. Wait for them and read them one by one rather than
assuming: a fuzzer that is still running is not a fuzzer that passed.

## Reporting a bug

The useful ones say what you expected, what happened, and how to make it
happen again. A `.qdb` that reproduces it is worth more than a paragraph
describing it. If it involves the server, the seed printed by the soak
replays the run exactly.

## What is not wanted

Reformatting for its own sake, renames without a reason, and dependencies.
Beyond that, ask in an issue before writing something large; the roadmap
is opinionated and it is easier to say no to an idea than to a weekend.

## The toolchain is pinned

CI runs on a named Rust version, not on `stable`. With `-D warnings` every
new lint is a build failure, so tracking stable means an unrelated commit
can be broken by a release that happened between two pushes. That is not
hypothetical: 1.98 added `for_unbounded_range` and a test loop that had
been correct for weeks stopped compiling on a commit that only deleted a
file.

Raising the pin is a deliberate change with a commit of its own, where the
new lints get read and dealt with rather than discovered by whoever pushed
next. The minimum supported version is pinned for the opposite reason: it
is a promise to people on older toolchains, and lowering it is a decision,
not a convenience.
