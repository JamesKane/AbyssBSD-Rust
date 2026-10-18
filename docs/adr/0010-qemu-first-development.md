# ADR-0010: QEMU-first development; physical machines gate from P6

- Status: Accepted
- Date: 2026-10-17

## Context

Physical bring-up is slow. The Orange Pi 6 Plus costs power cycles on every hang, and the X870E
has no serial console. Most of the early phases (memory management, scheduling, VFS, process
model) are architecture- and firmware-level logic that QEMU exercises faithfully, with far faster
iteration, a gdbstub, and deterministic CI.

## Decision

- Through P5, all development and all phase exit criteria run under QEMU (q35 + OVMF, virt +
  edk2), plus bhyve for amd64.
- The physical reference machines get **non-blocking smoke checks** at P1 (boot to banner) and P3
  (ACPI load, storage attach, watchdog). Real-firmware ACPI tables from both machines are part of
  the host-side AML corpus from P3 onward.
- From **P6** (stable FreeBSD userspace), both physical machines are **gating**. The Orange Pi 6
  Plus is reimaged for AbyssBSD at that point, after its Linux and FreeBSD ground truth has been
  captured into `tests/corpus/opi6p/`.

## Consequences

- Fast iteration through the core phases, with real-hardware surprises still surfacing early
  through the smoke checks and the AML corpus.
- Hardware-only drivers (RTL8125/8126, the watchdogs, the CIX GPT, deep idle, CPPC) can trail their
  QEMU-testable counterparts. Their phase placement stays, but they block nothing before P6.
