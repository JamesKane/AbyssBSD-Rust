// SPDX-License-Identifier: BSD-2-Clause
//! The Intel ICH TCO watchdog (`ichwd`), a child of `isa0` on Intel LPC
//! bridges, as FreeBSD's identify routine adds it.
//!
//! TCO version 2 (ICH6 on, including QEMU's ICH9): a countdown in 0.6 s
//! ticks from `TCO_TMR`; writing `TCO_RLD` reloads it; `TCO_TMR_HLT`
//! stops it. The first expiry sets a status bit, the second resets the
//! machine, unless the chipset's `NO_REBOOT` bit (in GCS) is set, which
//! attach clears as FreeBSD does, refusing the device if it sticks.
//!
//! Behavior reference: sys/dev/ichwd/ichwd.c.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::sync::Arc;

use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, IoGrant, MmioGrant, StartCx, StopCx,
    StopReason,
};
use regs::Bytes;
use sync::SpinLock;
use watchdog::{Watchdog, WatchdogError};

/// TCO registers, from the TCO base.
const TCO_RLD: u16 = 0x00;
const TCO1_STS: u16 = 0x04;
const TCO2_STS: u16 = 0x06;
const TCO1_CNT: u16 = 0x08;
const TCO_TMR: u16 = 0x12;
const TCO_TIMEOUT: u16 = 1 << 3;
const TCO_SECOND_TO: u16 = 1 << 1;
const TCO_TMR_HLT: u16 = 1 << 11;
/// Version 2's timer field.
const TMR_MASK: u16 = 0x3ff;
/// The shortest and longest counts the hardware honors.
const TMR_MIN: u16 = 4;
/// Milliseconds per tick.
const TICK_MS: u64 = 600;
/// GCS: no reboot on the second expiry.
const GCS_NO_REBOOT: u32 = 1 << 5;

/// The 16-bit TCO registers (I/O ports on real hardware).
pub trait TcoRegs: Send + Sync {
    /// Reads the register at `off`.
    fn read16(&self, off: u16) -> u16;
    /// Writes the register at `off`.
    fn write16(&self, off: u16, v: u16);
}

impl TcoRegs for IoGrant {
    fn read16(&self, off: u16) -> u16 {
        IoGrant::read16(self, off)
    }
    fn write16(&self, off: u16, v: u16) {
        IoGrant::write16(self, off, v);
    }
}

/// The TCO timer's logic, over its registers.
pub struct Tco<R: TcoRegs> {
    regs: R,
}

impl<R: TcoRegs> core::fmt::Debug for Tco<R> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Tco").finish_non_exhaustive()
    }
}

impl<R: TcoRegs> Tco<R> {
    /// The timer over `regs`.
    pub const fn new(regs: R) -> Self {
        Self { regs }
    }

    fn clear_status(&self) {
        self.regs.write16(TCO1_STS, TCO_TIMEOUT);
        self.regs.write16(TCO2_STS, TCO_SECOND_TO);
    }

    /// Stops the countdown.
    pub fn halt(&self) {
        let cnt = self.regs.read16(TCO1_CNT);
        self.regs.write16(TCO1_CNT, cnt | TCO_TMR_HLT);
    }

    /// Restarts the countdown from its initial value.
    pub fn reload(&self) {
        self.regs.write16(TCO_RLD, 1);
    }

    /// Sets the timeout, reloads, clears the expiry status, and starts it;
    /// returns the timeout in milliseconds as counted.
    pub fn arm(&self, timeout_ms: u64) -> Result<u64, WatchdogError> {
        let ticks = timeout_ms.div_ceil(TICK_MS);
        let ticks = u16::try_from(ticks)
            .ok()
            .filter(|&t| t <= TMR_MASK)
            .ok_or(WatchdogError::OutOfRange)?
            .max(TMR_MIN);
        let tmr = self.regs.read16(TCO_TMR);
        self.regs.write16(TCO_TMR, (tmr & !TMR_MASK) | ticks);
        self.reload();
        self.clear_status();
        let cnt = self.regs.read16(TCO1_CNT);
        self.regs.write16(TCO1_CNT, cnt & !TCO_TMR_HLT);
        Ok(u64::from(ticks) * TICK_MS)
    }

    /// Milliseconds left on the countdown.
    pub fn remaining_ms(&self) -> u64 {
        u64::from(self.regs.read16(TCO_RLD) & TMR_MASK) * TICK_MS
    }

    /// Whether it is counting.
    pub fn running(&self) -> bool {
        self.regs.read16(TCO1_CNT) & TCO_TMR_HLT == 0
    }
}

/// Intel LPC bridges with a TCO version 2 watchdog, as FreeBSD's table
/// describes them (device ID, description). `isa0` adds an `ichwd` child
/// on these.
pub const CHIPSETS: &[(u16, &str)] = &[
    (0x2640, "Intel 82801FB/FR watchdog timer"),
    (0x2810, "Intel ICH8 watchdog timer"),
    (0x2812, "Intel ICH8DH watchdog timer"),
    (0x2814, "Intel ICH8DO watchdog timer"),
    (0x2912, "Intel ICH9DH watchdog timer"),
    (0x2914, "Intel ICH9DO watchdog timer"),
    (0x2916, "Intel ICH9R watchdog timer"),
    (0x2918, "Intel ICH9 watchdog timer"),
    (0x3a16, "Intel ICH10R watchdog timer"),
    (0x3a18, "Intel ICH10 watchdog timer"),
];

/// The device: its TCO timer and name.
pub struct IchWatchdog {
    tco: SpinLock<Tco<IoGrant>>,
    name: String,
}

impl core::fmt::Debug for IchWatchdog {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IchWatchdog")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl Watchdog for IchWatchdog {
    fn name(&self) -> &str {
        &self.name
    }
    fn arm(&self, timeout_ms: u64) -> Result<u64, WatchdogError> {
        self.tco.lock().arm(timeout_ms)
    }
    fn pat(&self) {
        self.tco.lock().reload();
    }
    fn disarm(&self) {
        self.tco.lock().halt();
    }
    fn remaining_ms(&self) -> Option<u64> {
        let t = self.tco.lock();
        t.running().then(|| t.remaining_ms())
    }
}

/// An `ichwdN` instance.
#[derive(Debug)]
pub struct IchWd {
    dog: Arc<IchWatchdog>,
    _gcs: Option<MmioGrant<Bytes>>,
}

impl Driver for IchWd {
    const NAME: &'static str = "ichwd";
    const IDS: Ids = Ids::Nexus(&["ichwd"]);

    fn describe(ident: &Ident) -> String {
        match ident {
            Ident::Child { desc, .. } => (*desc).into(),
            _ => "Intel ICH watchdog timer".into(),
        }
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let io = cx.io_ports(0)?;
        // No reboot on expiry unless GCS lets it: clear NO_REBOOT, and
        // refuse the device if it will not clear.
        let gcs = cx.mmio_bytes(0).ok();
        if let Some(g) = &gcs {
            let v = g.read32(0);
            g.write32(0, v & !GCS_NO_REBOOT);
            if g.read32(0) & GCS_NO_REBOOT != 0 {
                return Err(AttachError::Device(
                    "ICH WDT present but disabled in BIOS or hardware",
                ));
            }
        }
        let tco = Tco::new(io);
        tco.halt();
        Ok(Self {
            dog: Arc::new(IchWatchdog {
                tco: SpinLock::new(tco),
                name: alloc::format!("ichwd{}", cx.unit()),
            }),
            _gcs: gcs,
        })
    }

    fn start(&self, _: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        watchdog::register(Arc::clone(&self.dog) as _);
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {
        self.dog.disarm();
        let w: Arc<dyn Watchdog> = Arc::clone(&self.dog) as _;
        watchdog::unregister(&w);
    }
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<IchWd>();

#[cfg(test)]
mod tests {
    extern crate std;
    use std::sync::Mutex;

    use super::*;

    /// TCO registers that behave as the timer does: a reload copies the
    /// initial value into the counter.
    #[derive(Default)]
    struct Fake {
        regs: Mutex<[u16; 0x10]>,
    }

    impl TcoRegs for Fake {
        fn read16(&self, off: u16) -> u16 {
            self.regs.lock().unwrap()[usize::from(off / 2)]
        }
        fn write16(&self, off: u16, v: u16) {
            let mut r = self.regs.lock().unwrap();
            match off {
                TCO_RLD => r[0] = r[usize::from(TCO_TMR / 2)] & TMR_MASK,
                TCO1_STS | TCO2_STS => r[usize::from(off / 2)] &= !v,
                _ => r[usize::from(off / 2)] = v,
            }
        }
    }

    #[test]
    fn arms_pats_and_halts() {
        let t = Tco::new(Fake::default());
        t.regs.regs.lock().unwrap()[usize::from(TCO1_STS / 2)] = TCO_TIMEOUT;
        assert_eq!(t.arm(30_000), Ok(30_000), "50 ticks of 0.6 s");
        assert!(t.running());
        assert_eq!(t.remaining_ms(), 30_000);
        assert_eq!(
            t.regs.read16(TCO1_STS) & TCO_TIMEOUT,
            0,
            "an old expiry cleared"
        );
        // Rounded up to whole ticks, and up to the hardware minimum.
        assert_eq!(t.arm(1000), Ok(2400));
        assert_eq!(t.arm(61_000), Ok(61_200));
        assert_eq!(t.arm(1_000_000), Err(WatchdogError::OutOfRange));
        t.halt();
        assert!(!t.running());
    }
}
