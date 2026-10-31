# Benchmarks

Until P4 there is no userland to share, so the P2 and P3 benchmarks run inside the kernel, once in
AbyssBSD and once in FreeBSD ([PERFORMANCE.md §5](../../docs/PERFORMANCE.md#5-benchmark-suites-by-phase)).
From P4 on they are userland programs, the same binary on both kernels (suite `p4`). This file is
the workload specification every implementation follows:

- AbyssBSD: `kernel/src/bench.rs`, run with the boot option `abyss.bench=<suite>`.
- FreeBSD: `kbench/`, a kernel module (test code; it only ever runs on FreeBSD), run with
  `sysctl debug.kbench.run=<suite>`.
- Both, from P4: `ubench/`, a static FreeBSD binary (built again with threads for `p6`).

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
| `intr.latency` | coordinator (CPU 0) | 1000 samples: arm an HPET comparator 20 µs ahead, delivered as an MSI to CPU 0, and spin until the handler runs; latency = handler TSC − (TSC at arming + 20 µs), p50 and p99. FreeBSD the per-CPU `HPET` event timer (`et_init`, `et_start`; the callback runs in the HPET filter, LAPIC being the active event timer); AbyssBSD programs a timer with FSB delivery and installs a filter through `irq::install` (its `hpet` driver claims nothing). x86 only |
| `aml.load` | coordinator | Load QEMU q35's DSDT, moved under `\KBNS` and compiled as an SSDT (`kbench/ssdt-q35.dsl`, made by `kbench/mkssdt.py`), beside the firmware's tables. Median of 16 loads. FreeBSD `AcpiLoadTable` into its live namespace, then `AcpiUnloadTable` (each copy has its own OEM revision); AbyssBSD `load_table` into a fresh interpreter holding the firmware's tables, loaded untimed |
| `dma.map` | 1 (CPU 1) | Map a page-aligned 4 KiB buffer for a device read, then unmap it. FreeBSD `bus_dmamap_load`, `bus_dmamap_sync` (PREREAD, POSTREAD), `bus_dmamap_unload` on a tag with no constraints; AbyssBSD `DmaTag::map` and `Mapped::unmap` on an unconstrained tag |
| `gate.t<T>` | 1, 8 | Enter and leave a driver's revocation gate. FreeBSD's counterpart is the reference devfs takes around a cdevsw call: `dev_refthread`, `dev_relthread` on a cdev of the module's own |
| `dev.vtblk.attach` | coordinator | Probe and attach vtblk1's transport (`virtio_pci`), until GEOM (FreeBSD `g_waitidle`) or the device manager (AbyssBSD `wait_idle`) is idle. Median of 16 cycles |
| `dev.vtblk.cycle` | coordinator | Detach that transport, wait for idle, attach it again, wait for idle. Median of 16 cycles. Runs last, since it re-creates vtbd1 |

`<d>` is `vtbd` or `nda`.

## Suite `p4`

From P4 on, benchmarks run in userland: `ubench/ubench.c`, one static FreeBSD binary built on the
server (`cargo xtask perf` builds it against the release's `base.txz`), run unchanged on both
kernels as `/tmp/ubench p4`. Until AbyssBSD runs it, `cargo xtask perf --suite p4 --freebsd-only`
records FreeBSD's baseline.

- `ubench` binds itself to CPU 1 (`cpuset_setaffinity`), and the processes it creates inherit
  that. Unbound, FreeBSD put each freshly exec'd child on an idle CPU, and `proc.vfork_exec`
  measured that CPU's wake-up and cold caches (550 µs on the build server, against 100 µs bound).
- The pipe benchmarks' partner process binds itself to CPU 2.
- Every benchmark is a throughput benchmark: three rounds of 1 s, median round, time from
  `clock_gettime(CLOCK_MONOTONIC)`.

| Name | Operation |
|---|---|
| `syscall.getpid` | `getpid()` (libc does not cache it) |
| `io.null.write` | Write 1 byte to `/dev/null` |
| `io.zero.read` | Read 4 KiB from `/dev/zero` |
| `proc.fork` | `fork`; the child `_exit`s at once; the parent reaps it with `wait4` |
| `proc.vfork_exec` | `vfork`; the child `execve`s `ubench exit`, which returns from `main`; `wait4` |
| `proc.exec` | A child `execve`s `ubench` 100 times in a row, each image counting down; the last writes the elapsed time to a pipe. One op = one `execve` (and the image's libc start-up), with the child's `fork` and exit outside the timed span |
| `vm.fault.anon` | `mmap` 64 MiB of anonymous memory, write one byte per 4 KiB page, `munmap`. One op = one page |
| `vm.fault.cow` | The parent writes every page of a 16 MiB anonymous region once, then repeatedly forks a child that writes every page again and reports the time its writes took. One op = one copy-on-write fault |
| `vm.mmap` | `mmap` one anonymous page, untouched, and `munmap` it |
| `sig.deliver` | `kill(getpid(), SIGUSR1)` with a handler installed: delivery on return from `kill`, the handler, and `sigreturn` |
| `pipe.pingpong` | One byte to the partner over one pipe and back over another. One op = one round trip |
| `pipe.bw` | 64 KiB `write`s to a partner that reads and discards. One op = one write |

## Suite `p5`

The file systems, in the same `ubench` binary: `/tmp/ubench p5`, as root. It formats the VM's NVMe
scratch disk (`/rescue/newfs -U /dev/nda0`: UFS2 with soft updates, FreeBSD's default without a
journal), mounts it on `/bench` and a tmpfs on `/benchtmp` with `nmount`, and runs every benchmark
on each, named `fs.ufs.*` and `fs.tmpfs.*`. The bench disk also carries `tree.tar` (`base.txz`'s
`/usr/share` and `/usr/include`, about 25,600 files and 124 MiB, uncompressed so the benchmark
measures the file system rather than xz) and `fsx` (built from the release's `src.txz`; it is under
the APSL and is not kept in this repository). Throughput benchmarks follow the `p4` rules; the
others run three rounds and report the median.

| Name | Operation |
|---|---|
| `stat.hit` | `stat` of `<fs>/a/b/c/file` (a name-cache hit). One op = one `stat` |
| `stat.miss` | `stat` of `<fs>/a/b/c/missing`, which fails with `ENOENT` (a negative-entry hit) |
| `open_close` | `open(O_RDONLY)` and `close` of the `stat.hit` file |
| `readdir` | `getdirentries` over a 10,000-entry directory with a 64 KiB buffer. One op = one entry |
| `create`, `rename`, `unlink` | Per round, 1,000 files created with `O_CREAT \| O_EXCL` and closed, then each renamed in the same directory, then each unlinked, timed apart. One op = one file |
| `seq_write` | Rewrite a 64 MiB file from offset 0 in 64 KiB `write`s (opened with `O_TRUNC`, no `fsync`). One op = one write |
| `seq_read.warm` | Sequential 64 KiB `pread`s of the 64 MiB file, which is in the cache |
| `seq_read.cold` | UFS only: unmount and mount `/bench`, then read the whole file in 64 KiB `read`s |
| `rand_read`, `rand_write` | 4 KiB `pread`/`pwrite` at random page-aligned offsets of the 64 MiB file |
| `fsync` | `pwrite` 4 KiB at offset 0 of a 1 MiB file, then `fsync` |
| `extract` | `/rescue/tar -xf tree.tar` into an empty directory (removed with `rm -rf` before each round, untimed). One op = one extraction |
| `find` | One `fts` walk of the extracted tree with `FTS_PHYSICAL`, every entry `stat`'ed. One op = one walk |
| `fsx` | `fsx -q -N 10000 -S <round>` on a file in the file system. One op = one run |

## Suite `p6`

Multi-user's components ([MULTIUSER.md §16](../../docs/MULTIUSER.md#16-performance)): `ubench p6`,
from a second build of the same source with `-DUB_P6` and `libthr`, so the earlier suites' binary
stays single-threaded and unchanged. The component is each name's first field.

All of `p6` runs multi-user: each run boots the root as the installer leaves it (FreeBSD's release
image with the multi-user boot cases' `rc.conf`, and AbyssBSD's installed root with the same), on
NVMe, with the bench disk on virtio-blk (`vtbd0`). The harness times the boot on the serial console,
logs in as root there, and runs the bench disk's `run.sh`: device interrupts to CPU 0, as for the
single-user suites, `/tmp/ubench p6` (the exec benchmarks run base's own dynamic programs), and
`shutdown -p now`. `p6.thr`, `p6.event`, `p6.unix`, and `p6.pty` run one component each from the
static `ubench`, single-user, as the earlier suites do.

- Team benchmarks (`thr.mutex*`, `thr.rwlock`) follow kbench's rules: worker *i* of *T* bound to
  CPU `(i + 1) mod 8`, the coordinator on CPU 0; the workers start together at a barrier, run until
  the coordinator's stop after 1 s, and the result is elapsed time over the total operations,
  median of three rounds.
- Ping-pong benchmarks pair `ubench` (CPU 1) with a partner thread or process on CPU 2. One op =
  one round trip.
- Everything else follows the `p4` rules: `ubench` on CPU 1, three rounds of 1 s, median.

| Name | Operation |
|---|---|
| `thr.mutex.t{1,2,4,8}` | Lock a default `pthread_mutex`, increment a shared counter, unlock |
| `thr.mutex.pi.t{1,2,4,8}` | As above, `PTHREAD_PRIO_INHERIT` |
| `thr.mutex.pshared.t{1,2,4,8}` | As above, `PTHREAD_PROCESS_SHARED` (libthr keeps the lock in a shared off-page: `UMTX_OP_SHM`, shared umtx keys) |
| `thr.rwlock.t{1,2,4,8}` | Each worker's every tenth op takes the `pthread_rwlock` for writing and increments; the other nine read. One op = one lock and unlock |
| `thr.cond.pingpong` | Strict alternation under one mutex, a `pthread_cond` per side |
| `thr.sem.pingpong` | Strict alternation with two unnamed process-private semaphores (`sem_post`, `sem_wait`) |
| `thr.create` | `pthread_create` of a thread that returns at once, then `pthread_join` |
| `event.kevent.pipe1` | One pipe's read end in a kqueue: write a byte, `kevent` returns the event, read the byte |
| `event.kevent.pipe1000` | As above with 1,000 pipes registered, a byte written to each in turn |
| `event.poll1000` | The 1,000 read ends in one `poll` set; a byte written to each in turn, `poll`, read. One op = one `poll` |
| `event.select1000` | As above with `select` (2,000 descriptors open; `FD_SETSIZE` 4096) |
| `event.user.pingpong` | Two kqueues, an `EV_CLEAR` `EVFILT_USER` event in each: each side triggers the other's (`NOTE_TRIGGER`) and waits for its own |
| `event.timer` | Register an `EV_ONESHOT` `EVFILT_TIMER` 1 µs out (`NOTE_USECONDS`; FreeBSD arms it with precision 0) and wait for it in the same `kevent` call |
| `unix.stream.pingpong`, `unix.seqpacket.pingpong` | One byte over a `socketpair` to the partner process and back |
| `unix.stream.bw4k`, `unix.stream.bw64k` | 4 KiB or 64 KiB `write`s to a partner that reads and discards. One op = one write |
| `unix.dgram` | Windows of 16 datagrams of 64 bytes over a `SOCK_DGRAM` pair; the partner answers each window with one byte (a full UNIX datagram socket fails the send with `ENOBUFS`, `uipc_sosend_dgram`). One op = one datagram |
| `unix.rights` | `sendmsg` of one byte with `/dev/null`'s descriptor (`SCM_RIGHTS`); the partner receives and closes it |
| `unix.connect` | `socket`, `connect` to a listening socket bound in `/tmp`, read the partner's one-byte greeting once it has accepted, `close`. Unpaced, the client overflows the listen queue, and a UNIX connect to a full queue is refused |
| `pty.raw` | A `posix_openpt` pair, the slave in raw mode (`cfmakeraw`): 4 KiB `write`s to the slave, the partner reads the master. One op = one write |
| `pty.canon` | Canonical mode without echo: 81-byte lines (80 characters and a newline) written to the master, the partner reads the slave a line at a time. One op = one line |
| `pty.relay` | `pty.raw` with a relay between, as `script` is: a process on CPU 3 `poll`s the master and its standard input and copies the master to a pipe, which the partner drains |
| `exec.true` | `vfork`, `execve` of `/usr/bin/true` (rtld, `libc`, `libsys`), `wait4` |
| `exec.lib10` | As above with `/usr/bin/tar --version`: `libarchive` and ten more libraries |
| `exec.rcsubr` | As above with `/usr/sbin/service -e`: a shell per `rc.d` script, each loading `rc.subr` and `rc.conf` |
| `boot.kernel` | Not in `ubench`: the harness's time from the kernel's first console line (FreeBSD's `---<<BOOT>>---`, AbyssBSD's banner), just after the loader's handoff, to the first `login:` on the serial console |
| `boot.rc` | Not in `ubench`: from `Trying to mount root from`, which both kernels print as they mount the root and start init, to that `login:` |

## Suite `p7`

Networking's components ([NETWORKING.md §17](../../docs/NETWORKING.md#17-performance)): `tcp`,
`udp`, and `sendfile`, each name's first field. Like `p6`, each run boots the installed root
multi-user from NVMe (FreeBSD's release image with the network boot cases' `rc.conf`, AbyssBSD's
installed root with the same), with a DHCP `vtnet0` on a tap of the build server's test bridge
(docs/NETWORKING.md §15), logs in as root on the serial console, and runs the bench disk's `run.sh`
(`xtask/src/perf/p7.rs`). The server is the peer, at `10.77.0.1`: for the whole command it runs
`iperf3 -s` and `ubench p7.peer 10.77.0.1`, pinned to its physical cores 4 to 7. The VM has
**4 vCPUs**, pinned to cores 0 to 3, so the guest, the peer, and bhyve's device emulation do not
share a core and neither end is the other's bottleneck.

`iperf3` is FreeBSD's package (3.22, its SHA-256 recorded in `xtask/src/perf/p7.rs`), the same
build in both guests and on the server; it is not part of base, so the bench disk carries the
binary and `libiperf.so.0`. Device interrupts go to CPU 0, as in every suite; `iperf3` runs
unbound, and `ubench` on CPU 1.

| Name | Operation |
|---|---|
| `tcp.rr.lo.p50`, `.p99` | One byte to an echo and back over a `TCP_NODELAY` connection on `127.0.0.1`, the echo a child on CPU 2: 20,000 round trips after 1,000 unmeasured |
| `tcp.rr.net.p50`, `.p99` | As above, to the server's echo |
| `tcp.connect.lo`, `tcp.connect.net` | `socket`, `connect`, read the peer's one-byte greeting, `close`. The peer closes first, so `TIME_WAIT` is on its side. Throughput rules as `p4`: three rounds of 1 s, median |
| `tcp.lo.s1`, `tcp.lo.s4` | `iperf3` over loopback in the guest, one stream and four: 10 s after 2 s omitted (`-t 10 -O 2`). One op = 1 KiB at the receiver's rate |
| `tcp.tx.s1`, `tcp.tx.s4` | As above, the guest sending to the server |
| `tcp.rx.s1`, `tcp.rx.s4` | As above, the server sending to the guest (`-R`) |
| `tcp.cpu.tx`, `tcp.cpu.rx` | One stream paced at 1 Gbit/s for 20 s (`-b 1000M`); the guest's busy CPU time over the run (`kern.cp_time`'s user, nice, system, and interrupt ticks over `stathz`) per KiB received |
| `udp.tx.bulk`, `udp.rx.bulk` | `iperf3 -u -b 0 -l 1448`, unpaced: 1,448-byte datagrams, each direction. One op = 1 KiB at the receiver's rate |
| `udp.tx.small`, `udp.rx.small` | As above with 64-byte datagrams. One op = one datagram received |
| `sendfile.plain` | A 64 MiB file just written (so in the page cache) sent whole with `sendfile` over one connection to the server's sink, which reads and discards. One op = 1 KiB; three rounds of 1 s, median |
| `sendfile.ktls` | As above on a connection with kernel TLS on its transmit side (`TCP_TXTLS_ENABLE`): TLS 1.2, AES-128-GCM, a fixed key. Stock FreeBSD encrypts with `aesni` (in `GENERIC`); the sink discards the records unread |

## Suite `vt`

Text output to the virtual terminals ([USB.md §10](../../docs/USB.md#10-performance)), in the same
`ubench` binary: `ubench vt`, as root, in the bhyve VM with its 1024×768 framebuffer (`fbuf`), which
both kernels' `vt` draw on in 8×16 cells (128×48). It opens `/dev/ttyv1` and `/dev/ttyv2` (`vt`
switches only to an open window), makes window 2 visible with `VT_ACTIVATE` and `VT_WAITACTIVE`,
and writes with the terminals' default settings (so `ONLCR` adds a CR to each NL). Each operation
is one `write` of 64 lines; the unit is one line. Throughput rules as `p4`.

| Name | Operation |
|---|---|
| `vt.cat` | 64 lines of 80 printable characters to the visible `ttyv1` |
| `vt.scroll` | 64 lines of 10 characters (`line NNNNN`), each one scrolling the screen once it is full |
| `vt.color` | 64 lines of 20 groups of an SGR foreground change (`ESC [ 3n m`) and four characters |
| `vt.hidden` | `vt.cat`'s lines to `ttyv2`, which is not visible |

## Suite `diag`

For investigation, not gating: `cargo xtask perf --suite diag`. AbyssBSD reports the cost of the
pieces its fast paths are made of (interrupt mask and unmask, `cpu_id`, `current`, an APIC register
read and write, a batch of allocations). Both kernels report:

| Name | Operation |
|---|---|
| `diag.ping_idle` | `ipi.rtt` to an idle CPU 2 |
| `diag.ping_busy` | `ipi.rtt` while CPU 2 runs a spinning kernel thread, so no halted CPU has to wake |
