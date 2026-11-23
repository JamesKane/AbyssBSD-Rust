// SPDX-License-Identifier: BSD-2-Clause
//! `amdsmn`: access to an AMD processor's System Management Network (SMN),
//! the internal bus through which the sensors and power management of the
//! chip are reached, a child of the host bridge on Family 15h, 17h, 19h and
//! 1Ah.
//!
//! The host bridge (the "root complex" function) has two registers in its
//! configuration space for it: writing an SMN address to the index register
//! makes the data register show, or write, the 32-bit register there.
//! The pair is not atomic, so each access is made under a lock
//! ([`Smn`]). Other drivers (`amdtemp`) ask for the accessor of their
//! bridge with [`find`].
//!
//! Behavior reference: sys/dev/amdsmn/amdsmn.c.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use bus::{AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, StartCx, StopCx, StopReason};
use devtree::strings::PciAddress;
use pci::bus::PciChild;
use pci::hostb::HostbChild;
use sync::SpinLock;

/// A host bridge that has an SMN, and where its registers are.
#[derive(Clone, Copy, Debug)]
struct Product {
    device: u16,
    addr_reg: u16,
    data_reg: u16,
}

const F15H: (u16, u16) = (0xb8, 0xbc);
const F17H: (u16, u16) = (0x60, 0x64);

/// The host bridges FreeBSD knows (`amdsmn_ids`), AMD's vendor ID 1022h.
const PRODUCTS: [Product; 12] = [
    Product {
        device: 0x1576,
        addr_reg: F15H.0,
        data_reg: F15H.1,
    }, // 15h model 60h
    Product {
        device: 0x1450,
        addr_reg: F17H.0,
        data_reg: F17H.1,
    }, // 17h
    Product {
        device: 0x15d0,
        addr_reg: F17H.0,
        data_reg: F17H.1,
    }, // 17h model 10h
    Product {
        device: 0x1480,
        addr_reg: F17H.0,
        data_reg: F17H.1,
    }, // 17h model 30h, 70h, 19h 00h/20h
    Product {
        device: 0x1630,
        addr_reg: F17H.0,
        data_reg: F17H.1,
    }, // 17h model 60h, 19h 50h
    Product {
        device: 0x14a4,
        addr_reg: F17H.0,
        data_reg: F17H.1,
    }, // 19h model 10h
    Product {
        device: 0x14b5,
        addr_reg: F17H.0,
        data_reg: F17H.1,
    }, // 19h model 40h
    Product {
        device: 0x14d8,
        addr_reg: F17H.0,
        data_reg: F17H.1,
    }, // 19h model 60h (Raphael), 1Ah 40h
    Product {
        device: 0x14e8,
        addr_reg: F17H.0,
        data_reg: F17H.1,
    }, // 19h model 70h
    Product {
        device: 0x153a,
        addr_reg: F17H.0,
        data_reg: F17H.1,
    }, // 1Ah model 00h
    Product {
        device: 0x1507,
        addr_reg: F17H.0,
        data_reg: F17H.1,
    }, // 1Ah model 20h
    Product {
        device: 0x1122,
        addr_reg: F17H.0,
        data_reg: F17H.1,
    }, // 1Ah model 60h
];

/// AMD's PCI vendor ID.
pub const VENDOR_AMD: u16 = 0x1022;

fn product(vendor: u16, device: u16) -> Option<Product> {
    (vendor == VENDOR_AMD)
        .then(|| PRODUCTS.iter().find(|p| p.device == device).copied())
        .flatten()
}

/// Whether `device` is a host bridge with an SMN (`amdsmn_match`).
#[must_use]
pub fn has_smn(vendor: u16, device: u16) -> bool {
    product(vendor, device).is_some()
}

/// The CPU family (CPUID 1, with the extended family added from 0Fh on).
#[must_use]
pub fn cpu_family() -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        let eax = core::arch::x86_64::__cpuid(1).eax;
        let base = (eax >> 8) & 0xf;
        if base == 0xf {
            base + ((eax >> 20) & 0xff)
        } else {
            base
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        0
    }
}

/// One host bridge's SMN.
pub struct Smn {
    bridge: Arc<PciChild>,
    addr_reg: u16,
    data_reg: u16,
    lock: SpinLock<()>,
}

impl core::fmt::Debug for Smn {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Smn").finish_non_exhaustive()
    }
}

impl Smn {
    /// Reads the SMN register at `addr` (`amdsmn_read`). `None` if the
    /// bridge's configuration space refuses the index write.
    #[must_use]
    pub fn read(&self, addr: u32) -> Option<u32> {
        let _held = self.lock.lock();
        self.bridge.write32(self.addr_reg, addr).ok()?;
        Some(self.bridge.read32(self.data_reg))
    }

    /// Writes the SMN register at `addr` (`amdsmn_write`).
    #[must_use]
    pub fn write(&self, addr: u32, value: u32) -> Option<()> {
        let _held = self.lock.lock();
        self.bridge.write32(self.addr_reg, addr).ok()?;
        self.bridge.write32(self.data_reg, value).ok()
    }
}

/// The SMNs that have attached, by host bridge.
static SMNS: SpinLock<Vec<(PciAddress, Arc<Smn>)>> = SpinLock::new(Vec::new());

/// The SMN of the host bridge `bridge` (`device_find_child(parent,
/// "amdsmn", -1)`), if `amdsmn` has attached to it.
#[must_use]
pub fn find(bridge: &PciChild) -> Option<Arc<Smn>> {
    let at = bridge.function.addr;
    SMNS.lock()
        .iter()
        .find(|(a, _)| *a == at)
        .map(|(_, s)| Arc::clone(s))
}

/// `amdsmnN`.
#[derive(Debug)]
pub struct AmdSmn {
    at: PciAddress,
}

impl Driver for AmdSmn {
    const NAME: &'static str = "amdsmn";
    const IDS: Ids = Ids::Nexus(&["amdsmn"]);

    fn describe(_: &Ident) -> String {
        alloc::format!("AMD Family {:02X}h System Management Network", cpu_family())
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let host = cx
            .bus_data_arc::<HostbChild>()
            .ok_or(AttachError::Decline)?;
        let id = host.bridge.function.header.ident;
        let p = product(id.vendor, id.device).ok_or(AttachError::Decline)?;
        // `amdsmn_probe`: the families with an SMN.
        if !matches!(cpu_family(), 0x15 | 0x17 | 0x19 | 0x1a) {
            return Err(AttachError::Decline);
        }
        let at = host.bridge.function.addr;
        SMNS.lock().push((
            at,
            Arc::new(Smn {
                bridge: Arc::clone(&host.bridge),
                addr_reg: p.addr_reg,
                data_reg: p.data_reg,
                lock: SpinLock::new(()),
            }),
        ));
        Ok(Self { at })
    }

    fn start(&self, _: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {
        SMNS.lock().retain(|(a, _)| *a != self.at);
    }
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<AmdSmn>();

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_host_bridges_with_an_smn() {
        // The X870E's: Raphael / Granite Ridge.
        assert!(has_smn(VENDOR_AMD, 0x14d8));
        assert_eq!(
            product(VENDOR_AMD, 0x14d8).map(|p| (p.addr_reg, p.data_reg)),
            Some(F17H)
        );
        // Family 15h model 60h uses the older register pair.
        assert_eq!(
            product(VENDOR_AMD, 0x1576).map(|p| (p.addr_reg, p.data_reg)),
            Some(F15H)
        );
        // Not AMD's, or not a host bridge FreeBSD lists.
        assert!(!has_smn(0x8086, 0x14d8));
        assert!(!has_smn(VENDOR_AMD, 0x14d9));
    }
}
