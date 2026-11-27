// SPDX-License-Identifier: BSD-2-Clause
//! The programming against a model of a BCM57765-family chip: the chip
//! ID, the reset and the bootcode's handshake, `bge_chipinit`,
//! `bge_blockinit`, the station address, MII and the PHY, the rings and
//! the status block. The model's identity is the Mac Pro (Late 2013)'s, as
//! FreeBSD 16 saw it (the fork's capture, 2026-11-27): device 0x1682, CHIP
//! ID 0x57766000, PHY OUI 0x001be9 model 0x24 rev 0.

#![allow(
    clippy::cast_possible_truncation,
    reason = "the model's registers are read back at their own widths"
)]

extern crate std;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::vec::Vec;

use crate::hw::{Chip, HwError, LinkMode, Regs, Rings, multicast_hash};
use crate::phy::{self, Mii, Phy, Speed, Status};
use crate::regs as r;
use crate::ring::{self, Barriers, ReturnRing, StdRing, TxRing, Words};

const EXPCAP: u16 = 0xac;

/// The chip.
struct Model {
    bar: RefCell<BTreeMap<usize, u32>>,
    cfg: RefCell<BTreeMap<u16, u32>>,
    sram: RefCell<BTreeMap<u32, u32>>,
    phy: RefCell<BTreeMap<u8, u16>>,
    eeprom: Option<Vec<u8>>,
    /// Whether the bootcode runs after a reset (answers the handshake).
    bootcode: bool,
    /// Writes to the MAC, in order, for checking sequences.
    log: RefCell<Vec<(usize, u32)>>,
    saves: RefCell<u32>,
    restores: RefCell<u32>,
}

impl Model {
    fn new(chipid: u32, mps_code: u32) -> Self {
        let mut cfg = BTreeMap::new();
        // The legacy ID says to read the product ID register.
        cfg.insert(r::CFG_MISC_CTL, 0xf000_0000);
        cfg.insert(r::CFG_GEN15_PRODID_ASICREV, chipid);
        cfg.insert(EXPCAP + 4, mps_code);
        let mut phy = BTreeMap::new();
        // 10/100 abilities, extended status, autonegotiation done, link.
        phy.insert(phy::reg::BMSR, 0x796d);
        phy.insert(phy::reg::PHYIDR1, 0x0362);
        phy.insert(phy::reg::PHYIDR2, 0x5e40);
        // AUXSTS: link, 1000 full duplex.
        phy.insert(phy::reg::AUXSTS, 0x0704);
        Self {
            bar: RefCell::new(BTreeMap::new()),
            cfg: RefCell::new(cfg),
            sram: RefCell::new(BTreeMap::new()),
            phy: RefCell::new(phy),
            eeprom: None,
            bootcode: true,
            log: RefCell::new(Vec::new()),
            saves: RefCell::new(0),
            restores: RefCell::new(0),
        }
    }

    /// The Mac Pro's 57762.
    fn mac_pro() -> Self {
        let m = Self::new(0x5776_6000, 1);
        // The bootcode's address (a made-up one) and configuration.
        m.sram.borrow_mut().insert(r::SRAM_MAC_HIGH, 0x484b_0200);
        m.sram.borrow_mut().insert(r::SRAM_MAC_LOW, 0x5e10_2030);
        m.sram.borrow_mut().insert(r::SRAM_DATA_SIG, r::FW_MB_MAGIC);
        m.sram.borrow_mut().insert(r::SRAM_DATA_CFG, 0x0000_0010);
        m
    }

    fn sram(&self, a: u32) -> u32 {
        self.sram.borrow().get(&a).copied().unwrap_or(0)
    }

    fn bar(&self, off: usize) -> u32 {
        self.bar.borrow().get(&off).copied().unwrap_or(0)
    }

    fn written(&self, off: usize) -> Vec<u32> {
        self.log
            .borrow()
            .iter()
            .filter(|(o, _)| *o == off)
            .map(|(_, v)| *v)
            .collect()
    }

    fn mii(&self, v: u32) -> u32 {
        let reg = ((v >> 16) & 0x1f) as u8;
        let mut phy = self.phy.borrow_mut();
        if v & r::mi::WRITE != 0 {
            let mut d = v as u16;
            // A BMCR reset completes at once.
            if reg == phy::reg::BMCR {
                d &= !phy::bmcr::RESET;
            }
            phy.insert(reg, d);
            v & !r::mi::BUSY
        } else {
            let d = phy.get(&reg).copied().unwrap_or(0);
            (v & !(r::mi::BUSY | 0xffff)) | u32::from(d)
        }
    }
}

impl Regs for &Model {
    fn read32(&self, off: usize) -> u32 {
        if (r::MEMWIN..r::MEMWIN + 0x8000).contains(&off) {
            let base = self
                .cfg
                .borrow()
                .get(&r::CFG_MEMWIN_BASE)
                .copied()
                .unwrap_or(0);
            return self.sram(base + (off - r::MEMWIN) as u32);
        }
        if off < 0x100 {
            return self.cfg_read32(off as u16);
        }
        self.bar(off)
    }

    fn write32(&self, off: usize, v: u32) {
        self.log.borrow_mut().push((off, v));
        if (r::MEMWIN..r::MEMWIN + 0x8000).contains(&off) {
            let base = self
                .cfg
                .borrow()
                .get(&r::CFG_MEMWIN_BASE)
                .copied()
                .unwrap_or(0);
            self.sram
                .borrow_mut()
                .insert(base + (off - r::MEMWIN) as u32, v);
            return;
        }
        if off < 0x100 {
            self.cfg.borrow_mut().insert(off as u16, v);
            return;
        }
        let v = match off {
            r::MISC_CFG if v & 1 != 0 => {
                // The core reset: the bootcode answers the handshake.
                if self.bootcode && self.sram(r::SRAM_FW_MB) == r::FW_MB_MAGIC {
                    self.sram
                        .borrow_mut()
                        .insert(r::SRAM_FW_MB, !r::FW_MB_MAGIC);
                }
                v & !1
            }
            r::FTQ_RESET => 0,
            r::MI_COMM => self.mii(v),
            r::EE_ADDR if v & 0x8000_0000 != 0 => {
                let a = (v & 0xffff) as usize;
                if let Some(ee) = &self.eeprom {
                    let word = a & !3;
                    let d = (0..4).fold(0u32, |d, i| {
                        d | u32::from(ee.get(word + i).copied().unwrap_or(0)) << (8 * i)
                    });
                    self.bar.borrow_mut().insert(r::EE_DATA, d);
                    v | r::ee::DONE
                } else {
                    v & !r::ee::DONE
                }
            }
            _ => v,
        };
        self.bar.borrow_mut().insert(off, v);
    }

    fn cfg_read32(&self, off: u16) -> u32 {
        if off == r::CFG_MEMWIN_DATA {
            let base = self
                .cfg
                .borrow()
                .get(&r::CFG_MEMWIN_BASE)
                .copied()
                .unwrap_or(0);
            return self.sram(base);
        }
        self.cfg.borrow().get(&off).copied().unwrap_or(0)
    }

    fn cfg_write32(&self, off: u16, v: u32) {
        if off == r::CFG_MEMWIN_DATA {
            let base = self
                .cfg
                .borrow()
                .get(&r::CFG_MEMWIN_BASE)
                .copied()
                .unwrap_or(0);
            self.sram.borrow_mut().insert(base, v);
            return;
        }
        // MISC_CTL keeps its chip ID half.
        let v = if off == r::CFG_MISC_CTL {
            (v & 0xffff) | 0xf000_0000
        } else {
            v
        };
        self.cfg.borrow_mut().insert(off, v);
    }

    fn cfg_read16(&self, off: u16) -> u16 {
        self.cfg_read32(off) as u16
    }

    fn cfg_write16(&self, off: u16, v: u16) {
        self.cfg.borrow_mut().insert(off, u32::from(v));
    }

    fn save_config(&self) {
        *self.saves.borrow_mut() += 1;
    }

    fn restore_config(&self) {
        *self.restores.borrow_mut() += 1;
    }

    fn delay_us(&self, _: u32) {}
}

struct ModelMii<'a>(&'a Chip<&'a Model>);

impl Mii for ModelMii<'_> {
    fn read(&self, reg: u8) -> Option<u16> {
        self.0.mii_read(reg).ok()
    }
    fn write(&self, reg: u8, v: u16) -> Option<()> {
        self.0.mii_write(reg, v).ok()
    }
    fn delay_us(&self, _: u32) {}
}

#[test]
fn the_mac_pros_chip_is_a_57766() {
    let m = Model::mac_pro();
    let chip = Chip::new(&m, EXPCAP).unwrap();
    assert_eq!(chip.id.0, 0x5776_6000);
    assert_eq!(chip.id.asicrev(), r::ASICREV_57766);
    assert_eq!(chip.id.name(), "BCM57766");
    assert_eq!(chip.mps, 256);
    // Another family is refused.
    let other = Model::new(0x5717_0000, 0);
    assert_eq!(
        Chip::new(&other, EXPCAP).err(),
        Some(HwError::NotSupported(0x5717_0000))
    );
}

#[test]
fn the_reset_hands_over_to_the_bootcode_and_back() {
    let m = Model::mac_pro();
    let chip = Chip::new(&m, EXPCAP).unwrap();
    assert_eq!(chip.reset(), Ok(()));
    assert_eq!(m.sram(r::SRAM_FW_MB), !r::FW_MB_MAGIC);
    assert_eq!(
        m.written(r::MISC_CFG),
        [r::MISC_CFG_NO_LINK_TRAINING, r::MISC_CFG_RESET]
    );
    assert_eq!(
        (&m).cfg_read32(r::CFG_MISC_CTL) & 0xffff,
        r::misc_ctl::RESET
    );
    assert_eq!((&m).cfg_read32(r::CFG_PCISTATE), r::PCISTATE_ROM);
    assert_eq!((*m.saves.borrow(), *m.restores.borrow()), (1, 1));
    assert_eq!(m.written(r::MODE_CTL), [r::MODE_CTL_SWAP]);
    // Relaxed ordering and no-snoop off, 4096-byte reads.
    assert_eq!((&m).cfg_read16(EXPCAP + 8) & 0x7810, 0x5000);

    let mut quiet = Model::mac_pro();
    quiet.bootcode = false;
    let chip = Chip::new(&quiet, EXPCAP).unwrap();
    assert_eq!(chip.reset(), Err("firmware handshake timed out"));
}

#[test]
fn chip_init_programs_the_57766_path() {
    let m = Model::mac_pro();
    m.sram.borrow_mut().insert(0x400, 0xdead_beef);
    let chip = Chip::new(&m, EXPCAP).unwrap();
    chip.chip_init(true);
    assert_eq!(
        (&m).cfg_read32(r::CFG_MISC_CTL) & 0xffff,
        r::misc_ctl::INIT | r::misc_ctl::TAGGED_STATUS
    );
    assert_eq!(m.sram(0x400), 0, "the statistics block zeroed");
    // Not a 57765 A-stepping: the PCIe training fix.
    assert_eq!(m.bar(r::CPMU_PADRNG_CTL), r::CPMU_PADRNG_RDIV2);
    assert_eq!((&m).cfg_read32(r::CFG_DMA_RW_CTL), r::DMA_RW_CTL_MPS256);
    assert_eq!(m.bar(r::MODE_CTL), 0x0412_0034);
    assert_eq!(m.bar(r::MISC_CFG), r::MISC_CFG_66MHZ);

    // A 57765 A0 takes neither the fix nor (MPS 128) the bigger watermark.
    let a0 = Model::new(r::CHIPID_57765_A0, 0);
    let chip = Chip::new(&a0, EXPCAP).unwrap();
    chip.chip_init(false);
    assert_eq!(a0.bar(r::CPMU_PADRNG_CTL), 0);
    assert_eq!((&a0).cfg_read32(r::CFG_DMA_RW_CTL), r::DMA_RW_CTL_MPS128);
}

#[test]
fn the_station_address_comes_from_the_bootcode_then_the_eeprom() {
    let m = Model::mac_pro();
    let chip = Chip::new(&m, EXPCAP).unwrap();
    assert_eq!(
        chip.station_address(),
        Ok([0x02, 0x00, 0x5e, 0x10, 0x20, 0x30])
    );
    let mut ee = Model::new(0x5776_6000, 1);
    let mut rom = std::vec![0u8; 0x100];
    rom[0x7e..0x84].copy_from_slice(&[0x00, 0x11, 0x22, 0x33, 0x44, 0x55]);
    ee.eeprom = Some(rom);
    let chip = Chip::new(&ee, EXPCAP).unwrap();
    assert_eq!(
        chip.station_address(),
        Ok([0x00, 0x11, 0x22, 0x33, 0x44, 0x55])
    );
    let none = Model::new(0x5776_6000, 1);
    let chip = Chip::new(&none, EXPCAP).unwrap();
    assert_eq!(chip.station_address(), Err(HwError::NoAddress));
}

#[test]
fn a_failed_config_read_is_fatal_only_on_a_57765() {
    let m = Model::mac_pro();
    let chip = Chip::new(&m, EXPCAP).unwrap();
    let cfg = chip.hw_config().unwrap();
    assert!(!cfg.asf() && !cfg.fiber());
    let bare = Model::new(0x5776_6000, 1);
    assert_eq!(
        Chip::new(&bare, EXPCAP)
            .unwrap()
            .hw_config()
            .map(|c| c.word),
        Ok(0)
    );
    let bare = Model::new(0x5778_5100, 1);
    assert_eq!(
        Chip::new(&bare, EXPCAP).unwrap().hw_config(),
        Err(HwError::NoHwConfig)
    );
}

#[test]
fn block_init_programs_the_rings_and_blocks() {
    let m = Model::mac_pro();
    let chip = Chip::new(&m, EXPCAP).unwrap();
    let rings = Rings {
        std_rx: 0x1_2340_0000,
        jumbo_rx: 0,
        rx_return: 0x5600_0000,
        tx: 0x7800_0000,
        status: 0x9a00_0000,
    };
    m.bar.borrow_mut().insert(r::BMAN_MODE, 0);
    chip.block_init(&rings, [0x02, 0x00, 0x5e, 0x10, 0x20, 0x30])
        .unwrap();
    assert_eq!(m.bar(r::RX_STD_RCB), 0x1);
    assert_eq!(m.bar(r::RX_STD_RCB + 4), 0x2340_0000);
    assert_eq!(m.bar(r::RX_STD_RCB + 8), 0x0200_1800);
    assert_eq!(m.bar(r::RX_STD_RCB + 12), r::NIC_STD_RX);
    assert_eq!(m.bar(r::RX_JUMBO_RCB + 8), 0x3, "jumbo ring disabled");
    // Send ring 0 enabled, ring 1 disabled.
    assert_eq!(m.sram(r::SRAM_SEND_RCB + 4), 0x7800_0000);
    assert_eq!(m.sram(r::SRAM_SEND_RCB + 8), 0x0200_0000);
    assert_eq!(m.sram(r::SRAM_SEND_RCB + 12), r::NIC_SEND_RING);
    assert_eq!(m.sram(r::SRAM_SEND_RCB + 16 + 8), r::RCB_DISABLED);
    // Return ring 0 enabled, rings 1-3 disabled.
    assert_eq!(m.sram(r::SRAM_RETURN_RCB + 4), 0x5600_0000);
    assert_eq!(m.sram(r::SRAM_RETURN_RCB + 8), 0x0400_0000);
    for i in 1..4 {
        assert_eq!(m.sram(r::SRAM_RETURN_RCB + 16 * i + 8), r::RCB_DISABLED);
    }
    assert_eq!(m.bar(r::HCC_STATUSBLK_ADDR + 4), 0x9a00_0000);
    assert_eq!(m.bar(r::HCC_MODE), 0x102);
    assert_eq!(m.bar(r::MAC_MODE), 0x00e0_d804);
    assert_eq!(m.bar(r::WDMA_MODE), 0x2000_03fe);
    assert_eq!(m.bar(r::RDMA_MODE), r::RDMA_INIT);
    assert_eq!(m.bar(r::TX_LENGTHS), 0x2620);
    assert_eq!(
        m.bar(r::TX_RANDOM_BACKOFF),
        (0x02 + 0x5e + 0x10 + 0x20 + 0x30) & 0x3ff
    );
    assert_eq!(
        m.bar(r::MAC_EVT_ENB) & r::EVT_LINK_CHANGED,
        r::EVT_LINK_CHANGED
    );
    assert_eq!(m.written(r::MAC_STS), [r::MAC_STS_ACK, r::MAC_STS_ACK]);
}

#[test]
fn the_phy_is_the_bcm57765s_and_negotiates() {
    let m = Model::mac_pro();
    let chip = Chip::new(&m, EXPCAP).unwrap();
    let mii = ModelMii(&chip);
    let p = Phy::attach(&mii).unwrap();
    assert_eq!(p.id.oui, phy::OUI_BROADCOM3);
    assert_eq!(p.id.model, phy::MODEL_BCM57765);
    assert_eq!(p.id.rev, 0);
    p.autonegotiate(&mii, false).unwrap();
    let ph = m.phy.borrow();
    // 10/100 abilities and CSMA, no pause.
    assert_eq!(ph[&phy::reg::ANAR], 0x01e1);
    assert_eq!(ph[&phy::reg::GTCR], 0x0300);
    assert_eq!(ph[&phy::reg::BMCR], phy::bmcr::AUTOEN | phy::bmcr::STARTNEG);
    assert_eq!(ph[&phy::reg::IMR], 0xff00);
    drop(ph);
    assert_eq!(
        p.status(&mii),
        Ok(Status::Up {
            speed: Speed::Thousand,
            full_duplex: true,
            tx_pause: false,
            rx_pause: false,
            master: false,
        })
    );
    // Negotiating: down.
    m.phy.borrow_mut().insert(phy::reg::BMSR, 0x794d);
    assert_eq!(p.status(&mii), Ok(Status::Down));
    // 100 half duplex.
    m.phy.borrow_mut().insert(phy::reg::BMSR, 0x796d);
    m.phy.borrow_mut().insert(phy::reg::AUXSTS, 0x0304);
    assert!(matches!(
        p.status(&mii),
        Ok(Status::Up {
            speed: Speed::Hundred,
            full_duplex: false,
            ..
        })
    ));
    // A bus with nothing on it.
    m.phy.borrow_mut().insert(phy::reg::BMSR, 0xffff);
    assert!(Phy::attach(&mii).is_err());
}

#[test]
fn the_link_sets_the_mac_port_mode_and_duplex() {
    let m = Model::mac_pro();
    let chip = Chip::new(&m, EXPCAP).unwrap();
    m.bar.borrow_mut().insert(r::MAC_MODE, r::mac_mode::INIT);
    chip.set_link(LinkMode {
        gigabit: true,
        full_duplex: true,
        tx_pause: false,
        rx_pause: false,
    });
    assert_eq!(m.bar(r::MAC_MODE) & 0xe, r::mac_mode::PORTMODE_GMII);
    chip.set_link(LinkMode {
        gigabit: false,
        full_duplex: false,
        tx_pause: true,
        rx_pause: true,
    });
    assert_eq!(
        m.bar(r::MAC_MODE) & 0xe,
        r::mac_mode::PORTMODE_MII | r::mac_mode::HALF_DUPLEX
    );
    assert_eq!(
        m.bar(r::TX_MODE) & r::txrx::TX_FLOWCTL,
        0,
        "no pause at half duplex"
    );
}

#[test]
fn interrupts_ack_with_the_status_tag() {
    let m = Model::mac_pro();
    let chip = Chip::new(&m, EXPCAP).unwrap();
    chip.enable_interrupts();
    assert_eq!(
        (&m).cfg_read32(r::CFG_MISC_CTL) & 0x3,
        r::misc_ctl::CLEAR_INTA
    );
    chip.ack(0x5a);
    chip.disable_interrupts();
    assert_eq!(m.written(r::MBX_IRQ0), [0, 0x5a00_0000, 1]);
}

#[test]
fn ether_crc32_le_matches_freebsds() {
    // `ether_crc32_le` has no final inversion: the CRC-32 of "123456789",
    // 0xcbf43926, inverted.
    assert_eq!(crate::hw::ether_crc32_le(b"123456789"), !0xcbf4_3926);
    let h = multicast_hash(&[[0x01, 0x00, 0x5e, 0x00, 0x00, 0x01]]);
    assert_eq!(h.iter().map(|w| w.count_ones()).sum::<u32>(), 1);
    assert_eq!(
        multicast_hash(&[[0x01, 0x00, 0x5e, 0x00, 0x00, 0x01]]),
        h,
        "the same address, the same bit"
    );
}

/// A ring in plain memory.
struct Mem(RefCell<Vec<u32>>);

impl Words for Mem {
    fn get(&self, w: usize) -> u32 {
        self.0.borrow()[w]
    }
    fn set(&self, w: usize, v: u32) {
        self.0.borrow_mut()[w] = v;
    }
}

struct NoBarriers;

impl Barriers for NoBarriers {
    fn write(&self) {}
    fn read(&self) {}
}

fn mem(words: usize) -> Mem {
    Mem(RefCell::new(std::vec![0; words]))
}

#[test]
fn producer_descriptors_name_their_slots() {
    let ring = mem(ring::STD_RX * ring::RX_WORDS);
    let mut std = StdRing::default();
    let prod = std.fill(&ring, &NoBarriers, |s| {
        0x1_0000_0000 + (s * ring::SLOT) as u64
    });
    assert_eq!(prod, 511, "one behind, as bge_init_rx_ring_std");
    assert_eq!(ring.get(0), 1);
    assert_eq!(ring.get(1), ring::RX_ALIGN as u32);
    assert_eq!(ring.get(2), ring::RX_LEN as u32, "slot 0, 2046 bytes");
    assert_eq!(ring.get(3) & 0xffff, u32::from(ring::rx_flags::END));
    assert_eq!(ring.get(ring::RX_WORDS + 2) >> 16, 1, "slot 1");
    std.repost(&ring, 7, 0x2000);
    assert_eq!(std.producer(), 0);
    assert_eq!(ring.get(2) >> 16, 7, "slot 7 posted at position 0");
}

#[test]
fn the_return_ring_wraps_and_decodes() {
    let ring = mem(ring::RETURN * ring::RX_WORDS);
    let put = |pos: usize, slot: u32, len: u32, flags: u32| {
        ring.set(pos * ring::RX_WORDS + 2, slot << 16 | len);
        ring.set(pos * ring::RX_WORDS + 3, flags);
        ring.set(pos * ring::RX_WORDS + 4, 0xffff);
    };
    put(
        1023,
        3,
        64,
        u32::from(ring::rx_flags::IP_CSUM | ring::rx_flags::TCP_UDP_CSUM),
    );
    put(0, 4, 1518, u32::from(ring::rx_flags::ERROR));
    let mut ret = ReturnRing { cons: 1023 };
    let got: Vec<_> = ret.take(&ring, 1, 16).collect();
    assert_eq!(got.len(), 2);
    assert_eq!((got[0].slot, got[0].len), (3, 64));
    assert_eq!(
        got[0].csum(),
        ring::RxCsum {
            ip_checked: true,
            ip_valid: true,
            l4: Some(0xffff)
        }
    );
    assert!(got[1].flags & ring::rx_flags::ERROR != 0);
    assert_eq!(ret.cons, 1);
    // An index past the ring is ignored.
    assert_eq!(ret.take(&ring, 5000, 16).count(), 0);
}

#[test]
fn the_send_ring_reaps_to_the_chips_consumer() {
    let descs = mem(ring::TX * ring::TX_WORDS);
    let mut tx = TxRing::default();
    for _ in 0..3 {
        let p = tx.push();
        ring::post_tx(&descs, p, 0x8000, 60, ring::tx_flags::END);
    }
    assert_eq!(descs.get(2), 60 << 16 | u32::from(ring::tx_flags::END));
    assert_eq!(tx.reap(2), 2);
    assert_eq!(tx.in_flight, 1);
    assert_eq!(tx.reap(9999), 0, "out of range");
    for _ in 0..ring::TX - ring::TX_RESERVE - 1 {
        tx.push();
    }
    assert!(!tx.has_room());
}

#[test]
fn the_status_block_is_read_and_cleared() {
    let block = mem(ring::STATUS_WORDS);
    block.set(0, ring::status::UPDATED | ring::status::LINKSTATE_CHANGED);
    block.set(1, 0x37);
    block.set(4, 12 << 16 | 900);
    let st = ring::take_status(&block, &NoBarriers);
    assert_eq!(st.tag, 0x37);
    assert_eq!((st.rx_prod, st.tx_cons), (900, 12));
    assert!(st.flags & ring::status::LINKSTATE_CHANGED != 0);
    assert_eq!(block.get(0), 0);
}
