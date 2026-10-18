# ADR-0007: BSD-2-Clause, with FreeBSD as specification rather than source

- Status: Accepted
- Date: 2026-10-17

## Context

The project reimplements FreeBSD's kernel interfaces. FreeBSD is BSD-licensed, so even derived code
would be license-compatible. The mandate, though, is all-new code designed around Rust idioms, not
a transliteration of C.

## Decision

- License the project under **BSD-2-Clause**, with SPDX headers in every file.
- FreeBSD source is used as a **behavioral specification** (semantics, ABI layouts, hardware
  quirks), not as source to translate. ABI constants and layouts are reproduced exactly because
  they are interfaces.
- Primary references for hardware are the published specifications (UEFI, ACPI, PCIe, NVMe, AHCI,
  xHCI, VIRTIO, Arm ARM, Intel SDM, AMD APM, GIC, SBSA/BSA, RISC-V privileged/SBI/AIA).
- GPL-licensed code (Linux, GPL ACPICA) is never consulted when writing code.

## Consequences

- License compatibility with FreeBSD, so code can flow back to FreeBSD if ever desired.
- Clear provenance for every file.
