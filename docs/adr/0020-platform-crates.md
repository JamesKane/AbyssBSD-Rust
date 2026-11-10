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
- `lib/devtree::dsd`'s table of `_DSD` reference properties mixes generic devicetree names
  (`clocks`, `phys`, `power-domains`) with CIX's vendor-prefixed bindings (`cix,hw`,
  `cix,usb_syscon`, `cix,usbphy_syscon`, `cix,gmac-ctrl`, `cix,dsp-ctrl`, `sky1,fch_cru`) and
  unprefixed vendor names (`audss_cru`).
- §5.5 places the `const` corrections in `lib/devtree`.

The extractor itself is sound. CIX's tables are not quirks: they are how this firmware states the
clocks, resets, resource names, and links that a devicetree would state with `clocks`, `resets`,
and `reg-names`, and something must read them. The trouble is where it plugs in. The second
platform (the Q8B's Qualcomm namespace, with `\_SB.SOID` and `PEP0`) and the third (SpacemiT K3,
ADR-0017) would each add a module to `sys/acpi`, a hard-coded call in `deps.rs`, a `Via` variant,
and property names to `dsd.rs`, and each would add its `const` data to `lib/devtree`. Both crates
would grow with every SoC and neither would stay generic. The ACPI bus could no longer be
host-tested against the QEMU and bhyve corpora without carrying every vendor's code.

FreeBSD keeps vendor knowledge in generic form. Its ACPI quirks (`sys/dev/acpica/acpi_quirk.c`
and the `acpi_quirks` data file) live inside `acpica`, but as vendor-neutral data matched by OEM
ID and revision, with no vendor code. Vendor-specific behavior lives in the drivers that need it
(the MCFG OEM-ID table in `pci_host_generic_acpi.c`). FreeBSD has no counterpart to vendor AML
tables, since it does not read them.

## Decision

1. **Four kinds of platform knowledge, each with one home.**

   | Kind | Example | Home |
   |---|---|---|
   | Standard ACPI, and generic devicetree property names in `_DSD` | `_CRS`, `_DEP`, `_DSD` `clocks`/`phys`/`power-domains`, GED, PCC, CSRT | `sys/acpi`, `lib/devtree::dsd` |
   | Vendor firmware conventions: data the firmware gives in a non-standard form | CIX `CLKT` … `RSVL`; vendor-prefixed `_DSD` bindings (`cix,usb_syscon`, `sky1,fch_cru`) | `platform/<soc>` |
   | Corrections: firmware wrong or silent | Sky1 `_CCA` on `CIXH5000`; Q8B GMU interrupts; synthetic clock-controller nodes | `platform/<soc>`, as `const` data |
   | Hardware drivers | SCMI clocks (`CIXHA010`), resets, pinctrl, PCIe host | `drivers/<name>`, matched on `_HID`/`_CID`; per-device quirks on match entries (DRIVER-MODEL.md §7.2) |

   A property name with a vendor prefix (`vendor,name`) or a vendor's unprefixed name
   (`audss_cru`) is a vendor convention. It is classified by the platform that uses it, not by
   `lib/devtree::dsd`.

2. **`sys/acpi` names no vendor.** The interface a platform implements lives in a small pure crate,
   `lib/acpi-platform` (depending only on `lib/aml` and `lib/devtree`), so platform crates stay
   `no_std`, `forbid(unsafe_code)`, and free of kernel dependencies. Roughly:

   ```rust
   /// The firmware's identity, from the root table and the FADT and MCFG headers.
   pub struct FirmwareIdentity { pub oem_id: [u8; 6], pub oem_table_id: [u8; 8], pub oem_revision: u32 }

   pub trait PlatformSource: Sync {
       /// The platform's name in diagnostics and edge origins ("cix-sky1").
       fn name(&self) -> &'static str;
       /// Whether this firmware is this platform's: OEM IDs, or a present
       /// namespace object (`CIXA1019`, `\_SB.SOID`).
       fn applies(&self, id: &FirmwareIdentity, interp: &mut aml::Interpreter,
                  host: &mut dyn aml::Host, devices: &[aml::NodeId]) -> bool;
       /// The vendor tables, read into the common types. Empty if it has none.
       fn extract(&self, interp: &mut aml::Interpreter, host: &mut dyn aml::Host,
                  devices: &[aml::NodeId]) -> Extracted;
       /// Its vendor `_DSD` reference properties and the edge each makes.
       fn reference_kinds(&self) -> &'static [(&'static str, EdgeKind)] { &[] }
       /// The corrections.
       fn overlay(&self) -> &'static Overlay;
   }
   ```

   `applies` and `extract` evaluate AML, so they take the interpreter and its host and run on the
   ACPI service thread, as `cix::extract` does today. `applies` and `extract` stay separate: a
   platform may apply by OEM ID with only an overlay and no tables (the Q8B). The identity needs a
   typed accessor on `acpi::Ops`, which today offers only `oem_description()`, a formatted string.

   Edge origins and diagnostics become generic: `Via::Platform { source: &'static str, table:
   &'static str }` replaces `Via::Cix`, its graph origin stays `Origin::Platform`, and `Malformed`
   names its source and table the same way. The exact signatures are settled in the
   implementation, but the interface stays vendor-neutral.

3. **`sys/acpi` applies what platforms supply; platforms supply only data.**
   - The graph builder runs every source that applies and concatenates their results: clock,
     reset, and link entries, resource names, and reserved memory. Two entries for the same
     consumer and line name, from one source or two, are a diagnostic (logged at boot and failing
     the corpus tests), never a silent overwrite.
   - `_DSD` reference properties are classified by `lib/devtree::dsd`'s generic table, then by
     each applying platform's `reference_kinds`. A name no table knows is still reported.
   - Each overlay entry kind (fix a `_CRS` entry, override an attribute such as `_CCA`, split a
     node into function children, create a synthetic node, add edges and provider bindings,
     describe reserved memory) has one generic applier in `sys/acpi` (in `acpi0`'s walk and
     resource conversion). Platform crates hold no code that changes what `acpi0` does, only the
     entries.

4. **`lib/devtree::platform` holds only the types** (`Description`, its line and link types, and
   the `Overlay` entry types from §5.5's table). `lib/devtree::dsd` holds only generic property
   names. Neither holds a vendor's data.

5. **One crate per SoC family under a new top-level `platform/`**: `platform/cix-sky1` now,
   later `platform/qcom-x1` and `platform/spacemit-k3`. Each crate holds its extractor, its vendor
   property names, its overlay, and its corpus tests against `tests/corpus/acpi/<board>`. Those
   tests enforce §5.5's rule that every entry names the defect it addresses and fails once the
   entry no longer applies.
   - A platform crate depends on `lib/acpi-platform`, `lib/aml`, and `lib/devtree`, never on
     `sys/acpi`, and `sys/acpi` never depends on a platform crate. A platform crate may take
     `sys/acpi` as a dev-dependency to build its machine's whole graph in tests.
   - `sys/acpi` cannot take a platform crate even as a dev-dependency (the cycle would build two
     copies of `sys/acpi`'s types).

6. **The platforms are listed in `kernel/table`**, beside `DRIVERS`:
   `pub static PLATFORMS: &[&dyn PlatformSource]`. It is already the build's reviewed, host-checked
   registry. There are no linker sets and no external crates (ADR-0002, ADR-0005), so a static
   list is the registration mechanism, and `sys/acpi` receives it through `acpi::Ops` (or
   `acpi::init`). Every platform is always built in and chosen by firmware identity alone, so
   "nothing special" holds: there is no build feature or boot option per board. The table's host
   check also asserts that each corpus machine is claimed by at most one platform.

7. **The boundary between platform crates and drivers stays as §5.5 draws it**: a platform crate
   changes what the firmware *says*, and a driver decides what the hardware *does*. A platform
   crate never contains driver behavior. A driver never matches on a board.

## Consequences

- `sys/acpi::cix` moves to `platform/cix-sky1`, with the CIX `_DSD` property names from
  `lib/devtree::dsd`. `deps.rs` loses its direct call and `Via::Cix`.
- The corpus tests split by what they test:
  - Standard ACPI stays in `sys/acpi`'s `deps_corpus`, run with no platform: every machine's
    `_DEP`, resource-source, pin, `FixedDMA`, power-resource, region, generic `_DSD`, and
    companion edges, the Sky1's included. Its expected counts lose the `Cix(...)` entries, and its
    Sky1 `_DSD` references under CIX property names become unclassified there.
  - `platform/cix-sky1` takes the CIX tables (`cix_tables`, `opi6p_children_get_their_lines`) and
    the vendor property names.
  - Whole-machine checks run with every platform registered: that no required or power edge
    closes a cycle (`only_soft_edges_close_cycles`; CIX alone adds about 170 required edges) and
    that every `_DSD` reference is classified. They live in `kernel/table`'s host tests, the one
    crate that sees `sys/acpi` and every platform. They do not stay in `sys/acpi` (decision 5).
- DRIVER-MODEL.md §5.5 is amended. The `const` data lives in `platform/<soc>`, not
  `lib/devtree`, the CIX extractor's home is `platform/cix-sky1`, and §5.5 names the generic
  appliers in `sys/acpi`.
- Adding a SoC means one new crate, one line in `PLATFORMS`, and a corpus under
  `tests/corpus/acpi/`. It needs no change to `sys/acpi` or `lib/devtree`, unless the SoC needs a
  new *kind* of entry, which is then added to the common types (and its applier to `sys/acpi`)
  for every platform.
- Every built-in platform runs `applies` at graph build. That is one namespace lookup or OEM-ID
  comparison each, which is negligible next to enumeration.
- A platform's const data is linked into every kernel. That is the price of a single kernel that
  boots every supported board without configuration.
- `cargo xtask check`'s policy check rejects vendor names and IDs (`CIX`, `cix,`, `QCOM`, vendor
  `_HID` prefixes) in identifiers and string literals of non-test code in `sys/acpi`,
  `lib/devtree`, and `lib/acpi-platform`, so the boundary does not erode. Comments and tests may
  name a board or its firmware as an example (`sys/acpi`'s notes on the Sky1's `\_SB.PMMX`
  explain real firmware behavior), and the corpus tests in `sys/acpi` name their machines.
- "Platform" already means two other things: `bus::Platform` (the kernel services the device
  manager uses) and `lib/devtree::platform` (the description's types). The implementation
  renames one of them, so the word means one thing per crate. This ADR does not choose the name.
