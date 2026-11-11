// SPDX-License-Identifier: BSD-2-Clause
//! The `GeneralPurposeIo` address-space handler a `gpiobus` installs on
//! its ACPI controller (FreeBSD's `acpi_gpiobus_space_handler`): a field
//! access in a region under the controller reads or drives the pins of
//! the field's `Connection` (`GpioIo`), bit `i` of the value the
//! connection's pin at the field's bit offset plus `i`.
//!
//! As FreeBSD's, it goes to the controller past any driver's claim on the
//! pin, refuses to drive an input-only connection or read an output-only
//! one, and leaves the pins' direction as they are. A pin index past the
//! connection's table, or a pin past the controller's, fails the access
//! (FreeBSD indexes the table unchecked).
//!
//! Behavior reference: sys/dev/gpio/acpi_gpiobus.c.

use alloc::sync::Arc;
use alloc::vec::Vec;

use aml::resource::{GpioKind, Resource};
use aml::{GpioAccess, HostError};
use bus::gpio::GpioSource;
use sched::Sleepable;

/// `ACPI_IO_RESTRICT_INPUT`, `ACPI_IO_RESTRICT_OUTPUT`.
const RESTRICT_INPUT: u8 = 1;
const RESTRICT_OUTPUT: u8 = 2;

/// The handler on one controller.
pub struct SpaceHandler {
    source: Arc<GpioSource>,
}

impl core::fmt::Debug for SpaceHandler {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SpaceHandler").finish_non_exhaustive()
    }
}

impl SpaceHandler {
    /// The handler for the controller `source`.
    #[must_use]
    pub fn new(source: Arc<GpioSource>) -> Self {
        Self { source }
    }

    /// The controller pins `a` names, and the connection's restriction.
    fn pins(a: &GpioAccess<'_>) -> Result<(Vec<u32>, u8), HostError> {
        let Ok(Resource::Gpio(g)) = aml::resource::connection(a.connection) else {
            return Err(HostError::Failed);
        };
        let restriction = match g.kind {
            GpioKind::Io { restriction, .. } => restriction,
            GpioKind::Interrupt { .. } => 0,
        };
        let first = usize::try_from(a.pin).map_err(|_| HostError::Failed)?;
        let n = usize::try_from(a.count.min(64)).map_err(|_| HostError::Failed)?;
        let pins = g
            .pins
            .get(first..first.checked_add(n).ok_or(HostError::Failed)?)
            .ok_or(HostError::Failed)?;
        Ok((pins.iter().map(|&p| u32::from(p)).collect(), restriction))
    }
}

impl acpi::opregion::RegionHandler for SpaceHandler {
    fn gpio_read(&self, s: &Sleepable, a: &GpioAccess<'_>) -> Result<u64, HostError> {
        let (pins, restriction) = Self::pins(a)?;
        if restriction == RESTRICT_OUTPUT {
            return Err(HostError::Denied);
        }
        let mut v = 0u64;
        for (i, &p) in pins.iter().enumerate() {
            let pin = self.source.raw(p).map_err(|_| HostError::Failed)?;
            if pin.get(s).map_err(|_| HostError::Failed)? {
                v |= 1 << i;
            }
        }
        Ok(v)
    }

    fn gpio_write(&self, s: &Sleepable, a: &GpioAccess<'_>, value: u64) -> Result<(), HostError> {
        let (pins, restriction) = Self::pins(a)?;
        if restriction == RESTRICT_INPUT {
            return Err(HostError::Denied);
        }
        for (i, &p) in pins.iter().enumerate() {
            let pin = self.source.raw(p).map_err(|_| HostError::Failed)?;
            pin.set(s, value & (1 << i) != 0)
                .map_err(|_| HostError::Failed)?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests;
