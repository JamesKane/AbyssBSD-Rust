// SPDX-License-Identifier: BSD-2-Clause
//! The ACPI firmware corpus (`tests/corpus/acpi`) for host tests: each
//! machine's AML tables, loaded into an interpreter over a host seeded
//! with the firmware state the AML reads (as `lib/aml`'s corpus tests
//! establish it from Linux's view of each machine), and its CSRT's DMA
//! controllers. Used by `sys/acpi`, the platform crates, and
//! `kernel/table`'s whole-machine checks.

use std::collections::BTreeMap;
use std::ops::Range;
use std::vec::Vec;

use aml::host::{Host, HostError, RegionAccess, RegionSpace};
use aml::{Config, Interpreter, Limits};

/// Registers as sparse memory (unwritten bytes read as zero), with
/// index/data port pairs, seeded with the firmware state each machine's
/// AML reads (as `lib/aml`'s corpus tests establish it from Linux's view
/// of the machine).
#[derive(Debug, Default)]
pub struct Machine {
    mem: BTreeMap<(RegionSpace, u64), u8>,
    /// `(space, index port, data port)`.
    pairs: Vec<(RegionSpace, u64, u64)>,
}

impl Machine {
    fn poke(&mut self, space: RegionSpace, addr: u64, bytes: &[u8]) {
        for (k, &b) in (addr..).zip(bytes) {
            self.mem.insert((space, k), b);
        }
    }

    /// Where byte `i` of an access lives: behind a data port, the register
    /// its index port selects.
    fn slot(&self, a: &RegionAccess, i: u64) -> (RegionSpace, u64) {
        let addr = a.address();
        for &(space, index, data) in &self.pairs {
            if space == a.space && data == addr {
                let reg = self.mem.get(&(space, index)).copied().unwrap_or(0);
                return (space, 1 << 63 | data << 8 | u64::from(reg));
            }
        }
        (a.space, addr.wrapping_add(i))
    }

    /// The firmware state `name`'s AML reads.
    #[must_use]
    pub fn for_machine(name: &str) -> Self {
        let mut m = Self::default();
        match name {
            // GETV's switches at 0xFFFE0000: on, but for I2C6, PCIe port
            // 4, and three xHCI controllers.
            "opi6p" => {
                let mut g = [1u8; 0x2c];
                for k in [0x0b, 0x11, 0x1c, 0x1d, 0x1e] {
                    g[k] = 0;
                }
                m.poke(RegionSpace::SystemMemory, 0xfffe_0000, &g);
            }
            // AMI's setup variables, HPET's enable, the FCH controller
            // block (behind extended CMOS 0x08-0x0B), the interrupt
            // router.
            "x870e" => {
                const FRTB: u32 = 0x7000_0000;
                m.poke(RegionSpace::SystemMemory, 0x8fe0_d000, &[1, 0, 2, 1]);
                m.poke(RegionSpace::SystemMemory, 0xfed8_0300, &[0x40]);
                m.pairs.push((RegionSpace::SystemIo, 0x72, 0x73));
                for (k, b) in (8u64..).zip(FRTB.to_le_bytes()) {
                    m.mem
                        .insert((RegionSpace::SystemIo, 1 << 63 | 0x73 << 8 | k), b);
                }
                m.poke(RegionSpace::SystemMemory, u64::from(FRTB) + 4, &[0x60]);
                m.poke(RegionSpace::SystemMemory, u64::from(FRTB) + 0x1c, &[0x0e]);
                m.pairs.push((RegionSpace::SystemIo, 0xc00, 0xc01));
                for k in 0..8u64 {
                    m.mem
                        .insert((RegionSpace::SystemIo, 1 << 63 | 0xc01 << 8 | k), 0x0b);
                }
            }
            _ => {}
        }
        m
    }
}

impl Host for Machine {
    fn read(&mut self, a: &RegionAccess) -> Result<u64, HostError> {
        let mut v = 0;
        for i in 0..a.width.bytes() {
            let b = self.mem.get(&self.slot(a, i)).copied().unwrap_or(0);
            v |= u64::from(b) << (8 * i);
        }
        Ok(v)
    }

    fn write(&mut self, a: &RegionAccess, value: u64) -> Result<(), HostError> {
        for (i, &b) in (0..a.width.bytes()).zip(&value.to_le_bytes()) {
            let slot = self.slot(a, i);
            self.mem.insert(slot, b);
        }
        Ok(())
    }
}

macro_rules! corpus {
    ($path:literal) => {
        include_bytes!(concat!("../../../tests/corpus/acpi/", $path)).as_slice()
    };
}

/// Each machine's AML tables, in XSDT order.
pub fn machines() -> Vec<(&'static str, Vec<&'static [u8]>)> {
    vec![
        ("qemu-q35", vec![corpus!("qemu-q35/dsdt.dat")]),
        ("qemu-virt", vec![corpus!("qemu-virt/dsdt.dat")]),
        ("bhyve", vec![corpus!("bhyve/dsdt.dat")]),
        ("radxa-q8b", vec![corpus!("radxa-q8b/dsdt.dat")]),
        (
            "opi6p",
            vec![
                corpus!("opi6p/dsdt.dat"),
                corpus!("opi6p/ssdt1.dat"),
                corpus!("opi6p/ssdt2.dat"),
            ],
        ),
        (
            "x870e",
            vec![
                corpus!("x870e/dsdt.dat"),
                corpus!("x870e/ssdt1.dat"),
                corpus!("x870e/ssdt2.dat"),
                corpus!("x870e/ssdt3.dat"),
                corpus!("x870e/ssdt4.dat"),
                corpus!("x870e/ssdt5.dat"),
                corpus!("x870e/ssdt6.dat"),
                corpus!("x870e/ssdt7.dat"),
                corpus!("x870e/ssdt8.dat"),
                corpus!("x870e/ssdt9.dat"),
                corpus!("x870e/ssdt10.dat"),
                corpus!("x870e/ssdt11.dat"),
                corpus!("x870e/ssdt12.dat"),
                corpus!("x870e/ssdt13.dat"),
                corpus!("x870e/ssdt14.dat"),
                corpus!("x870e/ssdt15.dat"),
            ],
        ),
    ]
}

/// A machine's namespace, loaded with its firmware state in place, and
/// `_INI` run.
pub fn load_tables(name: &str, tables: &[&[u8]]) -> (Interpreter, Machine) {
    let mut i = Interpreter::new(Config {
        limits: Limits::for_stack(64 * 1024),
        ..Config::default()
    });
    let mut h = Machine::for_machine(name);
    for t in tables {
        let r = i.load_table(t, &mut h).unwrap();
        assert!(r.warnings.is_empty(), "{name}: {:?}", r.warnings);
    }
    let _ = i.initialize(&mut h);
    (i, h)
}

/// Machine `name`'s namespace, loaded with its firmware state in place.
///
/// # Panics
///
/// If there is no such machine, or a table does not load cleanly.
#[must_use]
pub fn load(name: &str) -> (Interpreter, Machine) {
    let (_, tables) = machines()
        .into_iter()
        .find(|(n, _)| *n == name)
        .unwrap_or_else(|| panic!("no corpus machine {name}"));
    load_tables(name, &tables)
}

/// A table's bytes as the memory it is read from, at address 0.
struct Table(&'static [u8]);

impl acpi_tables::PhysMem<'static> for &Table {
    fn bytes(&self, pa: u64, len: usize) -> Option<&'static [u8]> {
        let start = usize::try_from(pa).ok()?;
        self.0.get(start..start.checked_add(len)?)
    }
}

/// The DMA controllers a machine's CSRT describes, as the kernel reads
/// them: each one's MMIO base and request lines.
pub fn dma_controllers(name: &str) -> Vec<(u64, Range<u16>)> {
    let csrt = match name {
        "opi6p" => corpus!("opi6p/csrt.dat"),
        _ => return Vec::new(),
    };
    let t = acpi_tables::Sdt::read(&&Table(csrt), 0).unwrap();
    acpi_tables::csrt::parse(&t)
        .unwrap()
        .groups()
        .filter_map(|g| g.ok()?.dma_controller())
        .map(|c| {
            let end = c.base_request_line.saturating_add(c.request_lines);
            (c.mmio_base, c.base_request_line..end)
        })
        .collect()
}
