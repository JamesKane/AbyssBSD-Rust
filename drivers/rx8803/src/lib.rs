// SPDX-License-Identifier: BSD-2-Clause
//! `rx8803`: the Epson RX8803 and RX8900 real-time clocks on an I2C bus,
//! as a time-of-day clock (`<Epson RX8900 Real Time Clock>` for the CIX
//! Sky1 boards' `RX008900`, at 0x32 on I2C3).
//!
//! - Read: the flag register first; with its voltage-low flag (`V2F`,
//!   the RX8900's `VLF`) set, the time was lost: "Low voltage flag set,
//!   date is incorrect", and no time (`ENXIO`). Otherwise the seven time
//!   registers from 0, BCD, the weekday one-hot.
//! - Write: the control register's `RESET` bit set (the clock stopped),
//!   the seven registers written, `RESET` cleared, then both voltage
//!   flags cleared, since the time is good again.
//! - Registered with a one-second resolution, written one nanosecond into
//!   a second (`clock_schedule(dev, 1)`).
//!
//! Two departures from FreeBSD's driver (docs/compat-deviations.md): it
//! rounds the time it writes up to the next second past half a second,
//! which with the framework's own half-second adjustment wrote the clock
//! a second fast, and did so by adding one to the BCD seconds byte, which
//! makes no BCD digit after a 9; and it subtracts `utc_offset` that the
//! framework already did. Here the framework's time is written as it is.
//!
//! Behavior reference: sys/dev/iicbus/rtc/rx8803.c (with the ACPI match
//! of the sibling port, ../AbyssBSD/src).

#![no_std]
// The tests make a `Sleepable` for the host; the driver has no unsafe
// code.
#![cfg_attr(not(test), forbid(unsafe_code))]
#![cfg_attr(test, deny(unsafe_code))]

extern crate alloc;

use alloc::string::String;
use alloc::sync::Arc;

use abi::errno::Errno;
use bus::i2c::I2cDevice;
use bus::{AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, StartCx, StopCx, StopReason};
use clock::{Bcd, DateTime, RealTimeClock, Timespec};
use devtree::ids::AcpiId;
use sched::Sleepable;

/// The time registers' first: seconds, minutes, hours, weekday, day,
/// month, year.
const TIME: u8 = 0x00;
/// The flag register, and its voltage flags.
const FLAGS: u8 = 0x0e;
const FLAGS_V1F: u8 = 1 << 0;
const FLAGS_V2F: u8 = 1 << 1;
/// The control register, and its bit that stops the clock.
const CTRL: u8 = 0x0f;
const CTRL_DISABLE: u8 = 1 << 0;

/// The clock's registers, through its bus.
pub struct Rx8803Clock {
    name: String,
    dev: I2cDevice,
}

impl core::fmt::Debug for Rx8803Clock {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Rx8803Clock")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl Rx8803Clock {
    /// The clock `name` behind `dev`.
    #[must_use]
    pub fn new(name: String, dev: I2cDevice) -> Self {
        Self { name, dev }
    }

    /// `iicdev_readfrom`: registers from `reg`, after a repeated START.
    fn read_from(&self, s: &Sleepable, reg: u8, buf: &mut [u8]) -> Result<(), Errno> {
        self.dev
            .write_read(s, &[reg], buf)
            .map_err(iicbus::iic2errno)
    }

    /// `iicdev_writeto`: registers from `reg`, in one write.
    fn write_to(&self, s: &Sleepable, reg: u8, data: &[u8]) -> Result<(), Errno> {
        let mut out = [0u8; 1 + 16];
        let out = out.get_mut(..=data.len()).ok_or(Errno::EOVERFLOW)?;
        out[0] = reg;
        out[1..].copy_from_slice(data);
        self.dev.write(s, out).map_err(iicbus::iic2errno)
    }

    /// `rx8803_check_status`.
    fn check_status(&self, s: &Sleepable) -> Result<(), Errno> {
        let mut flags = [0u8];
        self.read_from(s, FLAGS, &mut flags)?;
        if flags[0] & FLAGS_V2F != 0 {
            clock::rtc::log(format_args!(
                "{}: Low voltage flag set, date is incorrect",
                self.name
            ));
            return Err(Errno::ENXIO);
        }
        Ok(())
    }
}

/// The weekday register's one bit as a day number, `flsl(dow) - 1`.
fn weekday(v: u8) -> u8 {
    // Unused past the debugging line, as FreeBSD's.
    let v = v & 0x7f;
    if v == 0 {
        0
    } else {
        7 - u8::try_from(v.leading_zeros()).unwrap_or(7)
    }
}

impl RealTimeClock for Rx8803Clock {
    fn name(&self) -> &str {
        &self.name
    }

    fn gettime(&self, s: &Sleepable) -> Result<DateTime, Errno> {
        self.check_status(s)?;
        let mut d = [0u8; 7];
        self.read_from(s, TIME, &mut d)?;
        let b = Bcd {
            sec: d[0] & 0x7f,
            min: d[1] & 0x7f,
            hour: d[2] & 0x3f,
            dow: weekday(d[3]),
            day: d[4] & 0x3f,
            mon: d[5] & 0x1f,
            year: u16::from(d[6]),
            nsec: 0,
        };
        clock::dbgprint_bcd(&self.name, clock::rtc::DBG_READ, &b);
        b.to_datetime().ok_or(Errno::EINVAL)
    }

    fn settime(&self, s: &Sleepable, ts: Timespec) -> Result<(), Errno> {
        let t = ts.to_datetime().ok_or(Errno::EINVAL)?;
        let b = Bcd::from_datetime(&t);
        clock::dbgprint_bcd(&self.name, clock::rtc::DBG_WRITE, &b);
        let data = [
            b.sec,
            b.min,
            b.hour,
            1 << b.dow,
            b.day,
            b.mon,
            b.year.to_le_bytes()[0],
        ];
        // Stop the clock, set it, start it.
        let mut reg = [0u8];
        self.read_from(s, CTRL, &mut reg)?;
        let ctrl = reg[0] | CTRL_DISABLE;
        self.write_to(s, CTRL, &[ctrl])?;
        self.write_to(s, TIME, &data)?;
        self.write_to(s, CTRL, &[ctrl & !CTRL_DISABLE])?;
        // The time is good again: clear the voltage flags.
        self.read_from(s, FLAGS, &mut reg)?;
        self.write_to(s, FLAGS, &[reg[0] & !(FLAGS_V1F | FLAGS_V2F)])
    }
}

/// An `rx8803N` instance.
#[derive(Debug)]
pub struct Rx8803 {
    clock: Arc<Rx8803Clock>,
}

impl Driver for Rx8803 {
    const NAME: &'static str = "rx8803";
    /// The RX8900 keeps time in the same registers.
    const IDS: Ids = Ids::IicAcpi(&[AcpiId::new("RX008900")]);

    fn describe(_: &Ident) -> String {
        "Epson RX8900 Real Time Clock".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let dev = cx.i2c(0)?;
        Ok(Self {
            clock: Arc::new(Rx8803Clock::new(
                alloc::format!("{}{}", Self::NAME, cx.unit()),
                dev,
            )),
        })
    }

    fn start(&self, _: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let c: Arc<dyn RealTimeClock> = Arc::clone(&self.clock) as _;
        // One-second resolution, written just after the top of a second.
        clock::register(Arc::clone(&c), 1_000_000, 0);
        clock::schedule(&c, 1);
        Ok(())
    }

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        let c: Arc<dyn RealTimeClock> = Arc::clone(&self.clock) as _;
        clock::unregister(cx.sleepable(), &c);
    }
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<Rx8803>();

#[cfg(test)]
mod tests;
