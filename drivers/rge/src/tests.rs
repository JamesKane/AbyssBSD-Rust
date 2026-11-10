// SPDX-License-Identifier: BSD-2-Clause
//! The bring-up sequences against a model of an RTL8126's register
//! protocol: the OCP and CSI windows' busy semantics, the self-clearing
//! reset, the PHY's patch handshake and indirect data port.

#![allow(
    clippy::cast_possible_truncation,
    reason = "the model's registers are read back at their own widths"
)]

extern crate std;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::cell::RefCell;

use crate::generated;
use crate::hw::{Chip, MacType, Regs};
use crate::regs::{self, mii};

/// What the model saw, in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ev {
    Mac(u16, u16),
    Phy(u16, u16),
    Csi(u32, u32),
}

#[derive(Default)]
struct State {
    bytes: BTreeMap<usize, u8>,
    mac: BTreeMap<u16, u16>,
    phy: BTreeMap<u16, u16>,
    /// The PHY's indirect space behind 0xa436 (address) and 0xa438 (data).
    ind: BTreeMap<u16, u16>,
    ind_addr: u16,
    csi: BTreeMap<u32, u32>,
    mac_latch: u16,
    phy_latch: u16,
    csi_latch: u32,
    log: Vec<Ev>,
}

/// An RTL8126's register protocol.
#[derive(Default)]
struct Model(RefCell<State>);

impl State {
    fn get(&self, off: usize, n: usize) -> u32 {
        (0..n).fold(0, |v, i| {
            v | u32::from(self.bytes.get(&(off + i)).copied().unwrap_or(0)) << (8 * i)
        })
    }

    fn put(&mut self, off: usize, n: usize, v: u32) {
        for (i, b) in v.to_le_bytes().into_iter().take(n).enumerate() {
            self.bytes.insert(off + i, b);
        }
    }

    fn phy_read(&self, reg: u16) -> u16 {
        match reg {
            // LAN on, whatever was asked.
            0xa420 => 3,
            // The patch handshake follows the request.
            0xb800 => {
                if self.phy.get(&0xb820).copied().unwrap_or(0) & 0x10 != 0 {
                    0x40
                } else {
                    0
                }
            }
            0xa438 => self.ind.get(&self.ind_addr).copied().unwrap_or(0),
            r => self.phy.get(&r).copied().unwrap_or(0),
        }
    }

    fn phy_write(&mut self, reg: u16, val: u16) {
        self.log.push(Ev::Phy(reg, val));
        match reg {
            0xa436 => self.ind_addr = val,
            0xa438 => {
                self.ind.insert(self.ind_addr, val);
                self.ind_addr = self.ind_addr.wrapping_add(1);
            }
            // BMCR's reset bit clears itself.
            0xa400 => {
                self.phy.insert(reg, val & !mii::BMCR_RESET);
            }
            r => {
                self.phy.insert(r, val);
            }
        }
    }
}

impl Regs for Model {
    fn read8(&self, off: usize) -> u8 {
        let s = self.0.borrow();
        match off {
            regs::MCUCMD => regs::MCUCMD_RXFIFO_EMPTY | regs::MCUCMD_TXFIFO_EMPTY,
            _ => s.get(off, 1) as u8,
        }
    }

    fn read16(&self, off: usize) -> u16 {
        let s = self.0.borrow();
        match off {
            regs::IM => 0x0103,
            regs::TWICMD => 0x0200,
            _ => s.get(off, 2) as u16,
        }
    }

    fn read32(&self, off: usize) -> u32 {
        let s = self.0.borrow();
        match off {
            regs::MACOCP => u32::from(s.mac.get(&s.mac_latch).copied().unwrap_or(0)),
            // A read's data is ready once the busy flag is set.
            regs::PHYOCP => regs::OCP_BUSY | u32::from(s.phy_read(s.phy_latch)),
            regs::CSIAR => regs::CSIAR_BUSY,
            regs::CSIDR => s.csi.get(&s.csi_latch).copied().unwrap_or(0),
            _ => s.get(off, 4),
        }
    }

    fn write8(&self, off: usize, v: u8) {
        let mut s = self.0.borrow_mut();
        // The soft reset completes at once.
        let v = if off == regs::CMD {
            v & !regs::CMD_RESET
        } else {
            v
        };
        s.put(off, 1, v.into());
    }

    fn write16(&self, off: usize, v: u16) {
        self.0.borrow_mut().put(off, 2, v.into());
    }

    fn write32(&self, off: usize, v: u32) {
        let mut s = self.0.borrow_mut();
        let addr = ((v >> regs::OCP_ADDR_SHIFT & 0x7fff) << 1) as u16;
        match off {
            regs::MACOCP if v & regs::OCP_BUSY != 0 => {
                s.mac.insert(addr, v as u16);
                s.log.push(Ev::Mac(addr, v as u16));
            }
            regs::MACOCP => s.mac_latch = addr,
            regs::PHYOCP if v & regs::OCP_BUSY != 0 => s.phy_write(addr, v as u16),
            regs::PHYOCP => s.phy_latch = addr,
            regs::CSIAR => {
                let reg = v & regs::CSIAR_ADDR_MASK;
                if v & regs::CSIAR_BUSY != 0 {
                    let d = s.get(regs::CSIDR, 4);
                    s.csi.insert(reg, d);
                    s.log.push(Ev::Csi(reg, d));
                } else {
                    s.csi_latch = reg;
                }
            }
            _ => s.put(off, 4, v),
        }
    }

    fn delay_us(&self, _: u32) {}
}

fn chip() -> Chip<Model> {
    Chip::new(Model::default(), MacType::R26_2)
}

fn log(c: &Chip<Model>) -> Vec<Ev> {
    core::mem::take(&mut c.regs.0.borrow_mut().log)
}

#[test]
fn revisions() {
    // The Orange Pi 6 Plus's, as FreeBSD printed it.
    assert_eq!(MacType::of(0x64a0_0000), Some(MacType::R26_2));
    assert_eq!(MacType::of(0x64a0_0000).unwrap().name(), "RTL8126_2");
    // Only the revision field counts.
    assert_eq!(MacType::of(0x6490_0f00), Some(MacType::R26_1));
    assert_eq!(MacType::of(0x1234_0000), None);
    assert!(MacType::R26_2.supported() && !MacType::R25B.supported());
}

/// MII registers sit eight to a page from PHY OCP 0xa400.
#[test]
fn mii_through_ocp() {
    let c = chip();
    c.write_mii(mii::BMCR, mii::BMCR_AUTOEN);
    c.write_mii(mii::ANAR, 0x01e1);
    c.write_mii(mii::GTCR, 0x0300);
    assert_eq!(
        log(&c),
        [
            Ev::Phy(0xa400, mii::BMCR_AUTOEN),
            Ev::Phy(0xa408, 0x01e1),
            Ev::Phy(0xa412, 0x0300),
        ]
    );
    assert_eq!(c.read_mii(mii::ANAR), 0x01e1);
}

/// The MAC microcode goes in 256 words to page 0 and the rest to page
/// 1, then its version to page 2; a chip already running it gets only
/// the breakpoints.
#[test]
fn mac_microcode() {
    let mut c = chip();
    c.chip_init().unwrap();
    let ev = log(&c);
    let bps = generated::R26_2_MAC_BPS;
    // The microcode window: page 0's words end where page 2 keeps the
    // version (0xf9f8-0xf9fe), which follows them.
    let written: Vec<(u16, u16)> = ev
        .iter()
        .filter_map(|e| match *e {
            Ev::Mac(r, v) if (0xf800..0xfa00).contains(&r) => Some((r, v)),
            _ => None,
        })
        .collect();
    assert_eq!(written[..bps.len()], *bps);
    assert_eq!(written.len(), bps.len() + 4, "then the version");
    // The page switches around them: 0, 1, then 2 for the version.
    let pages: Vec<u16> = ev
        .iter()
        .filter_map(|e| match *e {
            Ev::Mac(0xe446, v) => Some(v & 3),
            _ => None,
        })
        .collect();
    assert_eq!(pages, [2, 0, 0, 1, 2, 0]);
    let version = Chip::<Model>::bin_version(bps);
    let words: Vec<u16> = (0..4).map(|k| (version >> (16 * (3 - k))) as u16).collect();
    let tail: Vec<u16> = bps[bps.len() - 4..].iter().map(|p| p.1).collect();
    assert_eq!(words, tail);
    // The version reads back as loaded: no reload on the next init.
    c.regs.0.borrow_mut().mac.insert(0xe446, 0);
    for (k, w) in words.iter().enumerate() {
        c.regs.0.borrow_mut().mac.insert(0xf9f8 + 2 * k as u16, *w);
    }
    c.chip_init().unwrap();
    assert_eq!(c.mcode_version, version);
    assert!(
        !log(&c)
            .iter()
            .any(|e| matches!(e, Ev::Mac(r, _) if (0xf800..0xf9f8).contains(r))),
        "reloaded"
    );
}

/// The PHY microcode loads inside the patch window unless the PHY runs
/// its version, and the configuration program follows.
#[test]
fn phy_microcode_and_config() {
    let mut c = chip();
    c.phy_config().unwrap();
    assert_eq!(c.rcode_version, 0);
    let ev = log(&c);
    let mcu = generated::R26_2_PHY_MCU;
    // The patch request, the microcode, the release, the version.
    let at = ev
        .iter()
        .position(|e| *e == Ev::Phy(mcu[0].0, mcu[0].1))
        .unwrap();
    let body: Vec<(u16, u16)> = ev[at..at + mcu.len()]
        .iter()
        .map(|e| match *e {
            Ev::Phy(r, v) => (r, v),
            _ => panic!("{e:?}"),
        })
        .collect();
    assert_eq!(body, mcu);
    assert!(ev[..at].contains(&Ev::Phy(0xb820, 0x10)), "patch request");
    let s = c.regs.0.borrow();
    assert_eq!(s.ind.get(&0x801e), Some(&0x0060), "version recorded");
    drop(s);
    // Configured once, a second run skips the microcode.
    c.phy_config().unwrap();
    assert_eq!(c.rcode_version, 0x0060);
    assert!(
        !log(&c)
            .windows(2)
            .any(|w| w == [Ev::Phy(mcu[0].0, mcu[0].1), Ev::Phy(mcu[1].0, mcu[1].1)])
    );
    // EEE ends off: 0xe040's low bits clear.
    assert_eq!(c.read_mac_ocp(0xe040) & 3, 0);
}

#[test]
fn station_address() {
    let c = chip();
    {
        let mut s = c.regs.0.borrow_mut();
        s.put(regs::ADDR0, 4, 0xf92b_74c0);
        s.put(regs::ADDR1, 2, 0x3442);
    }
    // FreeBSD printed c0:74:2b:f9:42:34 for rge0.
    let a = c.get_macaddr();
    assert_eq!(a, [0xc0, 0x74, 0x2b, 0xf9, 0x42, 0x34]);
    let s = c.regs.0.borrow();
    assert_eq!(
        (s.get(regs::MAC0, 4), s.get(regs::MAC4, 4)),
        (0xf92b_74c0, 0x3442)
    );
    assert_eq!(
        s.get(regs::EECMD, 1) as u8 & regs::EECMD_WRITECFG,
        0,
        "locked again"
    );
}

#[test]
fn configure_and_start() {
    let c = chip();
    c.configure(0x1_2345_6000, 0x8000_0000, true);
    c.set_filter(false, crate::multicast_hash(&[[1, 0, 0x5e, 0, 0, 1]]));
    c.start();
    let s = c.regs.0.borrow();
    assert_eq!(
        (
            s.get(regs::RXDESC_ADDR_LO, 4),
            s.get(regs::RXDESC_ADDR_HI, 4)
        ),
        (0x2345_6000, 1)
    );
    assert_eq!(s.get(regs::TXDESC_ADDR_LO, 4), 0x8000_0000);
    let rx = s.get(regs::RXCFG, 4);
    assert_eq!(
        rx & regs::RXCFG_FILTERS,
        regs::RXCFG_INDIV | regs::RXCFG_BROAD | regs::RXCFG_MULTI
    );
    assert_eq!(rx & !regs::RXCFG_FILTERS, regs::RXCFG_CONFIG_8126);
    // 01:00:5e:00:00:01 hashes to bit 31: hash word 0, swapped into MAR4.
    assert_eq!(s.get(regs::MAR4, 4), (1u32 << 31).swap_bytes());
    assert_eq!(s.get(regs::MAR0, 4), 0);
    assert_eq!(s.get(regs::CMD, 1) as u8, regs::CMD_TXENB | regs::CMD_RXENB);
    assert_eq!(s.get(regs::IMR, 4), regs::INTRS);
    assert_eq!(
        s.get(regs::CPLUSCMD, 2) as u16 & regs::CPLUSCMD_RXCSUM,
        regs::CPLUSCMD_RXCSUM
    );
    assert_eq!(s.get(regs::EECMD, 1) as u8 & regs::EECMD_WRITECFG, 0);
    // Every speed advertised, negotiation restarted.
    assert_eq!(s.phy.get(&0xa5d4).copied().unwrap_or(0) & 0x0180, 0x0180);
    assert_eq!(
        s.phy.get(&0xa400).copied(),
        Some(mii::BMCR_AUTOEN | mii::BMCR_STARTNEG)
    );
    // The CSI writes init makes.
    assert!(s.log.contains(&Ev::Csi(0x70c, 0x2700_0000)));
}

#[test]
fn multicast_hash_bits() {
    let h = crate::multicast_hash(&[[0x33, 0x33, 0, 0, 0, 1], [0xff; 6], [1, 0, 0x5e, 0, 0, 1]]);
    assert_eq!(h, [1 << 31, 1 << 30 | 1 << 31]);
}

/// The factory address stands; a missing or multicast one is replaced by
/// a local unicast one.
#[test]
fn station_fallback() {
    use crate::driver::station;
    let board = [0xc0, 0x74, 0x2b, 0xf9, 0x42, 0x34];
    assert_eq!(station(board, 0), board);
    for bad in [[0; 6], [0x01, 0, 0x5e, 0, 0, 1]] {
        let a = station(bad, 1);
        assert_eq!(a[0] & 0x03, 0x02, "local unicast: {a:02x?}");
        assert_ne!(station(bad, 1), station(bad, 2));
    }
}
