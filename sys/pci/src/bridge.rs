// SPDX-License-Identifier: BSD-2-Clause
//! `pcib` on a PCI bus: a PCI-PCI bridge (FreeBSD's `pci_pci`), with the
//! bus behind it as its `pci` child.
//!
//! Interrupts behind a bridge are routed by the standard swizzle through
//! the bridge's own slot on the parent bus, unless firmware gives the
//! bridge a routing table (the ACPI flavor of the driver, in
//! `sys/acpi`, uses [`secondary`] with its `_PRT`).
//!
//! Bus numbers are firmware's: the secondary bus must be above the
//! bridge's own, so nesting cannot loop, and bridges nest at most
//! [`MAX_DEPTH`] deep.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, Resource, StartCx, StopCx, StopReason,
};
use devtree::ids::PciId;
use devtree::matching::Rank;

use crate::PciAddress;
use crate::bus::{PciBusData, PciChild};
use crate::config::ConfigSpace;
use crate::header::{CMD_BUS_MASTER, CMD_IO, CMD_MEMORY, COMMAND};

/// Bridges nest at most this deep.
pub const MAX_DEPTH: u8 = 32;

/// FreeBSD's `pcib` probes at -10000, below every other bridge driver.
const RANK: Rank = Rank(-10_000);

/// The routes behind a bridge in slot `bridge_slot` of a bus routed by
/// `parent`: device `slot`'s pin `pin` (0 = INTA) arrives at the bridge's
/// pin `(slot + pin) % 4` (PCI-to-PCI Bridge Architecture §9.1).
#[must_use]
pub fn swizzle(parent: &[(u8, u8, Resource)], bridge_slot: u8) -> Vec<(u8, u8, Resource)> {
    let mut out = Vec::new();
    for slot in 0..32u8 {
        for pin in 0..4u8 {
            let at = (slot + pin) % 4;
            if let Some((_, _, r)) = parent
                .iter()
                .find(|(s, p, _)| *s == bridge_slot && *p == at)
            {
                out.push((slot, pin, *r));
            }
        }
    }
    out
}

/// The bus behind bridge function `child` and the data for its `pci`
/// child: the ACPI devices under the bridge by `_ADR`, and interrupts
/// routed by `intx` if firmware gave a table, else by [`swizzle`]. Turns
/// on the bridge's forwarding (decode and bus mastering).
pub fn secondary(
    child: &PciChild,
    adr_paths: Vec<(u32, String)>,
    intx: Option<Vec<(u8, u8, Resource)>>,
) -> Result<(u8, PciBusData), AttachError> {
    let f = &child.function;
    let b = f
        .bridge
        .ok_or(AttachError::Device("not a PCI-PCI bridge header"))?;
    if b.secondary <= f.addr.bus || b.subordinate < b.secondary {
        return Err(AttachError::Device("bridge bus numbers out of order"));
    }
    if f.depth >= MAX_DEPTH {
        return Err(AttachError::Device("bridges nested too deep"));
    }
    // In effect before the bus behind it is scanned and its devices
    // attach.
    let cmd = child.cfg.read16(f.addr, COMMAND);
    crate::config::write_command(
        child.cfg,
        f.addr,
        cmd | CMD_IO | CMD_MEMORY | CMD_BUS_MASTER,
    );
    let intx = intx.unwrap_or_else(|| swizzle(&child.bus.intx, f.addr.slot));
    let express = crate::requester::express_type(child.cfg, f.addr, &f.caps);
    Ok((
        b.secondary,
        PciBusData {
            cfg: child.cfg,
            adr_paths,
            intx,
            depth: f.depth + 1,
            upstream: child.bus.upstream.behind(f.addr, express),
        },
    ))
}

/// One of a bridge's forwarding windows: the addresses it passes to its
/// secondary bus, inclusive, and the highest its registers can name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Window {
    /// The first address.
    pub base: u64,
    /// The last address; below `base` when the window is closed.
    pub limit: u64,
    /// The highest address the window's registers can hold.
    pub max: u64,
}

impl Window {
    /// Whether it forwards anything (`pcib_is_window_open`).
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.base < self.limit
    }
}

/// A bridge's windows as firmware left them (FreeBSD's
/// `pcib_probe_windows`): the I/O window and the prefetchable one only if
/// the bridge implements them, the memory window always.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Windows {
    /// The I/O port window.
    pub io: Option<Window>,
    /// The memory window.
    pub mem: Window,
    /// The prefetchable memory window.
    pub pmem: Option<Window>,
}

/// Type 1 header window registers.
mod reg {
    pub(super) const IOBASEL: u16 = 0x1c;
    pub(super) const IOLIMITL: u16 = 0x1d;
    pub(super) const MEMBASE: u16 = 0x20;
    pub(super) const MEMLIMIT: u16 = 0x22;
    pub(super) const PMBASEL: u16 = 0x24;
    pub(super) const PMLIMITL: u16 = 0x26;
    pub(super) const PMBASEH: u16 = 0x28;
    pub(super) const PMLIMITH: u16 = 0x2c;
    pub(super) const IOBASEH: u16 = 0x30;
    pub(super) const IOLIMITH: u16 = 0x32;
}

/// Reads bridge `a`'s windows. An I/O or prefetchable base register that
/// reads 0 is probed by writing ones and put back, as FreeBSD does, to
/// tell an unimplemented window from a closed one.
pub fn windows(cfg: &dyn ConfigSpace, a: PciAddress) -> Windows {
    use crate::config::Width;
    let probe = |offset: u16, width: Width, ones: u32| {
        let v = cfg.read_width(a, offset, width);
        if v != 0 {
            return Some(v);
        }
        cfg.write_width(a, offset, width, ones);
        let back = cfg.read_width(a, offset, width);
        cfg.write_width(a, offset, width, 0);
        (back != 0).then_some(0)
    };
    let io = probe(reg::IOBASEL, Width::Byte, 0xff).map(|v| {
        let lo_limit = u64::from(cfg.read8(a, reg::IOLIMITL));
        let (hi_base, hi_limit, max) = if v & 0xf == 1 {
            (
                u64::from(cfg.read16(a, reg::IOBASEH)),
                u64::from(cfg.read16(a, reg::IOLIMITH)),
                0xffff_ffff,
            )
        } else {
            (0, 0, 0xffff)
        };
        Window {
            base: (hi_base << 16) | ((u64::from(v) & 0xf0) << 8),
            limit: (hi_limit << 16) | ((lo_limit & 0xf0) << 8) | 0xfff,
            max,
        }
    });
    let mem_base = u64::from(cfg.read16(a, reg::MEMBASE));
    let mem_limit = u64::from(cfg.read16(a, reg::MEMLIMIT));
    let mem = Window {
        base: (mem_base & 0xfff0) << 16,
        limit: ((mem_limit & 0xfff0) << 16) | 0xf_ffff,
        max: 0xffff_ffff,
    };
    let pmem = probe(reg::PMBASEL, Width::Word, 0xffff).map(|v| {
        let lo_limit = u64::from(cfg.read16(a, reg::PMLIMITL));
        let (hi_base, hi_limit, max) = if v & 0xf == 1 {
            (
                u64::from(cfg.read32(a, reg::PMBASEH)),
                u64::from(cfg.read32(a, reg::PMLIMITH)),
                u64::MAX,
            )
        } else {
            (0, 0, 0xffff_ffff)
        };
        Window {
            base: (hi_base << 32) | ((u64::from(v) & 0xfff0) << 16),
            limit: (hi_limit << 32) | ((lo_limit & 0xfff0) << 16) | 0xf_ffff,
            max,
        }
    });
    Windows { io, mem, pmem }
}

/// Bridge function `child`'s windows ([`windows`]).
#[must_use]
pub fn child_windows(child: &PciChild) -> Windows {
    windows(child.cfg, child.function.addr)
}

/// What a bridge `name` (`pcib1`) shows in `hw.bus.rman`, as FreeBSD's
/// `pcib_attach_common` makes it: its bus numbers' rman, then one per
/// window it implements (I/O, memory, prefetchable), each managing the
/// window if it is open; and, taken from the bus above, its bus range
/// and its open windows.
pub fn publish_resources<D: Driver>(
    cx: &mut StartCx<'_, D>,
    name: &str,
    domain: u32,
    (secondary, subordinate): (u8, u8),
    w: &Windows,
) {
    use bus::{RmanKind, RmanSpec};
    let buses = (u64::from(secondary), u64::from(subordinate));
    let kind = RmanKind::BusNumber(domain);
    cx.add_rman(
        RmanSpec::new(alloc::format!("{name} bus numbers"), kind, 0, 255)
            .managing(alloc::vec![buses]),
    );
    cx.hold(kind, buses, None);
    let windows = [
        (w.io, "I/O port", RmanKind::IoPort),
        (Some(w.mem), "memory", RmanKind::Memory),
        (w.pmem, "prefetch", RmanKind::Memory),
    ];
    for (win, what, kind) in windows {
        let Some(win) = win else { continue };
        let open = win.is_open().then_some((win.base, win.limit));
        cx.add_rman(
            RmanSpec::new(alloc::format!("{name} {what} window"), kind, 0, win.max)
                .managing(open.into_iter().collect()),
        );
        if let Some(r) = open {
            cx.hold(kind, r, None);
        }
    }
}

/// `pcibN` (PCI-PCI bridge).
#[derive(Debug)]
pub struct PciPciBridge {
    data: Arc<PciBusData>,
    domain: u32,
    bus: u8,
    subordinate: u8,
    windows: Windows,
}

impl Driver for PciPciBridge {
    const NAME: &'static str = "pcib";
    const IDS: Ids = Ids::Pci(&[PciId::subclass(0x06, 0x04).rank(RANK)]);
    // Bridges behind it are numbered before its siblings' (FreeBSD's
    // one-at-a-time attach).
    const UNIT_ORDER: bus::UnitOrder = bus::UnitOrder::Serial;

    fn describe(_: &Ident) -> String {
        "PCI-PCI bridge".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let child = cx.bus_data_arc::<PciChild>().ok_or(AttachError::Decline)?;
        let (bus, data) = secondary(&child, Vec::new(), None)?;
        Ok(Self {
            data: Arc::new(data),
            domain: child.function.addr.domain,
            bus,
            subordinate: child.function.bridge.map_or(bus, |b| b.subordinate),
            windows: child_windows(&child),
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let name = alloc::format!("{}{}", Self::NAME, cx.unit());
        publish_resources(
            cx,
            &name,
            self.domain,
            (self.bus, self.subordinate),
            &self.windows,
        );
        cx.add_child(
            Ident::PciBus {
                domain: self.domain,
                bus: self.bus,
                acpi: false,
            },
            Some(Arc::clone(&self.data) as _),
            Vec::new(),
        );
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<PciPciBridge>();
