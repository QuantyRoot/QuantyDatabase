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

```
# two cores for the server, 7 GiB to match a small cloud box
systemd-run --user --scope \
  -p AllowedCPUs=0,1 \
  -p MemoryMax=7G \
  target/release/quanty serve /tmp/accept.qdb

# the client anywhere else
taskset -c 4-11 target/release/quanty-bench acceptance \
  --connections 10000 --qps 1000 --duration 30m
```

Loopback is faster and more reliable than any real link, so the network is
deliberately not the limit. That is the point: what is left is the server.

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

## Measurements

Each line is one run. A number without a machine attached is not a
measurement, so the machine is part of the record.

```
2026-08-17  container, 1 core, client on the same core, NotYet service
  2000 idle held, 800 qps, 30s, 0 failed, mean 56us, max 466us
   200 idle held, 5000 qps, 15s, 0 failed, mean 190us, max 1105us
   200 idle held, 20000 qps, 15s, 0 failed, mean 87us, max 2260us
```

What these show: the reactor and the protocol are not the limit at twenty
times the rate the criterion asks for, on one core, with the load generator
competing for it.

What they do not show, and the distinction matters. No statement is
executed: the service answers an error without touching the database, so
this times the accept path, the frame codec and the event loop and nothing
below them. The ten thousand idle connections have not been tried at full
size here either, because the descriptor limit in this container is 20000
and both ends of ten thousand connections need exactly that.

Neither replaces the run described above on two dedicated cores. They are
the floor: whatever the real number turns out to be, it is not being lost
in framing.
