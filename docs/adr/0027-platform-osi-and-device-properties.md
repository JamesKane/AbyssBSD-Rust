# ADR-0027: Platforms may change `_OSI` before the DSDT loads and supply typed device properties; firmware identity includes SMBIOS

- Status: Accepted (amends [ADR-0020](0020-platform-crates.md); implements
  [ADR-0023](0023-hardware-scope-consumer-decade.md) §4)
- Date: 2026-11-27

## Context

The Mac Pro (Late 2013, `MacPro6,1`) is the first machine whose platform knowledge is not about an
SoC's clocks and resets ([MACPRO.md](../MACPRO.md)). ADR-0020's interface does not cover two things
it needs:

1. **`_OSI("Darwin")`.** ADR-0023 §4 follows FreeBSD 16-CURRENT (`acpi_apple_darwin_osi`). When the
   SMBIOS maker is Apple's, `_OSI` answers `Darwin` and none of the Windows strings. Apple's `_INI`
   methods read `OSYS`, so the answer must be set before the DSDT loads. ADR-0020's `applies` and
   `extract` run at graph build, after the namespace is loaded and initialized, and
   `FirmwareIdentity` holds only ACPI OEM fields (`oem_id`, `oem_table_id`, `oem_revision`). Apple's
   ACPI OEM IDs are not a reliable match, and FreeBSD matches on the SMBIOS maker.

2. **A fan table for the Apple SMC.** One fan cools the CPU and both GPUs. From a cold boot the
   SMC's own curve leaves it at 790 rpm until the GPUs pass about 58 °C, and the firmware gives no
   ACPI thermal zone. The FreeBSD-fork project measured a fan floor policy that fixes this
   (`asmc_fan.c`, fork `50a46e98369f`). The policy needs:
   - the SMC keys to read (GPU `TG0D`/`TG1D`, CPU `TCXc`/`TC0P`);
   - the curve (GPU 45→65 °C, CPU 55→80 °C);
   - the firmware minimum (790) and the top (2600);
   - the platform-limit keys (`MSLT`, `MSTc`, `MSLG`).

   FreeBSD and the fork keep this as a model table inside `asmc`, keyed by SMBIOS product. Here that
   would break ADR-0020 §7, under which a driver never matches on a board. Putting the policy in the
   platform crate would break it the other way, because a platform crate never contains driver
   behavior.

The second need is the same shape as an existing one. A devicetree or `_DSD` would state such
parameters as properties of the device node: thermal trip points, a cooling map, a fan's speed
range. Apple's firmware is silent. The platform description already supplies what firmware should
have said and did not (DRIVER-MODEL.md §5.5).

## Decision

1. **`FirmwareIdentity` gains the SMBIOS system identity:** maker, product and family, taken from
   the SMBIOS structure the loader passes. Each field is empty when there is no SMBIOS, so ACPI-only
   matches keep working. Matching on it is still matching firmware identity. It is data, not a boot
   option.

2. **A pre-load hook: `PlatformSource::osi(&self, id: &FirmwareIdentity) -> OsiChanges`**, with an
   empty default. `sys/acpi` calls it for every platform **before** loading the DSDT and applies
   the union: interface strings to add, and strings to drop. Two platforms disagreeing on one string
   is a diagnostic, and the corpus check fails on it. A tunable can veto a change only when it is
   FreeBSD's own: `hw.acpi.apple_darwin_osi=0` turns the Darwin change off, as on FreeBSD.
   `sys/acpi` still names no vendor: it knows the tunable only as the name the platform's change
   carries.

3. **A new overlay entry kind: typed device properties.** A platform may attach properties to a
   namespace node, keyed by `_HID` (and `_UID`), as `_DSD` would carry them. They are typed values
   in `lib/acpi-platform` (`DeviceProperty { hid, name, value, why }`, static data like the rest of
   a platform). The values are integers, strings, and arrays of these, which is `_DSD`'s own value
   set.
   - **Names:** a vendor-prefixed name the device's driver documents (`apple,fan-floor-curve`), or a
     generic devicetree name where one exists.
   - **Precedence:** the driver reads its device's properties through the same accessor that returns
     `_DSD` properties, and firmware `_DSD` wins over a platform property of the same name. That
     way a firmware that does state it is believed.
   - **Driver behavior:** a driver whose device has no such property runs as it would with an empty
     `_DSD`. In `asmc` that means the SMC's own automatic curve.
   - **Diagnostics:** the applier is generic, in `sys/acpi`. A property for a node that does not
     exist is a diagnostic, and the corpus test fails on it.

4. **The ADR-0020 §7 boundary is restated, not moved.** A platform crate states parameters; it never
   holds code that runs against hardware. A driver holds all behavior and never matches on a board.
   It reads its own device's properties. `asmc` implements the floor policy, its clamps, and its
   restore at shutdown, reboot and panic. `platform/apple-mac` supplies, for `MacPro6,1`'s
   `APP0001`, the sensor keys, curve points, minimum, top and limit keys, and nothing else.

5. **Platform crates are per firmware family, not only per SoC.** `platform/apple-mac` covers
   Intel Macs with Apple's EFI (ADR-0023 §3 excludes the T2 models). ADR-0020 §5's "one crate per SoC
   family" reads "one crate per SoC or firmware family". `PLATFORMS` and the one-claimant check are
   unchanged.

## Consequences

- `lib/acpi-platform`: `FirmwareIdentity` grows. The `osi` method and `OsiChanges` are added.
  `acpi::Ops` gives the SMBIOS identity, taken from `lib/smbios`.
- `sys/acpi`: the `_OSI` change is applied before the DSDT loads. The interpreter's interface list
  (`lib/aml/src/interp/mod.rs`) becomes settable rather than fixed. The property applier is added.
  The policy check's vendor-name rule covers the new code.
- `lib/acpi-platform`: `DeviceProperty` and `PropertyValue`, and `PlatformSource::properties`.
  The property accessor drivers use (`sys/acpi::properties::property`) gives the device's `_DSD`
  value first and then a platform's for its `_HID`.
- `platform/apple-mac` is created with three things:
  - the Darwin `_OSI` change for the makers `Apple Inc.` and `Apple Computer, Inc.`;
  - the `MacPro6,1` SMC properties;
  - corpus tests against `tests/corpus/macpro61/`: `OSYS` = 10000 after `_INI` with the change and
    without it, and the `APP0001` node present.

  Each property names the omission it fills (no `_TZ`, an SMC curve that reacts late), per §5.5.
- DRIVER-MODEL.md §5.5 gains two rows: device properties, and `_OSI` changes before load.
- The next Mac (another Intel Mac with Apple EFI) adds only data to `platform/apple-mac`: its SMC
  keys and curve, measured on that machine. Without that, `asmc` runs the SMC's automatic curve.
- A board corpus is needed before a property ships, as for every overlay entry.
