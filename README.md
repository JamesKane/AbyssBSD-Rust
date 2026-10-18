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

Phase 1 (boot to Rust on both architectures) is complete (2026-10-18): our loader boots our
kernel on x86_64 and aarch64 in QEMU (and on bhyve), and the kernel runs on its own W^X page
tables with serial and framebuffer consoles. Phase 2 (core kernel) is next. See
[docs/ROADMAP.md](docs/ROADMAP.md).

## License

BSD-2-Clause (see [ADR-0007](docs/adr/0007-license-and-clean-implementation.md)).
