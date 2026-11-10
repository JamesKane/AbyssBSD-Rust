# AbyssBSD

**AbyssBSD** is a FreeBSD-compatible operating system kernel written from scratch in Rust for
modern 64-bit UEFI/ACPI hardware. It keeps FreeBSD's proven architecture and stable userland ABI,
drops thirty years of legacy hardware and compatibility baggage, and uses Rust's ownership and
type system to rule out whole classes of kernel bugs.

Unmodified FreeBSD 15.x binaries run directly on AbyssBSD—giving it a complete base system, package
ecosystem, and differential test oracle from day one—while every kernel subsystem (virtual memory,
ULE-style SMP scheduling, ACPI and AML interpreter, device framework, PCIe/MSI-X, NVMe/AHCI/xHCI,
VFS, and UFS2 with soft updates) is built in-house on stable Rust with **zero external crate
dependencies** (`core` and `alloc` only). AMD64 (`x86_64`) and ARM64 (`aarch64`) are developed side
by side as first-class targets, and every phase is gated on matching or beating stock FreeBSD 15.1's
performance on the same hardware. Once the kernel is self-hosting on real AMD64 and ARM64 machines,
the project extends to RV64 (`riscv64gc`), a Rust userland, and the AbyssBSD Swift desktop, run
unmodified as FreeBSD userland ([ADR-0018](docs/adr/0018-desktop-architecture.md)).

## Documents

| Document | Purpose |
|---|---|
| [docs/VISION.md](docs/VISION.md) | Goals, non-goals, scope, and what "modern 64-bit subsystems" means |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | Kernel structure, crate layout, architecture seams, subsystem map |
| [docs/COMPATIBILITY.md](docs/COMPATIBILITY.md) | The FreeBSD userland ABI contract and how we verify it |
| [docs/compat-deviations.md](docs/compat-deviations.md) | Documented deviations from FreeBSD behavior and why they exist |
| [docs/DRIVER-MODEL.md](docs/DRIVER-MODEL.md) | Device framework, buses, bindings, grants, DMA, and interrupts |
| [docs/DRIVER-ONBOARDING.md](docs/DRIVER-ONBOARDING.md) | Onboarding guide for writing and porting drivers, plus clean-room reverse engineering |
| [docs/VFS.md](docs/VFS.md) | VFS, vnodes, name cache, page and buffer caches, write-back, and file systems |
| [docs/USB.md](docs/USB.md) | xHCI, USB bus and hubs, HID and input (`kbd`, `evdev`), `vt` terminals, and `umass` |
| [docs/MULTIUSER.md](docs/MULTIUSER.md) | Phase 6 design: dynamic linking, threads, umtx, kqueue, sockets, ptys, security, and `init` |
| [docs/NETWORKING.md](docs/NETWORKING.md) | Phase 7 design: mbufs, interfaces, IPv4/IPv6, TCP, UDP, netlink, kernel TLS, and virtio-net |
| [docs/PERFORMANCE.md](docs/PERFORMANCE.md) | The 2% performance parity gate, methodology, fairness rules, and benchmark suites |
| [docs/ENGINEERING.md](docs/ENGINEERING.md) | Rust conventions: algebraic types, `unsafe` policy, errors, style |
| [docs/BUILD-AND-TEST.md](docs/BUILD-AND-TEST.md) | Toolchain, `xtask`, QEMU/bhyve, the build server, the test pyramid |
| [docs/HARDWARE.md](docs/HARDWARE.md) | Platform requirements, supported hardware, and the driver matrix |
| [docs/ROADMAP.md](docs/ROADMAP.md) | Phased roadmap with deliverables and exit criteria |
| [docs/adr/](docs/adr/) | Architecture Decision Records |

## Status

Phases 0 through 7 and the QEMU exit of the USB and input track (U.0–U.6) are complete:
unmodified FreeBSD 15.1 boots multi-user and networked on both architectures in QEMU and bhyve.
Phase 8 (bring-up on the physical reference machines) is next. See
[docs/ROADMAP.md](docs/ROADMAP.md) for the detail.

- **Boot (Phase 1):** our UEFI loader boots our kernel on x86_64 and aarch64, in QEMU and bhyve.
  It finds the kernel on UFS2 and passes the root named in `/etc/fstab`.
- **Core kernel (Phase 2):** physical and virtual memory, preemptive SMP threads, sleepable locks
  with priority propagation and a lock witness, tickless timers, and FP/SIMD state (including SVE
  and AVX-512). The Phase 2 benchmarks are within 2% of FreeBSD 15.1 in bhyve (geometric mean
  0.63).
- **Devices (Phase 3):** a FreeBSD-style device framework, PCIe, our own ACPI AML interpreter,
  platform clocks/watchdogs/entropy, and NVMe, AHCI, and virtio storage drivers. In bhyve the
  Phase 3 benchmarks take 0.34 of FreeBSD 15.1's time.
- **Userland and dynamic linking (Phase 4, P6.1):** unmodified FreeBSD 15.1 dynamically linked
  programs from `base.txz` (`/bin/sh`, `ls`, `ps`, `tar`, `find`, `ldd`, `kbdcontrol`,
  `vidcontrol`) and `/rescue` run interactively through `/libexec/ld-elf.so.1` (`rtld`), with PIE,
  RELRO, grow-down stacks (`MAP_STACK`, `MAP_GUARD`), the shared page (`vdso_timekeep` and amd64's
  `[vdso]` signal trampoline), arm64 `AT_HWCAP` and emulated ID-register `mrs`, processes,
  credentials, signals, pipes, and a terminal. FreeBSD's own programs are the tests.
- **File systems (Phase 5):** a VFS with a name cache, page cache, buffer cache, and write-back.
  tmpfs, devfs, `md(4)` memory disks, msdosfs (the ESP on `/boot/efi`), and UFS2 read-write with
  soft updates. Both architectures boot with the root on UFS2 on NVMe, holding all of FreeBSD
  15.1's `base.txz`, and `mtree` checks every file. fsx runs clean, crashes leave images
  `fsck_ffs` repairs, and the root is left clean at power-off. pjdfstest runs on tmpfs and UFS2
  against FreeBSD's results. In bhyve the file systems take 0.84 (UFS2) and 0.86 (tmpfs) of
  FreeBSD 15.1's time over the P5 suite.
- **USB, input, and virtual terminals (Track U):** xHCI (with MSI-X and bulk streams), USB 2/3
  hubs, HID report parsing, USB and virtio-input keyboards and mice/tablets, the `kbd`/`kbdmux`
  and `evdev` (`/dev/input/event*`, `/dev/sysmouse`) layers, `vt` virtual terminals
  (`ttyv0`–`ttyvb` over the framebuffer with `teken`, per-window fonts, switching, scrollback,
  bell, and `vt_fb_mmap`), and USB mass storage (`umass`/`da*` over Bulk-Only and UAS, with UFS2
  passing fsx and `fsck_ffs` and surviving hot-unplug of a mounted disk). In QEMU on both
  architectures, the shell on `ttyv0` is driven by a USB keyboard, and in bhyve `vt` text output
  takes 0.70 of FreeBSD 15.1's time.

- **Multi-user (Phase 6):** stock `/sbin/init` and `rc` reach `login:` on the serial console
  and `ttyv0`, from FreeBSD 15.1's `base.txz` as the installer leaves it, with `syslogd`, `cron`,
  and `devd` running. Underneath: `libthr` and the full `_umtx_op` set, `kqueue` and `poll`,
  UNIX-domain sockets with `SCM_RIGHTS`, ptys, Capsicum, jails, `ktrace` and `ptrace`, and the
  `kinfo` sysctls behind `ps`, `top`, and `procstat`. kyua pass rates against FreeBSD's own tests:
  `bin` 98.2%, `lib/libc` 97.6%, `sys/kern` 95.6%, `usr.bin` 98.5%. The P6 suite passed its gate
  at exit, but a later scheduler fix (idle CPUs spinning where FreeBSD halts) left `event` 1.08
  and `unix` 1.17 of FreeBSD's time; that work is parked and resumes from those benchmarks.
- **Networking (Phase 7):** mbufs, `ifnet`, Ethernet, ARP, IPv4, ICMP, IPv6 with ND and SLAAC,
  UDP, TCP (SACK, window scaling, timestamps, NewReno and CUBIC), loopback, `bpf`, VNET jails
  and `epair`, routing sockets and netlink for 15.x `ifconfig`, `route`, and `netstat`,
  `sendfile`, and kernel TLS (TLS 1.0–1.3 with AES-CBC, AES-GCM, and ChaCha20-Poly1305, written
  in-tree), over virtio-net. The system gets its addresses with `dhclient` and SLAAC, `sshd`
  accepts logins, `scp`, `fetch`, and `pkg install` work, and hour-long `iperf3` runs finish
  without leaks. Differential captures and command output match FreeBSD's, and in bhyve the P7
  suite (TCP and UDP throughput, latency, connection rate, `sendfile`, CPU per gigabit) takes
  0.935 of FreeBSD 15.1's time.

Next, Phase 8: board bring-up on the two physical reference machines, the Ryzen 9 7900X / X870E
(AMD64) and the Orange Pi 6 Plus (ARM64, CIX Sky1), starting with `ssh`, `scp`, and `pkg` on
both.

## License

BSD-2-Clause (see [ADR-0007](docs/adr/0007-license-and-clean-implementation.md)).
