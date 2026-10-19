// SPDX-License-Identifier: BSD-2-Clause
//! The AMD FCH watchdog (`amdsbwd`), on FCH revision 41h and later (every
//! Zen platform), a child of `isa0` on AMD LPC bridges.
//!
//! Attach enables the watchdog's decode and a one-second tick through the
//! PM registers (index and data ports 0xcd6 and 0xcd7), and finds its
//! registers at the ACPI MMIO block's offset 0xb00 if that block is
//! enabled, else at the fixed 0xfeb00000. The timer counts down from
//! `COUNT` while `RUN` is set; `RELOAD` restarts it; expiry resets the
//! machine (`SHUTDOWN` clear).
//!
//! Behavior reference: sys/dev/amdsbwd/amdsbwd.c, amd_chipset.h.

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

/// PM registers (FCH 41h+).
const PM_DECODE_EN0: u8 = 0x00;
const WDT_EN: u8 = 0x80;
const PM_DECODE_EN3: u8 = 0x03;
const WDT_RES_MASK: u8 = 0x03;
const WDT_RES_1S: u8 = 0x03;
const WDT_EN_MASK: u8 = 0x0c;
const PM_ISA_CTRL: u8 = 0x04;
const MMIO_EN: u8 = 0x02;
/// The watchdog's registers in the two places they can be.
pub const MMIO_WDT: u64 = 0xfed8_0000 + 0xb00;
/// See [`MMIO_WDT`].
pub const FIXED_WDT: u64 = 0xfeb0_0000;

/// Watchdog registers.
const CTRL: usize = 0x00;
const COUNT: usize = 0x04;
const RUN: u32 = 0x01;
const FIRED: u32 = 0x02;
const SHUTDOWN: u32 = 0x04;
const DISABLE: u32 = 0x08;
const RELOAD: u32 = 0x80;

/// The PM register file, by index.
pub trait PmIo {
    /// Reads PM register `index`.
    fn read(&self, index: u8) -> u8;
    /// Writes PM register `index`.
    fn write(&self, index: u8, v: u8);
}

impl PmIo for IoGrant {
    fn read(&self, index: u8) -> u8 {
        self.write8(0, index);
        self.read8(1)
    }
    fn write(&self, index: u8, v: u8) {
        self.write8(0, index);
        self.write8(1, v);
    }
}

/// Enables the watchdog's decode and a 1 s tick; returns whether its
/// registers are in the ACPI MMIO block (else at the fixed address).
pub fn setup(pm: &impl PmIo) -> bool {
    pm.write(PM_DECODE_EN0, pm.read(PM_DECODE_EN0) | WDT_EN);
    let in_mmio = pm.read(PM_ISA_CTRL) & MMIO_EN != 0;
    let v = pm.read(PM_DECODE_EN3) & !WDT_RES_MASK & !WDT_EN_MASK;
    pm.write(PM_DECODE_EN3, v | WDT_RES_1S);
    in_mmio
}

/// The watchdog's two 32-bit registers.
pub trait WdRegs: Send + Sync {
    /// Reads the register at `off`.
    fn read32(&self, off: usize) -> u32;
    /// Writes the register at `off`.
    fn write32(&self, off: usize, v: u32);
}

impl WdRegs for MmioGrant<Bytes> {
    fn read32(&self, off: usize) -> u32 {
        regs::Mmio::read32(self, off)
    }
    fn write32(&self, off: usize, v: u32) {
        regs::Mmio::write32(self, off, v);
    }
}

/// The timer's logic, over its registers.
pub struct Fch<R: WdRegs> {
    regs: R,
}

impl<R: WdRegs> core::fmt::Debug for Fch<R> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Fch").finish_non_exhaustive()
    }
}

impl<R: WdRegs> Fch<R> {
    /// The timer over `regs`.
    pub const fn new(regs: R) -> Self {
        Self { regs }
    }

    /// Counts `timeout_ms` (whole seconds, at most 65535) and starts; an
    /// expiry resets the machine.
    pub fn arm(&self, timeout_ms: u64) -> Result<u64, WatchdogError> {
        let secs = u16::try_from(timeout_ms.div_ceil(1000).max(1))
            .map_err(|_| WatchdogError::OutOfRange)?;
        self.regs.write32(COUNT, u32::from(secs));
        let ctrl = self.regs.read32(CTRL) & !(DISABLE | SHUTDOWN);
        // FIRED is write-one-to-clear.
        self.regs.write32(CTRL, ctrl | RUN | FIRED);
        self.pat();
        Ok(u64::from(secs) * 1000)
    }

    /// Restarts the countdown.
    pub fn pat(&self) {
        let ctrl = self.regs.read32(CTRL) & !FIRED;
        self.regs.write32(CTRL, ctrl | RELOAD);
    }

    /// Stops it.
    pub fn halt(&self) {
        let ctrl = self.regs.read32(CTRL) & !(RUN | FIRED);
        self.regs.write32(CTRL, ctrl);
    }

    /// Whether it is counting, and the seconds left.
    pub fn remaining_ms(&self) -> Option<u64> {
        (self.regs.read32(CTRL) & RUN != 0)
            .then(|| u64::from(self.regs.read32(COUNT) & 0xffff) * 1000)
    }

    /// Whether the last expiry fired (it reset the machine).
    pub fn fired(&self) -> bool {
        self.regs.read32(CTRL) & FIRED != 0
    }
}

/// The device.
pub struct FchWatchdog {
    fch: SpinLock<Fch<MmioGrant<Bytes>>>,
    name: String,
}

impl core::fmt::Debug for FchWatchdog {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FchWatchdog")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl Watchdog for FchWatchdog {
    fn name(&self) -> &str {
        &self.name
    }
    fn arm(&self, timeout_ms: u64) -> Result<u64, WatchdogError> {
        self.fch.lock().arm(timeout_ms)
    }
    fn pat(&self) {
        self.fch.lock().pat();
    }
    fn disarm(&self) {
        self.fch.lock().halt();
    }
    fn remaining_ms(&self) -> Option<u64> {
        self.fch.lock().remaining_ms()
    }
}

/// An `amdsbwdN` instance.
#[derive(Debug)]
pub struct AmdSbWd {
    dog: Arc<FchWatchdog>,
}

impl Driver for AmdSbWd {
    const NAME: &'static str = "amdsbwd";
    const IDS: Ids = Ids::Nexus(&["amdsbwd"]);

    fn describe(ident: &Ident) -> String {
        match ident {
            Ident::Child { desc, .. } => (*desc).into(),
            _ => "AMD FCH Rev 41h+ Watchdog Timer".into(),
        }
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        // Resources: the PM ports, then the MMIO block's and the fixed
        // registers.
        let pm = cx.io_ports(0)?;
        let index = if setup(&pm) { 0 } else { 1 };
        drop(pm);
        let regs = cx.mmio_bytes(index)?;
        let fch = Fch::new(regs);
        if fch.fired() {
            bus::manager().log(format_args!(
                "amdsbwd{}: the last reset was a watchdog expiry",
                cx.unit()
            ));
        }
        fch.halt();
        Ok(Self {
            dog: Arc::new(FchWatchdog {
                fch: SpinLock::new(fch),
                name: alloc::format!("amdsbwd{}", cx.unit()),
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
pub const DRIVER: DriverDecl = DriverDecl::of::<AmdSbWd>();

#[cfg(test)]
mod tests {
    extern crate std;
    use std::sync::Mutex;

    use super::*;

    #[derive(Default)]
    struct FakePm(Mutex<[u8; 8]>);

    impl PmIo for FakePm {
        fn read(&self, i: u8) -> u8 {
            self.0.lock().unwrap()[usize::from(i)]
        }
        fn write(&self, i: u8, v: u8) {
            self.0.lock().unwrap()[usize::from(i)] = v;
        }
    }

    /// CTRL and COUNT, with FIRED write-one-to-clear and RELOAD a strobe
    /// that copies the initial count back.
    #[derive(Default)]
    struct FakeWd {
        ctrl: Mutex<u32>,
        count: Mutex<u32>,
        initial: Mutex<u32>,
        reloads: Mutex<u32>,
    }

    impl WdRegs for FakeWd {
        fn read32(&self, off: usize) -> u32 {
            match off {
                CTRL => *self.ctrl.lock().unwrap(),
                _ => *self.count.lock().unwrap(),
            }
        }
        fn write32(&self, off: usize, v: u32) {
            match off {
                CTRL => {
                    let mut c = self.ctrl.lock().unwrap();
                    let fired = *c & FIRED & !(v & FIRED);
                    *c = (v & !(FIRED | RELOAD)) | fired;
                    if v & RELOAD != 0 {
                        *self.reloads.lock().unwrap() += 1;
                        *self.count.lock().unwrap() = *self.initial.lock().unwrap();
                    }
                }
                _ => {
                    *self.initial.lock().unwrap() = v;
                    *self.count.lock().unwrap() = v;
                }
            }
        }
    }

    #[test]
    fn pm_setup_finds_the_registers() {
        let pm = FakePm::default();
        pm.write(PM_DECODE_EN3, 0x0f);
        assert!(!setup(&pm), "MMIO block off: the fixed address");
        assert_eq!(pm.read(PM_DECODE_EN0) & WDT_EN, WDT_EN, "decode enabled");
        assert_eq!(pm.read(PM_DECODE_EN3), WDT_RES_1S, "1 s ticks, enabled");
        pm.write(PM_ISA_CTRL, MMIO_EN);
        assert!(setup(&pm), "MMIO block on");
    }

    #[test]
    fn arms_pats_and_halts() {
        let f = Fch::new(FakeWd::default());
        *f.regs.ctrl.lock().unwrap() = DISABLE | SHUTDOWN | FIRED;
        assert!(f.fired());
        assert_eq!(f.arm(7_500), Ok(8_000), "whole seconds, rounded up");
        let c = *f.regs.ctrl.lock().unwrap();
        assert_eq!(
            c & (RUN | DISABLE | SHUTDOWN | FIRED),
            RUN,
            "running, resets on expiry, status cleared"
        );
        assert_eq!(f.remaining_ms(), Some(8_000));
        *f.regs.count.lock().unwrap() = 3;
        f.pat();
        assert_eq!(f.remaining_ms(), Some(8_000), "a pat reloads");
        assert_eq!(f.arm(70_000_000), Err(WatchdogError::OutOfRange));
        f.halt();
        assert_eq!(f.remaining_ms(), None);
    }
}
