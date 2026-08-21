# One hundred commits

This file exists because the hundredth commit felt like a thing worth
marking. It is not a changelog. It is the part that does not survive in
commit messages: what broke, why it hid, and which ideas lost.

## Where the project stands

| | |
|---|---|
| Commits | 100 |
| Rust, source | 21210 lines across 11 crates |
| Rust, tests | 11704 lines, 373 test functions |
| Design notes | 2816 lines, 29 decision records |
| Dependencies | 0 |
| People | 1 |
| Funding | none |
| CI per push | 11 jobs |

More than one line of test for every two lines of code. Eleven packages in
the lock file, all eleven this workspace, so `cargo build` fetches nothing
and there is no supply chain to audit. Every push runs four fuzzers at ten
minutes each, two crash harnesses that kill -9 the process a thousand times
each, a ten minute soak against the server, a benchmark against SQLite, and
the whole suite twice: once on stable and once on a toolchain from 2023.

What runs today: the storage core with copy-on-write commits, snapshots,
branches and `as of` queries; two query front ends lowering to the same
plans; an importer that reads the SQLite file format directly with no
SQLite underneath it; and a network server with token authentication and a
client that speaks its protocol.

Measured, on one development core with the load generator competing for it,
so these are floors and not results:

| | |
|---|---|
| Reads over the network | 20000 QPS, 0 errors, 45 us mean |
| Writes over the network | 9357 statements/s, each committed and fsynced |
| Commit, in process | 292 us alone, 22 us when 512 share one |
| SQL parser, one hour | 4244088000 inputs, no panic, no broken roundtrip |
| SQLite reader, one hour | 1831872 damaged files, 12 billion cells, no panic |

## The lesson that kept coming back

**The instruments are harder to build right than the code they check.**

It showed up first in a codec fuzzer that compared decoded values instead
of bytes. Every `NaN` failed it, because `NaN != NaN`, and the codec had
been perfectly correct the whole time. The fix was to compare
`encode(decode(encode(x)))` against `encode(x)`, which is well defined and
happens to prove something stronger: that the encoding is canonical.

It showed up in a descriptor-leak test that counted open descriptors
process-wide while other tests in the same binary were opening sockets. It
now lives in a test binary of its own containing exactly one test, because
that is the only way the number means anything.

It showed up in a loop that stopped when nothing had moved, on a socket
that refused every second write, so "nothing moved" arrived long before
anything was finished. It waits for four idle rounds now.

And it showed up at its worst in the first version of the soak test for the
server. Three thousand requests, every assertion green, and **zero**
statements ever refused by the write queue -- because the connection holding
a transaction always finished in under a millisecond, so nothing ever
waited long enough to hit the deadline. The test exercised none of the
contended paths and reported success. It now makes a holder go quiet for
longer than the deadline on purpose, and it fails if nothing was refused,
nothing was abandoned mid-flight, and nothing ever shared a commit.

The habit that came out of all this: when a test covers something
invisible, break the thing on purpose and watch the test go red before
trusting it. Several tests here carry a comment naming exactly what was
switched off to prove they would catch it.

## The best bugs

**536 megabytes from a 16 megabyte frame.** A row reader took a length from
the wire and allocated for it before checking it against the bytes actually
present. The comment above it claimed the bound was enforced. A comment
that argues with the code is worse than no comment.

**The connection that hung with the answer in its hand.** After the
handshake, the request was already sitting in the application buffer. But
level-triggered epoll reports what is in the *socket*, and the socket was
empty, so no event ever came. The connection waited forever holding
complete data. The dispatch loop now keeps going as long as the input
buffer is shrinking.

**A test that demanded the impossible.** It sent a write on one connection
and `begin` on another and expected both to answer. If the `begin` won the
race, the write was correctly blocked behind the transaction and never
answered -- so the test was asking the design to violate itself. On one core
it happened to pass. On the four-core CI runner it did not. It now queues
both behind a third connection, which removes the race instead of hoping.

**A read that let the writers go.** When reads were allowed past an open
transaction, the bookkeeping recorded "this statement left no transaction
open, so nobody holds one" -- which is true for the reader and false for the
server. The waiting writers would have been released and the first commit
among them would have killed the parked transaction. The test for it was
written first and was red before the fix landed.

## The ideas that lost

Commit messages record what was done. They never record what was rejected,
which is usually the more interesting half.

**tokio.** The roadmap said "tokio server" from the beginning. By the time
phase 5 arrived the workspace had already gone to zero dependencies and
nobody had reconciled the two lines. ADR-022 did, and picked threads.

**Thread per connection.** ADR-022 chose it and argued that the alternative
meant either `libc` or declaring the syscalls by hand, and that hand-written
unsafe had already been turned down twice, so a third time was consistent.
ADR-023 then declared the syscalls by hand anyway, confined to a single
module the compiler polices. **A decision that overturned its own reasoning
inside one phase**, and both records are still in the file, because
striking a decision through is honest and deleting it is not.

**`EPOLLEXCLUSIVE` on a shared listener.** It worked, and it distributed
354/82/64 across three workers. One socket per worker with `SO_REUSEPORT`
does 155/173/172.

**Sharing the key encoding with the wire encoding.** The tags happen to
agree today. They are kept separate anyway, because otherwise a change to
the on-disk format silently becomes a protocol change, and the two have
different version histories.

**`Arc<Db>` for a shared database.** It looked obvious: one database, a
session per connection. But garbage collection takes `&mut Db` on purpose,
which is how the borrow checker proves no snapshot is still alive before a
page is reused. Behind an `Arc` that proof is unavailable and `gc` becomes
unreachable. The compile-time proof was worth more than the shape, so one
thread owns the session and each connection's transaction is parked in and
out around its statements.

**Group commit, twice.** Deferred through two decision records for one
reason: nothing had measured what a commit cost. When the measurement
finally happened it said thirteen times -- and it also said the fsync was
only about half of what was being amortized, which nobody had guessed. The
rest was the copy-on-write page copying and the meta record. There was a
third number nobody had asked for: writing one statement per commit leaves
twelve kilobytes on disk per statement, and five hundred leaves a hundred
bytes. A hundredfold in write amplification, invisible in any latency
graph.

**Inserting a phase into the roadmap.** It would have renumbered phases six
through nine and broken every reference into them, including from a
decision record. The gap went in as its own section instead.

## Housekeeping, honestly

There is a commit in this history named `style: cut the comments back to
what the code needs`. It contains a worker, a slot registry, a command line
flag and a decision record. A staging mistake put everything into one
commit under the wrong message, and rewriting published history costs more
than a crooked message does, so it stayed.

`unsafe` appears in exactly two places in the whole workspace, both in the
server: twelve syscall declarations in `crates/quanty-server/src/sys.rs`,
and one `from_raw_fd` in `listener.rs` where a socket built by hand becomes
a `TcpListener`. The library carries `deny(unsafe_code)` and those two
carry the only `allow`, so the compiler holds the boundary instead of a
convention. Every other crate is `forbid` or `deny`.

## What is still missing

Readers do not stall any more, but they still cross one thread one at a
time. Whether they should run in parallel is an open question that needs a
machine with more than one core to answer, and the answer has to survive
the fact that garbage collection wants unique access.

The acceptance run behind phase 5 has been done, one commit after this
file was written: ten thousand idle connections and a thousand mixed
statements a second on two cores for half an hour, 1800064 of them, none
failed, descriptors and resident memory flat throughout. That run was never
a box to tick. ADR-023 made it the decision procedure for the whole server
design, so a red one would have meant rewriting the design rather than
fixing a bug.

What has not been done is killing the server mid-write and checking that
every write a client was told had succeeded is still there. That is the
interesting one: it is the durability promise crossing the protocol
boundary, and it is the next thing to build.

Here is to the next hundred. :3
