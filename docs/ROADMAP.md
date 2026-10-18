# Roadmap

The roadmap is a sequence of **phases with hard exit criteria**. A phase is done when its exit
criteria pass in CI on **both AMD64 and ARM64**, not when the code is written. Sizes are relative
(S < M < L < XL) rather than calendar estimates.

**QEMU first** ([ADR-0010](adr/0010-qemu-first-development.md)). Through P5, development and every
exit criterion run under QEMU (plus bhyve for amd64). The physical reference machines get
**non-blocking smoke checks** at P1 and P3 to catch real-hardware surprises early. From P6, when
userspace is stable, they become **gating**.

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
                                                 P7 Networking ─► P8 Self-hosting & hardening = v1.0 (Day 1)
                                                                                │
                                                                                ▼
                                                                         P9 RV64 (Day 2)
                                                                                │
                                              ┌─────────────────────────────────┼──────────────────┐
                                              ▼                                 ▼                  ▼
                                    M-U Userland milestone        M-G Graphics milestone   M-D Desktop milestone
```

---

## Phase 0: Foundations (S)

**Goal:** an empty but complete engineering loop.

- Cargo workspace, `rust-toolchain.toml` (pinned stable, edition 2024, the four targets), lint
  configuration, `rustfmt.toml`, and CI policy checks (no external deps, `SAFETY:` comments).
- `xtask` skeleton: `build`, `run`, `test`, `image` (FAT32 ESP via our own `lib/fat` writer),
  and `--remote` execution on the build server.
- `tools/syscallgen` producing `lib/abi` syscall tables from `../freebsd-src/sys/kern/syscalls.master`.
- `tools/abi-check` pipeline proven on a handful of structs (`timespec`, `stat`, `sigaction`).
- Build server provisioned (rustup, qemu, edk2, bare git repo, CI hook).
- In-house test harness for kernel tests (`#[ktest]` collection via linker section).

**Exit:** `cargo xtask test --tier unit` passes on both hosts. `cargo xtask run --arch {x86_64,aarch64}`
boots a stub UEFI app that prints "AbyssBSD" on QEMU.

## Phase 1: Boot to Rust on both architectures (M)

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
*Hardware smoke check (non-blocking):* the **Orange Pi 6 Plus** boots from USB to the banner on its debug-header UART (found through DBG2)
and on the GOP framebuffer. The **X870E** boots from its media NVMe to the banner on the GOP
framebuffer, and the loader recovers the previous boot's persistent log to the ESP.

## Phase 2: Core kernel (L)

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
(allocator accounting balances).

## Phase 3: Device framework, PCIe, ACPI AML, storage (L/XL)

**Goal:** discover and drive real hardware.

- Bus model (`Driver`/`Device` traits, typestate attach, owned resources), device tree exported for
  later `devinfo`/`sysctl dev.*`.
- AML interpreter: parse DSDT/SSDTs, namespace, method execution, `_STA`/`_INI`/`_CRS`/`_PRT`/
  `_OSC`/`_S5`, power resources, GPE/SCI and GED events, and PCC operation regions (PCCT
  channels). Host-tested against an `acpidump` corpus (QEMU, bhyve, the Orange Pi 6 Plus, and
  every other physical machine we can reach).
- Watchdogs, so hangs on the physical machines become resets: SBSA generic watchdog (GTDT),
  including Sky1's `WOR`-refresh quirk, and the AMD FCH watchdog (WDRT).
- PCIe: ECAM enumeration, BAR sizing/assignment validation, capabilities, MSI/MSI-X (LAPIC / ITS).
- busdma-equivalent (no IOMMU yet).
- virtio-pci modern transport, virtio-blk, virtio-rng.
- NVMe (admin + I/O queues, MSI-X per queue), AHCI.
- RTC, entropy pool (RDRAND/RNDR/virtio-rng → fxrng-like CSPRNG), UEFI runtime services, ACPI power
  off / reset, and the power button.

**Exit:** on both arches the kernel enumerates PCIe, attaches NVMe, AHCI, and virtio-blk, reads the
GPT and dumps partition tables, sustains a multi-queue random-read stress test, and powers off via
ACPI/PSCI when the power button is pressed (QEMU `system_powerdown`). The real-firmware AML corpus
(Orange Pi 6 Plus, X870E) passes on the host.

*Hardware smoke check (non-blocking):* on the Orange Pi 6 Plus, the full DSDT/SSDTs load without
fatal errors, the NVMe attaches, and the watchdog resets a deliberately hung kernel. On the X870E: the DSDT
and 15 SSDTs load, NVMe (media disk) and AHCI attach, the FCH watchdog resets a hung kernel, and
ACPI S5 powers the machine off.

## Phase 4: First userspace: `/rescue/sh` (L)

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

**Exit:** both arches boot to an interactive `/rescue/sh` (from FreeBSD 15.1 `base.txz`) on the
serial console. Pipelines, job control (`^Z`, `fg`), `ls`, `cat`, `sysctl kern.ostype`, and
`/rescue/ps` work. A boot-to-shell expect test is in CI.

## Phase 5: VFS and file systems (L)

**Goal:** real persistent storage.

- Full VFS: vnode lifecycle, namecache (with negative entries), mount table, `nmount`, permissions
  and ACL hooks, `stat`/`statfs`/`getdirentries` fidelity, and file locking (`flock`, `fcntl`
  advisory locks).
- Unified buffer/page cache, write-back with a syncer thread, and `fsync` semantics.
- Storage stack: `BlockDevice`, GPT providers, FreeBSD device naming (`nda*`, `ada*`, `vtbd*`).
- **tmpfs** and **devfs** (complete).
- **msdosfs** read/write (FAT12/16/32, LFN). Mount the ESP at `/boot/efi`.
- **UFS2** read, then write (without soft updates; clean unmount required), then **soft updates**.
  **SU+J** follows in Phase 8.
- Validation: images written by AbyssBSD must pass FreeBSD `fsck_ffs -f` and `fsck_msdosfs` on the
  build server with zero errors, under a randomized-workload stress test.

**Exit:** both arches boot with the root on UFS2 on NVMe and `/rescue/sh` as init. A file-system
stress workload (fsx-style, run unmodified from FreeBSD's tests) completes, and the image passes
`fsck_ffs` afterwards. The loader can read the kernel from UFS2.

## Track: USB and input (parallel, starts after P3) (M)

- xHCI (rings, contexts, MSI-X), hub driver, device enumeration, HID boot protocol, then report
  parser (keyboard, mouse), USB mass storage (BBB, then UAS), and virtio-input.
- Framebuffer virtual terminals (`ttyv*`) with a `vt(4)`-compatible keyboard layer.

**Exit (merges into P6):** a physical machine without a serial port can be driven entirely from the
GOP console and a USB keyboard.

## Phase 6: FreeBSD multi-user (XL)

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

**Exit:** unmodified FreeBSD 15.1 `base.txz` boots **multi-user** from UFS2 on both arches (QEMU,
bhyve, and **both physical reference machines**, which gate from this phase on). Login works on the serial console and on `ttyv0`.
`syslogd`, `cron`, and `devd` run. The kyua pass rate meets the agreed target for kern/, lib/libc,
bin/, and usr.bin/.

## Phase 7: Networking (XL)

**Goal:** a networked FreeBSD system.

- `ifnet`, packet buffers, Ethernet, ARP, IPv4, ICMP, IPv6 + ND + SLAAC, UDP, TCP (RFC 9293
  baseline, SACK, window scaling, timestamps, NewReno + CUBIC), loopback, `bpf` (for `dhclient`
  and `tcpdump`).
- Socket API completeness for INET/INET6, `sendfile`, multicast basics.
- Control plane: routing tables (FIB), routing socket, **netlink** (`NETLINK_ROUTE` +
  `NETLINK_GENERIC` as used by 15.x `ifconfig`/`route`/`netstat`), and interface ioctls.
- Drivers: virtio-net (multi-queue, checksum/TSO offload), e1000e, igc, and Realtek RTL8126 (the
  Orange Pi 6 Plus has no other NIC).
- Differential tests against the build server for TCP behavior (packet captures).

**Exit:** both arches obtain an address with `dhclient` and SLAAC. `sshd` accepts logins.
`fetch`/`pkg update && pkg install <curated set>` succeed. Sustained `iperf3`-class throughput
(target set in Phase 7 planning) completes without stalls or leaks.

## Phase 8: Self-hosting and hardening = **v1.0 / Day 1 complete** (L)

- **Self-hosting:** `pkg install rust` on AbyssBSD, then build AbyssBSD on AbyssBSD (amd64 and
  arm64) and boot the result.
- UFS2 **SU+J**, `fsck` on unclean boot, and swap (swap pager on a GPT partition).
- IOMMU (VT-d, AMD-Vi, SMMUv3 with IORT RMR identity maps) with busdma integration. Watch for the
  SMMUv3 event-queue interrupt storm reported on Sky1 under Linux.
- CPU idle and frequency: `_LPI` (with PSCI power-down states and a broadcast event timer, e.g.
  Sky1's CIX GPT), CPPC (SystemMemory + AMU counters, per `_PSD` domain), `_CST`, HWP. ACPI thermal
  zones with per-domain passive cooling. SCMI clients over PCC.
- Security hardening: SMEP/SMAP/PAN enforced, KASLR, stack canaries in the loader, kernel W^X
  audit, speculative-execution mitigations, and arm64 PAC/BTI if stable-toolchain support allows.
- Suspend/resume (S3 / s2idle) is a **stretch goal**.
- Performance pass: lock contention profiling, scheduler tuning on hybrid cores, NVMe and network
  throughput targets.
- Documentation: installation guide, supported-hardware list, compat-deviations list.

**Exit (v1.0):** the success criteria in [VISION.md](VISION.md#success-criteria-for-day-1-v10) are
met.

## Phase 9: RV64, Day 2 (L)

- `arch/riscv64`: S-mode entry from UEFI, Sv48 paging, traps, SBI (timer, IPI, HSM, SRST), and the
  AIA interrupt controllers (IMSIC/APLIC), all from ACPI (RHCT, MADT RINTC/IMSIC/APLIC).
- `abi` for FreeBSD riscv64 (struct layouts, signal frames, and the syscall convention:
  `ecall`, `t0` number, `a0..a7` arguments).
- Boot FreeBSD riscv64 `base.txz` multi-user on QEMU virt (`acpi=on`). Real hardware follows as
  UEFI+ACPI RV64 boards become available.

**Exit:** the P6 exit criteria for RV64 on QEMU. Every HAL change the port needed is recorded as
an ADR or follow-up, which tells us how well the seams held.

---

## Later milestones (post Day 1)

### M-U: Userland

- Decide which base components to reimplement in Rust: init/rc replacement, libc (FreeBSD-ABI
  compatible), core utilities, and the pkg toolchain. Keep binary compatibility as the contract so
  stock and new components can mix.
- Rust `std` support for an AbyssBSD target (or keep `x86_64-unknown-freebsd` as the userland
  target, since we are ABI-compatible).

### M-G: Graphics

- Kernel modesetting interface (KMS-equivalent: CRTCs, planes, connectors, atomic commits, dma-buf
  style buffer sharing, sync files). Decide between FreeBSD/Linux DRM ioctl compatibility and a
  new interface. The decision drives which userland GPU stacks can be reused.
- Drivers: GOP-backed simple KMS, then virtio-gpu (2D, then 3D/venus), then a native GPU (Intel Xe
  or AMD RDNA; one first, chosen by reference hardware).
- evdev-compatible input interface (likely needed by a Wayland stack).

### M-D: Desktop

- An original Wayland compositor with dynamic tiling, workspaces, animations, IPC, and declarative
  configuration (in the spirit of Hyprland, with new code and design).
- Wayland protocol library, renderer (software → Vulkan), panel/launcher/notifications,
  terminal emulator, and a minimal toolkit.
- Prerequisites: M-G, plus audio (HDA/USB audio, later) and power management maturity.

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
| Physical hardware variety | Slow v1.0 | A small set of Tier 1 machines, SystemReady-only on ARM64 |
| The X870E holds user data (Fedora NVMe, RAID array) | Catastrophic data loss | Non-designated disks attach read-only until P8. `xtask deploy` identifies the media NVMe by serial number. AbyssBSD never writes the Fedora ESP or boot entries |
| No serial console on the X870E | Blind debugging of early hangs | Framebuffer console, persistent RAM log saved by the loader, FCH watchdog, an optional PCIe serial card or polled network log |
| Bring-up on the Orange Pi 6 Plus costs power cycles | Slow iteration | QEMU first. Serial console from day one, the watchdog in P3. Capture the Linux/FreeBSD ground truth (ACPI dumps, register state) before the board is reimaged, and read it before experimenting |

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
