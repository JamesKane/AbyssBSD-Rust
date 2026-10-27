# ADR-0017: Banana Pi BPI-SM10 as the RV64 physical reference board

- Status: Accepted
- Date: 2026-10-27

## Context

RV64 is Day 2 (P10). Its exit is QEMU `virt` with `acpi=on`, and the roadmap left real hardware
"as UEFI+ACPI RV64 boards become available". A board is now on order: the Banana Pi **BPI-SM10**
developer kit, a SpacemiT **K3-CoM260** module (eight X100 harts, RV64GCV with H and AIA; eight
A100 harts beside them) on SpacemiT's K3-COM260 KIT carrier. A clean-room specification for the
SoC exists in `~/Projects/OS/chinese-wall/specs/spacemit-k3/`, which also records the board.

Two facts shape the decision:

- **Firmware.** Shipped boards boot Boot ROM → SPL → OpenSBI (with RPMI services) → U-Boot from an
  8 MiB SPI NOR, with a devicetree. They have no UEFI firmware. SpacemiT publishes a K3 port of
  edk2 (`spacemit-com/edk2-platforms`, branch `k3`, BSD-2-Clause-Patent) with ACPI tables (DSDT,
  MADT, RHCT, MCFG, PPTT, SPCR), so UEFI with ACPI is possible but is not what the board ships.
- **FreeBSD.** FreeBSD's riscv64 kernel is devicetree-only (`options FDT`, no ACPI), and its APLIC
  driver supports only direct delivery, which the K3 does not use. FreeBSD does not run on this
  board today, so it provides neither a behavioral reference for ACPI on RISC-V nor a performance
  baseline on this hardware.

## Decision

- The BPI-SM10 is the **RV64 physical reference board**.
- [ADR-0006](0006-uefi-acpi-only-own-loader-and-acpi.md) holds: AbyssBSD boots it through **UEFI
  with ACPI**, using the K3 edk2 port, never the shipped U-Boot and devicetree. Getting that
  firmware onto the board (flashed to the SPI NOR, or loaded as a payload from removable media) is
  P10 work, decided once the board's recovery path has been tried. If the firmware cannot be made
  to work, the board drops to Tier 2 and RV64 stays QEMU-only until a board can.
- On ACPI RISC-V, **firmware owns the SoC plumbing**: clocks, resets, pad and pin configuration,
  power domains, and PMIC regulators are set up by firmware and changed at run time only through
  ACPI methods or RPMI over SBI MPXY. AbyssBSD has no K3 clock-tree or pin-controller driver unless
  the ACPI tables leave no other way, which would be recorded as a firmware gap first.
- **QEMU first**, as [ADR-0010](0010-qemu-first-development.md) does for the other machines: the
  board is untouched until FreeBSD riscv64 userland runs end to end in QEMU (P10), then brought up
  and gating for RV64 from then on. Before anything is written to it, its shipped firmware and
  U-Boot environment are saved, and the SPI NOR image is dumped, so the board can be returned to its
  shipped state.
- Only harts 0–7 (X100) are used. The A100 harts (8–15) differ in vector length and lack the H
  extension. They stay stopped until the scheduler has a reason to use them.

## Consequences

- P10 gains a hardware milestone, and the RV64 column of HARDWARE.md gains a board profile.
- AbyssBSD will be one of the first systems to run this board under ACPI. Firmware defects will be
  ours to find; the platform description ([DRIVER-MODEL.md §5.5](../DRIVER-MODEL.md)) is where
  their fixes go.
- Interrupts need an APLIC driver in MSI mode with an IMSIC, from the ratified AIA specification.
  That is already the P10 plan for QEMU `virt`.
- **Performance parity has no FreeBSD baseline on this board.** PERFORMANCE.md has no RV64
  rule yet. The likely one: compare with FreeBSD riscv64 under QEMU to catch regressions, and on the
  board record without gating until FreeBSD runs there. It must be written before P10 exits.
- The SoC's GPU (PowerVR BXM-4-64), video codec (Linlon, same family as Sky1's), and display are
  later work. The GPU's upstream kernel driver and firmware are MIT-usable and fit
  [GRAPHICS.md](../GRAPHICS.md).
