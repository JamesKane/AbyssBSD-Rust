# AbyssBSD

A from-scratch reimplementation of the **modern 64-bit FreeBSD kernel subsystems in Rust**,
targeting **UEFI + ACPI** machines. Its goal is binary compatibility with unmodified FreeBSD
userland.

- **First-class architectures:** AMD64 (`x86_64`) and ARM64 (`aarch64`), developed side by side
  so the architecture seams are real from the first commit.
- **Day-2 architecture:** RV64 (`riscv64gc`).
- **Dependencies:** none outside this repository except the Rust toolchain's own `core` / `alloc`
  (see [ADR-0002](docs/adr/0002-dependency-policy.md)).
- **First major goal:** boot the kernel on real and virtual UEFI/ACPI hardware with UFS2 and FAT32,
  and run the stock FreeBSD base system (`base.txz`) multi-user.
- **Later goals:** a new userland and an original Wayland tiling desktop.

## Documents

| Document | Purpose |
|---|---|
| [docs/VISION.md](docs/VISION.md) | Goals, non-goals, scope, and what "modern 64-bit subsystems" means |
| [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) | Kernel structure, crate layout, architecture seams, subsystem map |
| [docs/COMPATIBILITY.md](docs/COMPATIBILITY.md) | The FreeBSD userland ABI contract and how we verify it |
| [docs/ENGINEERING.md](docs/ENGINEERING.md) | Rust conventions: algebraic types, `unsafe` policy, errors, style |
| [docs/BUILD-AND-TEST.md](docs/BUILD-AND-TEST.md) | Toolchain, `xtask`, QEMU/bhyve, the build server, the test pyramid |
| [docs/HARDWARE.md](docs/HARDWARE.md) | Platform requirements and the driver matrix by phase |
| [docs/ROADMAP.md](docs/ROADMAP.md) | Phased roadmap with deliverables and exit criteria |
| [docs/adr/](docs/adr/) | Architecture Decision Records |

## Status

Phases 0 through 5 are complete. See [docs/ROADMAP.md](docs/ROADMAP.md) for the detail.

- **Boot:** our UEFI loader boots our kernel on x86_64 and aarch64, in QEMU and bhyve. It finds
  the kernel on UFS2 and passes the root named in `/etc/fstab`.
- **Core kernel (Phases 1–2):** physical and virtual memory, preemptive SMP threads, sleepable
  locks with priority propagation and a lock witness, tickless timers, and FP/SIMD state
  (including SVE and AVX-512). The Phase 2 benchmarks are within 2% of FreeBSD 15.1 in bhyve,
  and faster on most.
- **Devices (Phase 3):** a FreeBSD-style device framework, PCIe, our own ACPI AML interpreter,
  and NVMe, AHCI, and virtio storage drivers.
- **Userland (Phase 4):** unmodified FreeBSD 15.1 `/rescue/sh` runs interactively, with
  processes, signals, pipes, and a terminal. FreeBSD's own programs are the tests.
- **File systems (Phase 5):** a VFS with a name cache, page cache, and write-back. tmpfs, devfs,
  msdosfs (the ESP on `/boot/efi`), and UFS2 read-write with soft updates. Both architectures
  boot with the root on UFS2 on NVMe, holding all of FreeBSD 15.1's `base.txz`, and `mtree`
  checks every file. fsx runs clean, crashes leave images `fsck_ffs` repairs, and the root is
  left clean at power-off. pjdfstest runs on tmpfs and UFS2 against FreeBSD's results. In bhyve
  the file systems take 0.84 (UFS2) and 0.86 (tmpfs) of FreeBSD 15.1's time over the P5 suite.

Next is Phase 6: FreeBSD multi-user (dynamic linking, threads, and `init` to a `login:` prompt).

## License

BSD-2-Clause (see [ADR-0007](docs/adr/0007-license-and-clean-implementation.md)).
