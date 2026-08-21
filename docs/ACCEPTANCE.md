# Running the phase 5 acceptance test

The criterion: 10k idle connections plus 1k active mixed QPS on a 2 vCPU
box, stable for 30 minutes, no descriptor or memory growth.

ADR-023 makes this number the decision procedure for the whole server
design rather than a box to tick at the end. If the reactor cannot meet it,
the thread-per-connection fallback is the design and the record is
rewritten with the measurement attached. So the test needs to be run
honestly, and honestly here means being careful about two things that will
otherwise quietly measure something else.

## What is being measured, and what is not

Being measured: whether the server holds ten thousand mostly-idle
connections without the per-connection cost adding up, and serves a
thousand statements a second across them, on two cores, for long enough
that a slow leak would show.

Not being measured: raw throughput, latency percentiles under saturation,
or anything about the storage engine. Those have their own benchmarks. A
run that fails should say which of the four properties it failed, because
"the acceptance test is red" is not a finding.

## Trap 1: the load generator competing for the measured cores

If the client runs on the same two cores as the server, it takes CPU the
server would otherwise have had, and the result is pessimistic by an
unknown amount. Passing anyway is still a pass. Failing tells you nothing,
which is the worse half.

So the client runs somewhere else. A second machine is ideal. On one
machine with more than two cores, pin the server to two and the client to
others; the sections below do that.

## Trap 2: a virtual network stack becoming the bottleneck

QEMU's default networking is user mode, also called SLIRP, and it is a
TCP/IP stack running in userspace on one thread. It is fine for logging
into a VM and useless here: at ten thousand connections it is the thing
under test, and the server never gets a chance to be the limit.

If the test runs in a VM it needs `tap` networking with `vhost-net`, and
KVM. Without KVM the CPU is emulated and everything below is meaningless.
Check `/dev/kvm` exists and is readable before anything else.

## Constraining the server without virtualization

The one to use. Also the most faithful for the CPU question, because
nothing sits between the two processes but the host's loopback.

First the database, because the load reads and writes a table called `t`
and the server refuses to start without a database that exists:

```
cargo build --release
target/release/quanty create /tmp/accept.qdb
target/release/quanty run /tmp/accept.qdb "table t { id: int @key, n: int }"
target/release/quanty run /tmp/accept.qdb "put t { id: 1, n: 1 }"
```

Then the server on two cores and the client on others:

```
ulimit -n 65536

systemd-run --user --scope \
  -p AllowedCPUs=0,1 \
  -p MemoryMax=7G \
  target/release/quanty serve /tmp/accept.qdb --workers 2

# the client anywhere else, in its own shell
ulimit -n 65536
taskset -c 4-11 target/release/quanty-acceptance \
  --connections 10000 --active 32 --qps 1000 --duration 30m --mixed
```

`--workers 2` is passed rather than left to default. The default is one
worker per core, and whether that sees two cores or all of them inside a
cpuset is not something a measurement should depend on.

`--mixed` sends one write in every ten requests and reads the rest. The
criterion says mixed and does not say in what proportion; nine to one is an
ordinary online transaction mix, and the ratio is printed in the result
line so the number stays interpretable. Without the flag the load is reads
only, which is a different measurement.

If `systemd-run` refuses the properties, which happens when the user
manager has no cpuset delegation, `taskset -c 0,1` pins the server just as
well. That loses the memory cap, which is worth having but is not what the
criterion turns on: it watches memory grow rather than capping it.

Loopback is faster and more reliable than any real link, so the network is
deliberately not the limit. That is the point: what is left is the server.

## Watching the three failure modes while it runs

Thirty minutes of "no descriptor or memory growth" is a claim about a
curve, not about the last line. Sample it in a third shell:

```
while :; do
  SRV=$(pgrep -f 'quanty serve /tmp/accept.qdb' | tail -1)
  if [ -z "$SRV" ]; then
    echo "$(date +%H:%M:%S) no server"
  else
    echo "$(date +%H:%M:%S) pid=$SRV \
rss_kb=$(grep VmRSS /proc/$SRV/status | tr -dc 0-9) \
fds=$(ls /proc/$SRV/fd | wc -l)"
  fi
  sleep 30
done | tee /tmp/accept-watch.log
```

The process id is looked up on every pass and printed, rather than
captured once before the loop. Restarting the server between attempts is
normal, and a watcher holding the old id reports `rss_kb= fds=0` for half
an hour without ever saying that it lost the process.

Descriptors should climb to roughly the connection count and then sit
flat. Resident memory should flatten too. A slow rise in either across
thirty minutes is the failure this test exists to catch, and it does not
show up in the summary line at the end.

## If the test should stay off your own kernel

Ten thousand sockets and a raised descriptor limit touch host state, and a
virtual machine keeps that off a desktop you are using. It costs a virtual
network path that the cgroup setup does not have, and that path must be
`tap` with `vhost-net` and KVM: QEMU's default user-mode networking is a
TCP/IP stack running in userspace on one thread, which at ten thousand
connections becomes the thing under test. Reach for this only if there is
a reason to; setup A answers the question with less standing in the way.

## Host preparation, either way

Ten thousand connections need descriptors at both ends and ports at the
client end.

```
ulimit -n 65536                      # both sides, per shell
sysctl -w net.ipv4.ip_local_port_range="10000 65535"
sysctl -w net.core.somaxconn=8192
sysctl -w net.ipv4.tcp_max_syn_backlog=8192
```

The default ephemeral range gives about 28k ports, which is enough for ten
thousand connections from one address but leaves little room; widening it
removes a limit that has nothing to do with what is being tested. None of
these should be made permanent on a machine somebody uses for other things.

## Recording the result

A run prints one machine-readable line. That line, the date, and a
description of the machine go into the measurements table, committed. A
number without a machine attached is not a measurement, and a number that
lives only in somebody's memory cannot be regressed against.

The continuous-integration job is not this test. It runs a shorter version
on whatever runner it gets and checks the qualitative failures: descriptors
that do not come back, memory that grows, connections that hang. It cannot
validate the 2 vCPU number and does not claim to.

## Where each criterion stands

- `[x]` **10k idle + 1k mixed QPS on 2 vCPU, 30 min, no leaks.** Met on
  2026-08-21. All four properties held: ten thousand connections accepted
  and still open at the end, a thousand mixed statements a second for the
  full half hour, not one failure in 1800064 of them, and neither
  descriptors nor resident memory growing. The run is in the table below.
- `[x]` **kill -9 under write load, reopen, zero corruption.** Met, and in
  the stronger form: `crates/quanty-cli/tests/crash.rs` writes from four
  connections, kills the serving process with SIGKILL in the middle,
  reopens the file and requires that every write the server answered with
  a row count is still there. Corruption would show up as a database that
  does not reopen; a broken promise shows up as a missing row. Three
  hundred kills per CI run.

  Only that direction is required. A write the executor committed whose
  reply died with the process may or may not be in the file, and demanding
  either would be demanding something that was never offered.

  The harness was checked against a deliberately broken server before it
  was trusted: answering inside the batch rather than after the shared
  commit makes it fail in the first round, and the lost rows come one from
  each connection, which is one batch that replied and then died.
- `[ ]` **Versioned handshake, old client against new server fails
  cleanly.** The handshake is nine bytes out and four back and is frozen by
  design, and `ServerHello::Refused` exists for exactly this. It is covered
  by unit tests on the codec, but no test has yet put a client that speaks
  an older version in front of a running server.

## Measurements

Each line is one run. A number without a machine attached is not a
measurement, so the machine is part of the record.

```
2026-08-21  Ryzen 5 5600G, 12 threads, Arch, kernel loopback
            server: systemd-run scope, AllowedCPUs=0,1, MemoryMax=7G,
                    --workers 2, release build, rustc 1.96.0-nightly
            client: taskset -c 4-11, same machine

  ACCEPTANCE idle_held=10000 idle_target=10000 idle_refused=0
             still_open=10000 answered=1800064 failed=0 rate=1000.0
             mean_us=123 max_us=7649 seconds=1800.0 mix=1-write-in-10

  descriptors  10042 across all 57 samples, then 10 once the client left.
               That is 10032 connections plus the listener, the epoll and
               eventfd, the database and the standard streams. Every one
               came back.
  resident     6632 kB at the first sample, 9920 kB by three and a half
               minutes in as the connection buffers fill, then 9996 kB at
               the end: +76 kB over the remaining 24.5 minutes, and not
               monotone, since it peaked at 10008 and finished 12 kB below
               that. Roughly 3 kB a minute of allocator noise with no
               trend under it.
  per conn     under a kilobyte, and that is an upper bound rather than an
               estimate: 9996 kB of resident memory divided by 10032 open
               connections, with the server itself counted in the numerator
  accepts      split 5090/4942 across the two workers

This is the phase 5 criterion and it is met. ADR-023 made this run the
decision procedure for the server design rather than a box to tick, so it
is worth saying plainly what it decided: the hand written epoll reactor
holds ten thousand connections on two cores, and the thread per connection
fallback in ADR-022 stays superseded.

Two honest qualifications. The client shares the machine: its eight cores
are not the server's two, but they share a memory controller and a last
level cache, so a dedicated pair of boxes would not give exactly this
number. And loopback is faster than any real link, which is deliberate,
because the criterion is about the server rather than about a network.

2026-08-17  container, 1 core, client on the same core, no execution
  2000 idle held, 800 qps, 30s, 0 failed, mean 56us, max 466us
   200 idle held, 5000 qps, 15s, 0 failed, mean 190us, max 1105us
   200 idle held, 20000 qps, 15s, 0 failed, mean 87us, max 2260us

2026-08-20  container, 1 core, client on the same core, statements executed
   200 idle held,  2000 qps, 10s, 0 failed, mean 140us, max  697us
   200 idle held,  5000 qps, 15s, 0 failed, mean  94us, max 3527us
   200 idle held, 20000 qps, 15s, 0 failed, mean  45us, max 24479us
     0 idle,      writes, 32 active, 15s, 0 failed, 9357 stmt/s, mean 3418us
```

What these show: the reactor and the protocol are not the limit at twenty
times the rate the criterion asks for, on one core, with the load generator
competing for it. The second block is the same paths with the engine
actually behind them, so a read reaches the B-tree and a write commits and
fsyncs before its answer goes out.

Do not read the second block as "the executor made it faster". The first
block executed nothing, and the two were taken on a container where the
load generator and the server fight over one core; the means moved for
reasons that have nothing to do with either being better. The write line is
the one worth keeping: 9357 statements per second, every one of them
durable, is what group commit bought and it is measured in ADR-028 from
the other side too.

What they do not show. The ten thousand idle connections have not been
tried at full size here, because the descriptor limit in this container is
20000 and both ends of ten thousand connections need exactly that.

Neither replaces the run described above on two dedicated cores. They are
the floor: whatever the real number turns out to be, it is not being lost
in framing.
