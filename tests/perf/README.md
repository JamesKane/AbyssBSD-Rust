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
- Throughput benchmarks start all workers together, run them for 1 s of wall time, and report
  `elapsed / total operations`. Workers check a shared stop flag once per operation (or per batch).
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
