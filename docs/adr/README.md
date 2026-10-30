# Architecture Decision Records

Each significant, hard-to-reverse decision gets a short record: context, decision, consequences.
Records are numbered and never deleted. A reversed decision is marked **Superseded by ADR-NNNN**.

Status values: **Proposed** (awaiting owner confirmation) · **Accepted** · **Superseded**.

| # | Title | Status |
|---|---|---|
| [0001](0001-record-architecture-decisions.md) | Record architecture decisions | Accepted |
| [0002](0002-dependency-policy.md) | Dependency policy: `core` + `alloc` in kernel, `std` on host, no external crates | Accepted |
| [0003](0003-stable-rust-builtin-targets.md) | Pinned stable Rust with built-in targets only | Accepted |
| [0004](0004-abi-target.md) | FreeBSD 15.x native 64-bit userland as the primary ABI target | Accepted |
| [0005](0005-monolithic-static-kernel.md) | Monolithic, statically linked kernel | Accepted |
| [0006](0006-uefi-acpi-only-own-loader-and-acpi.md) | UEFI + ACPI only, with our own loader and our own ACPI/AML implementation | Accepted |
| [0007](0007-license-and-clean-implementation.md) | BSD-2-Clause, with FreeBSD as specification rather than source | Accepted |
| [0008](0008-arm64-reference-board-orange-pi-6-plus.md) | Orange Pi 6 Plus as the ARM64 physical reference board | Accepted |
| [0009](0009-amd64-reference-machine-ryzen-x870e.md) | Ryzen 9 7900X / X870E AORUS ELITE WIFI7 as the AMD64 physical reference machine | Accepted |
| [0010](0010-qemu-first-development.md) | QEMU-first development; physical machines gate from P8 | Accepted; amended 2026-10-27, 2026-10-29 |
| [0011](0011-performance-parity.md) | Performance parity with FreeBSD is an exit criterion | Accepted; amended by 0014 |
| [0012](0012-driver-model.md) | Driver model: FreeBSD's userland view, framework-owned lifecycle, dependency graph, typed grants | Accepted |
| [0013](0013-vfs.md) | VFS and file systems: typed references, generic-layer protocol, one page cache, engines in `lib/` | Accepted |
| [0014](0014-performance-parity-per-component.md) | Performance parity is judged per component; benchmarks are diagnostics | Accepted |
| [0015](0015-usb-input-vt.md) | USB, input, and virtual terminals: xHCI only, transfers completed once, `hidbus`, one input class, `vt` over a pure emulator, `umass` without CAM | Accepted |
| [0016](0016-multi-user.md) | FreeBSD multi-user: FreeBSD's boot as the test plan, processes split into threads, host-tested umtx, one readiness type under poll and kqueue, a protocol-neutral socket layer, typed privilege and Capsicum rights, no networking until P7 | Accepted; amended 2026-10-29 |
| [0017](0017-rv64-reference-board-bpi-sm10.md) | Banana Pi BPI-SM10 (SpacemiT K3) as the RV64 physical reference board, booted UEFI with ACPI through SpacemiT's edk2 port | Accepted |
| [0018](0018-desktop-architecture.md) | The desktop is the AbyssBSD Swift desktop, run unmodified as FreeBSD userland; this repository builds the kernel beneath it | Accepted; amended 2026-10-30 |

Template:

```markdown
# ADR-NNNN: Title
- Status: Proposed | Accepted | Superseded by ADR-XXXX
- Date: YYYY-MM-DD

## Context
## Decision
## Consequences
```
