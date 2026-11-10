// SPDX-License-Identifier: BSD-2-Clause
//! `cdnc_i2c`: the Cadence I2C controller (r1p14) of the CIX Sky1
//! (`CIXH200B`, eight buses), in host mode, from the clean-room
//! specification `sky1-peripherals` §2, with FreeBSD's `cdnc_i2c` (the
//! same IP on Zynq) as the behavioral reference.
//!
//! - The bus clock comes from the controller's input clock, which SCMI
//!   reports, through two dividers: SCL = F_in / (22 (A + 1) (B + 1)),
//!   the fastest rate not above the target (§2.4). The target is the
//!   `_DSD` `clock-frequency` (the Sky1 says 400 kHz, or 100 kHz on its
//!   PD controllers' buses), at most 400 kHz, else 100 kHz.
//! - A transfer is FreeBSD's `iic_msg` array: each message one
//!   direction, a repeated START between them (the controller's `HOLD`
//!   keeps the bus between messages), one STOP at the end (§2.5). A write
//!   longer than the 16-byte FIFO is fed as it drains; a read longer than
//!   it holds the bus while the FIFO is drained, up to the transfer-size
//!   counter's chunk (252 bytes on an 8-bit counter). A longer read, a
//!   zero-length message (SMBus quick), is refused.
//! - Transfers sleep on the controller's interrupt, as FreeBSD's do: the
//!   filter acknowledges the status, records it, and wakes the transfer,
//!   which unmasks only what it waits for (completion, data, the errors)
//!   and gives up if that does not come within the specification's
//!   message timeout (the message's bit time plus 500 ms, at least a
//!   second), as FreeBSD's sleeps each give up after `hz`. Only the bus-free check before
//!   a transfer polls: nothing interrupts when the bus goes free.
//! - Any error resets the controller's state (§2.5.4); the bus-hold
//!   timeout is maxed out and its interrupt never used (§2.6).
//!
//! Once the provider is published, the controller adds its `iicbus`
//! child, as FreeBSD's `cdnc_i2c_attach` does; the bus takes the devices
//! whose `I2cSerialBusV2` names the controller from `acpi0`, and they
//! reach the bus through `AttachCx::i2c` (DRIVER-MODEL.md §9.3).

#![no_std]
// The tests make a `Sleepable` for the host; the crate itself has no
// unsafe code.
#![cfg_attr(not(test), forbid(unsafe_code))]
#![cfg_attr(test, deny(unsafe_code))]

extern crate alloc;

use alloc::string::String;
use alloc::sync::Arc;

use bus::i2c::{I2c, Msg, Op};
use bus::provider::{ClockHandle, ProviderError, ResetHandle};
use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, IrqLine, MmioGrant, ResourceError,
    StartCx, StopCx, StopReason,
};
use core::sync::atomic::{AtomicU32, Ordering};
use devtree::ids::AcpiId;
use irq::Filter;
use regs::Bytes;
use sched::{IrqCx, Sleepable, WaitQueue};
use sync::SpinLock;

/// The register map (§2.2).
pub mod reg {
    /// Control.
    pub const CR: usize = 0x00;
    /// Status.
    pub const SR: usize = 0x04;
    /// The target address; writing it starts a message.
    pub const ADDR: usize = 0x08;
    /// The FIFOs: write to send, read to take a received byte.
    pub const DATA: usize = 0x0c;
    /// Interrupt status, write 1 to clear.
    pub const ISR: usize = 0x10;
    /// Bytes still to receive (read), or still queued (write).
    pub const XFER_SIZE: usize = 0x14;
    /// The bus-hold timeout, in SCL periods.
    pub const TIME_OUT: usize = 0x1c;
    /// Interrupt mask (1: masked).
    pub const IMR: usize = 0x20;
    /// Write 1: unmask.
    pub const IER: usize = 0x24;
    /// Write 1: mask.
    pub const IDR: usize = 0x28;

    /// `CR`: the next message receives.
    pub const CR_RW: u32 = 1 << 0;
    /// `CR`: host mode.
    pub const CR_MS: u32 = 1 << 1;
    /// `CR`: 7-bit addressing (clear: 10-bit).
    pub const CR_NEA: u32 = 1 << 2;
    /// `CR`: acknowledge received bytes.
    pub const CR_ACK_EN: u32 = 1 << 3;
    /// `CR`: keep the bus at the end of the data (no STOP).
    pub const CR_HOLD: u32 = 1 << 4;
    /// `CR`: flush both FIFOs (self-clearing).
    pub const CR_CLR_FIFO: u32 = 1 << 6;
    /// `CR`: the second divider's shift (6 bits).
    pub const CR_DIV_B_SHIFT: u32 = 8;
    /// `CR`: the first divider's shift (2 bits).
    pub const CR_DIV_A_SHIFT: u32 = 14;

    /// `SR`: the receive FIFO holds a byte.
    pub const SR_RXDV: u32 = 1 << 5;
    /// `SR`: the bus is active.
    pub const SR_BA: u32 = 1 << 8;

    /// Transfer complete.
    pub const ISR_COMP: u32 = 1 << 0;
    /// Receive FIFO at its threshold.
    pub const ISR_DATA: u32 = 1 << 1;
    /// Not acknowledged.
    pub const ISR_NACK: u32 = 1 << 2;
    /// Bus-hold timeout.
    pub const ISR_TO: u32 = 1 << 3;
    /// Receive overflow.
    pub const ISR_RX_OVF: u32 = 1 << 5;
    /// Transmit overflow.
    pub const ISR_TX_OVF: u32 = 1 << 6;
    /// Receive underflow.
    pub const ISR_RX_UNF: u32 = 1 << 7;
    /// Arbitration lost.
    pub const ISR_ARB_LOST: u32 = 1 << 9;
    /// Every interrupt.
    pub const ISR_ALL: u32 = 0x2ff;

    /// The FIFOs' depth on the Sky1 (§2.3).
    pub const FIFO: usize = 16;
}

/// The `_HID`.
pub const HID: &str = "CIXH200B";

/// The bus clock when firmware names none, and the fastest it may ask.
pub const DEFAULT_SPEED: u32 = 100_000;
/// The fastest bus clock the driver runs (fast mode).
pub const MAX_SPEED: u32 = 400_000;

/// The dividers for the fastest SCL not above `target` from `f_in`, and
/// that rate (§2.4); `None` if no pair reaches down to it or the target
/// is zero.
#[must_use]
pub fn dividers(f_in: u64, target: u32) -> Option<(u32, u32, u32)> {
    let target = u64::from(target);
    if target == 0 || f_in < 22 * target {
        return None;
    }
    let mut best: Option<(u32, u32, u32)> = None;
    for a in 0..4u32 {
        let a1 = u64::from(a) + 1;
        let b1 = f_in.div_ceil(22 * target * a1).max(1);
        // At most 64, so the divider fits.
        let Ok(b) = u32::try_from(b1 - 1) else {
            continue;
        };
        if b1 > 64 {
            continue;
        }
        let rate = u32::try_from(f_in / (22 * a1 * b1)).ok()?;
        if best.is_none_or(|(_, _, r)| rate > r) {
            best = Some((a, b, rate));
        }
    }
    best
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

/// How a transfer waits for its controller's interrupt.
pub trait Wait: Send + Sync {
    /// Sleeps until `cond` holds or `ns` nanoseconds pass; whether it
    /// held. Whatever makes it true calls [`Wait::wake`].
    fn wait(&self, s: &Sleepable, cond: &mut dyn FnMut() -> bool, ns: u64) -> bool;
    /// Wakes a waiter (from the interrupt filter).
    fn wake(&self);
    /// Sleeps `ms` milliseconds (the bus-free check, which has no
    /// interrupt).
    fn sleep_ms(&self, s: &Sleepable, ms: u64);
}

/// The kernel's waits: a wait queue the filter wakes, and the timed sleep
/// on it (FreeBSD's `mtx_sleep(sc, ..., hz)`).
#[derive(Debug)]
pub struct Kernel {
    q: WaitQueue,
}

impl Kernel {
    /// A queue nobody waits on yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            q: WaitQueue::new(),
        }
    }
}

impl Default for Kernel {
    fn default() -> Self {
        Self::new()
    }
}

impl Wait for Kernel {
    fn wait(&self, s: &Sleepable, cond: &mut dyn FnMut() -> bool, ns: u64) -> bool {
        bus::manager().wait_timeout(s, &self.q, cond, ns)
    }
    fn wake(&self) {
        self.q.wake_all();
    }
    fn sleep_ms(&self, s: &Sleepable, ms: u64) {
        bus::manager().sleep_ms(s, ms);
    }
}

/// The interrupts a transfer waits on besides what it wants: the errors.
/// The bus-hold timeout stays masked (§2.6).
const ERRORS: u32 =
    reg::ISR_NACK | reg::ISR_ARB_LOST | reg::ISR_RX_OVF | reg::ISR_TX_OVF | reg::ISR_RX_UNF;

/// A controller in host mode, set up for its bus clock.
pub struct Controller<W> {
    words: W,
    /// `CR` between transfers: host mode, acknowledging, the dividers.
    idle: u32,
    /// The longest read message (the counter's chunk).
    max_read: usize,
    /// The bus clock it runs, in Hz.
    speed: u32,
    /// Status the interrupt filter acknowledged and the transfer has not
    /// taken yet.
    pending: AtomicU32,
}

impl<W> core::fmt::Debug for Controller<W> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Controller")
            .field("speed", &self.speed)
            .field("max_read", &self.max_read)
            .finish_non_exhaustive()
    }
}

/// Why a controller could not be set up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupError {
    /// The input clock is unknown (SCMI reported 0).
    NoClock,
    /// No dividers reach the bus clock from the input clock.
    Unreachable,
}

impl<W: Words> Controller<W> {
    /// Sets the controller up (§2.7): probes the transfer-size counter,
    /// sets the dividers for the fastest bus clock not above `target`
    /// from `f_in`, maxes out the bus-hold timeout, and masks every
    /// interrupt.
    pub fn new(words: W, f_in: u64, target: u32) -> Result<Self, SetupError> {
        if f_in == 0 {
            return Err(SetupError::NoClock);
        }
        let (a, b, speed) = dividers(f_in, target).ok_or(SetupError::Unreachable)?;
        // The counter takes a write only in host receive mode (§2.3).
        words.write32(reg::CR, reg::CR_MS | reg::CR_RW);
        words.write32(reg::XFER_SIZE, 0xff);
        let width = words.read32(reg::XFER_SIZE) as usize;
        words.write32(reg::XFER_SIZE, 0);
        words.write32(reg::CR, 0);
        let idle = reg::CR_ACK_EN
            | reg::CR_NEA
            | reg::CR_MS
            | a << reg::CR_DIV_A_SHIFT
            | b << reg::CR_DIV_B_SHIFT;
        let c = Self {
            words,
            idle,
            max_read: width.saturating_sub(3).max(1),
            speed,
            pending: AtomicU32::new(0),
        };
        c.words.write32(reg::TIME_OUT, 0xff);
        c.reset();
        Ok(c)
    }

    /// The bus clock it runs.
    #[must_use]
    pub fn speed(&self) -> u32 {
        self.speed
    }

    /// The longest read message it takes.
    #[must_use]
    pub fn max_read(&self) -> usize {
        self.max_read
    }

    /// Back to idle after a message or an error (§2.5.4): every interrupt
    /// masked, no hold, the FIFOs and counter cleared, the status
    /// acknowledged.
    fn reset(&self) {
        let w = &self.words;
        w.write32(reg::IDR, reg::ISR_ALL);
        w.write32(reg::CR, self.idle | reg::CR_CLR_FIFO);
        w.write32(reg::XFER_SIZE, 0);
        let isr = w.read32(reg::ISR);
        w.write32(reg::ISR, isr);
        let sr = w.read32(reg::SR);
        w.write32(reg::SR, sr);
        self.pending.store(0, Ordering::Relaxed);
    }

    /// The interrupt's work, FreeBSD's `cdnc_i2c_intr`: acknowledges the
    /// unmasked status and records it for the waiting transfer. False if
    /// none was pending (not this controller's interrupt).
    pub fn intr(&self) -> bool {
        let st = self.words.read32(reg::ISR) & !self.words.read32(reg::IMR) & reg::ISR_ALL;
        if st == 0 {
            return false;
        }
        self.words.write32(reg::ISR, st);
        self.pending.fetch_or(st, Ordering::AcqRel);
        true
    }

    /// Unmasks `want` and the errors, sleeps until the interrupt reports
    /// one of them or `budget` runs out, and masks them again. Returns
    /// the status taken, errors mapped.
    fn wait_for(
        &self,
        w: &dyn Wait,
        s: &Sleepable,
        want: u32,
        budget: u64,
    ) -> Result<u32, ProviderError> {
        let mask = want | ERRORS;
        self.words.write32(reg::IER, mask);
        let pending = &self.pending;
        w.wait(
            s,
            &mut || pending.load(Ordering::Acquire) & mask != 0,
            budget,
        );
        self.words.write32(reg::IDR, mask);
        let isr = pending.swap(0, Ordering::AcqRel);
        if isr & reg::ISR_NACK != 0 {
            return Err(ProviderError::NoAck);
        }
        if isr & reg::ISR_ARB_LOST != 0 {
            return Err(ProviderError::ArbitrationLost);
        }
        if isr & (reg::ISR_RX_OVF | reg::ISR_TX_OVF | reg::ISR_RX_UNF) != 0 {
            return Err(ProviderError::BusError);
        }
        if isr & want == 0 {
            return Err(ProviderError::Timeout);
        }
        Ok(isr)
    }

    /// A message's time budget in nanoseconds: its bit time plus 500 ms,
    /// at least a second (§2.5.4).
    fn budget_ns(&self, len: usize) -> u64 {
        let bits = (len as u64 + 1) * 9;
        let us = (bits * 1_000_000 / u64::from(self.speed.max(1)) + 500_000).max(1_000_000);
        us * 1000
    }

    /// Queues as many of `data` as the FIFO has room for; returns how
    /// many.
    fn push(&self, data: &[u8]) -> usize {
        let queued = self.words.read32(reg::XFER_SIZE) as usize;
        let n = data.len().min(reg::FIFO.saturating_sub(queued));
        for &b in &data[..n] {
            self.words.write32(reg::DATA, u32::from(b));
        }
        n
    }

    /// Acknowledges what the status holds from before a message.
    fn clear_status(&self) {
        let isr = self.words.read32(reg::ISR);
        self.words.write32(reg::ISR, isr);
        self.pending.store(0, Ordering::Release);
    }

    /// The `CR` for a message: host mode, the dividers, the direction,
    /// the addressing, and a hold if the bus was held or must be.
    fn message_cr(cr: u32, read: bool, ten_bit: bool, hold: bool) -> u32 {
        let mut v = cr & !(reg::CR_RW | reg::CR_NEA);
        if read {
            v |= reg::CR_RW;
        }
        if !ten_bit {
            v |= reg::CR_NEA;
        }
        if hold {
            v |= reg::CR_HOLD;
        }
        v
    }

    #[allow(clippy::too_many_arguments)]
    fn write_msg(
        &self,
        wt: &dyn Wait,
        s: &Sleepable,
        cr: &mut u32,
        address: u16,
        ten_bit: bool,
        data: &[u8],
        last: bool,
    ) -> Result<(), ProviderError> {
        let w = &self.words;
        *cr = Self::message_cr(*cr, false, ten_bit, !last || data.len() > reg::FIFO);
        w.write32(reg::CR, *cr | reg::CR_CLR_FIFO);
        self.clear_status();
        let budget = self.budget_ns(data.len());
        let mut sent = self.push(data);
        if sent == data.len() && last {
            *cr &= !reg::CR_HOLD;
            w.write32(reg::CR, *cr);
        }
        w.write32(reg::ADDR, u32::from(address));
        loop {
            self.wait_for(wt, s, reg::ISR_COMP, budget)?;
            if sent == data.len() {
                return Ok(());
            }
            sent += self.push(&data[sent..]);
            if sent == data.len() && last {
                *cr &= !reg::CR_HOLD;
                w.write32(reg::CR, *cr);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn read_msg(
        &self,
        wt: &dyn Wait,
        s: &Sleepable,
        cr: &mut u32,
        address: u16,
        ten_bit: bool,
        buf: &mut [u8],
        last: bool,
    ) -> Result<(), ProviderError> {
        let w = &self.words;
        let len = buf.len();
        if len > self.max_read {
            return Err(ProviderError::Unsupported);
        }
        let held = *cr & reg::CR_HOLD != 0;
        *cr = Self::message_cr(*cr, true, ten_bit, held || !last || len > reg::FIFO);
        w.write32(reg::CR, *cr | reg::CR_CLR_FIFO);
        self.clear_status();
        let budget = self.budget_ns(len);
        // At most `max_read`, which came from the 8-bit counter.
        w.write32(reg::XFER_SIZE, u32::try_from(len).unwrap_or(0));
        w.write32(reg::ADDR, u32::from(address));
        // All of it fits: let go of the bus at once, before the counter
        // can run out (§2.5.3 step 4).
        if last && len <= reg::FIFO && *cr & reg::CR_HOLD != 0 {
            *cr &= !reg::CR_HOLD;
            w.write32(reg::CR, *cr);
        }
        let mut got = 0;
        let mut complete = false;
        loop {
            let isr = self.wait_for(wt, s, reg::ISR_COMP | reg::ISR_DATA, budget)?;
            complete |= isr & reg::ISR_COMP != 0;
            while w.read32(reg::SR) & reg::SR_RXDV != 0 {
                // More than asked for: the counter wrapped.
                let slot = buf.get_mut(got).ok_or(ProviderError::BusError)?;
                // The FIFO is a byte wide.
                *slot = w.read32(reg::DATA).to_le_bytes()[0];
                got += 1;
            }
            // What is left fits the FIFO: end with NACK and STOP.
            if last && *cr & reg::CR_HOLD != 0 && len - got <= reg::FIFO {
                *cr &= !reg::CR_HOLD;
                w.write32(reg::CR, *cr);
            }
            if complete {
                return if got == len {
                    Ok(())
                } else {
                    Err(ProviderError::BusError)
                };
            }
        }
    }

    /// Runs `msgs` as one transfer (§2.5), sleeping on `wt` for the
    /// controller's interrupts.
    pub fn transfer(
        &self,
        wt: &dyn Wait,
        s: &Sleepable,
        msgs: &mut [Msg<'_>],
    ) -> Result<(), ProviderError> {
        if msgs.iter().any(|m| m.op.is_empty()) {
            return Err(ProviderError::Unsupported);
        }
        // The bus free first. Nothing interrupts when it becomes free:
        // checked every 100 ms, for up to 500 ms (§2.5.1).
        let mut tries = 0;
        while self.words.read32(reg::SR) & reg::SR_BA != 0 {
            if tries == 5 {
                return Err(ProviderError::BusBusy);
            }
            wt.sleep_ms(s, 100);
            tries += 1;
        }
        let mut cr = self.idle;
        let n = msgs.len();
        let mut r = Ok(());
        for (k, m) in msgs.iter_mut().enumerate() {
            let last = k + 1 == n;
            r = match &mut m.op {
                Op::Write(d) => self.write_msg(wt, s, &mut cr, m.address, m.ten_bit, d, last),
                Op::Read(b) => self.read_msg(wt, s, &mut cr, m.address, m.ten_bit, b, last),
            };
            if r.is_err() {
                break;
            }
        }
        self.reset();
        r
    }
}

/// The controller as an I2C provider.
pub struct Bus<W> {
    ctl: Controller<W>,
    wait: Arc<dyn Wait>,
}

impl<W> core::fmt::Debug for Bus<W> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.ctl.fmt(f)
    }
}

impl<W: Words> Bus<W> {
    /// `ctl`, its transfers sleeping on `wait`.
    pub fn new(ctl: Controller<W>, wait: Arc<dyn Wait>) -> Self {
        Self { ctl, wait }
    }

    /// The interrupt filter's work: acknowledges and records the status,
    /// and wakes the transfer. False if it was not this controller's.
    pub fn intr(&self) -> bool {
        if self.ctl.intr() {
            self.wait.wake();
            true
        } else {
            false
        }
    }
}

impl<W: Words> I2c for Bus<W> {
    fn transfer(&self, s: &Sleepable, msgs: &mut [Msg<'_>]) -> Result<(), ProviderError> {
        self.ctl.transfer(&*self.wait, s, msgs)
    }

    fn speed(&self) -> u32 {
        self.ctl.speed()
    }
}

/// `cdnc_i2cN`.
pub struct CdncI2c {
    bus: Arc<Bus<MmioGrant<Bytes>>>,
    irq: SpinLock<Option<IrqLine>>,
    _clock: ClockHandle,
    _reset: Option<ResetHandle>,
}

impl core::fmt::Debug for CdncI2c {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.bus.fmt(f)
    }
}

fn intr(d: &CdncI2c, _: usize, _: &IrqCx) -> Filter {
    if d.bus.intr() {
        Filter::Handled
    } else {
        Filter::Stray
    }
}

fn provider(e: ProviderError) -> AttachError {
    AttachError::from(ResourceError::Provider(e))
}

impl Driver for CdncI2c {
    const NAME: &'static str = "cdnc_i2c";
    const IDS: Ids = Ids::Acpi(&[AcpiId::new(HID)]);

    fn describe(_: &Ident) -> String {
        "Cadence I2C Controller".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let s = cx.sleepable();
        // Clock, then reset, before any register (§2.7).
        let mut clock = cx.clock(None)?;
        clock.enable(s).map_err(provider)?;
        let mut reset = match cx.reset(None) {
            Ok(r) => Some(r),
            Err(ResourceError::NoSuchLine) => None,
            Err(e) => return Err(e.into()),
        };
        if let Some(r) = &mut reset {
            r.deassert(s).map_err(provider)?;
        }
        let f_in = clock.rate(s).map_err(provider)?.0;
        let target = acpi::properties::integer(cx, "clock-frequency")
            .and_then(|f| u32::try_from(f).ok())
            .filter(|&f| f > 0)
            .map_or(DEFAULT_SPEED, |f| f.min(MAX_SPEED));
        let regs = cx.mmio_bytes(0)?;
        if regs.len() < reg::IDR + 4 {
            return Err(AttachError::Device("register window too small"));
        }
        let ctl = Controller::new(regs, f_in, target).map_err(|e| match e {
            SetupError::NoClock => AttachError::Device("input clock rate unknown"),
            SetupError::Unreachable => AttachError::Device("bus clock out of range"),
        })?;
        let irq = cx.irq(0)?;
        Ok(Self {
            bus: Arc::new(Bus::new(ctl, Arc::new(Kernel::new()))),
            irq: SpinLock::new(Some(irq)),
            _clock: clock,
            _reset: reset,
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        // Taken in its own statement: installing may sleep. Every
        // interrupt is masked until a transfer waits for one.
        let line = self.irq.lock().take();
        if let Some(line) = line {
            cx.interrupt(line, Some(intr), None, 0)?;
        }
        cx.provide_i2c(Arc::clone(&self.bus) as Arc<dyn I2c>);
        cx.add_iicbus();
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<CdncI2c>();

#[cfg(test)]
#[allow(unsafe_code)]
mod tests;
