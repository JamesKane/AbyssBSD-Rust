# ADR-0008: Orange Pi 6 Plus as the ARM64 physical reference board

- Status: Accepted
- Date: 2026-10-17

## Context

ARM64 needs a physical Tier 1 machine that boots UEFI with ACPI only, which rules out device-tree
SBCs. The sibling project `../AbyssBSD` already brought FreeBSD 16-CURRENT up on an **Orange Pi 6
Plus** (CIX P1 / Sky1): GICv3/ITS, PSCI SMP, NVMe, 2× RTL8126, 10× xHCI, `_LPI` idle, CPPC,
thermal, the SBSA watchdog, and SMMUv3 translation all work there. Its firmware is a SystemReady-style
ACPI platform written for an OS (power resources, `_CPC`, `_LPI`, SCMI over PCC), and EL2 belongs
to the OS (no vendor hypervisor).

## Decision

The Orange Pi 6 Plus is the ARM64 Tier 1 physical machine: smoke-checked at P1 and P3, and gating
from P6 ([ADR-0010](0010-qemu-first-development.md)).
The Radxa Orion O6 (same SoC) is Tier 2. Board facts live in [HARDWARE.md §4](../HARDWARE.md#4-board-profile-orange-pi-6-plus-cix-p1--sky1).

## Consequences

- Bring-up has a working Linux and a working FreeBSD on the same hardware as ground truth. ACPI
  dumps, register state, and differential tests come from both.
- Requirements this pulls forward: DBG2 console fallback (no SPCR), EL2 entry handling, SVE
  context, PCC operation regions in AML, xHCI on ACPI, an RTL8126 driver for P7, and the SBSA
  watchdog with its quirk.
- Deep idle requires a broadcast event timer (CIX GPT), which is board-specific code. It stays
  behind a generic "always-on event timer" interface.
- The board is reimaged for AbyssBSD when it becomes gating. Its Linux and FreeBSD ground truth
  must be captured into the test corpus first.
