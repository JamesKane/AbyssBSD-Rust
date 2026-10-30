# ADR-0010: QEMU-first development; physical machines gate from P8

- Status: Accepted
- Date: 2026-10-17
- Amended: 2026-10-27. Board bring-up became its own phase, P7, after P6 (FreeBSD
  multi-user in QEMU and bhyve). The physical machines gate from P7, not P6
- Amended: 2026-10-29. Networking moved ahead of board bring-up, as P7, so the machines come up
  with `ssh`, `scp`, and `pkg`; board bring-up is P8, and the physical machines gate from P8.

## Context

Physical bring-up is slow. The Orange Pi 6 Plus costs power cycles on every hang, and the X870E
has no serial console. Most of the early phases (memory management, scheduling, VFS, process
model) are architecture- and firmware-level logic that QEMU exercises faithfully, with far faster
iteration, a gdbstub, and deterministic CI.

## Decision

- Through P5, all development and all phase exit criteria run under QEMU (q35 + OVMF, virt +
  edk2), plus bhyve for amd64.
- **No physical hardware is touched until FreeBSD userland runs end to end in QEMU**: no smoke
  checks, no table captures, no boots from USB.
- From **P8** (board bring-up, after P6 makes FreeBSD userspace stable in QEMU), both physical
  machines are brought up and are
  **gating**. Their real-firmware ACPI tables join the host-side AML corpus then. The Orange Pi 6
  Plus is reimaged for AbyssBSD at that point, after its Linux and FreeBSD ground truth has been
  captured into `tests/corpus/opi6p/`.

## Consequences

- Fast iteration through the core phases. Real-hardware surprises (firmware quirks, AML the QEMU
  tables never exercise) surface late, at P8, and P8 is a phase of its own, sized for that.
- Hardware-only drivers (RTL8125/8126, the watchdogs, the CIX GPT, deep idle, CPPC) can trail their
  QEMU-testable counterparts. Their phase placement stays, but they block nothing before P8.
