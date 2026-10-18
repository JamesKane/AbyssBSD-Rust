// SPDX-License-Identifier: BSD-2-Clause

extern crate std;

use alloc::collections::BTreeMap;
use alloc::vec;
use core::cell::RefCell;

use super::*;
use crate::bar::BarKind;
use crate::header::{CMD_MEMORY, COMMAND, HeaderType};

/// One fake function: config dwords, and per-BAR size masks (the bits
/// that read back after writing all-ones).
#[derive(Clone, Default)]
struct FakeFn {
    regs: BTreeMap<u16, u32>,
    bar_masks: BTreeMap<u16, u32>,
    /// Set if a BAR was written while its decoding was on.
    sized_while_decoding: bool,
    /// Writes to the command register (each is costly under a hypervisor).
    command_writes: u32,
}

#[derive(Default)]
struct Fake {
    fns: RefCell<BTreeMap<(u8, u8, u8), FakeFn>>,
}

impl Fake {
    fn add(&self, bus: u8, slot: u8, function: u8, f: FakeFn) {
        self.fns.borrow_mut().insert((bus, slot, function), f);
    }
}

impl ConfigSpace for Fake {
    fn read32(&self, a: PciAddress, offset: u16) -> u32 {
        self.fns
            .borrow()
            .get(&(a.bus, a.slot, a.function))
            .map_or(u32::MAX, |f| *f.regs.get(&offset).unwrap_or(&0))
    }

    fn write32(&self, a: PciAddress, offset: u16, v: u32) {
        let mut fns = self.fns.borrow_mut();
        let Some(f) = fns.get_mut(&(a.bus, a.slot, a.function)) else {
            return;
        };
        if offset == COMMAND {
            f.command_writes += 1;
        }
        if let Some(&mask) = f.bar_masks.get(&offset) {
            let cmd = *f.regs.get(&COMMAND).unwrap_or(&0) as u16;
            let io = f.regs.get(&offset).is_some_and(|r| r & 1 != 0);
            let decoding = if io {
                crate::header::CMD_IO
            } else {
                CMD_MEMORY
            };
            if v == u32::MAX && cmd & decoding != 0 {
                f.sized_while_decoding = true;
            }
            // Writable address bits, plus read-only flag bits kept.
            let flags = f.regs.get(&offset).copied().unwrap_or(0) & !mask;
            f.regs.insert(offset, (v & mask) | (flags & 0xf));
        } else if (0x10..=0x24).contains(&offset)
            && f.regs.get(&0xc).is_some_and(|h| (h >> 16) & 0x7f == 0)
        {
            // An unimplemented BAR is hardwired to zero, except the upper
            // half of a 64-bit BAR, which is fully writable.
            if f.regs.contains_key(&offset) && f.bar_masks.contains_key(&(offset - 4)) {
                f.regs.insert(offset, v);
            }
        } else {
            f.regs.insert(offset, v);
        }
    }
}

fn device(vendor: u16, device: u16, class: u32, header: u8) -> FakeFn {
    let mut f = FakeFn::default();
    f.regs
        .insert(0, u32::from(vendor) | (u32::from(device) << 16));
    f.regs.insert(8, (class << 8) | 1);
    f.regs.insert(0xc, u32::from(header) << 16);
    f.regs.insert(COMMAND, u32::from(CMD_MEMORY));
    f
}

fn addr(bus: u8, slot: u8, function: u8) -> PciAddress {
    PciAddress {
        domain: 0,
        bus,
        slot,
        function,
    }
}

#[test]
fn bars_are_sized_with_decoding_off_and_restored() {
    let fake = Fake::default();
    let mut f = device(0x1af4, 0x1042, 0x01_00_00, 0);
    // BAR0: 32-bit memory, 16 KiB at 0xfe00_0000.
    f.regs.insert(0x10, 0xfe00_0000);
    f.bar_masks.insert(0x10, 0xffff_c000);
    // BAR2/3: 64-bit prefetchable, 1 MiB at 0x8_0000_0000.
    f.regs.insert(0x18, 0x0000_000c);
    f.regs.insert(0x1c, 0x8);
    f.bar_masks.insert(0x18, 0xfff0_0000);
    f.bar_masks.insert(0x1c, 0xffff_ffff);
    // BAR4: I/O, 32 ports at 0xc000, decoding 16 bits.
    f.regs.insert(0x20, 0xc001);
    f.bar_masks.insert(0x20, 0x0000_ffe0);
    fake.add(0, 3, 0, f);
    let bars = bar::read_all(&fake, addr(0, 3, 0), 6);
    assert_eq!(
        bars,
        vec![
            Ok(Bar {
                index: 0,
                kind: BarKind::Memory32,
                base: 0xfe00_0000,
                size: 0x4000,
                prefetchable: false
            }),
            Ok(Bar {
                index: 2,
                kind: BarKind::Memory64,
                base: 0x8_0000_0000,
                size: 0x10_0000,
                prefetchable: true
            }),
            Ok(Bar {
                index: 4,
                kind: BarKind::Io,
                base: 0xc000,
                size: 0x20,
                prefetchable: false
            }),
        ]
    );
    let fns = fake.fns.borrow();
    let f = &fns[&(0, 3, 0)];
    assert!(!f.sized_while_decoding);
    assert_eq!(f.regs[&0x10], 0xfe00_0000, "BAR restored");
    assert_eq!(f.regs[&0x1c], 0x8, "upper half restored");
    assert_eq!(f.regs[&COMMAND] as u16, CMD_MEMORY, "decoding restored");
    // Decoding goes off and back on once for all six slots.
    assert_eq!(f.command_writes, 2);
}

#[test]
fn hostile_bars_are_rejected() {
    let fake = Fake::default();
    let mut f = device(0x8086, 0x1234, 0x02_00_00, 0);
    // A 64-bit BAR in slot 5: truncated.
    f.regs.insert(0x24, 0x4);
    // A non-power-of-two size.
    f.regs.insert(0x10, 0xfe00_0000);
    f.bar_masks.insert(0x10, 0xffff_a000);
    // Reserved memory type.
    f.regs.insert(0x14, 0x6);
    fake.add(0, 1, 0, f);
    let bars = bar::read_all(&fake, addr(0, 1, 0), 6);
    assert_eq!(
        bars,
        vec![
            Err((0, BarError::BadSize)),
            Err((1, BarError::ReservedType)),
            Err((5, BarError::Truncated)),
        ]
    );
}

#[test]
fn capability_loops_are_bounded() {
    let fake = Fake::default();
    let mut f = device(0x1b36, 0x0010, 0x01_08_02, 0);
    f.regs.insert(COMMAND, u32::from(CMD_MEMORY) | (0x10 << 16));
    f.regs.insert(0x34, 0x40);
    // 0x40 (PCIe) -> 0x50 (MSI-X) -> 0x40: a loop.
    f.regs.insert(0x40, 0x5010);
    f.regs.insert(0x50, 0x4011);
    // Extended: 0x100 (AER, v1) -> 0x100: a loop.
    f.regs.insert(0x100, 0x1001_0001);
    fake.add(0, 4, 0, f);
    let caps = caps::list(&fake, addr(0, 4, 0));
    assert_eq!(
        caps,
        vec![
            Capability {
                id: 0x10,
                offset: 0x40
            },
            Capability {
                id: 0x11,
                offset: 0x50
            }
        ]
    );
    let ext = caps::ext_list(&fake, addr(0, 4, 0));
    assert_eq!(
        ext,
        vec![ExtCapability {
            id: 1,
            version: 1,
            offset: 0x100
        }]
    );
}

#[test]
fn enumeration_is_depth_first_and_follows_bridges() {
    let fake = Fake::default();
    // Bus 0: host bridge at 0:0.0, a multifunction device at 0:1 with
    // functions 0 and 2, a bridge at 0:2.0 to bus 1..2, and a device at
    // 0:3.0.
    fake.add(0, 0, 0, device(0x8086, 0x29c0, 0x06_00_00, 0));
    fake.add(0, 1, 0, device(0x8086, 0x2918, 0x06_01_00, 0x80));
    fake.add(0, 1, 2, device(0x8086, 0x2922, 0x01_06_01, 0));
    let mut br = device(0x1b36, 0x000c, 0x06_04_00, 1);
    br.regs.insert(0x18, 0x0002_0100);
    fake.add(0, 2, 0, br);
    fake.add(0, 3, 0, device(0x1af4, 0x1041, 0x02_00_00, 0));
    // Bus 1: a device, and a bridge whose secondary bus loops to its own.
    fake.add(1, 0, 0, device(0x1b36, 0x0010, 0x01_08_02, 0));
    let mut bad = device(0x1b36, 0x000c, 0x06_04_00, 1);
    bad.regs.insert(0x18, 0x0001_0101);
    fake.add(1, 1, 0, bad);
    // A device on bus 5, outside every bridge: never found.
    fake.add(5, 0, 0, device(0x1234, 0x1111, 0x03_00_00, 0));

    let (fns, problems) = enumerate(&fake, 0, 0..=255);
    let found: alloc::vec::Vec<(u8, u8, u8, u8)> = fns
        .iter()
        .map(|f| (f.addr.bus, f.addr.slot, f.addr.function, f.depth))
        .collect();
    assert_eq!(
        found,
        vec![
            (0, 0, 0, 0),
            (0, 1, 0, 0),
            (0, 1, 2, 0),
            (0, 2, 0, 0),
            (1, 0, 0, 1),
            (1, 1, 0, 1),
            (0, 3, 0, 0),
        ]
    );
    assert_eq!(
        problems,
        vec![enumerate::BridgeProblem::BadBus(addr(1, 1, 0))]
    );
    assert_eq!(fns[3].header.kind, HeaderType::Bridge);
    assert_eq!(fns[3].bridge.unwrap().secondary, 1);
    assert_eq!(fns[2].header.ident.class, 0x01_06_01);
    assert_eq!(fns[2].header.ident.revision, 1);
    // A range that excludes bus 1 stops at the bridge.
    let (fns, problems) = enumerate(&fake, 0, 0..=0);
    assert_eq!(fns.len(), 5);
    assert_eq!(problems.len(), 1);
}

#[test]
fn ecam_rejects_out_of_range_addresses() {
    let mut window = vec![0u32; 2 * (1 << 20) / 4];
    window[(1 << 20) / 4 + (3 << 15) / 4] = 0x1234_5678;
    // SAFETY: a 2 MiB buffer standing in for buses 4 and 5.
    let ecam = unsafe { Ecam::new(window.as_mut_ptr().expose_provenance(), 0, 4, 5) };
    let a = PciAddress {
        domain: 0,
        bus: 5,
        slot: 3,
        function: 0,
    };
    assert_eq!(ecam.read32(a, 0), 0x1234_5678);
    assert_eq!(ecam.read16(a, 2), 0x1234);
    assert_eq!(ecam.read8(a, 1), 0x56);
    assert_eq!(ecam.read32(PciAddress { bus: 6, ..a }, 0), u32::MAX);
    assert_eq!(ecam.read32(PciAddress { domain: 1, ..a }, 0), u32::MAX);
    assert_eq!(ecam.read32(a, 4096), u32::MAX);
    ecam.write32(PciAddress { bus: 3, ..a }, 0, 0);
}

#[test]
fn interrupts_swizzle_through_bridges() {
    use ::bus::Resource;
    use alloc::vec::Vec;
    let line = |hw| Resource::Irq {
        domain: &irq_test::NONE,
        hw,
        trigger: irq::Trigger::Level,
        polarity: irq::Polarity::Low,
    };
    // A bridge in slot 2 whose four pins reach lines 10..13.
    let parent: Vec<(u8, u8, Resource)> = (0..4).map(|p| (2, p, line(10 + u32::from(p)))).collect();
    let routes = crate::bridge::swizzle(&parent, 2);
    assert_eq!(routes.len(), 32 * 4);
    let hw = |slot: u8, pin: u8| {
        routes
            .iter()
            .find(|(s, p, _)| *s == slot && *p == pin)
            .map(|(_, _, r)| match r {
                Resource::Irq { hw, .. } => *hw,
                _ => unreachable!(),
            })
    };
    // Slot 0 INTA arrives at the bridge's INTA; slot 1 INTA at its INTB;
    // slot 3 INTD at INTC ((3 + 3) % 4 = 2).
    assert_eq!(hw(0, 0), Some(10));
    assert_eq!(hw(1, 0), Some(11));
    assert_eq!(hw(3, 3), Some(12));
    // A bridge with no routes of its own passes none on.
    assert!(crate::bridge::swizzle(&parent, 5).is_empty());
}

mod irq_test {
    /// A domain for resources that are never installed.
    pub(super) struct None;

    impl irq::IrqDomain for None {
        fn name(&self) -> &'static str {
            "none"
        }
        fn connect(
            &self,
            _: u32,
            _: irq::Trigger,
            _: irq::Polarity,
            _: hal::CpuId,
        ) -> Result<u32, irq::IrqError> {
            Err(irq::IrqError::NoSuchInput)
        }
        fn disconnect(&self, _: u32, _: hal::CpuId, _: u32) {}
        fn mask(&self, _: u32) {}
        fn unmask(&self, _: u32) {}
    }

    pub(super) static NONE: None = None;
}
