// SPDX-License-Identifier: BSD-2-Clause
//! `sky1_reset`: the CIX Sky1's two reset controllers, which the
//! platform's `RSTL` tables name (`platform/cix-sky1`):
//!
//! - `RST0` (`CIXHA020`), the always-on S5 system controller at
//!   `0x16000000`: 143 lines (DDR, GIC, GPU, NPU, VPU, display, PCIe, USB,
//!   and each subsystem's RCSU);
//! - `RST1` (`CIXHA021`), the FCH system controller at `0x04160000`: 29
//!   lines (UARTs, I2C, SPI, I3C, GPIO, the GPT timer, the FCH DMA).
//!
//! Every line is one bit, active low: clear holds the block in reset, set
//! releases it. Other bits of a word belong to other blocks, so every
//! change is a read-modify-write under the controller's lock. A plain
//! assert or deassert does not wait; a consumer that pulses a line waits
//! as its block needs. Attach changes nothing: firmware leaves the blocks
//! a booted system needs out of reset, and their drivers deassert what
//! they use.
//!
//! The index maps are the clean-room `sky1-soc` specification's (§2.1 and
//! Annex A): the values RSTL entries carry.
//!
//! FreeBSD has no driver for them (its drivers rely on firmware's reset
//! state); the name is recorded in docs/compat-deviations.md.

#![no_std]
// The tests make a `Sleepable` for the host; the crate itself has no
// unsafe code.
#![cfg_attr(not(test), forbid(unsafe_code))]
#![cfg_attr(test, deny(unsafe_code))]

extern crate alloc;

use alloc::string::String;
use alloc::sync::Arc;

use bus::provider::{ProviderError, Reset};
use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, MmioGrant, StartCx, StopCx, StopReason,
};
use devtree::ids::AcpiId;
use regs::Bytes;
use sched::Sleepable;
use sync::SpinLock;

/// The S5 controller's `_HID`.
pub const S5_HID: &str = "CIXHA020";
/// The FCH controller's `_HID`.
pub const FCH_HID: &str = "CIXHA021";

/// Where a reset line's bit is: the word's offset and the bit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bit {
    /// The register word's offset.
    pub offset: usize,
    /// The bit in it.
    pub bit: u32,
}

const fn at(offset: usize, bit: u32) -> Option<Bit> {
    Some(Bit { offset, bit })
}

/// Which controller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Controller {
    /// The S5 system controller (`RST0`).
    S5,
    /// The FCH system controller (`RST1`).
    Fch,
}

impl Controller {
    /// The controller a `_HID` names.
    #[must_use]
    pub fn of(hid: &str) -> Option<Self> {
        match hid {
            S5_HID => Some(Self::S5),
            FCH_HID => Some(Self::Fch),
            _ => None,
        }
    }

    /// The bit of line `index`, or `None` for a line it does not have.
    #[must_use]
    pub const fn bit(self, index: u32) -> Option<Bit> {
        match self {
            Self::S5 => match index {
                0 => at(0x304, 0),
                1 => at(0x308, 0),
                2 => at(0x30c, 0),
                // Group 0, S0 domain words 0 and 1.
                3..=29 => at(0x400, index - 3),
                30..=50 => at(0x404, index - 30),
                // Group 1: USB PHYs, then USB controllers.
                51..=70 => at(0x408, index - 51),
                71..=90 => at(0x40c, index - 71),
                // The subsystems' RCSUs.
                91..=122 => at(0x800, index - 91),
                123..=142 => at(0x804, index - 123),
                _ => None,
            },
            Self::Fch => match index {
                // I3C functional, then UART0-3 functional.
                0..=7 => at(0x008, index),
                // The GPT timer.
                8 => at(0x008, 20),
                // I3C APB, the FCH DMA's AXI.
                9..=11 => at(0x00c, index - 9),
                // UART0-3 APB, SPI0-1 APB.
                12..=17 => at(0x00c, index - 8),
                // I2C0-7 APB.
                18..=25 => at(0x00c, index - 6),
                // GPIO APB.
                26 => at(0x00c, 21),
                // XSPI register and system.
                27 | 28 => at(0x010, index - 27),
                _ => None,
            },
        }
    }

    /// The highest offset it uses, plus the word.
    #[must_use]
    pub const fn span(self) -> usize {
        match self {
            Self::S5 => 0x808,
            Self::Fch => 0x014,
        }
    }
}

/// A controller's 32-bit registers.
pub trait Words: Send + Sync {
    /// Reads the word at `offset`.
    fn read32(&self, offset: usize) -> u32;
    /// Writes the word at `offset`.
    fn write32(&self, offset: usize, v: u32);
}

impl Words for MmioGrant<Bytes> {
    fn read32(&self, offset: usize) -> u32 {
        regs::Mmio::read32(self, offset)
    }
    fn write32(&self, offset: usize, v: u32) {
        regs::Mmio::write32(self, offset, v);
    }
}

/// A controller's lines: [`Reset`] over its words.
pub struct Lines<W> {
    controller: Controller,
    words: SpinLock<W>,
}

impl<W> core::fmt::Debug for Lines<W> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Lines")
            .field("controller", &self.controller)
            .finish_non_exhaustive()
    }
}

impl<W: Words> Lines<W> {
    /// `controller`'s lines in `words`.
    pub fn new(controller: Controller, words: W) -> Self {
        Self {
            controller,
            words: SpinLock::new(words),
        }
    }

    /// Whether line `index` holds its block in reset.
    ///
    /// # Errors
    ///
    /// [`ProviderError::NoSuchLine`] for a line the controller lacks.
    pub fn asserted(&self, index: u32) -> Result<bool, ProviderError> {
        let b = self
            .controller
            .bit(index)
            .ok_or(ProviderError::NoSuchLine)?;
        Ok(self.words.lock().read32(b.offset) & 1 << b.bit == 0)
    }

    /// Sets line `index` to `released`, read-modify-write.
    fn set(&self, index: u32, released: bool) -> Result<(), ProviderError> {
        let b = self
            .controller
            .bit(index)
            .ok_or(ProviderError::NoSuchLine)?;
        let w = self.words.lock();
        let v = w.read32(b.offset);
        let v = if released {
            v | 1 << b.bit
        } else {
            v & !(1 << b.bit)
        };
        w.write32(b.offset, v);
        Ok(())
    }
}

impl<W: Words> Reset for Lines<W> {
    fn assert(&self, _: &Sleepable, line: u32) -> Result<(), ProviderError> {
        self.set(line, false)
    }

    fn deassert(&self, _: &Sleepable, line: u32) -> Result<(), ProviderError> {
        self.set(line, true)
    }
}

/// `sky1_resetN`.
pub struct Sky1Reset {
    lines: Arc<Lines<MmioGrant<Bytes>>>,
}

impl core::fmt::Debug for Sky1Reset {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.lines.fmt(f)
    }
}

fn controller(ident: &Ident) -> Option<Controller> {
    match ident {
        Ident::Acpi(id, _) => Controller::of(id.hid.as_deref()?),
        _ => None,
    }
}

impl Driver for Sky1Reset {
    const NAME: &'static str = "sky1_reset";
    const IDS: Ids = Ids::Acpi(&[AcpiId::new(S5_HID), AcpiId::new(FCH_HID)]);

    fn describe(ident: &Ident) -> String {
        match controller(ident) {
            Some(Controller::S5) => "CIX Sky1 S5 reset controller".into(),
            _ => "CIX Sky1 FCH reset controller".into(),
        }
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let c = controller(cx.ident()).ok_or(AttachError::Decline)?;
        let regs = cx.mmio_bytes(0)?;
        if regs.len() < c.span() {
            return Err(AttachError::Device("register window too small"));
        }
        Ok(Self {
            lines: Arc::new(Lines::new(c, regs)),
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        cx.provide_resets(Arc::clone(&self.lines) as Arc<dyn Reset>);
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<Sky1Reset>();

#[cfg(test)]
#[allow(unsafe_code)]
mod tests;
