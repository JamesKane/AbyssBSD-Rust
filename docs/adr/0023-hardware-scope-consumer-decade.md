# ADR-0023: Hardware scope is what consumers could buy in the last decade, where we have a machine; the AMD64 firmware floor is 64-bit EFI 1.10

- Status: Proposed
- Date: 2026-11-10

## Context

HARDWARE.md §1 set the AMD64 floor at UEFI 2.7+ with the CSM disabled and ACPI 6.0+. Nothing chose
those numbers: they described the reference machines (X870E, B650E, Z690-A, all 2021 or later), not
anything the code needs. The loader and kernel need much less:

- **Boot services.** The loader uses `HandleProtocol`, `LocateProtocol`, `GetMemoryMap`,
  `ExitBootServices` (retried when the map key goes stale), Loaded Image, Simple File System,
  Block I/O, and Device Path, all in EFI 1.10. Beyond that it uses GOP (UEFI 2.0) for the console
  and the RNG protocol (UEFI 2.3.1) only if present (`boot/loader-efi/src/rng.rs`). It reads the
  revision in the system table only to print it (`boot/loader-efi/src/main.rs:100`).
- **Runtime services.** The kernel calls the time, variable, and reset services, all in EFI 1.10,
  under its own 1:1 map. It never calls `SetVirtualAddressMap` (`boot/bootproto`), and never reaches
  the UEFI 2.0 entries (`QueryVariableInfo`, the capsule calls) that a 1.10-sized runtime table
  lacks.
- **Tables.** The loader needs an ACPI 2.0 RSDP (`main.rs:233`) and takes SMBIOS 2.x as well as 3.x.

The owner wants AbyssBSD to run on what mainstream consumers could buy in the last decade, wherever
we have a machine to test on. The first such machine is a **Mac Pro (Late 2013, `MacPro6,1`)**, which
the owner has. Apple's firmware is its own fork of Intel's pre-edk2 EFI. It reports **EFI 1.10** in
its system table, adds the UEFI 2.x protocols Apple needed (GOP among them), keeps a CSM for Boot
Camp, and has ACPI tables older than 6.0. Under the current floor it is out of scope. Under what the
code actually needs, it is in.

FreeBSD supports Macs of this age. 16-CURRENT answers `_OSI("Darwin")` and drops the Windows
strings when `smbios.system.maker` is `Apple Inc.` or `Apple Computer, Inc.` (`sys/dev/acpica/acpi.c`,
`acpi_apple_darwin_osi`, tunable `hw.acpi.apple_darwin_osi`, default on; commit 90d79dd549,
2026-02-14, not in `release/15.1.0`). Apple's AML sets `OSYS` to 10000 for Darwin, and methods keyed
on it decide what is powered and visible, including Thunderbolt. Its loader keeps UGA quirks for the
2006–2008 Macs (`stand/efi/loader/framebuffer.c`), which are older than this scope.

## Decision

1. **Scope.** AbyssBSD targets the machines mainstream consumers could buy in the ten years before
   the current date, that boot UEFI with ACPI (ADR-0006 stands). A machine joins when we have one:
   it gets a board profile in HARDWARE.md and is booted at milestone exits. Machines we do not have
   stay Tier 2, best effort. A machine bought new within the decade counts even if its model came
   out earlier (the Mac Pro 2013 was on sale until 2019).

2. **AMD64 firmware floor: 64-bit EFI 1.10.** The loader and kernel require only what EFI 1.10
   provides, plus GOP for the console and an ACPI 2.0+ RSDP with an XSDT. Every newer service is
   optional, with a fallback, and the code says which it is. A CSM may be present: AbyssBSD boots
   through EFI and never uses it. The ACPI floor is the 2.0 table layout. Parsers accept any later
   revision of each table, and older FADT and MADT revisions are tested against a real corpus, not
   assumed.

3. **Out of scope until we have such a machine:** 32-bit EFI on 64-bit CPUs (Bay Trail and Cherry
   Trail tablets, some until about 2017), and Intel Macs with the T2 chip (2018–2020), which need
   their Secure Boot lowered in macOS Recovery and put the internal SSD behind Apple's own NVMe
   variant. Machines without UEFI and ACPI (Apple silicon, most SBCs) remain out, by ADR-0006.

4. **Apple's `_OSI` follows FreeBSD 16-CURRENT.** On a machine whose SMBIOS maker is Apple's,
   `_OSI` answers `Darwin` and none of the Windows strings, unless `hw.acpi.apple_darwin_osi=0`.
   This is FreeBSD's tunable with FreeBSD's default, not a board setting. The match is data in a
   platform crate (ADR-0020), `platform/apple-mac`, so `sys/acpi` still names no vendor. The crate
   supplies an interface list that `sys/acpi` applies **before** it loads the DSDT, since Apple's
   `_INI` methods read `OSYS`. That needs one addition to ADR-0020's interface: a hook taking the
   firmware identity (extended with the SMBIOS maker) and returning `_OSI` changes, and running
   before `applies`.

5. **ARM64 and RV64 floors do not change.** Consumer ARM64 machines of the decade that boot UEFI
   with ACPI (Windows-on-Arm laptops) already fall under Tier 2. The Radxa Dragon Q8B is the case
   on hand.

## Consequences

- HARDWARE.md §1 states the floors above. §2 gains a "Tier 2, physical (in hand)" row: machines we
  have, with a profile, booted at milestone exits but not gating merges. The Mac Pro is the first,
  profiled in §9.
- The loader's EFI 1.10 baseline becomes a rule: a new boot or runtime service that is not in
  EFI 1.10 needs a fallback, and the unit tests run the loader's protocol lookups against a
  1.10-sized system table and runtime table.
- `lib/acpi-tables` and `lib/aml` get the Mac Pro's tables as a corpus (`tests/corpus/macpro61/`),
  the first one older than ACPI 5.0.
- The work to run on the Mac Pro is drivers and an installer change, not loader changes. Each is
  needed by other decade-old machines too:
  - `ichwd` for the C600-series (Patsburg) PCH.
  - `bge` for the BCM57762.
  - Southern Islands graphics (FirePro D-series) under M-G.
  - Boot-variable handling for Apple firmware, which mostly ignores `BootOrder` in favor of its
    own variables (to be checked on the machine).
  The order of this work goes in the roadmap.
- Following 16-CURRENT, not 15.1, for the Darwin `_OSI` answer is kernel behavior, not ABI, so
  ADR-0004 is unaffected.
- The ±2% performance gate (ADR-0014) is measured on the reference machines. A Tier 2 physical
  machine is checked for correctness, not performance.
