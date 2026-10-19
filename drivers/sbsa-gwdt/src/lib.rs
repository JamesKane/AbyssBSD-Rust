// SPDX-License-Identifier: BSD-2-Clause
//! The SBSA generic watchdog (`sbsa_gwdt`), from the GTDT: a refresh
//! frame and a control frame (Arm BSA, "Generic Watchdog").
//!
//! The watchdog compares the system counter with `WCV`, which a refresh
//! sets to now + `WOR`: the first expiry raises WS0 (an interrupt), the
//! second WS1 (a reset). Arming for a timeout sets `WOR` to half of it, so
//! the reset comes at the timeout, as FreeBSD's out-of-tree and Linux's
//! drivers do without a WS0 handler.
//!
//! Refreshing writes the refresh frame's `WRR`, except on CIX Sky1, whose
//! refresh frame ignores writes: attach tests whether a refresh moves
//! `WCV` and, if not, refreshes by rewriting `WOR` (an explicit refresh by
//! the architecture's rules).
//!
//! FreeBSD has no driver for it; the name is recorded in
//! docs/compat-deviations.md.

#![no_std]
#![forbid(unsafe_code)]
#![allow(
    clippy::cast_possible_truncation,
    reason = "64-bit registers are written and read as 32-bit halves"
)]

extern crate alloc;

use alloc::string::String;
use alloc::sync::Arc;

use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, MmioGrant, StartCx, StopCx, StopReason,
};
use regs::Bytes;
use sync::SpinLock;
use watchdog::{Watchdog, WatchdogError};

/// Control frame registers.
const WCS: usize = 0x000;
const WOR: usize = 0x008;
const WORH: usize = 0x00c;
const WCV: usize = 0x010;
const W_IIDR: usize = 0xfcc;
const WCS_EN: u32 = 1;
/// Refresh frame: any write refreshes.
const WRR: usize = 0x000;

/// A frame's 32-bit registers.
pub trait Frame: Send + Sync {
    /// Reads the register at `off`.
    fn read32(&self, off: usize) -> u32;
    /// Writes the register at `off`.
    fn write32(&self, off: usize, v: u32);
}

impl Frame for MmioGrant<Bytes> {
    fn read32(&self, off: usize) -> u32 {
        regs::Mmio::read32(self, off)
    }
    fn write32(&self, off: usize, v: u32) {
        regs::Mmio::write32(self, off, v);
    }
}

/// The watchdog's logic, over its two frames.
pub struct Gwdt<F: Frame> {
    refresh: F,
    control: F,
    /// System counter ticks per second.
    frequency: u64,
    /// Architecture version 1: `WOR` is 48 bits.
    wide: bool,
    /// Refresh by rewriting `WOR` (the refresh frame does nothing).
    refresh_by_wor: bool,
    /// The offset last set.
    offset: u64,
}

impl<F: Frame> core::fmt::Debug for Gwdt<F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Gwdt")
            .field("wide", &self.wide)
            .field("refresh_by_wor", &self.refresh_by_wor)
            .finish_non_exhaustive()
    }
}

impl<F: Frame> Gwdt<F> {
    fn wcv(&self) -> u64 {
        u64::from(self.control.read32(WCV)) | u64::from(self.control.read32(WCV + 4)) << 32
    }

    fn set_offset(&self, ticks: u64) {
        self.control.write32(WOR, ticks as u32);
        if self.wide {
            self.control.write32(WORH, (ticks >> 32) as u32 & 0xffff);
        }
    }

    /// Takes over the watchdog: disabled, and with the refresh path
    /// tested. `delay` lets the counter advance between two samples.
    pub fn new(refresh: F, control: F, frequency: u64, delay: impl Fn()) -> Self {
        let wide = (control.read32(W_IIDR) >> 16) & 0xf >= 1;
        let mut g = Self {
            refresh,
            control,
            frequency: frequency.max(1),
            wide,
            refresh_by_wor: false,
            offset: 0,
        };
        // Enabled with the longest offset: does a refresh-frame write move
        // the compare value?
        g.control.write32(WCS, 0);
        g.set_offset(u64::from(u32::MAX));
        g.control.write32(WCS, WCS_EN);
        let before = g.wcv();
        delay();
        g.refresh.write32(WRR, 0);
        g.refresh_by_wor = g.wcv() == before;
        g.control.write32(WCS, 0);
        g
    }

    /// Whether refreshes go through `WOR` (Sky1).
    pub fn refreshes_by_wor(&self) -> bool {
        self.refresh_by_wor
    }

    /// Arms it to reset the machine `timeout_ms` from now.
    pub fn arm(&mut self, timeout_ms: u64) -> Result<u64, WatchdogError> {
        let ticks = (timeout_ms / 2)
            .checked_mul(self.frequency)
            .map(|t| t / 1000)
            .filter(|&t| t > 0)
            .ok_or(WatchdogError::OutOfRange)?;
        let max = if self.wide {
            (1 << 48) - 1
        } else {
            u64::from(u32::MAX)
        };
        if ticks > max {
            return Err(WatchdogError::OutOfRange);
        }
        self.offset = ticks;
        self.set_offset(ticks);
        self.control.write32(WCS, WCS_EN);
        self.pat();
        Ok(ticks * 2 * 1000 / self.frequency)
    }

    /// Restarts the countdown.
    pub fn pat(&self) {
        if self.refresh_by_wor {
            self.set_offset(self.offset);
        } else {
            self.refresh.write32(WRR, 0);
        }
    }

    /// Stops it.
    pub fn halt(&self) {
        self.control.write32(WCS, 0);
    }

    /// Whether it is counting.
    pub fn running(&self) -> bool {
        self.control.read32(WCS) & WCS_EN != 0
    }
}

/// What the kernel hands an `sbsa_gwdt` child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timebase {
    /// System counter ticks per second.
    pub frequency: u64,
}

/// The device.
pub struct SbsaWatchdog {
    gwdt: SpinLock<Gwdt<MmioGrant<Bytes>>>,
    name: String,
}

impl core::fmt::Debug for SbsaWatchdog {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SbsaWatchdog")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl Watchdog for SbsaWatchdog {
    fn name(&self) -> &str {
        &self.name
    }
    fn arm(&self, timeout_ms: u64) -> Result<u64, WatchdogError> {
        self.gwdt.lock().arm(timeout_ms)
    }
    fn pat(&self) {
        self.gwdt.lock().pat();
    }
    fn disarm(&self) {
        self.gwdt.lock().halt();
    }
}

/// An `sbsa_gwdtN` instance.
#[derive(Debug)]
pub struct SbsaGwdt {
    dog: Arc<SbsaWatchdog>,
}

impl Driver for SbsaGwdt {
    const NAME: &'static str = "sbsa_gwdt";
    const IDS: Ids = Ids::Nexus(&["sbsa_gwdt"]);

    fn describe(_: &Ident) -> String {
        "SBSA Generic Watchdog".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let frequency = cx
            .bus_data::<Timebase>()
            .ok_or(AttachError::Decline)?
            .frequency;
        let refresh = cx.mmio_bytes(0)?;
        let control = cx.mmio_bytes(1)?;
        let s = cx.sleepable();
        let gwdt = Gwdt::new(refresh, control, frequency, || {
            bus::manager().sleep_ms(s, 2)
        });
        if gwdt.refreshes_by_wor() {
            bus::manager().log(format_args!(
                "sbsa_gwdt{}: refresh frame writes do not refresh; refreshing through WOR",
                cx.unit()
            ));
        }
        Ok(Self {
            dog: Arc::new(SbsaWatchdog {
                gwdt: SpinLock::new(gwdt),
                name: alloc::format!("sbsa_gwdt{}", cx.unit()),
            }),
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
pub const DRIVER: DriverDecl = DriverDecl::of::<SbsaGwdt>();

#[cfg(test)]
mod tests {
    extern crate std;
    use std::sync::{Arc, Mutex};

    use super::*;

    /// Both frames over one model: a counter that advances on `tick`, and
    /// a refresh frame that works or (Sky1) does not.
    #[derive(Default)]
    struct Model {
        now: u64,
        wcs: u32,
        wor: u64,
        wcv: u64,
        iidr: u32,
        refresh_works: bool,
        refreshes: u32,
    }

    #[derive(Clone)]
    struct Side(Arc<Mutex<Model>>, bool);

    impl Frame for Side {
        fn read32(&self, off: usize) -> u32 {
            let m = self.0.lock().unwrap();
            match (self.1, off) {
                (false, WCS) => m.wcs,
                (false, WOR) => m.wor as u32,
                (false, WORH) => (m.wor >> 32) as u32,
                (false, WCV) => m.wcv as u32,
                (false, o) if o == WCV + 4 => (m.wcv >> 32) as u32,
                (false, W_IIDR) => m.iidr,
                _ => 0,
            }
        }
        fn write32(&self, off: usize, v: u32) {
            let mut m = self.0.lock().unwrap();
            let refresh = |m: &mut Model| {
                m.wcv = m.now + m.wor;
                m.refreshes += 1;
            };
            match (self.1, off) {
                (true, WRR) => {
                    if m.refresh_works {
                        refresh(&mut m);
                    }
                }
                (false, WCS) => {
                    m.wcs = v;
                    // Enabling refreshes.
                    if v & WCS_EN != 0 {
                        refresh(&mut m);
                    }
                }
                (false, WOR) => {
                    m.wor = (m.wor & !0xffff_ffff) | u64::from(v);
                    refresh(&mut m);
                }
                (false, WORH) => {
                    m.wor = (m.wor & 0xffff_ffff) | u64::from(v & 0xffff) << 32;
                    refresh(&mut m);
                }
                _ => {}
            }
        }
    }

    fn make(refresh_works: bool, version: u32) -> (Gwdt<Side>, Arc<Mutex<Model>>) {
        let m = Arc::new(Mutex::new(Model {
            refresh_works,
            iidr: version << 16,
            ..Model::default()
        }));
        let tick = Arc::clone(&m);
        let g = Gwdt::new(
            Side(Arc::clone(&m), true),
            Side(Arc::clone(&m), false),
            1_000_000,
            move || {
                tick.lock().unwrap().now += 100;
            },
        );
        (g, m)
    }

    #[test]
    fn refresh_frame_that_works() {
        let (mut g, m) = make(true, 0);
        assert!(!g.refreshes_by_wor());
        assert!(!g.running(), "left disabled");
        // 1 MHz: a 10 s timeout is two 5 s offsets.
        assert_eq!(g.arm(10_000), Ok(10_000));
        assert!(g.running());
        assert_eq!(m.lock().unwrap().wor, 5_000_000);
        m.lock().unwrap().now += 1_000_000;
        let before = m.lock().unwrap().refreshes;
        g.pat();
        let mm = m.lock().unwrap();
        assert_eq!(mm.refreshes, before + 1);
        assert_eq!(mm.wcv, mm.now + 5_000_000);
        drop(mm);
        g.halt();
        assert!(!g.running());
    }

    #[test]
    fn sky1_refreshes_through_wor() {
        let (mut g, m) = make(false, 1);
        assert!(g.refreshes_by_wor(), "the refresh frame did not move WCV");
        g.arm(4_000).unwrap();
        m.lock().unwrap().now += 500_000;
        g.pat();
        let mm = m.lock().unwrap();
        assert_eq!(mm.wcv, mm.now + 2_000_000, "the WOR rewrite refreshed");
    }

    #[test]
    fn offsets_are_bounded() {
        let (mut g, _) = make(true, 0);
        // 32-bit WOR at 1 MHz: at most ~4295 s per half.
        assert!(g.arm(8_000_000).is_ok());
        assert_eq!(g.arm(9_000_000), Err(WatchdogError::OutOfRange));
        assert_eq!(g.arm(1), Err(WatchdogError::OutOfRange), "zero ticks");
        let (mut wide, _) = make(true, 1);
        assert!(wide.arm(9_000_000).is_ok(), "48-bit WOR in version 1");
    }
}
