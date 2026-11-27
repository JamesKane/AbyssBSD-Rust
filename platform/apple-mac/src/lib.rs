// SPDX-License-Identifier: BSD-2-Clause
//! Intel Macs with Apple's EFI (ADR-0023, ADR-0027): what their firmware
//! expects of the operating system outside the standard ACPI objects.
//!
//! - `_OSI`: Apple's AML sets `OSYS`, and decides what is powered and
//!   visible (Thunderbolt among it), on `_OSI("Darwin")`. As in FreeBSD
//!   16-CURRENT's `acpi_apple_darwin_osi` (`sys/dev/acpica/acpi.c`), when
//!   the SMBIOS maker is Apple's, `_OSI` answers `Darwin` and none of the
//!   Windows strings, unless `hw.acpi.apple_darwin_osi` is 0.
//!
//! - Device properties for the SMC (`APP0001`, ADR-0027 §3) on models
//!   whose SMC curve needs help, read by `asmc`: the Mac Pro (Late 2013)'s
//!   fan floor curve and platform-limit keys ([`MACPRO6_1`]).
//!
//! Apple's machines are matched by the SMBIOS maker, as FreeBSD matches
//! them, not by their ACPI OEM IDs; a model's properties by the SMBIOS
//! product.

#![no_std]
#![forbid(unsafe_code)]

use acpi_platform::{
    DeviceProperty, FirmwareIdentity, OsiChanges, PlatformSource, PropertyValue as V,
};
use aml::{Host, Interpreter, NodeId};

/// The platform's name in diagnostics.
pub const NAME: &str = "apple-mac";

/// The SMBIOS makers FreeBSD's `acpi_apple_darwin_osi` matches.
const MAKERS: &[&str] = &["Apple Inc.", "Apple Computer, Inc."];

/// FreeBSD's tunable for the Darwin `_OSI` answer (default on).
pub const DARWIN_OSI_TUNABLE: &str = "hw.acpi.apple_darwin_osi";

/// What Apple firmware gets: Darwin, and no Windows.
const DARWIN: OsiChanges = OsiChanges {
    drop_vendor_strings: true,
    add: &["Darwin"],
    tunable: Some(DARWIN_OSI_TUNABLE),
};

/// The Apple SMC's `_HID`.
pub const SMC_HID: &str = "APP0001";

/// Why the Mac Pro (Late 2013) needs the fan floor: its firmware has no
/// thermal zone, and from a power-off the SMC's own curve holds the one
/// fan (CPU and both GPUs on one heatsink) at 790 rpm until a GPU passes
/// about 58 °C (docs/MACPRO.md §2; the FreeBSD fork's `asmc_fan.c`).
const MACPRO_FAN_WHY: &str = "no _TZ; the SMC curve reacts late on the shared heatsink";

/// What the SMC reports when it limits the platform under combined load:
/// it throttles the CPU itself, and the GPUs are the OS's to slow
/// (docs/MACPRO.md §2; the FreeBSD fork's `asmc_plimit.c`).
const MACPRO_LIMIT_WHY: &str = "the SMC's platform limit is reported only in its keys";

const fn smc(name: &'static str, value: V, why: &'static str) -> DeviceProperty {
    DeviceProperty {
        hid: SMC_HID,
        name,
        value,
        why,
    }
}

/// `MacPro6,1`'s SMC properties: the fan floor curve fitted to macOS's
/// fan from a power-off (the fork's `50a46e98369f`), and the limit keys.
pub const MACPRO6_1: &[DeviceProperty] = &[
    smc("apple,fan-floor-fan", V::Integer(0), MACPRO_FAN_WHY),
    // The firmware's own `F0Mn`, which a warm reboot does not reset.
    smc(
        "apple,fan-floor-firmware-minimum",
        V::Integer(790),
        MACPRO_FAN_WHY,
    ),
    // The fan's real maximum: `F0Mx` and the target stop there under macOS.
    smc("apple,fan-floor-top", V::Integer(2600), MACPRO_FAN_WHY),
    // Both GPU dies, which the SMC reads without a GPU driver.
    smc(
        "apple,fan-floor-gpu-keys",
        V::Strings(&["TG0D", "TG1D"]),
        MACPRO_FAN_WHY,
    ),
    smc(
        "apple,fan-floor-gpu-range",
        V::Integers(&[45, 65]),
        MACPRO_FAN_WHY,
    ),
    smc(
        "apple,fan-floor-cpu-keys",
        V::Strings(&["TCXc", "TC0P"]),
        MACPRO_FAN_WHY,
    ),
    smc(
        "apple,fan-floor-cpu-range",
        V::Integers(&[55, 80]),
        MACPRO_FAN_WHY,
    ),
    // The limit (`flt`, 0 to 100), its GPU step and its CPU step.
    smc(
        "apple,power-limit-keys",
        V::Strings(&["MSLT", "MSLG", "MSTc"]),
        MACPRO_LIMIT_WHY,
    ),
];

/// The models with SMC properties, by SMBIOS product.
const MODELS: &[(&str, &[DeviceProperty])] = &[("MacPro6,1", MACPRO6_1)];

/// The platform, for `kernel/table`'s `PLATFORMS`.
pub static PLATFORM: AppleMac = AppleMac;

/// Intel Macs with Apple's EFI.
#[derive(Debug)]
pub struct AppleMac;

impl AppleMac {
    /// Whether `id` is an Apple machine's firmware.
    #[must_use]
    pub fn matches(id: &FirmwareIdentity) -> bool {
        MAKERS.contains(&id.system.maker.as_str())
    }
}

impl PlatformSource for AppleMac {
    fn name(&self) -> &'static str {
        NAME
    }

    fn applies(
        &self,
        id: &FirmwareIdentity,
        _: &mut Interpreter,
        _: &mut dyn Host,
        _: &[NodeId],
    ) -> bool {
        Self::matches(id)
    }

    fn properties(&self, id: &FirmwareIdentity) -> &'static [DeviceProperty] {
        if !Self::matches(id) {
            return &[];
        }
        MODELS
            .iter()
            .find(|(product, _)| *product == id.system.product)
            .map_or(&[], |(_, p)| p)
    }

    fn osi(&self, id: &FirmwareIdentity) -> OsiChanges {
        if Self::matches(id) {
            DARWIN
        } else {
            OsiChanges::NONE
        }
    }
}

#[cfg(test)]
mod tests;
