// SPDX-License-Identifier: BSD-2-Clause
//! The interpreter's host: operation regions and time, through [`Ops`].

#![allow(
    clippy::cast_possible_truncation,
    reason = "region accesses narrow values to the declared access width"
)]

use aml::{AccessWidth, Host, HostError, NodeId, RegionAccess, RegionSpace};
use sched::Sleepable;

use crate::Ops;

pub(crate) struct AmlHost<'a> {
    pub(crate) ops: &'static dyn Ops,
    pub(crate) s: &'a Sleepable,
    /// `Notify`s sent during the current job, delivered after it.
    pub(crate) notified: alloc::vec::Vec<(NodeId, u64)>,
}

fn bytes(w: AccessWidth) -> u8 {
    match w {
        AccessWidth::U8 => 1,
        AccessWidth::U16 => 2,
        AccessWidth::U32 => 4,
        AccessWidth::U64 => 8,
    }
}

fn pci_addr(a: &RegionAccess) -> Option<pci::PciAddress> {
    let p = a.pci?;
    Some(pci::PciAddress {
        domain: u32::from(p.segment),
        bus: p.bus,
        slot: p.device,
        function: p.function,
    })
}

impl AmlHost<'_> {
    fn pci_read(&self, a: &RegionAccess) -> Result<u64, HostError> {
        let f = pci_addr(a).ok_or(HostError::Failed)?;
        let cfg = self.ops.pci_config(f).ok_or(HostError::Unsupported)?;
        let off = u16::try_from(a.address()).map_err(|_| HostError::Denied)?;
        if off >= 4096 {
            return Err(HostError::Denied);
        }
        Ok(match a.width {
            AccessWidth::U8 => u64::from(cfg.read8(f, off)),
            AccessWidth::U16 => u64::from(cfg.read16(f, off)),
            AccessWidth::U32 => u64::from(cfg.read32(f, off)),
            AccessWidth::U64 => {
                u64::from(cfg.read32(f, off)) | (u64::from(cfg.read32(f, off + 4)) << 32)
            }
        })
    }

    fn pci_write(&self, a: &RegionAccess, v: u64) -> Result<(), HostError> {
        let f = pci_addr(a).ok_or(HostError::Failed)?;
        let cfg = self.ops.pci_config(f).ok_or(HostError::Unsupported)?;
        let off = u16::try_from(a.address()).map_err(|_| HostError::Denied)?;
        if off >= 4096 {
            return Err(HostError::Denied);
        }
        // Narrow writes merge into their dword, as FreeBSD's config writes
        // of a byte or word do on ECAM.
        let dword = off & !3;
        let shift = u32::from(off & 3) * 8;
        let merge = |mask: u32, val: u32| {
            let old = cfg.read32(f, dword);
            cfg.write32(f, dword, (old & !(mask << shift)) | ((val & mask) << shift));
        };
        match a.width {
            AccessWidth::U8 => merge(0xff, v as u32),
            AccessWidth::U16 => merge(0xffff, v as u32),
            AccessWidth::U32 => cfg.write32(f, off, v as u32),
            AccessWidth::U64 => {
                cfg.write32(f, off, v as u32);
                cfg.write32(f, off + 4, (v >> 32) as u32);
            }
        }
        Ok(())
    }
}

impl Host for AmlHost<'_> {
    fn read(&mut self, a: &RegionAccess) -> Result<u64, HostError> {
        match a.space {
            RegionSpace::SystemMemory => self
                .ops
                .mem_read(a.address(), bytes(a.width))
                .ok_or(HostError::Denied),
            RegionSpace::SystemIo => {
                let port = u16::try_from(a.address()).map_err(|_| HostError::Denied)?;
                self.ops
                    .io_read(port, bytes(a.width).min(4))
                    .map(u64::from)
                    .ok_or(HostError::Unsupported)
            }
            RegionSpace::PciConfig => self.pci_read(a),
            _ => Err(HostError::Unsupported),
        }
    }

    fn write(&mut self, a: &RegionAccess, v: u64) -> Result<(), HostError> {
        match a.space {
            RegionSpace::SystemMemory => self
                .ops
                .mem_write(a.address(), bytes(a.width), v)
                .ok_or(HostError::Denied),
            RegionSpace::SystemIo => {
                let port = u16::try_from(a.address()).map_err(|_| HostError::Denied)?;
                self.ops
                    .io_write(port, bytes(a.width).min(4), v as u32)
                    .ok_or(HostError::Unsupported)
            }
            RegionSpace::PciConfig => self.pci_write(a, v),
            _ => Err(HostError::Unsupported),
        }
    }

    fn sleep(&mut self, ms: u64) {
        self.ops.sleep_ms(self.s, ms);
    }

    fn stall(&mut self, us: u64) {
        self.ops.stall_us(us.min(100));
    }

    fn timer(&mut self) -> u64 {
        self.ops.timer_100ns()
    }

    fn notify(&mut self, node: NodeId, value: u64) {
        self.notified.push((node, value));
    }

    fn fatal(&mut self, kind: u8, code: u32, arg: u64) {
        self.ops.log(format_args!(
            "acpi: AML Fatal(type {kind:#x}, code {code:#x}, arg {arg:#x})"
        ));
    }
}
