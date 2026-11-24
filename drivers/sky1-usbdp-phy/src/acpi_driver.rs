// SPDX-License-Identifier: BSD-2-Clause
//! The combo PHY as firmware describes it (`CIXH2033`): its registers,
//! register clock, resets and the system-controller field that holds the
//! plug orientation (clean-room specification `sky1-dp-tx` 03 §3.1,
//! §3.2.2).
//!
//! The PHY core is shared with USB 3 and is powered by whichever user
//! comes first, so attach only publishes it: it takes nothing out of
//! reset. The DisplayPort controller it serves finds it by its own unit
//! number ([`crate::registry`]) and adds its interface window.

use alloc::string::String;
use alloc::sync::Arc;

use bus::provider::{ClockHandle, ResetHandle};
use bus::region::RegionHandle;
use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, MmioGrant, StartCx, StopCx, StopReason,
};
use devtree::ids::AcpiId;
use regs::Bytes;
use sched::Sleepable;
use sync::SpinLock;

use crate::phy::{Core, Platform, Regs16};
use crate::registry::{self, Shared};
use crate::tables::Mode;

/// The firmware's hardware ID.
pub const HID: &str = "CIXH2033";

/// The system controller's orientation register, one bit per PHY
/// (section 3.5.3).
pub const ORIENTATION_OFFSET: usize = 0x420;

/// The DisplayPort controller each PHY serves (section 3.1): PHYs 0 to 3
/// are DP 0, 1, 3 and 4.
#[must_use]
pub fn dp_unit(phy: u32) -> Option<u32> {
    [0, 1, 3, 4].get(usize::try_from(phy).ok()?).copied()
}

/// The mode firmware starts the PHY in (`default_conf`, section 3.2.1).
#[must_use]
pub fn default_mode(conf: Option<u64>) -> Mode {
    match conf {
        Some(1) => Mode::UsbOnly,
        Some(3) => Mode::UsbDp,
        // 2, and a PHY with a DisplayPort port and no setting.
        _ => Mode::DpOnly,
    }
}

/// The PHY's registers: 16 bits in 32-bit slots, by register index.
struct Window(MmioGrant<Bytes>);

impl Regs16 for Window {
    fn read(&self, index: u32) -> u16 {
        let v = regs::Mmio::read32(&self.0, index as usize * 4);
        u16::try_from(v & 0xFFFF).unwrap_or(0)
    }

    fn write(&self, index: u32, v: u16) {
        regs::Mmio::write32(&self.0, index as usize * 4, u32::from(v));
    }
}

/// The clock, resets and orientation field.
struct Machine {
    index: u32,
    pclk: SpinLock<Option<ClockHandle>>,
    reset: SpinLock<Option<ResetHandle>>,
    preset: SpinLock<Option<ResetHandle>>,
    region: Option<RegionHandle>,
}

/// The thread a PHY operation runs on. The operations are the
/// DisplayPort driver's: its bring-up and training, and the
/// orientation update of a Type-C event, all on threads that may sleep.
#[allow(unsafe_code)]
fn thread() -> Sleepable {
    // SAFETY: see above; the PHY is never touched from an interrupt
    // filter.
    unsafe { Sleepable::assume() }
}

impl Machine {
    fn with<T, R>(
        cell: &SpinLock<Option<T>>,
        f: impl FnOnce(&mut T, &Sleepable) -> R,
    ) -> Option<R> {
        let mut taken = cell.lock().take()?;
        let r = f(&mut taken, &thread());
        *cell.lock() = Some(taken);
        Some(r)
    }
}

impl Platform for Machine {
    fn set_orientation(&self, flipped: bool) {
        let Some(r) = &self.region else { return };
        let bit = 1u32 << self.index;
        let lease = r.clone().field(ORIENTATION_OFFSET, bit);
        let _ = lease.update(ORIENTATION_OFFSET, bit, if flipped { bit } else { 0 });
    }

    fn assert_resets(&self) {
        Self::with(&self.reset, |r, s| {
            let _ = r.assert(s);
        });
        Self::with(&self.preset, |r, s| {
            let _ = r.assert(s);
        });
    }

    fn release_apb_reset(&self) {
        Self::with(&self.preset, |r, s| {
            let _ = r.deassert(s);
        });
    }

    fn release_core_reset(&self) {
        Self::with(&self.reset, |r, s| {
            let _ = r.deassert(s);
        });
    }

    fn pclk_enable(&self) -> bool {
        Self::with(&self.pclk, |c, s| c.enable(s).is_ok()).unwrap_or(false)
    }

    fn pclk_disable(&self) {
        Self::with(&self.pclk, |c, s| {
            let _ = c.disable(s);
        });
    }

    fn delay_us(&self, us: u64) {
        bus::manager().delay_us(us);
    }
}

/// `sky1_udphyN`.
pub struct UsbDpPhyDevice {
    unit: u32,
    shared: Shared,
}

impl core::fmt::Debug for UsbDpPhyDevice {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UsbDpPhyDevice")
            .field("unit", &self.unit)
            .finish()
    }
}

impl Driver for UsbDpPhyDevice {
    const NAME: &'static str = "sky1_udphy";
    const IDS: Ids = Ids::Acpi(&[AcpiId::new(HID)]);

    fn describe(_: &Ident) -> String {
        "USB-C/DisplayPort combo PHY (CIX Sky1)".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let id = acpi::properties::integer(cx, "id").ok_or(AttachError::Device("no PHY index"))?;
        let index = u32::try_from(id).map_err(|_| AttachError::Device("PHY index"))?;
        let unit = dp_unit(index).ok_or(AttachError::Device("PHY index"))?;
        let mode = default_mode(acpi::properties::integer(cx, "default_conf"));
        let pclk = cx.clock(Some("pclk"))?;
        let reset = cx.reset(Some("reset"))?;
        let preset = cx.reset(Some("preset"))?;
        // Without the system controller the orientation cannot be set and
        // the table's default stands.
        let region = cx.region("cix,usbphy_syscon").ok();
        let regs = cx.mmio_bytes(0)?;
        let regs: Arc<dyn Regs16> = Arc::new(Window(regs));
        let plat: Arc<dyn Platform> = Arc::new(Machine {
            index,
            pclk: SpinLock::new(Some(pclk)),
            reset: SpinLock::new(Some(reset)),
            preset: SpinLock::new(Some(preset)),
            region,
        });
        let core = Arc::new(Core::new(regs.clone(), plat.clone(), mode));
        Ok(Self {
            unit,
            shared: Shared { core, regs, plat },
        })
    }

    fn start(&self, _: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        registry::publish(self.unit, self.shared.clone());
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {
        registry::withdraw(self.unit);
        self.shared.core.power_down();
    }
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<UsbDpPhyDevice>();

#[cfg(test)]
mod tests {
    extern crate std;

    use std::collections::BTreeMap;

    use acpi_platform::PlatformSource;

    use super::*;

    #[test]
    fn units_and_modes() {
        assert_eq!(dp_unit(0), Some(0));
        assert_eq!(dp_unit(2), Some(3));
        assert_eq!(dp_unit(3), Some(4));
        assert_eq!(dp_unit(4), None);
        assert_eq!(default_mode(Some(1)), Mode::UsbOnly);
        assert_eq!(default_mode(Some(2)), Mode::DpOnly);
        assert_eq!(default_mode(Some(3)), Mode::UsbDp);
        assert_eq!(default_mode(None), Mode::DpOnly);
    }

    /// The four PHYs as the Orange Pi 6 Plus firmware gives them: window,
    /// clock and resets.
    #[test]
    fn the_firmware_gives_each_phy_the_lines_the_driver_asks_for() {
        let (mut i, mut h) = acpi_corpus::load("opi6p");
        let devices = acpi::deps::devices(&mut i, &mut h);
        let x = cix_sky1::PLATFORM.extract(&mut i, &mut h, &devices);
        let bases = [0x0903_0000u64, 0x090A_0000, 0x0911_0000, 0x0918_0000];
        let mut seen = 0;
        for &d in &devices {
            if i.hid(d, &mut h).ok().flatten().as_deref() != Some(HID) {
                continue;
            }
            let Some(aml::Uid::Integer(uid)) = i.uid(d, &mut h).ok().flatten() else {
                panic!("a unit number");
            };
            let n = usize::try_from(uid).unwrap();
            let crs = i.crs(d, &mut h).ok().flatten().unwrap_or_default();
            let mem = acpi::ranges::consumed(&crs);
            assert_eq!(
                mem.first().map(|r| (r.base, r.len)),
                Some((bases[n], 0x4_0000))
            );
            let clocks: BTreeMap<_, _> = x
                .description
                .clocks
                .iter()
                .filter(|c| c.consumer == d)
                .map(|c| (c.name.clone().unwrap_or_default(), c.id))
                .collect();
            let m = u32::try_from(n).unwrap();
            assert_eq!(clocks.get("pclk"), Some(&(107 + 4 * m)), "PHY {n}");
            let resets: BTreeMap<_, _> = x
                .description
                .resets
                .iter()
                .filter(|r| r.consumer == d)
                .map(|r| (r.name.clone().unwrap_or_default(), r.index))
                .collect();
            assert_eq!(resets.get("preset"), Some(&(51 + m)));
            assert_eq!(resets.get("reset"), Some(&(55 + m)));
            seen += 1;
        }
        assert_eq!(seen, 4);
    }
}
