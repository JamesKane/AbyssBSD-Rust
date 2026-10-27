# ADR-0011: Performance parity with FreeBSD is an exit criterion

- Status: Accepted; amended by [ADR-0014](0014-performance-parity-per-component.md) (parity is
  judged per component, not per benchmark)
- Date: 2026-10-18

## Context

The roadmap's exit criteria covered correctness and compatibility but not speed. Performance was
left to a P9 "performance pass" and a P8 throughput target "set in planning". A rewrite that is
correct but slower than the code it replaces is not a credible replacement, and performance lost
in early layers (allocator, locks, scheduler, VM) is expensive to win back once everything is built
on top of them.

AbyssBSD runs unmodified FreeBSD userland, so it has an exact baseline: the same binaries, on the
same machine, under the stock FreeBSD kernel.

## Decision

- From P2 on, every phase's exit criteria include **performance parity**: on each benchmark in
  that phase's suite, AbyssBSD is **no more than 2% slower** than stock FreeBSD 15.1 on the same
  machine and configuration. The **goal** is to be faster. Results at or below parity are reported
  and celebrated, but do not gate.
- ~~The comparison is per benchmark, not an average: a 10% win in one place does not pay for a 5%
  loss in another.~~ Superseded by [ADR-0014](0014-performance-parity-per-component.md): parity is
  judged on each component's geometric mean; individual benchmarks are diagnostics.
- Suites are cumulative. A benchmark that gated a phase keeps running nightly, and a regression past
  the 2% bound fails the build like a kyua regression does.
- The method (baseline configuration, fairness rules, statistics, and the benchmark list per phase)
  is in [PERFORMANCE.md](../PERFORMANCE.md).
- Before P7, the gating platform is **bhyve on the build server** for amd64. aarch64 runs under QEMU
  TCG on x86 hosts until P7, which says nothing reliable about real performance, so aarch64 results
  are recorded but do not gate before P7. From P7 on, both physical reference machines gate, with
  FreeBSD 15.1 on bare metal as the baseline
  ([ADR-0010](0010-qemu-first-development.md)).
- There are no per-phase waivers. A benchmark can be excluded or given a different bound only by a
  new ADR that the owner accepts, stating the cause and cost (for example, a safety property that
  cannot be had for free).

## Consequences

- Each phase now includes building its benchmarks and their FreeBSD counterparts, so phases grow.
  Until P4, benchmarks run inside the kernel. Their FreeBSD counterparts are a small kernel module
  of our own (`tests/perf/kbench`), written in C because FreeBSD kernel modules are C. It is test
  code that runs only on FreeBSD and is never linked into AbyssBSD. From P4 on, benchmarks are
  ordinary FreeBSD binaries that run unchanged on both kernels.
- Design choices in hot paths (generics vs `dyn`, lock granularity, per-CPU caches, bounds checks in
  loops) are now measured against a number, not argued in the abstract.
- Benchmarks must be quiet enough to resolve 2% (see the noise rule in PERFORMANCE.md). That forces
  pinned vCPUs, an otherwise idle build server during runs, and enough repetitions, which costs
  nightly CI time.
- arm64 performance problems can surface late, at P7. Hot paths with arch-specific code (context
  switch, TLB maintenance, atomics, user copy) should be designed with ARM64 costs in mind from the
  start.
