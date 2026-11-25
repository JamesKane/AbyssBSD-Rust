// SPDX-License-Identifier: BSD-2-Clause
//! `sky1_gptN`: the CIX Sky1's general purpose timer (ACPI `CIXH1007`).
//!
//! Two 32-bit timers chained into a 64-bit free-running counter with a
//! 64-bit compare, in the peripheral domain: it keeps counting where the
//! cores' own timers stop in deep idle states, which is what it is for as
//! a global event timer.
//!
//! This driver brings the timer up as the specification has it (the reset
//! released, the clocks enabled, both timers free-running and enabled),
//! parks its interrupt, checks that the counter runs at the rate its clock
//! reports, and publishes the counter and its frequency in
//! `dev.sky1_gpt.N`. It does not make it one of the kernel's event timers:
//! AbyssBSD has no idle state that stops the cores' timers (its idle is
//! WFI), so nothing needs a broadcast timer yet, and the kernel's event
//! timers are its own. [`hw::Gpt::arm`] and the compare registers are the
//! part that role would use, tested against the model.
//!
//! The specification says instance 0's window overlaps the PWMs' and to
//! prefer instances 1 to 3; this board's firmware describes only instance 3
//! (`0x04116000`).
//!
//! Behavior reference: the clean-room specification `sky1-soc/03-gpt-timer`
//! and the AbyssBSD Project's `sky1_gpt` for FreeBSD.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod hw;

#[cfg(test)]
mod firmware_tests;
#[cfg(test)]
mod tests;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;

use abi::sysctl::ctlflag;
use bus::provider::{ClockHandle, ProviderError, ResetHandle};
use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, MmioGrant, ResourceError, StartCx,
    StopCx, StopReason,
};
use devtree::ids::AcpiId;
use regs::Bytes;
use sync::SpinLock;
use sysctl::{Number, Value};

use crate::hw::{Gpt, Regs, WINDOW};

/// The firmware's name for the counter clock and the register clock, and
/// for the reset line they share (the DSDT's `CLKT` and `RSTL`).
const FUNC_CLOCK: &str = "fch_timer_func_clk";
const APB_CLOCK: &str = "fch_timer_apb_clk";
const RESET: &str = "func_reset";

/// How far the measured rate may be from the clock's: 10 percent.
const TOLERANCE_PERCENT: u64 = 10;

/// The registers.
#[derive(Clone, Debug)]
pub struct Bar(Arc<MmioGrant<Bytes>>);

impl Regs for Bar {
    fn read32(&self, off: usize) -> u32 {
        self.0.read32(off)
    }
    fn write32(&self, off: usize, v: u32) {
        self.0.write32(off, v);
    }
}

fn provider(e: ProviderError) -> AttachError {
    AttachError::from(ResourceError::Provider(e))
}

/// Whether `measured` is within the tolerance of `clock`.
#[must_use]
pub fn rate_agrees(measured: u64, clock: u64) -> bool {
    measured.abs_diff(clock) * 100 <= clock * TOLERANCE_PERCENT
}

/// A `sky1_gptN` instance.
pub struct Sky1Gpt {
    gpt: Arc<Gpt<Bar>>,
    hz: u64,
    unit: u32,
    _apb: Option<ClockHandle>,
    _func: ClockHandle,
    _reset: SpinLock<Option<ResetHandle>>,
}

impl core::fmt::Debug for Sky1Gpt {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Sky1Gpt")
            .field("hz", &self.hz)
            .finish_non_exhaustive()
    }
}

impl Driver for Sky1Gpt {
    const NAME: &'static str = "sky1_gpt";
    const IDS: Ids = Ids::Acpi(&[AcpiId::new("CIXH1007")]);

    fn describe(_: &Ident) -> String {
        "CIX Sky1 general purpose timer".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let s = cx.sleepable();
        // The register clock may already be on: it is optional. The
        // counter's is not, and must report a rate.
        let mut apb = cx.clock(Some(APB_CLOCK)).ok();
        if let Some(c) = &mut apb {
            c.enable(s).map_err(provider)?;
        }
        let mut func = cx.clock(Some(FUNC_CLOCK))?;
        func.enable(s).map_err(provider)?;
        let hz = func.rate(s).map_err(provider)?.0;
        if hz == 0 {
            return Err(AttachError::Device("counter clock rate unknown"));
        }
        let mut reset = match cx.reset(Some(RESET)) {
            Ok(r) => Some(r),
            Err(ResourceError::NoSuchLine) => None,
            Err(e) => return Err(e.into()),
        };
        if let Some(r) = &mut reset {
            r.deassert(s).map_err(provider)?;
        }
        let regs = cx.mmio_bytes(0)?;
        if regs.len() < WINDOW {
            return Err(AttachError::Device("register window too small"));
        }
        let gpt = Gpt::new(Bar(Arc::new(regs)));
        gpt.init();
        gpt.park();
        // Does it count, and at the clock's rate?
        let before = gpt.counter();
        bus::manager().delay_us(10_000);
        let after = gpt.counter();
        if after <= before {
            return Err(AttachError::Device("counter does not run"));
        }
        let measured = (after - before) * 100;
        let unit = cx.unit();
        if !rate_agrees(measured, hz) {
            bus::manager().log(format_args!(
                "sky1_gpt{unit}: the counter runs at about {measured} Hz, the clock says {hz} Hz"
            ));
        }
        Ok(Self {
            gpt: Arc::new(gpt),
            hz,
            unit,
            _apb: apb,
            _func: func,
            _reset: SpinLock::new(reset),
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let (gpt, hz, unit) = (Arc::clone(&self.gpt), self.hz, self.unit);
        bus::manager().sysctl(cx.sleepable(), &mut |t| {
            let Some(node) = bus::device_node(t, Self::NAME, unit) else {
                return;
            };
            let g = Arc::clone(&gpt);
            if let Ok(id) = t.add_value(
                node,
                Number::Auto,
                "counter",
                "The 64-bit free-running counter",
                Value::Str(Box::new(move |out| {
                    out.push_str(&format!("{}", g.counter()))
                })),
            ) {
                t.set_flags(id, ctlflag::MPSAFE);
            }
            if let Ok(id) = t.add_value(
                node,
                Number::Auto,
                "frequency",
                "The counter's rate (Hz), from its clock",
                Value::Str(Box::new(move |out| out.push_str(&format!("{hz}")))),
            ) {
                t.set_flags(id, ctlflag::MPSAFE);
            }
        });
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {
        // Its interrupt stays off.
        self.gpt.park();
    }
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<Sky1Gpt>();
