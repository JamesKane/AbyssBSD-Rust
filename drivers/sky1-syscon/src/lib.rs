// SPDX-License-Identifier: BSD-2-Clause
//! `sky1_syscon`: the CIX Sky1's shared register windows (`CIXHA018`),
//! the ACPI form of a devicetree `syscon` node. Each instance is a range
//! of registers other devices program through it, which they name by a
//! `_DSD` reference: the FCH CRU (`\_SB.UCRU`, the UARTs'
//! `sky1,fch_cru`), the audio subsystem's CRU (`audss_cru`), and others
//! (clean-room `sky1-audio` specification, §0.6.1).
//!
//! An instance has no behavior of its own and must not be touched when
//! bound: the audio CRU is unpowered until its power resource is on. So
//! attach reads nothing; it maps the window and publishes it as a
//! `RegisterRegion` (DRIVER-MODEL.md §9.3), which the devices that name
//! it lease, the read-modify-write under one lock. The first client is
//! the USB-C/DisplayPort PHY's plug orientation bit.
//!
//! The FCH CRU's window (`CRU0`) is the S5 reset controller's too: the
//! platform declares it shared, and the reset controller's words are not
//! the clients' fields.
//!
//! FreeBSD has no driver for it; the name is recorded in
//! docs/compat-deviations.md.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;

use alloc::sync::Arc;

use bus::region::RegisterRegion;
use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, MmioGrant, StartCx, StopCx, StopReason,
};
use devtree::ids::AcpiId;
use regs::Bytes;
use sync::SpinLock;

/// The window, with its updates serialised.
struct Window {
    regs: MmioGrant<Bytes>,
    lock: SpinLock<()>,
}

impl RegisterRegion for Window {
    fn len(&self) -> usize {
        self.regs.len()
    }

    fn read(&self, offset: usize) -> u32 {
        regs::Mmio::read32(&self.regs, offset)
    }

    fn update(&self, offset: usize, mask: u32, value: u32) {
        let _g = self.lock.lock();
        let v = regs::Mmio::read32(&self.regs, offset);
        regs::Mmio::write32(&self.regs, offset, v & !mask | value & mask);
    }
}

/// `sky1_sysconN`.
pub struct Sky1Syscon {
    window: Arc<Window>,
}

impl core::fmt::Debug for Sky1Syscon {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Sky1Syscon")
            .field("len", &self.window.regs.len())
            .finish()
    }
}

impl Driver for Sky1Syscon {
    const NAME: &'static str = "sky1_syscon";
    const IDS: Ids = Ids::Acpi(&[AcpiId::new("CIXHA018")]);

    fn describe(_: &Ident) -> String {
        "CIX Sky1 shared register window".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        // An instance firmware hides (`_STA` 0) is never attached.
        let regs = cx.mmio_bytes(0)?;
        Ok(Self {
            window: Arc::new(Window {
                regs,
                lock: SpinLock::new(()),
            }),
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        cx.provide_region(Arc::clone(&self.window) as Arc<dyn RegisterRegion>);
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<Sky1Syscon>();
