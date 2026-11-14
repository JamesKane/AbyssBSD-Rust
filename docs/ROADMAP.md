# Roadmap

The roadmap is a sequence of **phases with hard exit criteria**. A phase is done when its exit
criteria pass in CI on **both AMD64 and ARM64**, not when the code is written. Sizes are relative
(S < M < L < XL) rather than calendar estimates.

**Performance parity** ([ADR-0011](adr/0011-performance-parity.md),
[ADR-0014](adr/0014-performance-parity-per-component.md)). From P2 on, every exit criterion includes
the phase's benchmark suite: each component it measures is, overall, **no more than 2% slower**
than stock FreeBSD 15.1 on the same machine and configuration, and the goal is to be faster. Suites are cumulative and run nightly after the phase that introduces them. Method,
fairness rules, and the full benchmark list: [PERFORMANCE.md](PERFORMANCE.md). Before P8 the
gating platform is bhyve (amd64). aarch64 numbers are recorded, but only gate from P8, when real
ARM64 hardware is in the loop.

**QEMU first** ([ADR-0010](adr/0010-qemu-first-development.md)). Through P5, development and every
exit criterion run under QEMU (plus bhyve for amd64). **No physical hardware is touched until
FreeBSD userland runs end to end in QEMU**: no smoke checks, no table captures. That is P6's
exit. P7 adds networking, still in QEMU and bhyve, so the machines come up with `ssh` and `scp`;
P8 brings up the Orange Pi 6 Plus, P9 hardens it, and P10 brings up the X870E; each machine is
**gating** from its bring-up on.

```
P0 Foundations ─► P1 Boot ─► P2 Core kernel ─► P3 Devices & ACPI ─► P4 First userspace
                                                      │                    │
                                                      │                    ▼
                                                      │             P5 VFS & file systems
                                                      │                    │
                                                      ├─(parallel)─► USB/HID track
                                                      ▼                    ▼
                                                 P6 FreeBSD multi-user ◄──┘
                                                      │
                                                      ▼
                                                 P7 Networking
                                                      │
                                                      ▼
                                                 P8 Board bring-up (Sky1) ─► P9 Sky1 hardening ─► P10 X870E bring-up
                                                                                                         │
                                                                                                         ▼
                                                                        P11 Self-hosting & hardening = v1.0 (Day 1)
                                                                                │
                                                                                ▼
                                                                         P12 RV64 (Day 2)
                                                                                │
                                              ┌─────────────────────────────────┼──────────────────┐
                                              ▼                                 ▼                  ▼
                                    M-U Userland milestone        M-G Graphics milestone   M-D Desktop milestone
```

---

## Phase 0: Foundations (S) — done 2026-10-17

**Goal:** an empty but complete engineering loop.

- Cargo workspace, `rust-toolchain.toml` (pinned stable, edition 2024, the four targets), lint
  configuration, `rustfmt.toml`, and CI policy checks (no external deps, `SAFETY:` comments).
- `xtask` skeleton: `build`, `run`, `test`, `image` (FAT32 ESP via our own `lib/fat` writer),
  and `--remote` execution on the build server.
- `tools/syscallgen` producing `lib/abi` syscall tables from `../freebsd-src/sys/kern/syscalls.master`.
- `cargo xtask abi-check` pipeline proven on a handful of structs (`timespec`, `stat`, `sigaction`).
- Build server provisioned (rustup, qemu, edk2, bare git repo, CI hook).
- In-house test harness for kernel tests (`#[ktest]` collection via linker section).

**Exit:** `cargo xtask test --tier unit` passes on both hosts. `cargo xtask run --arch {x86_64,aarch64}`
boots a stub UEFI app that prints "AbyssBSD" on QEMU.

## Phase 1: Boot to Rust on both architectures (M) — done 2026-10-18

**Goal:** our loader hands off to our kernel, which owns the machine.

- `loader-efi`: file loading from the ESP, ELF loading with KASLR, memory map, GOP, RSDP, SMBIOS,
  runtime services table, `bootproto::BootInfo` v1, initial page tables, `ExitBootServices`.
- Kernel entry per arch: stack, BSS, per-CPU block, trap/exception vectors (with full register
  dumps), early serial (16550 / PL011 from SPCR, or DBG2 when there is no SPCR) and framebuffer
  text console. On arm64, EL2 entry with an EL2 stub and a drop to EL1.
- Panic handler with a symbolized backtrace. Kernel `log!` facility, mirrored into a persistent RAM ring
  that the loader saves to the ESP on the next boot (the X870E has no serial port).
- Physical memory map ingestion. Kernel's own page tables with the DMAP. W^X kernel mappings.
- `#[ktest]` runner, and QEMU exit-code signalling.

**Exit:** both arches boot in QEMU to a kernel banner on serial and framebuffer, run kernel unit
tests, deliberately take and report a page fault, and exit cleanly. Boot also works on bhyve.

## Phase 2: Core kernel (L) — done 2026-10-18

**Goal:** SMP, preemptive, timed, and memory-managed.

- Physical allocator (buddy, NUMA-aware data structures even if single-node). Slab/zone allocator
  with per-CPU caches, used as `GlobalAlloc`.
- Kernel virtual memory: `kmem` arena, guard-paged kernel stacks.
- ACPI static tables: MADT, FADT, MCFG, SPCR, HPET, GTDT, SRAT, SLIT, PPTT.
- Interrupt controllers: x2APIC/IOAPIC; GICv3 distributor and redistributors.
- Timekeeping: timecounters (TSC / generic counter), event timers, callouts, tickless idle.
- SMP bring-up (INIT-SIPI / PSCI), per-CPU data, IPIs, TLB shootdown.
- Synchronization: spin, mutex (turnstiles with priority propagation), rw, sx, epoch, wait
  queues. Lock-level types and runtime witness.
- Scheduler v1 (ULE-like, kernel threads only), preemption, CPU topology including asymmetric
  capacities (the Orange Pi 6 Plus has three core classes).
- CPU errata framework (MIDR/CPUID-keyed). Investigate GIC erratum 2941627 for Sky1.
- FP/SIMD context switching including SVE.

**Exit:** a stress test of 64 kernel threads doing lock ping-pong, allocator churn, and sleeps with
timeouts runs for 10 minutes on `-smp 8` on both arches, with no witness violations and no leaks
(allocator accounting balances). The P2 performance suite (context switch, locks, zone and page
allocators, callouts, IPIs and TLB shootdown) is within 2% of FreeBSD's `kbench` module on bhyve.

**As delivered.** `cargo xtask test --tier stress` runs the exit test on both architectures
(10 minutes, 8 CPUs, 64 threads). The threads do lock ping-pong, ordered multi-lock
contention, sx locking, allocator churn, timed sleeps, and FP/SIMD register checks. The run
passes with no witness violations and with heap, page, and kernel-stack accounting balanced.
Where Phase 2 departs from the plan above:

- **Lock order:** levels are declared per lock class and enforced by the runtime witness on
  every acquisition, in all builds. Compile-time level tokens are not built yet.
- **NUMA:** the allocator keeps per-domain free lists, but domains from the SRAT are not wired
  in yet. Every machine so far is one node.
- **Interrupt controllers:** the GICv3 ITS and I/O APIC routing come with MSI and devices in
  Phase 3. HPET is parsed but unused; the TSC is calibrated against the ACPI PM timer.
- **CPU capacities:** set by core type until CPPC (`_CPC`, Phase 3) supplies real performance
  levels.
- **GIC erratum 2941627:** the errata framework and the GICD_IIDR boot log are in place. The
  erratum itself is to be assessed against Arm's published errata notice at Sky1 bring-up
  (P6), since only that hardware can confirm it.
- **Performance (measured in P3.0).** `cargo xtask perf` compares the P2 suite with FreeBSD
  15.1's `kbench` module in identical pinned bhyve VMs, 10 runs per side. No benchmark exceeds
  the bound: 16 are faster with a confidence interval below 1.0, 2 tie, and the geometric mean
  ratio is 0.63. 18 contended or latency benchmarks vary by more than 1% from run to run on the
  FreeBSD side (up to 11%), so by PERFORMANCE.md's rule they cannot resolve 2%. One of those,
  `rw.rd.t4`, has a median ratio above 1.02 (1.036, with a confidence interval spanning 1.0).
  Getting there took a round of fixes:
  - critical sections in place of interrupt masking;
  - depot magazines in the heap and per-CPU page caches;
  - caller-owned timers with no allocation;
  - lazy timer reprogramming;
  - lock-word rw/sx locks and FreeBSD-style mutex spinning;
  - ULE's idle spin.
  The aarch64 numbers are not yet recorded.

## Phase 3: Device framework, PCIe, ACPI AML, storage (L/XL) — done 2026-10-19

**Goal:** discover and drive real hardware through the driver model of
[DRIVER-MODEL.md](DRIVER-MODEL.md) ([ADR-0012](adr/0012-driver-model.md)).

**Scope.** Everything in DRIVER-MODEL.md §16's P3 row, plus the AML interpreter, platform services,
and the storage drivers below. Two items move earlier than DRIVER-MODEL.md §16 placed them: the
threaded-only interrupt domain for ACPI GED (the aarch64 power button needs it), and the kernel
sysctl tree (`hw.bus.*` and `dev.*` need a home before the `__sysctl` syscall arrives in P4).

**Reference machines.** Every step is tested on the same device configurations:
- **x86_64:** QEMU `q35`.
- **aarch64:** QEMU `virt`.
- **bhyve:** on the build server.

Each configuration has virtio-blk, two NVMe controllers (one with two namespaces), an AHCI disk, and
virtio-rng. FreeBSD 15.1 guests on the build server, in bhyve and QEMU with the same configurations,
supply two things: `kbench` baselines, and recorded `devinfo -rv`, `sysctl dev`, `pciconf -lv`, and
devd event logs for differential tests.

### Milestones

What each milestone delivered, with its measurements and the fixes made on the way, is in git
(`git log`; this file as of `ad67714` holds the full record) and in the design documents
named above.

**Exit:** on both arches:
- The kernel enumerates PCIe through the device manager and attaches NVMe, AHCI, and virtio-blk
  with FreeBSD's device names and units, matching the FreeBSD recordings.
- It reads the GPT and dumps partition tables.
- It sustains a multi-queue random-read stress test.
- It passes the detach stress test with balanced ledgers.
- It powers off via ACPI or PSCI when the power button is pressed (QEMU `system_powerdown`), after
  quiescing devices in reverse dependency order.

Also required:
- The AML corpus and `lib/devtree` pass on the host.
- CI runs with the verifier on.
- The P3 performance suite is within 2% of FreeBSD on bhyve.

**As delivered.** Every exit criterion runs in CI on both architectures (`cargo xtask test --tier
boot`, local QEMU and the build server's bhyve, verifier on in every case):
- PCIe enumeration attaches `nvme`, `ahci`, and `vtblk` under FreeBSD's names and units, and the
  differential tests match the bhyve recordings line for line.
- GPT tables are read and dumped for every disk.
- Every disk takes 256 concurrent requests across the per-CPU queues, and the detach stress test
  (vtblk1 and nvme0 detached under random reads) passes with balanced ledgers.
- `system_powerdown` runs `_PTS(5)`, quiesces devices in reverse dependency order, and powers off
  through ACPI S5 or PSCI.
- The AML corpus and `lib/devtree` pass on the host.
- The P3 suite: all 17 benchmarks faster than FreeBSD on bhyve (geometric mean 0.34, P3.9).

Carried into P4.0: the aarch64 P2 and P3 numbers (recorded under TCG, not gating) are still not
recorded, and the two known issues of P3.9 stay open.


## Phase 4: First userspace: `/rescue/sh` (L) — done 2026-10-19

**Goal:** an unmodified, statically linked FreeBSD binary runs.

- VM: `VmMap`/`VmObject`/COW shadow chains, page-fault handler, `mmap`/`munmap`/`mprotect`/`madvise`,
  user-copy primitives (`UserPtr<T>`), and ASLR.
- Processes and threads: `fork` (COW), `execve` (ELF64, FreeBSD branding, auxv, shared page,
  ps_strings), `exit`/`wait4`, sessions and process groups.
- Syscall entry per arch with generated dispatch. Tier A syscall set
  ([COMPATIBILITY.md §2.1](COMPATIBILITY.md#21-system-calls)).
- Signals: delivery, frames, `sigreturn`, masks, default actions.
- Minimal VFS over a **memory disk** (preloaded image) with a read-only in-memory FS, plus devfs
  with `console`, `null`, `zero`, and `random`.
- TTY layer v1 on the serial console (line discipline, termios, job control signals).

**Reference configuration.** The P3 machines (QEMU `q35` and `virt`, bhyve). Userland is
FreeBSD 15.1's `base.txz` for each architecture, fetched and unpacked on the build server; its
`/rescue` (one static, hard-linked binary) goes into a memory-disk image that the loader preloads.

### Milestones

What each milestone delivered, with its measurements and the fixes made on the way, is in git
(`git log`; this file as of `ad67714` holds the full record) and in the design documents
named above.

**Exit:** both arches boot to an interactive `/rescue/sh` (from FreeBSD 15.1 `base.txz`) on the
serial console. Pipelines, job control (`^Z`, `fg`), `ls`, `cat`, `sysctl kern.ostype`, and
`/rescue/ps` work. A boot-to-shell expect test is in CI. The P4 performance suite (syscall, process,
VM-fault, signal, and pipe microbenchmarks, run as the same static binaries on both kernels) is
within 2% of FreeBSD.

## Phase 5: VFS and file systems (L) — done 2026-10-23

**Goal:** real persistent storage.

- Full VFS: vnode lifecycle, namecache (with negative entries), mount table, `nmount`, permissions
  and ACL hooks, `stat`/`statfs`/`getdirentries` fidelity, and file locking (`flock`, `fcntl`
  advisory locks).
- Unified buffer/page cache, write-back with a syncer thread, and `fsync` semantics.
- Storage stack: `BlockDevice`, GPT providers, FreeBSD device naming (`nda*`, `ada*`, `vtbd*`).
- **tmpfs** and **devfs** (complete).
- **msdosfs** read/write (FAT12/16/32, LFN). Mount the ESP at `/boot/efi`.
- **UFS2** read, then write (without soft updates; clean unmount required), then **soft updates**.
  **SU+J** follows in Phase 11.
- Validation: images written by AbyssBSD must pass FreeBSD `fsck_ffs -f` and `fsck_msdosfs` on the
  build server with zero errors, under a randomized-workload stress test.

**Exit:** both arches boot with the root on UFS2 on NVMe and `/rescue/sh` as init. A file-system
stress workload (fsx-style, run unmodified from FreeBSD's tests) completes, and the image passes
`fsck_ffs` afterwards. The loader can read the kernel from UFS2. The P5 performance suite (namecache,
directory, UFS2 and tmpfs I/O, `fsync`, `base.txz` extraction) is within 2% of FreeBSD for each file
system overall ([ADR-0014](adr/0014-performance-parity-per-component.md)).

**Design.** [VFS.md](VFS.md) ([ADR-0013](adr/0013-vfs.md)): typed memory and use references to
vnodes, the protocol in the generic layer, one page cache for file data, a buffer cache for
metadata, and the on-disk engines in host-tested `lib/` crates.

**Reference configuration.** The P4 machines (QEMU `q35` and `virt`, bhyve), each with an NVMe disk
holding a GPT with a FAT ESP and a UFS2 partition made on the build server with `makefs`/`newfs`.
The FreeBSD 15.1 guests use the same disks for the baselines.

### Milestones

What each milestone delivered, with its measurements and the fixes made on the way, is in git
(`git log`; this file as of `ad67714` holds the full record) and in the design documents
named above.

## Track: USB and input (parallel, starts after P3) (M) — in progress

**Goal:** the machine can be used from its own screen and keyboard, and boots or mounts from a USB
disk.

- xHCI (rings, contexts, MSI-X), hub driver, device enumeration, HID boot protocol, then report
  parser (keyboard, mouse), USB mass storage (BBB, then UAS), and virtio-input.
- Framebuffer virtual terminals (`ttyv*`) with a `vt(4)`-compatible keyboard layer.

**Exit (merges into P6):** a physical machine without a serial port can be driven entirely from the
GOP console and a USB keyboard. Physical machines stay untouched until FreeBSD userland runs end to
end in QEMU ([ADR-0010](adr/0010-qemu-first-development.md)), so the track exits in two steps:
- **In QEMU, now:** on both arches, `/rescue/sh` on `ttyv0` is driven entirely by a USB keyboard
  (keystrokes injected over QMP, the result read back from the screen), with a USB mouse and a
  virtio keyboard present and working. A UFS2 file system on a USB disk (`da0`) passes fsx and then
  `fsck_ffs`, over BBB and over UAS. The amd64 bhyve configuration (`xhci,tablet`) attaches cleanly.
- **On hardware, at P6 bring-up:** the sentence above, on both reference machines, with the X870E's
  PCI xHCIs and the Sky1's ACPI (`PNP0D10`) ones.

**Design.** [USB.md](USB.md) ([ADR-0015](adr/0015-usb-input-vt.md)): xHCI rings whose types say who
owns each slot, transfers completed exactly once, interfaces as the children drivers match, HID as
a bus, one input class under kbd and evdev, and `vt` over a pure terminal emulator.

**Behavior references.** FreeBSD's `sys/dev/usb` (controller/xhci.c, usb_hub.c, input/usbhid.c,
storage/umass.c), `sys/dev/hid` (hidbus.c, hkbd.c, hms.c), `sys/dev/vt` with `sys/teken`, `sys/dev/kbd` and `sys/dev/kbdmux`, and
`sys/dev/evdev`. FreeBSD has no virtio-input driver and no UAS: there, the specifications (virtio
1.2 §5.8, T10 UAS-2) are the reference, and the user interface is the same `kbd`/evdev one.

### Milestones

What each milestone delivered, with its measurements and the fixes made on the way, is in git
(`git log`, tag `exit/u6`; this file as of `ad67714` holds the full record) and in the design documents
named above.

Status: the QEMU exit is met (U.4 and U.5 done 2026-10-24; `ugen`, `hidraw`, `hcons` and `hsctrl`
came with P9). The hardware step is P8's on the Sky1 and P10's on the X870E.

## Phase 6: FreeBSD multi-user (XL) — done 2026-10-30

**Goal:** stock FreeBSD `/sbin/init` → `rc` → multi-user `login:`.

- Dynamic linking: `rtld` (`ld-elf.so.1`) runs. `mmap` semantics for shared objects, PT_INTERP,
  RELRO, and TLS (`sysarch` FSBASE, `TPIDR_EL0`).
- `libthr`: `thr_new`/`thr_exit`/`thr_kill`, and the full `_umtx_op` set including robust and
  shared-memory mutexes.
- Tier B syscalls, sysctl tree (boot-critical and tooling sets), `kqueue` (all filters used by base),
  pipes, UNIX domain sockets with `SCM_RIGHTS`/`SCM_CREDS`, ptys (`posix_openpt`, `/dev/pts`), and
  `devctl`/`devd` events.
- Credentials (setuid exec, `issetugid`), resource limits, `procctl`, **Capsicum** (capability mode
  and rights), **jails** (basic: `jail_set`/`attach`, path and hostname isolation), and securelevel.
- Shared page `vdso_timehands` (fast `clock_gettime`). `ktrace` (so `kdump` works). `reboot`/
  `shutdown` paths.
- `kinfo_proc`/`kinfo_file`/`procstat` sysctls so `ps`, `top`, `fstat`, and `procstat` work.
- `abi-check` covers every struct crossing the boundary.
- kyua test suite runs nightly, with the baseline recorded.

**Exit:** unmodified FreeBSD 15.1 `base.txz` boots **multi-user** from UFS2 on both arches (QEMU
and bhyve; the physical machines follow in P8). Login works on the serial console and on `ttyv0`.
`syslogd`, `cron`, and `devd` run. The kyua pass rate meets the agreed target for kern/, lib/libc,
bin/, and usr.bin/ in bhyve. The P6 performance suite (boot time, `libthr`/umtx, `kqueue`, UNIX
sockets, ptys, dynamic exec) is within 2% of FreeBSD in bhyve.

**Design.** [MULTIUSER.md](MULTIUSER.md) ([ADR-0016](adr/0016-multi-user.md)): FreeBSD's own boot
recorded as the test plan, processes split into threads as FreeBSD's are, umtx in a host-tested
`sys/umtx`, one readiness type under `poll` and kqueue, a protocol-neutral socket layer with UNIX
domain first, a device trait under the line discipline, typed privilege and Capsicum rights, a
jail tree, and no networking until P7 (`AF_INET` behaves as on a kernel without `INET`).

**Reference configuration.** The P5 machines (QEMU `q35` and `virt`, bhyve) with the USB and input
set, and an NVMe disk holding a GPT with the ESP and a UFS2 root with all of FreeBSD 15.1's
`base.txz`, `/etc` as `bsdinstall` leaves it (`rc.conf` with a hostname, the time zone, `fstab`).
The FreeBSD 15.1 guests boot the same disks for the baselines. The X870E and the Orange Pi 6 Plus
join in P8.

**kyua target** (ADR-0016): for each of `kern/`, `lib/libc/`, `bin/`, and `usr.bin/`, at least 95%
of the test cases that pass on FreeBSD 15.1 in the same machine, with every other failure listed
with its reason.

### Milestones

What each milestone delivered, with its measurements and the fixes made on the way, is in git
(`git log`, tag `exit/p6`; this file as of `ad67714` holds the full record) and in the design documents
named above.

## Phase 7: Networking (XL) — done 2026-11-09

**Goal:** a networked FreeBSD system, in QEMU and bhyve, before the physical machines: board
bring-up (P8) then starts with `ssh`, `scp`, and `pkg` on both of them.

- `ifnet`, packet buffers, Ethernet, ARP, IPv4, ICMP, IPv6 + ND + SLAAC, UDP, TCP (RFC 9293
  baseline, SACK, window scaling, timestamps, NewReno + CUBIC), loopback, `bpf` (for `dhclient`
  and `tcpdump`).
- Socket API completeness for INET/INET6, `sendfile`, multicast basics.
- Kernel TLS in software (`TCP_TXTLS_ENABLE`, `TCP_RXTLS_ENABLE`; TLS 1.0–1.3 records with
  AES-CBC + HMAC, AES-GCM, and ChaCha20-Poly1305, written in-tree) and `sendfile` over it. kyua's
  `sys/kern/ktls_test` rejoins the `sys/kern` target here (ADR-0016).
- Control plane: routing tables (FIB), routing socket, **netlink** (`NETLINK_ROUTE` +
  `NETLINK_GENERIC` as used by 15.x `ifconfig`/`route`/`netstat`), and interface ioctls.
- Drivers: virtio-net (multi-queue, checksum/TSO offload). The physical machines' NICs come with
  them in P8.
- Differential tests against the build server for TCP behavior (packet captures).

**Exit:** in QEMU on both arches and in bhyve, the system obtains an address with `dhclient` and
SLAAC, `sshd` accepts logins, `scp` copies both ways, and `fetch` and
`pkg update && pkg install <curated set>` succeed. Sustained `iperf3` runs complete without stalls
or leaks. The kyua target is met in `sys/kern` with `ktls_test`. The P7 performance suite (TCP/UDP
throughput, request/response latency, connection rate, `sendfile`, CPU per gigabit) is within 2% of
FreeBSD over loopback and virtio-net in bhyve.

**Design.** [NETWORKING.md](NETWORKING.md) ([ADR-0019](adr/0019-networking.md)): the wire
as the specification, checked by differential captures against FreeBSD; all network state per
VNET from the start, with VNET jails and `epair` as the test bed; host-tested crates for packets
(`sys/mbuf`), interfaces and the control plane (`sys/net`), TCP (`sys/tcp`, on a simulated
network), and ciphers (`sys/crypto`); lock-free lookups in a network epoch; FreeBSD 15.1's default
TCP stack and defaults; `vtnet` as the first driver of the `EtherQueues` class; P6's loopback
slice (`sys/inet`) keeping its semantics on real packets.

**Reference configuration.** The P6 reference disk with `bsdinstall`'s lines for a DHCP interface
(`ifconfig_vtnet0="DHCP"`, `ifconfig_vtnet0_ipv6="inet6 accept_rtadv"`) and `sshd` enabled, and a
test overlay with `iperf3` from FreeBSD's packages. QEMU `q35` and `virt` with virtio-net on
user-mode networking; bhyve with virtio-net on a dedicated bridge on the build server, which serves
DHCPv4, router advertisements, and DNS and is the performance peer
(NETWORKING.md §15). The FreeBSD 15.1 guests boot the same disks for the baselines.

**kyua target** (ADR-0019): ADR-0016's target for `sys/net`, `sys/netinet`, `sys/netinet6`,
`sys/netlink`, `sbin/ifconfig`, `sbin/route`, `sbin/ping`, and `sys/kern` with `ktls_test`; test
programs for P7's non-goals (packet filters, tunnels, bridges, VLANs, CARP, SCTP, IPsec) are left
out until their phase.

### Milestones

What each milestone delivered, with its measurements and the fixes made on the way, is in git
(`git log`, tag `exit/p7`; this file as of `ad67714` holds the full record) and in the design documents
named above.

## Phase 8: Board bring-up, Orange Pi 6 Plus (XL) — done 2026-11-10

**Goal:** the P7 system, networked, on the ARM64 reference board, with AbyssBSD's own drivers for
its firmware, SoC, and NIC, and the board gating every phase from here on
([ADR-0010](adr/0010-qemu-first-development.md)). The phase was planned for both physical
machines; on 2026-11-10 the X870E's half moved to its own phase (P10), after the board is hardened
(P9). The items below that name the X870E, and the exit criteria not yet run on the board, moved
with them.

This is the first contact with real firmware. The clean-room specifications
(`~/Projects/OS/chinese-wall/specs/`) and the Q8B experience show that arm64 SoC firmware describes
clocks, resets, and power partly outside standard ACPI, and is sometimes wrong. The driver model
gains what that needs ([DRIVER-MODEL.md §16](DRIVER-MODEL.md#16-phasing)), held to the board
corpus rather than designed in the abstract.

- **Ground truth.** The Orange Pi 6 Plus's Linux and FreeBSD state captured into
  `tests/corpus/opi6p/` (MULTIUSER.md §15) before anything else runs on it; the X870E's tables,
  `pciconf`, and `devinfo` from FreeBSD 15.1 beside Fedora. Both machines' ACPI tables in the AML
  corpus, loading without warnings.
- **Deploy.** The Orange Pi 6 Plus boots AbyssBSD from USB, then its NVMe is reimaged with AbyssBSD
  and a FreeBSD baseline. The X870E: `cargo xtask deploy --machine x870e` onto the media NVMe only,
  by serial number; FreeBSD 15.1 beside it for baselines.
- **Driver-model extensions** (DRIVER-MODEL.md amendments of 2026-10-27), each host-tested against
  the corpus before it touches a board:
  - the platform description (§5.5), with the Sky1 entries (CIX's `CLKT`/`RSTL`/`RSVL` tables) and
    any the X870E needs;
  - the Order and Power edge kinds, and the new edge sources (`_DSD` references, `FixedDMA`, pin
    resources, serial-bus and GPIO operation regions) (§6.1);
  - the `Clock`, `Reset`, and `PowerDomain` provider traits and their first providers, including
    SCMI over whichever transport the Sky1 DSDT uses (§8.5, §9.3, §17 question 7);
  - `PciFunction::acpi_companion`, and firmware-shared and declared-shared ranges (§5.2, §8.1);
  - the firmware store (§8.6), if a P8 driver needs an image.
- **Drivers** for what both machines need to reach multi-user: storage, USB (PCI and ACPI xHCI),
  console, watchdogs, RTC, and their NICs (HARDWARE.md §4, §5; P7's stack over e1000e, igc, and
  Realtek RTL8126, the Orange Pi 6 Plus's only NIC), and the Sky1 controllers (GPIO, I2C, pin
  control) the dependency graph shows they depend on. No node may stay in `Waiting` without a
  named reason.
- The USB track's hardware exit on both machines.
- `cargo xtask perf --platform x870e|opi6p`, and kyua's arm64 baseline on the Orange Pi 6 Plus.
- The Radxa Dragon Q8B is Tier 2: its platform-description entries are welcome but not gating.

**Exit (as planned for both machines):** the `<arch>-multiuser` scenario passes on both machines (on the X870E from the
framebuffer and a USB keyboard alone). fsx with `fsck_ffs` passes on their NVMe and on a USB disk.
Their device trees match FreeBSD's `devinfo` wherever FreeBSD has a driver. The kyua target is met
on the Orange Pi 6 Plus. Every component of every suite so far is within 2% of FreeBSD on bare
metal on both machines (the Orange Pi 6 Plus per core class), P7's over their physical NICs too.

### Milestones

What each milestone delivered, with its measurements and the fixes made on the way, is in git
(`git log`, tag `exit/p8`; this file as of `ad67714` holds the full record) and in the design documents
named above.

**Held on the Sky1 (2026-11-10):** multi-user, networked, from its own NVMe (the Samsung 990 PRO,
EFI `Boot0007` by BootNext from FreeBSD, which stays the default boot), root over `ssh`, the
kernel updated in place; CPU frequency (`acpi_cppc`) and thermal zones as under FreeBSD. kyua,
fsx, the `devinfo` comparison, and the performance gate moved to P9; the X870E to P10.

## Phase 9: Sky1 hardening (L)

**Goal:** the Orange Pi 6 Plus as dependable as FreeBSD on it, before the second machine comes up:
no hangs, no reboot loops, nothing that needs the screen, and P8's remaining exit criteria met.

The record of each item (what was built, how it was checked, the numbers) is in git and in the
documents named; this section keeps what is done and what is open.

**Done:**
- **Bring-up defects.** `MNT_RELOAD` of the UFS root; `getty` on the serial console (`ttyu2`);
  `/dev/efi`, so `efibootmgr` sets BootNext from AbyssBSD; the boot's noise (`devmatch`, `usbhid`,
  `sky1_pinctrl`, the loader's power-off). The silent hangs were untranslated PCIe DMA, ended by
  the SMMU.
- **SMMUv3** ([IOMMU.md](IOMMU.md) §18, stages 0 to 5): the board uses all its RAM with DMA
  translated, and fsx with `fsck_ffs` passes on the NVMe and a USB disk.
- **The rest of the board:** `gpiobus`/`gpioc` and `iicbus`/`iic` as FreeBSD's tree has them; the
  `GeneralPurposeIo` and `GenericSerialBus` region handlers; the RX8900 RTC (`rx8803`) and the PD
  controllers (`rts5453`), in QEMU and on the host.
- **Driver selection** ([ADR-0021](adr/0021-driver-selection-and-modules.md), MODULES.md §3): one
  driver table per ISA.
- **What desktop and development software need** (anything P9 or earlier; each done when FreeBSD
  15.1's own tool or library runs unmodified against it, in QEMU on both architectures):
  - device interfaces: `/dev/pci` (on the board every `pciconf -lvbc` line matches FreeBSD's),
    `ugen` and `/dev/usb`, `hidraw`, `/dev/nvmeN`, CAM's `xpt0` and `pass`, `/dev/geom.ctl` (GPT
    and MBR, BSD labels read), devstat, `/dev/full`, real `rman` resources (`devinfo -u`);
  - system calls: the Tier gaps of 2026-11-10 (`getdtablesize` to `abort2`), `ntp_adjtime` and
    `adjtime` slewing, POSIX AIO;
  - subsystems: `tun`/`tap`, `if_bridge`, `nullfs`, `fdescfs`, `cd9660`;
  - fastfetch's fields (SMBIOS kenv, `/dev/mem`, `hw.model`, routes, devstat, `hw.acpi.battery`);
    `hcons` and `hsctrl`.
  - Left where they are (compat-deviations where userland sees them): graphics (M-G); sound, the
    MAC framework, ACPI sleep, buttons, the battery, the AC adapter, `/dev/acpi` (M-D); GELI and
    `crypto(9)` (M-Z); video codec and NPU (post Day 1). Not planned: `pf`, `ipfw`, `vlan`,
    `lagg`, IPsec, NFS, `procfs`, `spigen`, kernel dumps, swap (P11). Not in FreeBSD's GENERIC,
    so not needed: `mqueuefs`, SCTP, `hwpmc`, RACCT.
- **kyua on the board** (BUILD-AND-TEST.md; the baseline is the board's FreeBSD 16-CURRENT running
  15.1's tests): the target is met in all four areas (`bin` 98.6%, `lib/libc` 99.2%, `sys/kern`
  98.5%, `usr.bin` 99.2%), with no hang since `preempt_point` masks interrupts.
- **The performance harness** on the board (BUILD-AND-TEST.md, `tests/perf/board/`): the userland
  suites per core class against the board's FreeBSD built without `WITNESS`/`INVARIANTS`, and the
  in-kernel suite `p2` (`kbench.ko` for arm64; AbyssBSD runs it at boot and resets).

**Open:**
- **Performance** (the exit's gate: every suite within 2% of FreeBSD on the board, per core
  class, P7's over `rge`). The first runs (`tests/perf/board/results/`) found arm64 well short;
  the gaps closed since are in git (`fc2c7d0` to `3e968c5`). Three runs a side of `p4`, `p5`,
  and `p6` at `c03b69d` (`tests/perf/board/results/2026-11-12-c03b69d.txt`): every component
  passes in every class (big, mid, little, all), overall 0.64 to 0.72 of FreeBSD's time, the
  closest `sig` (1.018) and, on the little cores, `syscall` (0.993). Benchmarks behind: `thr.create`
  (1.10 to 1.12 everywhere), `thr.sem.pingpong` (1.11), the unix socket ping-pongs (1.06 to
  1.10), contended `thr.mutex` at two and eight threads (1.1 to 1.4), `fs.ufs.fsx` (1.05); on the
  little cores `io.null.write` (1.68), `kevent.pipe1` (1.6), `unix.rights` (1.49),
  `tmpfs.open_close` (1.36), where `getpid` is level. In the kernel (`p2`, big cores, one run):
  `ctxsw.same` (1.27), `ipi.rtt` (1.2), `callout.armcancel` (1.18), `ctxsw.cross` (1.02 to 1.14).
  To do:
  - the little cores: short system calls that do more than `getpid` took 1.4 to 1.7 of FreeBSD's
    time there and 0.75 on the big cores. One cause is fixed: the image past its first 4 MiB was
    in 4 KiB pages, about 20 first-level data-TLB refills a `write` on an A520 (none on the big
    cores), which cost most when the core's neighbour ran. Its segments are now 2 MiB-aligned
    and it is in 2 MiB leaves, as FreeBSD's: a 1-byte `write` to `/dev/null` on cpu2 went from
    537 ns to about 475 (FreeBSD: 500 to 540) and `getpid` from 294 to 270. The second: once a
    process had run some 30 pages of code (the A520's first-level instruction TLB), every
    system call took 2 to 4 instruction-TLB refills of about 230 cycles each, one of them at
    EL0, until the thread next slept: that `write` at 715 to 800 ns. FreeBSD sets
    `TCR_EL1.E0PD1` (FEAT_E0PD: an EL0 access to the kernel half faults with no lookup and no
    walk) and is immune; now AbyssBSD sets it too and the state is gone (475 to 490 ns
    whatever ran before). Running at EL2 with VHE, CnP, and no PAN were tried and change
    nothing. Left: a program's own code layout still moves that `write` between 470 and
    620 ns (one test program built `-O1`, `-O2`, `-O3`), where FreeBSD stays at about 510.
    The suites rerun with both (`tests/perf/board/results/2026-11-12-6aae600.txt`): on the
    little cores `io.null.write` 1.68 to 0.95, `syscall` 0.89, every component passing; but
    `sig` failed on the big and mid cores (`sig.deliver` 1.018 to 1.044), see the next item;
  - `sig.deliver` with E0PD1: the user copies of a signal's frame cost more with it, and
    AbyssBSD made nine a round trip to FreeBSD's four, because it wrote and read an SVE
    context (some 580 bytes each way) for every thread. FreeBSD traps SVE at EL0 until a
    thread's first SVE instruction and writes the context only after it (`pcb_svesaved`); now
    AbyssBSD does the same (a new thread, a forked child, and an `execve` start without it),
    and `sig.deliver` on the big cores is 949 ns to FreeBSD's 973. The rerun, three runs a
    side (`tests/perf/board/results/2026-11-13-382361c.txt`), passes every component in
    every class, `sig` at 0.98 on the big and mid cores, overall 0.67 to 0.69. Still behind:
    `unix.rights` (1.10 mid, 1.55 little, 1.44 with all cores), `thr.create` (1.06 to 1.09),
    `thr.sem.pingpong` (1.08 to 1.11), contended `thr.mutex` at two and eight threads (1.1
    to 1.4), `fs.ufs.fsx` (1.06); on the little cores `tmpfs.open_close` (1.21),
    `kevent.pipe1000` (1.14), `tmpfs.extract` and `thr.cond.pingpong` (1.08); with all cores
    the unix socket ping-pongs (1.10 to 1.13).
    Lifting PAN around a copy as FreeBSD does (plain loads and stores in place of
    `ldtr`/`sttr`) was tried and is slower here (`sig.deliver` 1370 ns, `pipe.pingpong` 8%);
  - `unix.rights`: a passed descriptor took some thirteen allocations a round trip to
    FreeBSD's four; with four fewer (the control on the stack, one list of files, one buffer
    of received messages) the round trip is 6765 instructions for 7776, and the benchmark
    1347 ns on the big cores (FreeBSD: 1484) and about 7250 on the little (it was 8850;
    FreeBSD: 5700 to 6300);
  - the little cores and where the kernel's code lies: an A520 pays 75 to 145 cycles for
    each first-level instruction-cache miss (a big core next to nothing), and how many a
    system call takes depends on where the linker put its functions, which every change
    moves. The same code linked in three orders: `dup` and `close` 840 to 985 ns, a byte
    written to and read from a UNIX socket 2400 to 3500, a passed descriptor 7800 to 9800
    (counted with the PMU: 0 to 2.5, 14 to 33, and 80 to 112 misses), the big cores the
    same throughout. With the functions a profile finds hot placed together
    (`--symbol-ordering-file`) each is at its best or near it, and at or under FreeBSD's
    (887, 2760, 8460). This is most of the little cores' "noisy" verdicts and of what a
    change appears to cost or gain there. Ordering the hot functions in the build was
    tried and dropped: `p4`, `p5`, and `p6` three times each on three kernels of the same
    code (FreeBSD's side that of the last report), as linked, with the 454 functions the
    profiler found hot grouped by the kind of benchmark that runs them, and with them
    clustered by what calls what (the profiler recording each sample's caller). Overall on
    the little cores 0.684, 0.674, and 0.691 of FreeBSD's time; on the mid 0.671, 0.677,
    0.679; on the big 0.676, 0.675, 0.673. Single benchmarks on the little cores moved by
    20 to 60% either way under both orders (`io.null.write` 0.70 and 0.68 of its time as
    linked, `sig.deliver` 0.83 and 1.18, `fs.ufs.open_close` 1.10 and 1.62): placing the
    hot functions together gives another layout, not a better one, so the misses are
    likely functions meeting in the same cache sets rather than hot code spread too far.
    The A520s have no second-level cache of their own (the firmware's 4 MB is the eight
    A720s' 512 KB each), so a miss goes to the shared third level, on FreeBSD too. The
    report as linked: `tests/perf/board/results/2026-11-13-bc14163.txt`, every component
    passing on every class. Until something controls it, a small change is compared on
    the little cores by instructions executed (PMU event `0x08`), not by time;
  - amd64 in bhyve had slipped since its phases' exits: at `e124112`, ten runs a side, `p6`
    failed `unix` (1.143) and `event` (1.070), and in `p4` `getpid` took 115 ns to FreeBSD's
    71 (it was 69 at P5's exit), a byte to `/dev/null` 189 to 108. The causes, fixed in
    `13afb3d` to `cf4b48c`: without SSE the compiler copied each structure of 64 to 128 bytes
    with `rep movsq`, some 30 cycles a time (`-max-store-memcpy=16`, x86_64 only); P6.7 and
    P6.8 had put a lock or a lookup on every call (the thread list's lock to ask whether a
    process has one thread, a mutex round a device's file offset, the limits' and the wired
    count's locks, the current thread and address space found through `dyn Any`); the call
    was decoded twice and found through the slot list. Between two processes a wake-up there
    is an IPI that costs the sender some 5 µs in the hypervisor: a pipe now wakes its reader
    before it copies a transfer of a page or more (`pipe.bw` 22.7 µs to 14.9; FreeBSD's
    19.0), and a socket's waiter spins up to 2 µs for a change, while spinning has lately
    paid, before it sleeps (`unix.rights` 1405 ns to about 810; FreeBSD's 756 to 842). Three
    runs a side at `cf4b48c`: every component of `p4` and `p6` passes (`syscall` 0.999, `io`
    0.964, `pipe` 0.844, `proc` 0.764, `sig` 0.889, `vm` 0.571; `event` 1.008, `unix` 0.978,
    `thr` 0.882, `exec` 0.888, `pty` 0.846, `boot` 0.522). Behind inside them:
    `event.kevent.pipe1` and `pipe1000` (1.26, 1.33), `unix.stream.bw4k` (1.17),
    `thr.mutex.pi` at four and eight threads (about 1.25 and 1.7), `thr.mutex.t2` (1.07),
    `vm.mmap` (1.03). Still to do there: the ten-run gate, and `p2`, `p3`, `p5`, and `p7`
    again. The kernel's profiler drops samples taken in user mode (`prof.rs`: `USER` and
    `OTHER` are the same bit). The same changes on the board, three runs of `p4`, `p5`, and
    `p6` (`tests/perf/board/results/2026-11-14-cf4b48c.txt`, FreeBSD's side that of the last
    report): every component passes on every class, overall 0.634, 0.639, 0.632 and 0.630
    of FreeBSD's time on the big, mid, little, and all cores (0.676, 0.671, 0.684, 0.647
    before), `syscall` 0.56 from 0.70 on the big cores and 0.70 from 0.93 on the little,
    `io` 0.24 from 0.34, `unix` 0.51 from 0.72. One thing is slower there: `pipe.bw` on the
    big and mid cores by 6 and 8% (9431 and 10576 ns; FreeBSD's 9302 and 11160), where a
    wake-up is cheap and the early one buys nothing;
  - a release kernel built with the tests (`cargo xtask test --tier boot --release`)
    overflows a kernel stack in `detach_stress_nvme` on both architectures
    (`devfs::Registry::insert` under the NVMe driver's start); the boot tier's own (debug)
    build passes;
  - the 10-run gating set, `p2` on the mid and little cores, and `p7`;
  - the arm64 kernel's target makes no unaligned access, so an integer read out of a byte
    slice is a byte load each (it was 144 ns of every signal): a sweep of the hot paths that
    parse bytes (packet headers, on-disk structures) for the same;
  - after `p5` has run in the same boot, `kevent.pipe1000` is 7% slower until a reboot
    (`poll1000` and `select1000` were a quarter slower and are no longer: ADR-0024's zones, and
    the pipe's `Notify` in the pipe). Each knote's filter is still its own allocation;
  - `rw`/`sx` readers at four threads are 1.24 to 1.3 of FreeBSD's (they were 1.8 to 2.0): a
    reader's first try is the ARMv8.0 compare-and-swap, which fails with others at the word. A
    kernel built all LSE is level there (65 ns) and 5 ns slower on every uncontended pair
    (`tests/perf/board/results/2026-11-12-p2-big-lse.txt`); and `page.miss` (1.6);
  - the contended writer locks (`mtx`, `rw.wr`, `sx.wr` at t2 and t4) read 18 to 23 ns against
    FreeBSD's 47 to 70, with the same backoff and now the same spin hint: not explained;
  - `p7` over `rge`; the SMMU's stage 6; then the 10-run gate on every class.
- **The board does not always come back from a reset** (three times in some forty: after the
  first kyua run, from FreeBSD, and from AbyssBSD on 2026-11-12): the system shuts down, issues
  the reset (`PSCI SYSTEM_RESET` on AbyssBSD's console), and no loader starts; a power cycle
  recovers. Seen from both kernels, so the firmware's reset path is the suspect.
- **Hangs seen once, not reproduced:** a held key on `vt`; `aarch64-el2` (INTx) in
  `tests/user/cam`, once in three runs in QEMU.
- **To check on the board:** the RX8900 and the PD controllers, `hcons`' media keys, the `devinfo`
  comparison, and FreeBSD's recordings for storage and USB (`tests/corpus/opi6p/freebsd/`).
- **Small gaps** (compat-deviations): `vgapci` and `ichsmb` names, the old `mount(2)` for cd9660,
  writing BSD labels, `glabel`'s `label/` names, USB isochronous transfers and alternate
  settings.

**Exit:** the P8 exit criteria on the Orange Pi 6 Plus, every known defect above fixed or recorded in
compat-deviations with a reason, and kyua and the performance suites run on the board without a
hang.

## Phase 10: Board bring-up, X870E (L)

**Goal:** the hardened system on the AMD64 reference machine
([ADR-0009](adr/0009-amd64-reference-machine-ryzen-x870e.md), HARDWARE.md §5), gating from then on.
This is P8's X870E half (P8.2), with its ground truth already in the corpus (P8.0).

- **Deploy:** `cargo xtask deploy --machine x870e` onto the media NVMe only, by serial number;
  FreeBSD 15.1 beside it for baselines. Never the Fedora disk, the RAID members, the Fedora ESP, or
  its boot entry.
- **Drivers:** the RTL8125 family (`rge`), `amdsbwd`, the persistent log the loader saves to the
  ESP, ACPI CPPC (`acpi_cppc`, shared with the Sky1), and whatever else the dependency graph shows
  multi-user needs.
- **Exit:** the `x86_64-multiuser` scenario from the framebuffer and a USB keyboard alone; fsx with
  `fsck_ffs` on the media NVMe and a USB disk; the device tree against FreeBSD's `devinfo`; every
  suite so far within 2% of FreeBSD on the machine, P7's over its physical NIC.

## Phase 11: Self-hosting and hardening = **v1.0 / Day 1 complete** (L)

- **Self-hosting:** `pkg install rust` on AbyssBSD, then build AbyssBSD on AbyssBSD (amd64 and
  arm64) and boot the result.
- UFS2 **SU+J**, `fsck` on unclean boot, and swap (swap pager on a GPT partition).
- IOMMU on x86 (VT-d, AMD-Vi) and interrupt remapping, and the SC8280XP's SMMUv2, as units of the
  framework P9 built for the SMMUv3 ([ADR-0022](adr/0022-smmuv3-in-p9.md), IOMMU.md §9). amd64
  FreeBSD leaves them off by default, so they are not needed to match it.
- CPU idle and frequency: `_LPI` (with PSCI power-down states and a broadcast event timer, e.g.
  Sky1's CIX GPT), `_CST`, HWP. (CPPC and ACPI thermal zones came in P8.4.) SCMI clients over PCC.
- Security hardening: SMEP/SMAP/PAN enforced, KASLR, stack canaries in the loader, kernel W^X
  audit, speculative-execution mitigations, and arm64 PAC/BTI if stable-toolchain support allows.
- Suspend/resume (S3 / s2idle) is a **stretch goal**.
- Performance pass: lock contention profiling and scheduler tuning on hybrid cores, aimed at
  beating FreeBSD rather than matching it. The P11 suite adds self-hosted build time, SU+J
  metadata workloads, and swap throughput.
- Documentation: installation guide ([INSTALLER.md](INSTALLER.md) (draft)), supported-hardware list,
  compat-deviations list. Forward design for the installed manuals: [MANUALS.md](MANUALS.md) (draft).

**Exit (v1.0):** the success criteria in [VISION.md](VISION.md#success-criteria-for-day-1-v10) are
met.

## Phase 12: RV64, Day 2 (L)

- `arch/riscv64`: S-mode entry from UEFI, Sv48 paging, traps, SBI (timer, IPI, HSM, SRST), and the
  AIA interrupt controllers (IMSIC/APLIC), all from ACPI (RHCT, MADT RINTC/IMSIC/APLIC).
- `abi` for FreeBSD riscv64 (struct layouts, signal frames, and the syscall convention:
  `ecall`, `t0` number, `a0..a7` arguments).
- Boot FreeBSD riscv64 `base.txz` multi-user on QEMU virt (`acpi=on`).
- **Board bring-up** on the Banana Pi BPI-SM10 (SpacemiT K3,
  [ADR-0017](adr/0017-rv64-reference-board-bpi-sm10.md), [HARDWARE.md §8](HARDWARE.md#8-board-profile-rv64-banana-pi-bpi-sm10-spacemit-k3)),
  once the QEMU exit holds, in the order P8 used for the other boards:
  - save the shipped state (SPI NOR dump, U-Boot environment) and Linux's view of the hardware;
  - get SpacemiT's K3 edk2 port running (UEFI with ACPI), and its tables into the AML corpus;
  - boot from NVMe in the M.2 2280 slot, console on UART0;
  - an RPMI provider for the firmware services, and the platform description (DRIVER-MODEL.md §5.5)
    for whatever the K3 tables get wrong.
- A PERFORMANCE.md rule for RV64 parity, since FreeBSD does not run on the board (ADR-0017).

**Exit:** the P6 exit criteria for RV64 on QEMU, and on the BPI-SM10 if its UEFI+ACPI firmware
works (otherwise the board is Tier 2 and the exit is QEMU's alone). Every HAL change the port needed
is recorded as an ADR or follow-up, which tells us how well the seams held.

---

## Later milestones (post Day 1)

### M-U: Userland

- Decide which base components to reimplement in Rust: init/rc replacement, libc (FreeBSD-ABI
  compatible), core utilities, and the pkg toolchain. Keep binary compatibility as the contract so
  stock and new components can mix.
- Rust `std` support for an AbyssBSD target (or keep `x86_64-unknown-freebsd` as the userland
  target, since we are ABI-compatible).

### M-G: Graphics

Forward design: [GRAPHICS.md](GRAPHICS.md) (draft; it proposes the DRM user interface as FreeBSD's
ports see it, implemented natively, and a step order for this milestone).

- Kernel modesetting interface (KMS-equivalent: CRTCs, planes, connectors, atomic commits, dma-buf
  style buffer sharing, sync files). Decide between FreeBSD/Linux DRM ioctl compatibility and a
  new interface. The decision drives which userland GPU stacks can be reused.
- Drivers: GOP-backed simple KMS, then virtio-gpu (2D, then 3D/venus), then a native GPU: AMD
  RDNA2, which the desktop's metal target and all three amd64 Tier 1 machines have
  ([ADR-0018](adr/0018-desktop-architecture.md)).
- evdev-compatible input interface: done in the USB track.
- Acceptance is **stock Wayland and Sway** from FreeBSD 15.1 packages, unmodified: on `gop-kms`
  with software rendering, then accelerated on virtio-gpu (Venus), then on the Orange Pi 6 Plus
  (`panthor`) and RDNA2. Each step exits with a `grim` screenshot of Sway showing a `foot` window
  running `fastfetch` and, in another window, glmark2's 3D jellyfish scene with its frame rate on
  screen ([GRAPHICS.md](GRAPHICS.md) §14).
- The desktop (M-D) follows on the same kernel: its `undertow` compositor is M-D's acceptance, not
  M-G's ([DESKTOP.md](DESKTOP.md) §4).

### M-D: Desktop

Plan: [DESKTOP.md](DESKTOP.md) under [ADR-0018](adr/0018-desktop-architecture.md).

- **The desktop is the AbyssBSD desktop** (`../AbyssBSD/desktop`, Rust since 2026-10-31): the `undertow`
  compositor over wlroots, the Aqua toolkit and shell, portals, its applications, the login window,
  application jails, and its installer. AbyssBSD designs no desktop of its own. It runs this one
  as FreeBSD userland, installed from packages built for FreeBSD 15.1, unmodified.
- The work here is the kernel beneath it. Most of what the desktop uses is P4–P6 ABI and is done.
  Still to schedule: the MAC framework with `mac_priority`, `sound(4)` (OSS, `sndstat`,
  `snd_dummy`, then HDA), ACPI sleep and buttons, and ZFS (M-Z) for the installer's default
  layout and boot environments (DESKTOP.md §3.2).
- The desktop's own harness becomes a conformance suite, each lane compared with FreeBSD 15.1 as
  kyua is. Its headless lanes need no graphics, so **M-D.1 can start once P6 exits**, before
  M-G, and does not gate Day 1. First light follows M-G.1, acceleration M-G.3, and metal M-G.6.
- Changes the desktop itself needs (a 15.1 package build, a UFS install layout, arm64 packages)
  are made in the monorepo's plan, not carried here (DESKTOP.md §8).
- Prerequisites by step: P6 (headless), M-G (display), P7 (network panes, Firefox), audio, P11 power
  management (sleep), M-Z (ZFS install), INSTALLER.md (the boot loader for installed systems).

### M-Z: OpenZFS

- Host the upstream **OpenZFS** C code on our kernel rather than rewriting it. A Rust ZFS would
  be a project of its own, and OpenZFS's on-disk format and pool semantics are defined by that
  codebase in practice.
- The work is the plumbing: a Solaris Porting Layer equivalent (memory, locks, condvars, taskqs,
  threads, time, kstats, the `vnode`/`uio` shapes ZPL expects), a VFS adapter so ZPL datasets
  mount like any other file system, a GEOM-style vdev provider over our disk stack, and the
  `/dev/zfs` ioctl interface so stock `zpool`/`zfs` binaries work unmodified.
- Needs its own ADR before work starts: it breaks the no-C rule (ADR-0002/0003 build policy),
  brings CDDL code into a BSD-2-Clause tree (kept in a separate, clearly licensed directory, as
  FreeBSD does), and must fit the static-kernel model (ADR-0005), so it is linked in at build time
  rather than loaded as `zfs.ko`.
- Prerequisites: VFS and buffer/page cache maturity (P5), GEOM-equivalent disk providers (P3),
  and SMP-safe sleep locks.

---

## Key risks

| Risk | Impact | Mitigation |
|---|---|---|
| AML interpreter correctness on real firmware | Devices missing, no power off, hangs | Large `acpidump` corpus, fuzzing, ACPICA-compatible `_OSI` answers, early real-hardware testing |
| Breadth of the FreeBSD ABI (sysctl/ioctl/netlink long tail) | P6/P7 stall on small issues | Syscall/ioctl usage telemetry (log the first use of anything unimplemented), kyua and differential tests to prioritize |
| VM + UFS2 + buffer cache interaction (data corruption) | Lost data, loss of trust | Host-tested on-disk crates, `fsck_ffs` validation on every CI image, crash-consistency tests (kill the VM mid-write) |
| Weak memory ordering bugs only visible on ARM64 | Heisenbugs | Both arches gate every merge, real ARM64 hardware early, Miri/loom-style in-house model checks for lock-free code |
| No external crates means everything is built here | Scope creep | Keep `lib/*` crates minimal and purpose-built, and favor the simplest correct design |
| Rust stable-toolchain gaps (e.g. arm64 branch protection) | Hardening delayed | Track upstream. ADR required for any nightly opt-in |
| Performance below FreeBSD's | AbyssBSD is not a credible replacement | The 2% bound gates every phase from P2 ([ADR-0011](adr/0011-performance-parity.md)), so losses are caught in the layer that causes them. Nightly cumulative suites. Hot-path designs weighed against ARM64 costs before P8 makes arm64 gating |
| Physical hardware variety | Slow v1.0 | A small set of Tier 1 machines, SystemReady-only on ARM64 |
| The X870E holds user data (Fedora NVMe, RAID array) | Catastrophic data loss | Non-designated disks attach read-only until P11. `xtask deploy` identifies the media NVMe by serial number. AbyssBSD never writes the Fedora ESP or boot entries |
| No serial console on the X870E | Blind debugging of early hangs | Framebuffer console, persistent RAM log saved by the loader, FCH watchdog, an optional PCIe serial card or polled network log |
| Bring-up on the Orange Pi 6 Plus costs power cycles | Slow iteration | QEMU first. Serial console from day one, the watchdog in P3. Capture the Linux/FreeBSD ground truth (ACPI dumps, register state) before the board is reimaged, and read it before experimenting |
| The desktop is another project's plan, on FreeBSD `main` | M-D waits on changes made elsewhere; the desktop drifts toward 16-only interfaces | Its harness runs against FreeBSD 15.1 here, so drift shows as a failing lane. The asks are listed in DESKTOP.md §8 and tracked in the monorepo's plan |

## Open decisions

Decided items are struck through. Each decision is recorded as an ADR.

1. ~~Physical reference machines~~: decided. ARM64 is the Orange Pi 6 Plus
   ([ADR-0008](adr/0008-arm64-reference-board-orange-pi-6-plus.md)). AMD64 is the Ryzen 9 7900X /
   X870E ([ADR-0009](adr/0009-amd64-reference-machine-ryzen-x870e.md)). Remaining: install the
   AMD64 machine's second NVMe, and decide whether to add a PCIe serial card to it.
2. ~~`alloc` in the kernel, `std` in host tooling~~: accepted ([ADR-0002](adr/0002-dependency-policy.md)).
3. ~~Primary ABI target~~: accepted, FreeBSD 15.x until 16.0-RELEASE ([ADR-0004](adr/0004-abi-target.md)).
4. ~~Disk space on the Orange Pi 6 Plus~~: decided. The board is reimaged for AbyssBSD when its
   hardware phase arrives.
5. **Hosting / forge**: self-hosted on the build server initially, with public hosting later.
6. **Graphics ABI** (DRM-compatible or new), decided at the start of M-G.
7. **Drivers ported from Linux**: leaning against adopting FreeBSD's LinuxKPI compatibility layer
   (and so against porting `drm-kmod`, `iwlwifi`, and similar). Instead, rebuild each driver in
   Rust for our kernel's semantics. Vendor firmware blobs are treated as part of the hardware:
   loaded and talked to, never reimplemented. Drivers are written from published specifications,
   with clean-room reverse engineering where none exist (observing the device and documenting it,
   never reading GPL driver source, per [ADR-0007](adr/0007-license-and-clean-implementation.md)).
   Support a limited set of hardware and expand organically, driven by the reference machines.
   Decided by ADR at the start of M-G, the first milestone that would need it.
8. ~~Desktop~~: decided. The AbyssBSD desktop (Rust), run as FreeBSD userland, with its packages
   built for FreeBSD 15.1 ([ADR-0018](adr/0018-desktop-architecture.md)). 16-CURRENT follows once
   the desktop is stable here.
