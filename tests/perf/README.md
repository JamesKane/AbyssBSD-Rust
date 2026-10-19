# In-kernel benchmarks

Until P4 there is no userland to share, so each phase's benchmarks run inside the kernel, once in
AbyssBSD and once in FreeBSD ([PERFORMANCE.md §5](../../docs/PERFORMANCE.md#5-benchmark-suites-by-phase)).
This file is the workload specification both implementations follow:

- AbyssBSD: `kernel/src/bench.rs`, run with the boot option `abyss.bench=<suite>`.
- FreeBSD: `kbench/`, a kernel module (test code; it only ever runs on FreeBSD), run with
  `sysctl debug.kbench.run=<suite>`.

`cargo xtask perf` boots both in identical bhyve VMs, interleaves the runs, and applies the
statistics in PERFORMANCE.md. A change to a workload changes both implementations in the same
commit.

## Output

One line per result, on the console:

```
kbench begin <suite> cpus=<n>
kbench <name> <value> <unit>
kbench end <suite>
```

`<value>` is a decimal number. Every unit is "larger is slower": `ns/op` (elapsed time over
operations completed, so the inverse of throughput) or `ns` (a latency percentile).

## Common rules

- The VM has 8 vCPUs. The coordinator runs bound to CPU 0. Worker *i* of *T* runs bound to CPU
  `(i + 1) mod 8`, so a one-thread benchmark never shares a CPU with the coordinator.
- Workers run at kernel thread priority (FreeBSD `PRI_MIN_KERN`, AbyssBSD `Priority::KERNEL`).
- Workers sleep until the coordinator's start signal (a spinning worker could keep another
  bound to the same CPU from starting: FreeBSD does not time-slice kernel-priority threads).
- Throughput benchmarks start all workers together, run them for 1 s of wall time, and compute
  `elapsed / total operations`. They do this three times and report the median round, which
  damps the run-to-run variation of contended benchmarks.
- Lock benchmarks (`mtx`, `rw`, `sx`) instead run on eight lock sets, each in its own page with
  each lock's counter beside it, for 250 ms each, and report the mean. A contended lock's cost
  depends on where its cache line lives (its L3 slice): on the build server the same benchmark
  measures about 50 or about 90 ns/op depending on the lock's address, which made single-lock
  results bimodal from boot to boot on both kernels. Workers check a shared stop flag once per operation (or per batch).
- Latency benchmarks collect the stated number of samples and report the 50th and 99th percentiles
  as `<name>.p50` and `<name>.p99`.
- Time comes from the CPU counter (TSC), converted with each kernel's calibrated frequency.
- Each kernel uses its idiomatic primitive for each job, with the same semantics. Where they
  differ, the table says how.

## Suite `p2`

| Name | Workers | Operation |
|---|---|---|
| `ctxsw.same` | 2 on one CPU (CPU 1) | Strict alternation: each waits until it is its turn, passes the turn, and wakes the other. One op = one handoff. FreeBSD: `mtx_sleep`/`wakeup_one` under an `MTX_DEF` mutex. AbyssBSD: `WaitQueue::wait_until`/`wake_one` |
| `ctxsw.cross` | 2, on CPUs 1 and 2 | As above, across CPUs |
| `mtx.t{1,2,4,8}` | 1, 2, 4, 8 | Lock, increment a shared counter, unlock. FreeBSD `mtx(9)` `MTX_DEF`; AbyssBSD `Mutex` |
| `rw.rd.t{1,2,4,8}` | 1, 2, 4, 8 | Read-lock, read the counter, unlock. FreeBSD `rwlock(9)`; AbyssBSD `RwLock` |
| `rw.wr.t{1,2,4,8}` | 1, 2, 4, 8 | Write-lock, increment, unlock |
| `sx.rd.t{1,2,4,8}`, `sx.wr.t{1,2,4,8}` | 1, 2, 4, 8 | As for rw, with FreeBSD `sx(9)` and AbyssBSD `SxLock` |
| `malloc.hit.t{1,8}` | 1, 8 | Allocate 256 bytes and free them at once (served by the per-CPU cache). One op = one pair. FreeBSD `malloc(9)` `M_WAITOK`; AbyssBSD the global allocator |
| `malloc.miss.t{1,8}` | 1, 8 | Allocate 4096 objects of 256 bytes, then free all 4096 (overflows the per-CPU cache). One op = one allocation and its free |
| `page.hit.t{1,8}` | 1, 8 | Allocate one physical page and free it. FreeBSD `vm_page_alloc_noobj(VM_ALLOC_WAITOK)`/`vm_page_free`; AbyssBSD `mm::alloc_pages(Order(0))`/`free_pages` |
| `page.miss.t1` | 1 | Allocate 1024 pages, then free all 1024. One op = one page |
| `callout.armcancel` | 1 | Arm a callout 1 s out, then cancel it. FreeBSD `callout_reset_sbt` (relative, default precision)/`callout_stop`; AbyssBSD `Callout::after`/`cancel` |
| `callout.latency` | 1 | 1000 samples. Arm a callout for an absolute deadline 1 ms ahead and sleep until it fires; the sample is how late the handler started. FreeBSD: `C_ABSOLUTE \| C_DIRECT_EXEC`, precision 0, so the timer path is measured, not the coalescing policy; the handler wakes the worker under a spin mutex (`msleep_spin`). AbyssBSD: `Callout::after` (always exact, interrupt context), waking a `WaitQueue` |
| `wakeup.latency` | 2: sleeper on CPU 1, waker on CPU 2 | 10000 samples. The sleeper announces it is about to sleep and sleeps. The waker waits for the announcement, spins 50 µs so the sleeper's CPU goes idle, stamps the time, and wakes it. The sample is from the stamp to the sleeper running. FreeBSD: `mtx_sleep`/`wakeup` (the announcement is made under the mutex); AbyssBSD: `WaitQueue` (the announcement is made in the condition check, under the queue lock) |
| `ipi.rtt` | 1 (CPU 1) | Run an empty function on CPU 2 and wait for it to finish. FreeBSD `smp_rendezvous_cpus` with no barriers; AbyssBSD `smp::ping` |
| `tlb.shootdown` | 1 (CPU 1) | Invalidate one kernel page on every CPU and wait. FreeBSD `pmap_invalidate_page(kernel_pmap, va)`; AbyssBSD `mm::flush_range` plus `smp::shootdown` |

## Suite `p3`

The VM also has a 64 MiB file-backed NVMe disk (`5,nvme,<file>`; bhyve's RAM-backed namespaces fail
128 KiB reads). Block benchmarks read the second
virtio-blk disk (`vtbd1`, the 16 MiB bench disk, read-only) and the NVMe namespace (`nda0`), within
their first 16 MiB. Offsets are 4 KiB-aligned, from xorshift64 seeded with 1. Requests go straight to
the disk: FreeBSD through a GEOM consumer of its own class on the provider (`g_io_request`),
AbyssBSD through `Disk::strategy`. Buffers are page-aligned.

Both sides run under the same conditions. Where the two differ by default, the benchmark matches
FreeBSD to how it really runs:
- **Direct dispatch.** The FreeBSD consumer sets `G_CF_DIRECT_SEND | G_CF_DIRECT_RECEIVE`, as
  `g_vfs` and `geom_dev` do. Without it, every request takes the `g_down` and `g_up` thread hops,
  which no file system pays.
- **Interrupt placement.** FreeBSD's `rc.local` pins the virtio and NVMe interrupts to CPU 0
  (`cpuset -x`), where AbyssBSD routes them. With FreeBSD's default spread, `blk.nda.qd32` ran
  five times slower.
- **Nothing mounted.** `vtbd1` is unmounted before the suite runs. `rc.local` copies `kbench.ko`
  off it first.
- **Same topology reading.** bhyve 15.1 virtualizes CPUID leaf 0xB but passes leaf 0x1F through
  from the host, whose P-cores have SMT. FreeBSD prefers 0x1F, so it sees SMT pairs, and ULE never
  idle-spins on an SMT sibling. AbyssBSD reads the same leaf and applies the same rule. Before that
  fix, AbyssBSD idle-spun where FreeBSD halted, and much of its QD1 lead was halted-vCPU wake-up
  time on FreeBSD's side.
- **Console muted for attach.** Both kernels print attach lines to an emulated UART that costs about
  a millisecond a line. `dev.vtblk.*` runs with the console muted on both sides (FreeBSD `cn_mute`).

| Name | Workers | Operation |
|---|---|---|
| `blk.<d>.qd1` | 1 (CPU 1) | 10000 random 4 KiB reads, one at a time, waiting for each. Latency samples, p50 and p99. FreeBSD `biowait`; AbyssBSD a sleeping waiter |
| `blk.<d>.qd32` | 1 (CPU 1) | Random 4 KiB reads with 32 outstanding: each completion issues the next, until the stop. One op = one completed read. On both sides the completion hands the request to the worker, which resubmits it. With direct dispatch the completion runs in the driver's interrupt thread, and resubmitting there never lets that thread sleep |
| `blk.<d>.seq` | 1 (CPU 1) | Sequential 128 KiB reads, one at a time, wrapping at 16 MiB. One op = one read |
| `aml.sta_crs` | coordinator | Evaluate `_STA` and `_CRS` of every namespace device (`_CRS` decoded into resources). One op = one node; the median of three passes. The nodes are every device plus the predefined `\_SB` and `\_TZ` scopes, which ACPICA types as devices. FreeBSD `AcpiWalkNamespace`, `AcpiEvaluateObjectTyped`, `AcpiGetCurrentResources`; AbyssBSD the AML service |
| `dma.map` | 1 (CPU 1) | Map a page-aligned 4 KiB buffer for a device read, then unmap it. FreeBSD `bus_dmamap_load`, `bus_dmamap_sync` (PREREAD, POSTREAD), `bus_dmamap_unload` on a tag with no constraints; AbyssBSD `DmaTag::map` and `Mapped::unmap` on an unconstrained tag |
| `gate.t<T>` | 1, 8 | Enter and leave a driver's revocation gate. FreeBSD's counterpart is the reference devfs takes around a cdevsw call: `dev_refthread`, `dev_relthread` on a cdev of the module's own |
| `dev.vtblk.attach` | coordinator | Probe and attach vtblk1's transport (`virtio_pci`), until GEOM (FreeBSD `g_waitidle`) or the device manager (AbyssBSD `wait_idle`) is idle. Median of 16 cycles |
| `dev.vtblk.cycle` | coordinator | Detach that transport, wait for idle, attach it again, wait for idle. Median of 16 cycles. Runs last, since it re-creates vtbd1 |

`<d>` is `vtbd` or `nda`.

## Suite `diag`

For investigation, not gating: `cargo xtask perf --suite diag`. AbyssBSD reports the cost of the
pieces its fast paths are made of (interrupt mask and unmask, `cpu_id`, `current`, an APIC register
read and write, a batch of allocations). Both kernels report:

| Name | Operation |
|---|---|
| `diag.ping_idle` | `ipi.rtt` to an idle CPU 2 |
| `diag.ping_busy` | `ipi.rtt` while CPU 2 runs a spinning kernel thread, so no halted CPU has to wake |
