# Performance

The rule ([ADR-0011](adr/0011-performance-parity.md)): on every benchmark in a phase's suite,
AbyssBSD is **no more than 2% slower** than stock FreeBSD 15.1 on the same machine and
configuration. The goal is to be faster. This document defines what is measured and how.

## 1. The bound

For each benchmark, normalize so that larger means slower (elapsed time, latency, or the inverse of
throughput). Then:

```
ratio = median(AbyssBSD) / median(FreeBSD)
pass  ⇔  ratio ≤ 1.02
```

- Latency benchmarks check the bound at both p50 and p99.
- Each report shows the ratio with a 95% bootstrap confidence interval. A ratio below 1.00 whose
  interval lies entirely below 1.00 is reported as a win.
- A suite also reports the geometric mean of its ratios, as a headline number only. Gating is per
  benchmark.

## 2. Baseline and fairness

The comparison must be like for like. Otherwise the number means nothing.

| Rule | Detail |
|---|---|
| Same machine, same configuration | Same host, VMM, firmware, vCPU count and pinning, memory size, and devices. Only the kernel (and, where it must differ, the loader) changes |
| Same userland | Identical FreeBSD 15.1 `base.txz` and identical benchmark binaries on both sides. From P4 on, benchmarks are unmodified FreeBSD binaries |
| Release builds | FreeBSD: the stock 15.1-RELEASE `GENERIC` kernel, which ships without `INVARIANTS` or `WITNESS`. AbyssBSD: `--release` with the runtime witness and debug assertions compiled out. A debug-vs-debug comparison may be added, but it never replaces this one |
| Same security posture | Speculative-execution mitigations, SMEP/SMAP/PAN, KASLR, and W^X match what FreeBSD enables by default on that CPU. A kernel that skips a mitigation FreeBSD applies is not being compared fairly |
| Same semantics | No winning by doing less. `fsync` reaches stable storage, the same TCP options are negotiated, page zeroing and ASLR stay on, and the same sysctl tunables (FreeBSD defaults) apply on both sides |
| Same state | Each benchmark declares cold or warm caches, and both sides are prepared the same way (fresh boot, or a fixed warm-up) |

## 3. Method

- **Interleave** runs (FreeBSD, AbyssBSD, FreeBSD, …) so drift in the host affects both sides
  equally. At least 10 runs per side. Microbenchmarks run enough inner iterations that one run
  lasts at least 1 second.
- **Noise rule.** If a benchmark's coefficient of variation on the FreeBSD side exceeds 1%, it cannot
  resolve 2%, so it does not count. The fix is to the benchmark (more iterations, pinning, an idle
  host), not a wider bound.
- **Hosts.** The build server runs nothing else during a gating perf run. vCPUs are pinned to host
  cores (`bhyve -p`, or `taskset` for QEMU/KVM), and P-cores and E-cores are never mixed in one VM.
  On physical machines, frequency scaling is left at firmware defaults on both sides. Turbo is not
  disabled, because that is how the machines are used.
- **Records.** Every run stores the raw samples, both kernels' revisions, the host and VMM
  configuration, and the CPU microcode version, so any result can be reproduced and re-analyzed.

## 4. Platforms

| Phases | amd64 | aarch64 |
|---|---|---|
| P2–P5 | **Gating:** bhyve on the build server (i7-12700KF, P-cores only) | Recorded, not gating: QEMU TCG on x86 says nothing reliable about real performance |
| P6 on | **Gating:** bhyve, and bare metal on the Ryzen 9 7900X / X870E | **Gating:** bare metal on the Orange Pi 6 Plus |

On bare metal, the baseline is FreeBSD 15.1-RELEASE on the same machine. If 15.1 cannot run on a
reference machine, the closest FreeBSD release that does is used, and the report names it. On the
X870E, the baseline install also lives only on the media NVMe (see
[CLAUDE.md](../CLAUDE.md) and [HARDWARE.md §5](HARDWARE.md#5-board-profile-amd64-ryzen-9-7900x--x870e-aorus-elite-wifi7)).
On the Orange Pi 6 Plus, the three core classes are measured separately (each benchmark pinned to
one class) as well as together.

## 5. Benchmark suites by phase

Suites are cumulative: each phase adds to the nightly set and nothing leaves it except by ADR.

Until P4 there is no userland to share, so benchmarks run inside the kernel. The FreeBSD side is our
own kernel module, `tests/perf/kbench`, which implements the same workload with FreeBSD's kernel
APIs (`malloc(9)`/`uma(9)`, `mtx(9)`, `kthread(9)`, `callout(9)`, and so on) and prints results in the
same format. It is test code that runs only on FreeBSD.

| Phase | Benchmarks |
|---|---|
| **P2** Core kernel | Context switch between two kernel threads (same CPU, cross CPU). Uncontended and contended mutex, rw, and sx lock/unlock, at 1, 2, 4, and 8 threads. Zone allocator alloc/free throughput (per-CPU cache hit, and cache miss), at 1 and 8 threads. Physical page alloc/free. Callout arm/cancel and timer-expiry latency. IPI round trip and TLB shootdown latency. Wakeup latency from sleep |
| **P3** Devices and storage | Interrupt latency (MSI-X to handler). Block read IOPS and p50/p99 latency through the virtio-blk and NVMe drivers at queue depth 1 and 32, 4 KiB random reads, measured in kernel on both sides. Sequential read bandwidth. AML: boot-time namespace load and `_STA`/`_CRS` evaluation on QEMU's tables |
| **P4** First userspace | First shared userspace benchmarks, as static FreeBSD binaries (`tests/perf/ubench`, built on the server): null syscall (`getpid`), `read`/`write` on `/dev/null` and `/dev/zero`, `fork`+`exit`+`wait4`, `execve` of a static binary, `vfork`+`execve`, page-fault cost (anonymous `mmap` touch, COW after `fork`), `mmap`/`munmap`, signal delivery and `sigreturn`, pipe ping-pong latency and bandwidth |
| **P5** VFS and file systems | `stat`/`open`/`close` (namecache hit and negative-entry hit), `getdirentries` on large directories, create/unlink/rename storms, sequential and random read/write on UFS2 (cold and warm cache), `fsync` latency, extracting `base.txz` onto UFS2 and onto tmpfs, `find /` over a populated tree, the fsx workload's runtime |
| **P6** Multi-user | Kernel time from loader handoff to multi-user `login:`. `_umtx_op` mutex and condvar contention (`libthr`), thread create/join, `kqueue` event throughput, UNIX-socket ping-pong and bandwidth, pty throughput, dynamic `execve` (rtld startup), a shell-script-heavy workload (`rc` itself, timed), kyua suite wall time. All earlier suites now also run on both physical machines |
| **P7** Networking | `iperf3` TCP and UDP throughput, single and multiple streams, over loopback, virtio-net, and the physical NICs. TCP request/response latency (p50/p99). TCP connection-establishment rate. `sendfile` throughput. CPU cost per gigabit at a fixed rate |
| **P8** Self-hosting | Building AbyssBSD on AbyssBSD vs on FreeBSD on the same machine (`cargo xtask build --release --arch all`, clean and incremental). `make buildkernel` of FreeBSD. UFS2 SU+J metadata-heavy workloads. Swap-in/swap-out throughput under memory pressure. Idle power draw and wake latency on the physical machines, where measurable. The full cumulative suite on all gating platforms |

## 6. Tooling

`cargo xtask --remote perf [--suite p2|diag] [--runs N] [--prepare]` runs on the build server. It
boots each kernel in an identical bhyve VM (8 vCPUs pinned one per P-core, 2 GiB, the same
devices), alternating between the two kernels, for `N` runs each (default 10). It applies the rules
above, writes raw samples, console logs, host facts, and a report to
`~/abyss/ci/perf/<time>-<rev>/`, and exits non-zero if any benchmark exceeds the bound. It takes
CI's lock, so a perf run never overlaps a CI run. The workloads and the output format are specified
in [tests/perf/README.md](../tests/perf/README.md). The FreeBSD side is the 15.1-RELEASE VM image
with `kbench.ko` (built against the release's sources, both checked against the release's
published SHA-256 sums) loaded at boot from a second disk.

The FreeBSD guest boots with `vm.pmap.pti=0`. Stock FreeBSD on the bare i7-12700KF runs without
PTI, because the CPU reports itself immune to Meltdown (`RDCL_NO` in `IA32_ARCH_CAPABILITIES`).
bhyve does not pass that register through, so a default guest turns PTI on. Its `getpid` then
costs 240 ns against 67 ns on the host, and the comparison would break the same-security-posture
rule in the other direction. The guest prints `vm.pmap.pti`, `hw.ibrs_active`, and
`hw.mds_disable_state` into its console log before each run. PTI is the only mitigation whose
default the missing register changes.

Still planned: `--platform x870e|opi6p` (P6), and `--compare <rev>`, which compares two AbyssBSD
revisions with the same statistics to check a change before it lands.
