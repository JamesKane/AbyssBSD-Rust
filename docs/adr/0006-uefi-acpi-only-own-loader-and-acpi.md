# ADR-0006: UEFI + ACPI only, with our own loader and our own ACPI/AML implementation

- Status: Accepted
- Date: 2026-10-17

## Context

FreeBSD supports BIOS and UEFI, ACPI and FDT, and uses the third-party ACPICA library. The mandate
is modern UEFI/ACPI machines with all-new code. On ARM64, ACPI is standard on server and SystemReady
platforms, and QEMU `virt` with edk2 provides ACPI tables.

## Decision

- **Firmware interface:** UEFI only (no BIOS/CSM). Platform description: ACPI only (no FDT) for
  Day 1 on all architectures.
- **Loader:** our own Rust UEFI application (`boot/loader-efi`), handing off through a versioned
  `bootproto::BootInfo`. We do not support FreeBSD's `loader.efi` boot protocol (`modinfo`
  metadata).
- **ACPI:** our own table parser and AML interpreter in host-testable crates. No ACPICA.

## Consequences

- The loader is an early deliverable (Phase 1) and also gives us the ESP/FAT and ELF code reused
  elsewhere.
- The AML interpreter is a major, high-risk component (Phase 3). It needs a real-firmware corpus
  and fuzzing.
- Boards that only ship device trees are unsupported for now. An FDT path might be added later
  for RV64 hardware if needed.
- FreeBSD tools that read loader metadata (`kenv` values set by `loader.efi`) see the equivalent
  values that our loader provides (e.g. `kern.bootfile`, `smbios.*`, and `hint.*` where relevant).
