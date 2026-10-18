// SPDX-License-Identifier: BSD-2-Clause
//! The PC/AT real-time clock (`atrtc`): the MC146818-compatible CMOS
//! clock behind I/O ports 0x70 and 0x71, from its ACPI device (PNP0B00).
//!
//! A read waits for no update to be in progress, reads every field, and
//! reads again until two reads agree; fields are BCD or binary, and the
//! hour 12- or 24-hour, as register B says. Years below 70 are 20xx, as
//! FreeBSD takes them.
//!
//! Behavior reference: sys/x86/isa/atrtc.c.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::sync::Arc;

use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, IoGrant, StartCx, StopCx, StopReason,
};
use clock::{DateTime, RealTimeClock};
use devtree::ids::AcpiId;
use sync::SpinLock;

/// CMOS registers.
const SECONDS: u8 = 0x00;
const MINUTES: u8 = 0x02;
const HOURS: u8 = 0x04;
const DAY: u8 = 0x07;
const MONTH: u8 = 0x08;
const YEAR: u8 = 0x09;
const STATUS_A: u8 = 0x0a;
const STATUS_B: u8 = 0x0b;
/// Register A: an update is in progress.
const A_UIP: u8 = 0x80;
/// Register B: binary (else BCD), 24-hour (else 12-hour).
const B_BINARY: u8 = 0x04;
const B_24H: u8 = 0x02;
/// 12-hour mode: the hour's PM bit.
const PM: u8 = 0x80;

/// The clock's registers, serialized (index then data).
pub struct Cmos {
    io: SpinLock<IoGrant>,
    name: String,
}

impl core::fmt::Debug for Cmos {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Cmos")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

fn bcd(v: u8) -> u8 {
    (v >> 4) * 10 + (v & 0xf)
}

impl Cmos {
    fn reg(io: &IoGrant, r: u8) -> u8 {
        // Index port, then data port; bit 7 of the index keeps NMI as is.
        io.write8(0, r);
        io.read8(1)
    }

    /// One pass over the fields: (seconds, minutes, hours, day, month,
    /// year, register B), raw.
    fn raw(io: &IoGrant) -> Option<[u8; 7]> {
        for _ in 0..1000 {
            if Self::reg(io, STATUS_A) & A_UIP == 0 {
                return Some([
                    Self::reg(io, SECONDS),
                    Self::reg(io, MINUTES),
                    Self::reg(io, HOURS),
                    Self::reg(io, DAY),
                    Self::reg(io, MONTH),
                    Self::reg(io, YEAR),
                    Self::reg(io, STATUS_B),
                ]);
            }
        }
        None
    }
}

impl RealTimeClock for Cmos {
    fn name(&self) -> &str {
        &self.name
    }

    fn read(&self) -> Option<DateTime> {
        let io = self.io.lock();
        let mut prev = Self::raw(&io)?;
        let mut stable = None;
        for _ in 0..10 {
            let now = Self::raw(&io)?;
            if now == prev {
                stable = Some(now);
                break;
            }
            prev = now;
        }
        let [s, m, h, d, mo, y, b] = stable?;
        let conv = |v: u8| if b & B_BINARY != 0 { v } else { bcd(v) };
        let mut hour = conv(h & !PM);
        if b & B_24H == 0 {
            hour %= 12;
            if h & PM != 0 {
                hour += 12;
            }
        }
        let y = conv(y);
        let t = DateTime {
            year: if y < 70 { 2000 } else { 1900 } + u16::from(y),
            month: conv(mo),
            day: conv(d),
            hour,
            minute: conv(m),
            second: conv(s),
            nanosecond: 0,
        };
        t.valid().then_some(t)
    }
}

/// An `atrtcN` instance.
#[derive(Debug)]
pub struct AtRtc {
    clock: Arc<Cmos>,
}

impl Driver for AtRtc {
    const NAME: &'static str = "atrtc";
    const IDS: Ids = Ids::Acpi(&[
        AcpiId::new("PNP0B00"),
        AcpiId::new("PNP0B01"),
        AcpiId::new("PNP0B02"),
    ]);

    fn describe(_: &Ident) -> String {
        "AT realtime clock".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let io = cx.io_ports(0).map_err(|_| AttachError::Decline)?;
        if io.len() < 2 {
            return Err(AttachError::Device("RTC ports"));
        }
        Ok(Self {
            clock: Arc::new(Cmos {
                io: SpinLock::new(io),
                name: alloc::format!("atrtc{}", cx.unit()),
            }),
        })
    }

    fn start(&self, _: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        clock::register(Arc::clone(&self.clock) as _);
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {
        let c: Arc<dyn RealTimeClock> = Arc::clone(&self.clock) as _;
        clock::unregister(&c);
    }
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<AtRtc>();
