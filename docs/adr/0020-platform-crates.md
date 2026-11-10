# ADR-0020: Platform knowledge in per-SoC crates behind one interface; the ACPI bus names no vendor

- Status: Proposed
- Date: 2026-11-09

## Context

DRIVER-MODEL.md §5.5 (ADR-0012, as amended) gives each supported platform a **platform
description**: what its firmware says outside the standard ACPI objects, and corrections where the
firmware is wrong or silent. The first one, for the Sky1, has landed (0e72196, 80e629e):

- `lib/devtree::platform` defines the vendor-neutral types (`Description`, `ClockLine`,
  `ResetLine`, `ResourceName`, `DeviceLink`, `ReservedMemory`).
- `sys/acpi::cix` extracts CIX's AML lookup tables (`CLKT`, `RSTL`, `RSNL`, `DLKL`, `RSVL`) into
  those types when a `CIXA1019` device is present.
- `sys/acpi::deps` calls `crate::cix::extract` directly, and the dependency origin type carries a
  vendor variant, `Via::Cix(cix::Table)`. `Malformed` and `Table` are CIX types.
- §5.5 places the `const` corrections in `lib/devtree`.

The extractor itself is sound. CIX's tables are not quirks: they are how this firmware states the
clocks, resets, resource names, and links that a devicetree would state with `clocks`, `resets`,
and `reg-names`, and something must read them. The trouble is where it plugs in. The second
platform (the Q8B's Qualcomm namespace, with `\_SB.SOID` and `PEP0`) and the third (SpacemiT K3,
ADR-0017) would each add a module to `sys/acpi`, a hard-coded call in `deps.rs`, and a `Via`
variant, and each would add its `const` data to `lib/devtree`. Both crates would grow with every
SoC and neither would stay generic. The ACPI bus could no longer be host-tested against the QEMU
and bhyve corpora without carrying every vendor's code.

FreeBSD keeps vendor knowledge out of `sys/dev/acpica`. It lives in the drivers that need it (the
MCFG OEM-ID table in `pci_host_generic_acpi.c`) or in quirk data (`acpi_quirks`). It has no
counterpart to vendor AML tables, since it does not read them.

## Decision

1. **Four kinds of platform knowledge, each with one home.**

   | Kind | Example | Home |
   |---|---|---|
   | Standard ACPI | `_CRS`, `_DEP`, `_DSD`, GED, PCC, CSRT | `sys/acpi` |
   | Vendor firmware conventions: data the firmware gives in a non-standard form | CIX `CLKT` … `RSVL` | `platform/<soc>` |
   | Corrections: firmware wrong or silent | Sky1 `_CCA` on `CIXH5000`; Q8B GMU interrupts; synthetic clock-controller nodes | `platform/<soc>`, as `const` data |
   | Hardware drivers | SCMI clocks (`CIXHA010`), resets, pinctrl, PCIe host | `drivers/<name>`, matched on `_HID`/`_CID`; per-device quirks on match entries (DRIVER-MODEL.md §7.2) |

2. **`sys/acpi` names no vendor.** It defines one interface that a platform implements, roughly:

   ```rust
   pub trait PlatformSource: Sync {
       /// The platform's name in diagnostics and edge origins ("cix-sky1").
       fn name(&self) -> &'static str;
       /// Whether this firmware is this platform's: OEM IDs, or a present
       /// namespace object (`CIXA1019`, `\_SB.SOID`).
       fn applies(&self, id: &FirmwareIdentity, ns: &mut Namespace<'_>) -> bool;
       /// The vendor tables, read into the common types. Empty if it has none.
       fn extract(&self, ns: &mut Namespace<'_>, devices: &[NodeId]) -> Extracted;
       /// The corrections.
       fn overlay(&self) -> &'static Overlay;
   }
   ```

   The graph builder runs every source that applies and merges the results. Edge origins and
   diagnostics become generic: `Via::Platform { source: &'static str, table: &'static str }`
   replaces `Via::Cix`, and `Malformed` names its table the same way. The exact signatures are
   settled in the implementation, but the interface stays vendor-neutral.

3. **`lib/devtree::platform` holds only the types** (`Description`, its line and link types, and
   the `Overlay` entry types from §5.5's table). It holds no vendor's data.

4. **One crate per SoC family under a new top-level `platform/`**: `platform/cix-sky1` now,
   later `platform/qcom-x1` and `platform/spacemit-k3`. Each crate holds its extractor, its
   overlay, and its corpus tests against `tests/corpus/acpi/<board>`. Those tests enforce §5.5's
   rule that every entry names the defect it addresses and fails once the entry no longer applies.
   A platform crate depends on `sys/acpi`'s interface and `lib/devtree`'s types, never the other
   way around.

5. **The kernel lists the platforms explicitly**: `static PLATFORMS: &[&dyn PlatformSource]`.
   There are no linker sets and no external crates (ADR-0002, ADR-0005), so a static list is the
   registration mechanism. Every platform is always built in and chosen by firmware identity
   alone, so "nothing special" holds: there is no build feature or boot option per board.

6. **The boundary between platform crates and drivers stays as §5.5 draws it**: a platform crate
   changes what the firmware *says*, and a driver decides what the hardware *does*. A platform
   crate never contains driver behavior. A driver never matches on a board.

## Consequences

- `sys/acpi::cix` moves to `platform/cix-sky1`. `deps.rs` loses its direct call and `Via::Cix`.
  The `deps_corpus` cases for `opi6p` move with it, and the remaining `sys/acpi` tests run against
  the QEMU and bhyve corpora with no platform registered.
- DRIVER-MODEL.md §5.5 is amended. The `const` data lives in `platform/<soc>`, not
  `lib/devtree`, and the CIX extractor's home is `platform/cix-sky1`.
- Adding a SoC means one new crate, one line in `PLATFORMS`, and a corpus under
  `tests/corpus/acpi/`. It needs no change to `sys/acpi` or `lib/devtree`, unless the SoC needs a
  new *kind* of entry, which is then added to the common types for every platform.
- Every built-in platform runs `applies` at graph build. That is one namespace lookup or OEM-ID
  comparison each, which is negligible next to enumeration.
- A platform's const data is linked into every kernel. That is the price of a single kernel that
  boots every supported board without configuration.
- `cargo xtask check`'s policy check should reject vendor names and `CIX`/`QCOM`-style IDs in
  `sys/acpi` and `lib/devtree`, so the boundary does not erode.
