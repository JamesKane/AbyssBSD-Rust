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
//! Apple's machines are matched by the SMBIOS maker, as FreeBSD matches
//! them, not by their ACPI OEM IDs. Device properties for the SMC (the Mac
//! Pro (Late 2013)'s fan floor, ADR-0027 §3) come with the `asmc` driver
//! that reads them.

#![no_std]
#![forbid(unsafe_code)]

use acpi_platform::{FirmwareIdentity, OsiChanges, PlatformSource};
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
