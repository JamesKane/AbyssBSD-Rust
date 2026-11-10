// SPDX-License-Identifier: BSD-2-Clause
//! `gpio`: the Cadence GPIO controller (r1p02) of the CIX Sky1
//! (`CIXH1002`, `CIXH1003`), from the clean-room specification
//! `sky1-peripherals` §1.
//!
//! - Seven controllers: four in the FCH domain (`0x04120000`–`0x04150000`;
//!   32, 32, 32, and 17 lines) and three in the always-on S5 domain
//!   (`0x16004000`–`0x16006000`; 32, 10, and 10 lines). Firmware gives no
//!   line count, so it comes from the base address (§1.1); lines past it
//!   read as zero.
//! - Each publishes its lines as a GPIO provider (`bus::gpio`) and as a
//!   cascaded interrupt domain (`irq::cascade`): one level-high GIC
//!   interrupt is the OR of every unmasked pending line, and its filter
//!   runs the pending lines' handlers. An edge line is acknowledged before
//!   its handler runs, so an edge during it is not lost; a level line
//!   after (§1.3.3). A pending line nobody handles is masked.
//! - Attach masks and acknowledges every line before the parent interrupt
//!   is unmasked (§1.3.1), and changes nothing else: a line's bypass bit is
//!   cleared only when a consumer configures it, and restored when the
//!   last one lets it go (the specification's conservative reading of its
//!   open question).
//! - Pull-ups and pull-downs belong to the pad, which the pin controller
//!   (`sky1_pinctrl`) owns and firmware sets: a bias asked of this
//!   controller is unsupported.
//! - The FCH controllers share their APB clock and their reset line, so
//!   each holds both counted and none ever asserts the reset (§1.1).
//! - Once it serves lines, it adds the `gpio_aei` child for its `_AEI`
//!   (the Sky1's EC line on `GPI4`).
//!
//! Wake (the firmware call of §1.5) and suspend come with P11.
//!
//! FreeBSD has no driver for this controller; the class is FreeBSD's
//! (`gpio0`, as `pl061` and the others take it). Recorded in
//! docs/compat-deviations.md.

#![no_std]
// The tests make a `Sleepable` for the host; the crate itself has no
// unsafe code.
#![cfg_attr(not(test), forbid(unsafe_code))]
#![cfg_attr(test, deny(unsafe_code))]

extern crate alloc;

use alloc::string::String;
use alloc::sync::Arc;

use bus::gpio::{Gpio, PinMode, Pull};
use bus::provider::{ClockHandle, ProviderError, ResetHandle};
use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, IrqLine, MmioGrant, Resource,
    ResourceError, StartCx, StopCx, StopReason,
};
use devtree::ids::AcpiId;
use irq::cascade::{Cascade, CascadeDomain};
use irq::{Filter, IrqDomain, IrqError, Polarity, Trigger};
use regs::Bytes;
use sched::{IrqCx, Sleepable};
use sync::SpinLock;

/// The register map (§1.2): one bit per line in every register.
pub mod reg {
    /// 1: the line passes through to another function.
    pub const BYPASS_MODE: usize = 0x00;
    /// 1: input; 0: output.
    pub const DIRECTION_MODE: usize = 0x04;
    /// 1: the output driver is on (while an output).
    pub const OUTPUT_EN: usize = 0x08;
    /// The level an output drives.
    pub const OUTPUT_VALUE: usize = 0x0c;
    /// The pad's level.
    pub const INPUT_VALUE: usize = 0x10;
    /// 1: the line's interrupt is masked (read only).
    pub const IRQ_MASK: usize = 0x14;
    /// Write 1: unmask.
    pub const IRQ_EN: usize = 0x18;
    /// Write 1: mask.
    pub const IRQ_DIS: usize = 0x1c;
    /// 1: pending; write 1 to acknowledge.
    pub const IRQ_STATUS: usize = 0x20;
    /// 1: level-sensitive; 0: edge.
    pub const IRQ_TYPE: usize = 0x24;
    /// Level: 1 high. Edge: 1 rising.
    pub const IRQ_VALUE: usize = 0x28;
    /// 1: both edges (overrides `IRQ_VALUE`).
    pub const IRQ_ANY_EDGE: usize = 0x2c;
    /// The bytes used.
    pub const SPAN: usize = 0x30;
}

/// The `_HID`s, both the same programming model (§1.1.2).
pub const HIDS: [&str; 2] = ["CIXH1002", "CIXH1003"];

/// How many lines the controller at `base` has (§1.1).
#[must_use]
pub fn lines_at(base: u64) -> u32 {
    match base {
        0x0415_0000 => 17,
        0x1600_5000 | 0x1600_6000 => 10,
        _ => 32,
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

/// One controller's lines.
pub struct Controller<W> {
    words: W,
    lines: u32,
    /// `BYPASS_MODE` as firmware left it, restored for a released line.
    bypass: u32,
    /// Serializes read-modify-writes of the shared configuration words
    /// (§1.3.4). The mask and acknowledge registers need none.
    config: SpinLock<()>,
}

impl<W> core::fmt::Debug for Controller<W> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Controller")
            .field("lines", &self.lines)
            .field("bypass", &format_args!("{:#x}", self.bypass))
            .finish_non_exhaustive()
    }
}

impl<W: Words> Controller<W> {
    /// Takes over `lines` lines in `words`: masks every line's interrupt
    /// and acknowledges what is pending, so the parent interrupt can be
    /// unmasked without a storm (§1.3.1).
    pub fn new(words: W, lines: u32) -> Self {
        let all = if lines >= 32 {
            u32::MAX
        } else {
            (1 << lines) - 1
        };
        words.write32(reg::IRQ_DIS, all);
        let pending = words.read32(reg::IRQ_STATUS);
        words.write32(reg::IRQ_STATUS, pending);
        let bypass = words.read32(reg::BYPASS_MODE);
        Self {
            words,
            lines,
            bypass,
            config: SpinLock::new(()),
        }
    }

    /// How many lines it has.
    #[must_use]
    pub fn lines(&self) -> u32 {
        self.lines
    }

    fn all(&self) -> u32 {
        if self.lines >= 32 {
            u32::MAX
        } else {
            (1 << self.lines) - 1
        }
    }

    fn bit(&self, line: u32) -> Option<u32> {
        (line < self.lines).then(|| 1 << line)
    }

    /// Sets or clears `bit` of the word at `offset`; the caller holds the
    /// configuration lock.
    fn update(&self, offset: usize, bit: u32, set: bool) {
        let v = self.words.read32(offset);
        let n = if set { v | bit } else { v & !bit };
        if n != v {
            self.words.write32(offset, n);
        }
    }

    /// Makes `line` an input, or an output driving `high` (the level
    /// first, then the direction, so it does not glitch; §1.3.2), under
    /// GPIO control.
    pub fn set_mode(&self, line: u32, mode: PinMode) -> Result<(), ProviderError> {
        let b = self.bit(line).ok_or(ProviderError::NoSuchLine)?;
        let _g = self.config.lock();
        match mode {
            PinMode::Input => self.update(reg::DIRECTION_MODE, b, true),
            PinMode::Output { high } => {
                self.update(reg::OUTPUT_VALUE, b, high);
                self.update(reg::OUTPUT_EN, b, true);
                self.update(reg::DIRECTION_MODE, b, false);
            }
        }
        self.update(reg::BYPASS_MODE, b, false);
        Ok(())
    }

    /// The level on `line`'s pad.
    pub fn level(&self, line: u32) -> Result<bool, ProviderError> {
        let b = self.bit(line).ok_or(ProviderError::NoSuchLine)?;
        Ok(self.words.read32(reg::INPUT_VALUE) & b != 0)
    }

    /// Drives `line` (an output) to `high`.
    pub fn drive(&self, line: u32, high: bool) -> Result<(), ProviderError> {
        let b = self.bit(line).ok_or(ProviderError::NoSuchLine)?;
        let _g = self.config.lock();
        self.update(reg::OUTPUT_VALUE, b, high);
        Ok(())
    }

    /// `line`'s last user is gone: masked, its both-edges bit cleared (a
    /// stale one overrides a later single edge, §1.2.1), and its bypass
    /// bit as firmware left it.
    pub fn release(&self, line: u32) {
        let Some(b) = self.bit(line) else { return };
        self.words.write32(reg::IRQ_DIS, b);
        let _g = self.config.lock();
        self.update(reg::IRQ_ANY_EDGE, b, false);
        self.update(reg::BYPASS_MODE, b, self.bypass & b != 0);
    }

    /// Sets `line` to interrupt on `trigger` and `polarity`, all three
    /// trigger registers together (§1.2.1), masked, with nothing left
    /// pending from before.
    pub fn set_trigger(
        &self,
        line: u32,
        trigger: Trigger,
        polarity: Polarity,
    ) -> Result<(), IrqError> {
        let b = self.bit(line).ok_or(IrqError::NoSuchInput)?;
        let (level, value, any) = match (trigger, polarity) {
            (Trigger::Level, Polarity::High) => (true, true, false),
            (Trigger::Level, Polarity::Low) => (true, false, false),
            (Trigger::Edge, Polarity::High) => (false, true, false),
            (Trigger::Edge, Polarity::Low) => (false, false, false),
            (Trigger::Edge, Polarity::Both) => (false, false, true),
            (Trigger::Level, Polarity::Both) => return Err(IrqError::Unsupported),
        };
        self.words.write32(reg::IRQ_DIS, b);
        {
            let _g = self.config.lock();
            self.update(reg::IRQ_TYPE, b, level);
            self.update(reg::IRQ_VALUE, b, value);
            self.update(reg::IRQ_ANY_EDGE, b, any);
        }
        self.words.write32(reg::IRQ_STATUS, b);
        Ok(())
    }

    /// Masks `line`'s interrupt.
    pub fn mask(&self, line: u32) {
        if let Some(b) = self.bit(line) {
            self.words.write32(reg::IRQ_DIS, b);
        }
    }

    /// Unmasks `line`'s interrupt.
    pub fn unmask(&self, line: u32) {
        if let Some(b) = self.bit(line) {
            self.words.write32(reg::IRQ_EN, b);
        }
    }

    /// The parent interrupt's work (§1.3.3): each pending unmasked line,
    /// lowest first, runs its handlers through `dispatch`. An edge line
    /// is acknowledged before, a level line after (a level line still
    /// active comes back once unmasked). A line `dispatch` has no handler
    /// for is masked. The status is read once: earlier versions of the IP
    /// cleared it on read. False if nothing was pending.
    pub fn service(&self, mut dispatch: impl FnMut(u32) -> bool) -> bool {
        let pending =
            self.words.read32(reg::IRQ_STATUS) & !self.words.read32(reg::IRQ_MASK) & self.all();
        if pending == 0 {
            return false;
        }
        let level = self.words.read32(reg::IRQ_TYPE);
        let mut left = pending;
        while left != 0 {
            let line = left.trailing_zeros();
            let b = 1 << line;
            left &= !b;
            if level & b == 0 {
                self.words.write32(reg::IRQ_STATUS, b);
                if !dispatch(line) {
                    self.words.write32(reg::IRQ_DIS, b);
                }
            } else {
                if !dispatch(line) {
                    self.words.write32(reg::IRQ_DIS, b);
                }
                self.words.write32(reg::IRQ_STATUS, b);
            }
        }
        true
    }
}

impl<W: Words> Cascade for Controller<W> {
    fn name(&self) -> &'static str {
        "gpio"
    }

    fn inputs(&self) -> u32 {
        self.lines
    }

    fn configure(&self, hw: u32, trigger: Trigger, polarity: Polarity) -> Result<(), IrqError> {
        self.set_trigger(hw, trigger, polarity)
    }

    fn release(&self, hw: u32) {
        // The pin claim's release restores the rest.
        self.mask(hw);
        let Some(b) = self.bit(hw) else { return };
        let _g = self.config.lock();
        self.update(reg::IRQ_ANY_EDGE, b, false);
    }

    fn mask(&self, hw: u32) {
        Controller::mask(self, hw);
    }

    fn unmask(&self, hw: u32) {
        Controller::unmask(self, hw);
    }
}

/// The lines as a GPIO provider, and the domain they interrupt in.
pub struct Pins<W> {
    ctl: Arc<Controller<W>>,
    domain: &'static dyn IrqDomain,
}

impl<W> core::fmt::Debug for Pins<W> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.ctl.fmt(f)
    }
}

impl<W: Words> Pins<W> {
    /// `ctl`'s lines, interrupting in `domain`.
    pub fn new(ctl: Arc<Controller<W>>, domain: &'static dyn IrqDomain) -> Self {
        Self { ctl, domain }
    }
}

impl<W: Words> Gpio for Pins<W> {
    fn pins(&self) -> u32 {
        self.ctl.lines()
    }

    fn configure(
        &self,
        _: &Sleepable,
        pin: u32,
        mode: PinMode,
        pull: Pull,
    ) -> Result<(), ProviderError> {
        self.ctl.set_mode(pin, mode)?;
        match pull {
            Pull::Default | Pull::None => Ok(()),
            Pull::Up | Pull::Down => Err(ProviderError::Unsupported),
        }
    }

    fn get(&self, _: &Sleepable, pin: u32) -> Result<bool, ProviderError> {
        self.ctl.level(pin)
    }

    fn set(&self, _: &Sleepable, pin: u32, high: bool) -> Result<(), ProviderError> {
        self.ctl.drive(pin, high)
    }

    fn release(&self, _: &Sleepable, pin: u32) {
        self.ctl.release(pin);
    }

    fn interrupts(&self) -> Option<&'static dyn IrqDomain> {
        Some(self.domain)
    }
}

/// `gpioN`.
pub struct CdnsGpio {
    ctl: Arc<Controller<MmioGrant<Bytes>>>,
    cascade: CascadeDomain,
    node: Option<aml::NodeId>,
    parent: SpinLock<Option<IrqLine>>,
    _clock: Option<ClockHandle>,
    _reset: Option<ResetHandle>,
}

impl core::fmt::Debug for CdnsGpio {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CdnsGpio")
            .field("ctl", &self.ctl)
            .finish_non_exhaustive()
    }
}

fn intr(d: &CdnsGpio, _: usize, cx: &IrqCx) -> Filter {
    if d.ctl.service(|line| d.cascade.dispatch(line, cx)) {
        Filter::Handled
    } else {
        Filter::Stray
    }
}

/// A line the device may lack: none is not an error.
fn optional<T>(r: Result<T, ResourceError>) -> Result<Option<T>, AttachError> {
    match r {
        Ok(t) => Ok(Some(t)),
        Err(ResourceError::NoSuchLine) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

impl Driver for CdnsGpio {
    const NAME: &'static str = "gpio";
    const IDS: Ids = Ids::Acpi(&[AcpiId::new(HIDS[0]), AcpiId::new(HIDS[1])]);

    fn describe(_: &Ident) -> String {
        "Cadence GPIO controller".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let base = cx
            .resources()
            .iter()
            .find_map(|r| match r {
                Resource::Memory { base, .. } => Some(*base),
                _ => None,
            })
            .ok_or(AttachError::Device("no registers"))?;
        let s = cx.sleepable();
        // The clock first, then the block out of reset (§1.3.1). The FCH
        // controllers share both: counted, and never asserted.
        let mut clock = optional(cx.clock(None))?;
        if let Some(c) = &mut clock {
            c.enable(s)
                .map_err(|e| AttachError::from(ResourceError::Provider(e)))?;
        }
        let mut reset = optional(cx.reset(None))?;
        if let Some(r) = &mut reset {
            r.deassert(s)
                .map_err(|e| AttachError::from(ResourceError::Provider(e)))?;
        }
        let regs = cx.mmio_bytes(0)?;
        if regs.len() < reg::SPAN {
            return Err(AttachError::Device("register window too small"));
        }
        let ctl = Arc::new(Controller::new(regs, lines_at(base)));
        let cascade = CascadeDomain::new(Arc::clone(&ctl) as Arc<dyn Cascade>)
            .map_err(|e| AttachError::from(ResourceError::Irq(e)))?;
        let parent = cx.irq(0)?;
        let node = cx.bus_data::<acpi::AcpiDevice>().map(|d| d.node);
        Ok(Self {
            ctl,
            cascade,
            node,
            parent: SpinLock::new(Some(parent)),
            _clock: clock,
            _reset: reset,
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        cx.provide_gpio(Arc::new(Pins::new(
            Arc::clone(&self.ctl),
            self.cascade.domain(),
        )));
        // Taken in its own statement: installing may sleep.
        let parent = self.parent.lock().take();
        if let Some(line) = parent {
            cx.interrupt(line, Some(intr), None, 0)?;
        }
        if let Some(node) = self.node {
            acpi::aei::add(cx, node);
        }
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {
        // Consumers have gone; nothing of theirs is left unmasked.
        for line in 0..self.ctl.lines() {
            self.ctl.mask(line);
        }
    }
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<CdnsGpio>();

#[cfg(test)]
#[allow(unsafe_code)]
mod tests;
