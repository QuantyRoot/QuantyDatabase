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

## Setup A: cgroups, no virtualization

Simplest, and the most faithful for the CPU question, because nothing sits
between the two processes but the host's loopback. Use this first.

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

## Setup B: QEMU, when the test should stay off your own kernel

Ten thousand sockets and a raised descriptor limit touch host state. If you
would rather not do that to a desktop you are using, put the server in a VM.
This also gives a known kernel rather than whatever the host is running.

Requirements, all of them:

- KVM. `ls -l /dev/kvm` must exist and be readable. On AMD this needs SVM
  enabled in firmware and the `kvm_amd` module loaded.
- `tap` networking with `vhost-net`. Not user mode. Not `-net user`.
- vCPUs pinned to host cores that the client does not use.

```
qemu-system-x86_64 \
  -enable-kvm -cpu host \
  -smp 2 -m 4096 \
  -drive file=accept.qcow2,if=virtio,cache=none \
  -netdev tap,id=n0,ifname=tap0,script=no,downscript=no,vhost=on \
  -device virtio-net-pci,netdev=n0 \
  -nographic
```

Then pin the vCPU threads to cores 0 and 1 on the host, and run the client
under `taskset -c 4-11` as above. QEMU exposes the vCPU thread ids on its
monitor socket; pinning them is what makes `-smp 2` mean two cores rather
than two threads sharing twelve.

Note that the virtio path still costs something the cgroup setup does not.
If setup A passes and setup B fails, the difference is the virtual NIC and
should be reported as such rather than as a server regression.

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
