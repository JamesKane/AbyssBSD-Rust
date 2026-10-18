// SPDX-License-Identifier: BSD-2-Clause
//! The `virtio_pci` drivers: a virtio device's PCI function as a bus with
//! one child.
//!
//! Two drivers share the name, as in FreeBSD: [`MODERN`] at the default
//! rank, which declines a function without the modern capabilities, and
//! [`LEGACY`] below it, for legacy-only devices (I/O ports, so x86 only).
//! The transport claims the function's registers and interrupts; the
//! child gets the [`VirtioDevice`] as bus data and the interrupts as its
//! resources.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, Resource, StartCx, StopCx, StopReason,
};
use devtree::ids::{PciId, PciIdent, VirtioIdent};
use devtree::matching::Rank;
use pci::bus::PciChild;
use pci::header::{CMD_BUS_MASTER, CMD_IO, CMD_MEMORY, COMMAND};
use pci::{BarKind, ConfigSpace, Function, caps};

use crate::transport::{COMMON_SIZE, Regs, Vectors, Window};
use crate::{VENDOR, VirtioDevice, device_name};

/// Device IDs: transitional devices, then modern-only ones.
const DEVICE_MIN: u16 = 0x1000;
const LEGACY_MAX: u16 = 0x103f;
const MODERN_MIN: u16 = 0x1040;
const MODERN_MAX: u16 = 0x107f;

/// The only revision the legacy interface defines.
const LEGACY_REVISION: u8 = 0;

/// Legacy queues probed at most (the interface does not say how many).
const LEGACY_MAX_QUEUES: u16 = 64;

/// The legacy registers, up to the device configuration with MSI-X on.
const LEGACY_REGS: u16 = 0x18;

/// Modern capability types (virtio 1.2 §4.1.4).
const CAP_COMMON: u8 = 1;
const CAP_NOTIFY: u8 = 2;
const CAP_ISR: u8 = 3;
const CAP_DEVICE: u8 = 4;

/// The device type a function carries, if it is a virtio device.
fn device_type(i: &PciIdent) -> Option<u32> {
    match i.device {
        DEVICE_MIN..=LEGACY_MAX => Some(u32::from(i.subdevice)),
        MODERN_MIN..=MODERN_MAX => Some(u32::from(i.device - MODERN_MIN)),
        _ => None,
    }
}

fn describe(kind: &str, ident: &Ident) -> String {
    let name = match ident {
        Ident::Pci(i, _, _) => device_type(i).and_then(device_name),
        _ => None,
    };
    alloc::format!("VirtIO PCI ({kind}) {} adapter", name.unwrap_or("Unknown"))
}

/// One modern capability: where its structure is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CapRef {
    pub(crate) bar: u8,
    pub(crate) offset: u32,
    pub(crate) len: u32,
}

/// The modern capabilities a driver needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ModernCaps {
    pub(crate) common: CapRef,
    pub(crate) notify: CapRef,
    pub(crate) notify_multiplier: u32,
    pub(crate) isr: CapRef,
    pub(crate) device: Option<CapRef>,
}

/// Finds the modern structures: the first of each type whose BAR is an
/// implemented memory BAR that holds it whole (§4.1.4.1).
pub(crate) fn modern_caps(cfg: &dyn ConfigSpace, f: &Function) -> Option<ModernCaps> {
    let (mut common, mut notify, mut isr, mut device) = (None, None, None, None);
    let mut multiplier = 0;
    for c in f.caps.iter().filter(|c| c.id == caps::id::VENDOR) {
        let at = u16::from(c.offset);
        let len = cfg.read8(f.addr, at + 2);
        if len < 16 {
            continue;
        }
        let r = CapRef {
            bar: cfg.read8(f.addr, at + 4),
            offset: cfg.read32(f.addr, at + 8),
            len: cfg.read32(f.addr, at + 12),
        };
        let fits = f.bars.iter().any(|b| {
            b.index == r.bar
                && b.kind != BarKind::Io
                && u64::from(r.offset) + u64::from(r.len) <= b.size
        });
        if !fits {
            continue;
        }
        match cfg.read8(f.addr, at + 3) {
            CAP_COMMON if common.is_none() && r.len as usize >= COMMON_SIZE => common = Some(r),
            CAP_NOTIFY if notify.is_none() && len >= 20 && r.len >= 2 => {
                notify = Some(r);
                multiplier = cfg.read32(f.addr, at + 16);
            }
            CAP_ISR if isr.is_none() && r.len >= 1 => isr = Some(r),
            CAP_DEVICE if device.is_none() => device = Some(r),
            _ => {}
        }
    }
    Some(ModernCaps {
        common: common?,
        notify: notify?,
        notify_multiplier: multiplier,
        isr: isr?,
        device,
    })
}

/// A `virtio_pciN` instance (either interface).
#[derive(Debug)]
pub struct VirtioPci {
    dev: Arc<VirtioDevice>,
    child: Arc<PciChild>,
    ident: VirtioIdent,
    irqs: Vec<Resource>,
    msix: Vec<u32>,
}

impl VirtioPci {
    /// Turns on decoding and bus mastering, chooses the interrupts, and
    /// builds the instance.
    fn finish(
        cx: &mut AttachCx<'_>,
        child: Arc<PciChild>,
        mut dev: VirtioDevice,
        decode: u16,
    ) -> Result<Self, AttachError> {
        let (cfg, f) = (child.cfg, &child.function);
        let cmd = cfg.read16(f.addr, COMMAND);
        cfg.write16(f.addr, COMMAND, cmd | decode | CMD_BUS_MASTER);
        dev.reset();
        let mut msix = Vec::new();
        let mut irqs = Vec::new();
        if pci::msi::available()
            && let Some(table) = pci::msi::msix_count(cfg, f)
            && let Ok(hws) = pci::msi::enable_msix(cfg, f, table.min(dev.queues.saturating_add(1)))
        {
            irqs = hws
                .iter()
                .map(|&hw| Resource::Irq {
                    domain: &pci::msi::PCI_MSI,
                    hw,
                    trigger: irq::Trigger::Edge,
                    polarity: irq::Polarity::High,
                })
                .collect();
            dev.vectors = Vectors::MsiX {
                count: u16::try_from(hws.len()).unwrap_or(1),
                queues: dev.queues,
            };
            if let Regs::Legacy { msix, .. } = &mut dev.regs {
                *msix = true;
            }
            msix = hws;
        } else if let Some(r) = cx
            .resources()
            .iter()
            .find(|r| matches!(r, Resource::Irq { .. }))
        {
            irqs.push(*r);
            dev.vectors = Vectors::Intx;
        }
        let i = f.header.ident;
        Ok(Self {
            ident: VirtioIdent {
                vendor: u32::from(i.vendor),
                device: i.device,
                subvendor: i.subvendor,
                device_type: dev.device_type,
            },
            dev: Arc::new(dev),
            child,
            irqs,
            msix,
        })
    }

    fn start_child<D: Driver>(&self, cx: &mut StartCx<'_, D>) {
        cx.add_child(
            Ident::Virtio(self.ident),
            Some(Arc::clone(&self.dev) as _),
            self.irqs.clone(),
        );
    }
}

impl Drop for VirtioPci {
    fn drop(&mut self) {
        let (cfg, f) = (self.child.cfg, &self.child.function);
        if !self.msix.is_empty() {
            pci::msi::PCI_MSI.remove(&self.msix);
            pci::msi::disable_msix(cfg, f);
        }
        let cmd = cfg.read16(f.addr, COMMAND);
        cfg.write16(f.addr, COMMAND, cmd & !CMD_BUS_MASTER);
    }
}

/// The modern interface.
#[derive(Debug)]
pub struct Modern(VirtioPci);

impl Driver for Modern {
    const NAME: &'static str = "virtio_pci";
    const IDS: Ids = Ids::Pci(&[PciId::vendor(VENDOR).rank(Rank::DEFAULT)]);

    fn describe(ident: &Ident) -> String {
        describe("modern", ident)
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let child = cx.bus_data_arc::<PciChild>().ok_or(AttachError::Decline)?;
        let f = &child.function;
        let device_type = device_type(&f.header.ident).ok_or(AttachError::Decline)?;
        let caps = modern_caps(child.cfg, f).ok_or(AttachError::Decline)?;
        // Map each BAR the structures use once; resources list the memory
        // BARs in order.
        let mut bars: Vec<u8> = Vec::new();
        let mut grants = Vec::new();
        let mut window = |r: CapRef| -> Result<Window, AttachError> {
            let grant = if let Some(g) = bars.iter().position(|&b| b == r.bar) {
                g
            } else {
                let index = f
                    .bars
                    .iter()
                    .filter(|b| b.kind != BarKind::Io)
                    .position(|b| b.index == r.bar)
                    .ok_or(AttachError::Device("capability BAR not described"))?;
                grants.push(cx.mmio_bytes(index)?);
                bars.push(r.bar);
                bars.len() - 1
            };
            Ok(Window {
                grant,
                offset: r.offset as usize,
                len: r.len as usize,
            })
        };
        let common = window(caps.common)?;
        let notify = window(caps.notify)?;
        let isr = window(caps.isr)?;
        let device = caps.device.map(&mut window).transpose()?;
        let mut dev = VirtioDevice {
            regs: Regs::Modern {
                grants,
                common,
                notify,
                notify_multiplier: caps.notify_multiplier,
                isr,
                device,
            },
            device_type,
            vectors: Vectors::None,
            queues: 0,
        };
        dev.reset();
        dev.queues = dev.modern_queues();
        VirtioPci::finish(cx, child, dev, CMD_MEMORY).map(Self)
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        self.0.start_child(cx);
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {
        self.0.dev.reset();
    }
}

/// The legacy interface.
#[derive(Debug)]
pub struct Legacy(VirtioPci);

impl Driver for Legacy {
    const NAME: &'static str = "virtio_pci";
    const IDS: Ids = Ids::Pci(&[PciId::vendor(VENDOR).rank(Rank::LOW_PRIORITY)]);

    fn describe(ident: &Ident) -> String {
        describe("legacy", ident)
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let child = cx.bus_data_arc::<PciChild>().ok_or(AttachError::Decline)?;
        let i = child.function.header.ident;
        if !(DEVICE_MIN..=LEGACY_MAX).contains(&i.device) || i.revision != LEGACY_REVISION {
            return Err(AttachError::Decline);
        }
        // BAR 0, the I/O registers (described on x86 only).
        let io = cx.io_ports(0).map_err(|_| AttachError::Decline)?;
        if io.len() < LEGACY_REGS {
            return Err(AttachError::Device("legacy registers too small"));
        }
        let mut dev = VirtioDevice {
            regs: Regs::Legacy { io, msix: false },
            device_type: u32::from(i.subdevice),
            vectors: Vectors::None,
            queues: 0,
        };
        dev.reset();
        dev.queues = (0..LEGACY_MAX_QUEUES)
            .find(|&q| dev.queue_max(q) == 0)
            .unwrap_or(LEGACY_MAX_QUEUES);
        VirtioPci::finish(cx, child, dev, CMD_IO).map(Self)
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        self.0.start_child(cx);
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {
        self.0.dev.reset();
    }
}

/// The declarations for the build's driver table.
pub const MODERN: DriverDecl = DriverDecl::of::<Modern>();
/// See [`MODERN`].
pub const LEGACY: DriverDecl = DriverDecl::of::<Legacy>();
