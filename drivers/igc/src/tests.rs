// SPDX-License-Identifier: BSD-2-Clause
//! The hardware layer against a model of the controller's register
//! protocol, and the rings against a simulated controller: reset and the
//! station address, the PHY's management interface (through the
//! semaphores, and the MMD registers), auto-negotiation to a 2.5 Gb/s
//! link, the filter, the queues, and descriptors round the ring.

#![allow(
    clippy::cast_possible_truncation,
    reason = "the model's registers are read back at their own widths"
)]

extern crate std;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::cell::RefCell;
use std::rc::Rc;

use crate::hw::{Chip, HwError, LinkState, Regs, Speed};
use crate::regs as r;
use crate::ring::{
    Assembled, Assembler, DESCS, Full, MAX_FRAME, RxCsum, RxRing, SLOT, TxRing, WORDS, Words,
};

const MAC: [u8; 6] = [0xd8, 0xbb, 0xc1, 0x6c, 0x63, 0x52];

/// The controller's registers and its PHY.
#[derive(Default)]
struct Model {
    regs: RefCell<BTreeMap<usize, u32>>,
    phy: RefCell<BTreeMap<u32, u16>>,
    mmd: RefCell<BTreeMap<(u16, u16), u16>>,
    /// The MMD access registers' state: the device and the address.
    mmd_dev: RefCell<u16>,
    mmd_addr: RefCell<u16>,
    mmd_data_mode: RefCell<bool>,
    /// The software semaphore, held.
    smbi: RefCell<bool>,
    /// Firmware holds the PHY: its bit in `SW_FW_SYNC`.
    fw_holds_phy: RefCell<bool>,
    /// Every MDIC access answers with the error bit.
    mdic_error: RefCell<bool>,
    /// All registers read all ones.
    gone: RefCell<bool>,
    /// The link partner can do 2.5 Gb/s.
    link_up: RefCell<bool>,
    /// Writes, in order.
    writes: RefCell<Vec<(usize, u32)>>,
    /// MDIC accesses: (read?, register, data).
    mdic_log: RefCell<Vec<(bool, u32, u16)>>,
}

#[derive(Clone)]
struct M(Rc<Model>);

impl M {
    fn new() -> Self {
        let m = Model::default();
        // The PHY's defaults after reset: auto-negotiation capable.
        m.phy
            .borrow_mut()
            .insert(r::PHY_CONTROL, r::MII_CR_AUTO_NEG_EN);
        Self(Rc::new(m))
    }

    fn get(&self, off: usize) -> u32 {
        self.0.regs.borrow().get(&off).copied().unwrap_or(0)
    }

    fn put(&self, off: usize, v: u32) {
        self.0.regs.borrow_mut().insert(off, v);
    }

    fn phy_reg(&self, reg: u32) -> u16 {
        self.0.phy.borrow().get(&reg).copied().unwrap_or(0)
    }

    /// A reset: the NVM's settings loaded, the queues and controls cleared.
    fn device_reset(&self) {
        let keep: Vec<(usize, u32)> = Vec::new();
        self.0.regs.borrow_mut().clear();
        for (k, v) in keep {
            self.put(k, v);
        }
        let lo = u32::from_le_bytes([MAC[0], MAC[1], MAC[2], MAC[3]]);
        let hi = u32::from(MAC[4]) | u32::from(MAC[5]) << 8 | r::RAH_AV;
        self.put(r::ral(0), lo);
        self.put(r::rah(0), hi);
        self.put(r::EECD, r::EECD_AUTO_RD);
        *self.0.link_up.borrow_mut() = false;
    }

    fn mdic(&self, v: u32) {
        let reg = (v & r::MDIC_REG_MASK) >> r::MDIC_REG_SHIFT;
        let data = (v & r::MDIC_DATA_MASK) as u16;
        let read = v & r::MDIC_OP_READ != 0;
        // The access needs the semaphore.
        let sync = self.get(r::SW_FW_SYNC);
        assert!(
            sync & r::SWFW_PHY0_SM != 0,
            "MDIC without the PHY semaphore"
        );
        if *self.0.mdic_error.borrow() {
            self.put(r::MDIC, v | r::MDIC_READY | r::MDIC_ERROR);
            return;
        }
        self.0.mdic_log.borrow_mut().push((read, reg, data));
        let result = if read {
            match reg {
                r::MMDAAD if *self.0.mmd_data_mode.borrow() => self
                    .0
                    .mmd
                    .borrow()
                    .get(&(*self.0.mmd_dev.borrow(), *self.0.mmd_addr.borrow()))
                    .copied()
                    .unwrap_or(0),
                r::PHY_STATUS => {
                    let mut s = r::MII_SR_AUTONEG_COMPLETE;
                    if *self.0.link_up.borrow() {
                        s |= r::MII_SR_LINK_STATUS;
                    }
                    s
                }
                _ => self.phy_reg(reg),
            }
        } else {
            match reg {
                r::MMDAC => {
                    *self.0.mmd_dev.borrow_mut() = data & 0x1f;
                    *self.0.mmd_data_mode.borrow_mut() = data & r::MMDAC_FUNC_DATA != 0;
                }
                r::MMDAAD => {
                    if *self.0.mmd_data_mode.borrow() {
                        self.0
                            .mmd
                            .borrow_mut()
                            .insert((*self.0.mmd_dev.borrow(), *self.0.mmd_addr.borrow()), data);
                    } else {
                        *self.0.mmd_addr.borrow_mut() = data;
                    }
                }
                r::PHY_CONTROL => {
                    self.0.phy.borrow_mut().insert(reg, data);
                    // A restart with link up forced in the MAC: the link
                    // comes up at the best speed both sides advertise.
                    if data & r::MII_CR_RESTART_AUTO_NEG != 0
                        && self.get(r::CTRL) & r::CTRL_SLU != 0
                    {
                        *self.0.link_up.borrow_mut() = true;
                    }
                }
                _ => {
                    self.0.phy.borrow_mut().insert(reg, data);
                }
            }
            data
        };
        self.put(
            r::MDIC,
            r::MDIC_READY | reg << r::MDIC_REG_SHIFT | u32::from(result),
        );
    }
}

impl Regs for M {
    fn read32(&self, off: usize) -> u32 {
        if *self.0.gone.borrow() {
            return u32::MAX;
        }
        match off {
            r::STATUS => {
                if *self.0.link_up.borrow() {
                    r::STATUS_LU | r::STATUS_FD | r::STATUS_SPEED_1000 | r::STATUS_SPEED_2500
                } else {
                    0
                }
            }
            r::SWSM => {
                // The software semaphore bit is set by the read that gets it.
                let mut v = self.get(r::SWSM) & !r::SWSM_SMBI;
                let mut held = self.0.smbi.borrow_mut();
                if *held {
                    v |= r::SWSM_SMBI;
                } else {
                    *held = true;
                }
                v
            }
            r::ICR => {
                let v = self.get(r::ICR);
                self.put(r::ICR, 0);
                v
            }
            _ => self.get(off),
        }
    }

    fn write32(&self, off: usize, v: u32) {
        self.0.writes.borrow_mut().push((off, v));
        match off {
            r::CTRL => {
                self.put(off, v);
                if v & r::CTRL_DEV_RST != 0 {
                    self.device_reset();
                    self.put(r::CTRL, v & !r::CTRL_DEV_RST);
                }
                if v & r::CTRL_PHY_RST != 0 {
                    self.put(r::PHPM, r::PHPM_RST_COMPL);
                }
            }
            r::SWSM => {
                if v & r::SWSM_SMBI == 0 {
                    *self.0.smbi.borrow_mut() = false;
                }
                self.put(off, v);
            }
            r::SW_FW_SYNC => {
                let fw = if *self.0.fw_holds_phy.borrow() {
                    r::SWFW_PHY0_SM << 16
                } else {
                    0
                };
                self.put(off, v | fw);
            }
            r::MDIC => self.mdic(v),
            r::IMS => self.put(r::IMS, self.get(r::IMS) | v),
            r::IMC => self.put(r::IMS, self.get(r::IMS) & !v),
            _ => self.put(off, v),
        }
    }

    fn delay_us(&self, _: u32) {}
}

fn chip() -> (Chip<M>, M) {
    let m = M::new();
    m.device_reset();
    (Chip::new(m.clone()), m)
}

#[test]
fn reset_masks_stops_and_loads_the_station_address() {
    let (c, m) = chip();
    m.put(r::IMS, 0xff);
    m.put(r::RCTL, r::RCTL_EN);
    c.reset().unwrap();
    assert_eq!(m.get(r::IMS), 0);
    assert_eq!(m.get(r::RCTL), 0);
    assert_eq!(c.mac_address(), MAC);
    // The master was disabled first, then the device reset.
    let w = m.0.writes.borrow();
    let at = |off: usize, f: &dyn Fn(u32) -> bool| w.iter().position(|&(o, v)| o == off && f(v));
    let disable = at(r::CTRL, &|v| v & r::CTRL_GIO_MASTER_DISABLE != 0).unwrap();
    let reset = at(r::CTRL, &|v| v & r::CTRL_DEV_RST != 0).unwrap();
    assert!(disable < reset);
    // The receiver stopped, and the transmitter stopped with its pad bit,
    // before the reset.
    assert!(at(r::RCTL, &|v| v == 0).unwrap() < reset);
    assert!(at(r::TCTL, &|v| v == r::TCTL_PSP).unwrap() < reset);
    assert!(at(r::IMC, &|v| v == u32::MAX).unwrap() < reset);
}

#[test]
fn a_device_that_left_the_bus_is_reported() {
    let (c, m) = chip();
    *m.0.gone.borrow_mut() = true;
    assert!(!c.responding());
    assert_eq!(c.reset(), Err(HwError::NotResponding));
}

#[test]
fn init_hw_sets_the_address_clears_the_rest_and_starts_the_link() {
    let (c, m) = chip();
    // Leftovers an earlier owner could have left.
    m.put(r::ral(3), 0x1234);
    m.put(r::rah(3), r::RAH_AV | 0x56);
    m.put(r::MTA + 8, 0xdead_beef);
    c.init_hw(MAC).unwrap();
    assert_eq!(
        m.get(r::ral(0)),
        u32::from_le_bytes([0xd8, 0xbb, 0xc1, 0x6c])
    );
    assert_eq!(m.get(r::rah(0)), r::RAH_AV | 0x5263);
    assert_eq!(m.get(r::ral(3)), 0);
    assert_eq!(m.get(r::rah(3)), 0);
    assert_eq!(m.get(r::MTA + 8), 0);
    // Link up in the MAC, speed and duplex left to auto-negotiation, no
    // flow control.
    let ctrl = m.get(r::CTRL);
    assert!(ctrl & r::CTRL_SLU != 0);
    assert_eq!(
        ctrl & (r::CTRL_FRCSPD | r::CTRL_FRCDPX | r::CTRL_RFCE | r::CTRL_TFCE),
        0
    );
    assert_eq!(m.get(r::PHPM) & r::PHPM_GO_LINKD, 0);
    // FreeBSD's flow control constants.
    assert_eq!(m.get(r::FCT), 0x8808);
    assert_eq!(m.get(r::FCAL), 0x00c2_8001);
    assert_eq!(m.get(r::FCRTH), 0);
}

#[test]
fn auto_negotiation_advertises_everything_and_the_link_comes_up_at_2500() {
    let (c, m) = chip();
    c.init_hw(MAC).unwrap();
    let adv = m.phy_reg(r::PHY_AUTONEG_ADV);
    assert_eq!(
        adv & (r::NWAY_AR_10T_HD | r::NWAY_AR_10T_FD | r::NWAY_AR_100TX_HD | r::NWAY_AR_100TX_FD),
        r::NWAY_AR_10T_HD | r::NWAY_AR_10T_FD | r::NWAY_AR_100TX_HD | r::NWAY_AR_100TX_FD
    );
    assert_eq!(adv & (r::NWAY_AR_PAUSE | r::NWAY_AR_ASM_DIR), 0);
    let gig = m.phy_reg(r::PHY_1000T_CTRL);
    assert!(gig & r::CR_1000T_FD != 0);
    assert_eq!(gig & r::CR_1000T_HD, 0);
    // 2.5 Gb/s is advertised through the auto-negotiation MMD.
    let multi = m.0.mmd.borrow().get(&(7, 0x20)).copied().unwrap();
    assert!(multi & r::CR_2500T_FD != 0);
    // Restarted, with auto-negotiation enabled and the PHY powered up.
    let ctl = m.phy_reg(r::PHY_CONTROL);
    assert!(ctl & (r::MII_CR_AUTO_NEG_EN | r::MII_CR_RESTART_AUTO_NEG) != 0);
    assert_eq!(ctl & r::MII_CR_POWER_DOWN, 0);
    // And the link is up, at what the status register says.
    assert_eq!(
        c.link(),
        LinkState {
            up: true,
            speed: Speed::M2500,
            full_duplex: true
        }
    );
    assert_eq!(c.phy_has_link(), Ok(true));
}

#[test]
fn the_phy_semaphores_are_let_go_after_every_access() {
    let (c, m) = chip();
    c.init_hw(MAC).unwrap();
    assert!(!*m.0.smbi.borrow());
    assert_eq!(m.get(r::SW_FW_SYNC) & r::SWFW_PHY0_SM, 0);
    assert_eq!(m.get(r::SWSM) & r::SWSM_SWESMBI, 0);
    // An access that fails lets go too (FreeBSD's gpy read leaks it).
    *m.0.mdic_error.borrow_mut() = true;
    assert_eq!(c.phy_read(r::PHY_STATUS), Err(HwError::Phy));
    assert!(!*m.0.smbi.borrow());
    assert_eq!(m.get(r::SW_FW_SYNC) & r::SWFW_PHY0_SM, 0);
}

#[test]
fn firmware_holding_the_phy_makes_an_access_fail_not_hang() {
    let (c, m) = chip();
    *m.0.fw_holds_phy.borrow_mut() = true;
    m.put(r::SW_FW_SYNC, r::SWFW_PHY0_SM << 16);
    assert_eq!(c.phy_read(r::PHY_STATUS), Err(HwError::Semaphore));
    // And it does not keep the hardware semaphore.
    assert!(!*m.0.smbi.borrow());
}

#[test]
fn mmd_registers_go_through_the_two_access_registers() {
    let (c, m) = chip();
    c.phy_write(7 << 16 | 0x20, 0x1234).unwrap();
    assert_eq!(m.0.mmd.borrow().get(&(7, 0x20)).copied(), Some(0x1234));
    assert_eq!(c.phy_read(7 << 16 | 0x20), Ok(0x1234));
    // The access register is put back to 0 at the end.
    assert_eq!(*m.0.mdic_log.borrow().last().unwrap(), (false, r::MMDAC, 0));
    assert!(!*m.0.mmd_data_mode.borrow());
}

#[test]
fn a_register_beyond_the_five_bit_address_is_refused() {
    let (c, _m) = chip();
    assert_eq!(c.phy_read(0x20), Err(HwError::Phy));
}

#[test]
fn link_state_reads_the_status_register() {
    let s = |v| LinkState::of(v);
    assert!(!s(0).up);
    assert_eq!(s(r::STATUS_LU).speed, Speed::M10);
    assert_eq!(s(r::STATUS_LU | r::STATUS_SPEED_100).speed, Speed::M100);
    assert_eq!(s(r::STATUS_LU | r::STATUS_SPEED_1000).speed, Speed::M1000);
    // The I225 says 1 Gb/s in both modes: the extra bit tells.
    let fast = s(r::STATUS_LU | r::STATUS_SPEED_1000 | r::STATUS_SPEED_2500);
    assert_eq!(fast.speed, Speed::M2500);
    assert!(!s(r::STATUS_LU | r::STATUS_SPEED_100).full_duplex);
    assert!(s(r::STATUS_LU | r::STATUS_FD).full_duplex);
}

#[test]
fn the_collision_distance_is_set_at_link_up() {
    let (c, m) = chip();
    m.put(r::TCTL, r::TCTL_PSP | 0x3ff << r::COLD_SHIFT);
    c.link_up_config();
    let t = m.get(r::TCTL);
    assert_eq!((t & r::TCTL_COLD) >> r::COLD_SHIFT, r::COLLISION_DISTANCE);
    assert!(t & r::TCTL_PSP != 0);
}

#[test]
fn the_multicast_hash_is_freebsds() {
    // The worked example in igc_hash_mc_addr_generic: 01:AA:00:12:34:56
    // hashes to 0x563.
    let t = Chip::<M>::multicast_table(&[[0x01, 0xaa, 0x00, 0x12, 0x34, 0x56]]);
    assert_eq!(t[0x563 >> 5], 1 << (0x563 & 0x1f));
    assert_eq!(t.iter().map(|w| w.count_ones()).sum::<u32>(), 1);
    // Several groups; the same group twice is one bit.
    let g = [
        [1, 0, 0x5e, 0, 0, 1],
        [1, 0, 0x5e, 0, 0, 1],
        [0x33, 0x33, 0, 0, 0, 2],
    ];
    let t = Chip::<M>::multicast_table(&g);
    assert_eq!(t.iter().map(|w| w.count_ones()).sum::<u32>(), 2);
}

#[test]
fn the_filter_is_promiscuous_all_multicast_or_the_table() {
    let (c, m) = chip();
    m.put(r::RCTL, r::RCTL_EN | r::RCTL_BAM);
    c.set_filter(true, false, &[]);
    let v = m.get(r::RCTL);
    assert!(v & (r::RCTL_UPE | r::RCTL_MPE) == r::RCTL_UPE | r::RCTL_MPE);
    assert!(v & (r::RCTL_EN | r::RCTL_BAM) == r::RCTL_EN | r::RCTL_BAM);
    c.set_filter(false, true, &[]);
    let v = m.get(r::RCTL);
    assert!(v & r::RCTL_MPE != 0 && v & r::RCTL_UPE == 0);
    let g = [[1, 0, 0x5e, 0, 0, 1]];
    c.set_filter(false, false, &g);
    let v = m.get(r::RCTL);
    assert_eq!(v & (r::RCTL_UPE | r::RCTL_MPE), 0);
    let words: u32 = (0..r::MTA_COUNT)
        .map(|i| m.get(r::MTA + i * 4).count_ones())
        .sum();
    assert_eq!(words, 1);
    // A table that is too long falls back to all multicast.
    let many: Vec<[u8; 6]> = (0..128u8).map(|i| [1, 0, 0x5e, 0, i, i]).collect();
    c.set_filter(false, false, &many);
    assert!(m.get(r::RCTL) & r::RCTL_MPE != 0);
}

#[test]
fn the_queues_are_programmed_as_igc_if_init_does() {
    let (c, m) = chip();
    let bus = 0x1_2345_6000u64;
    c.tx_init(bus, 4096);
    assert_eq!(m.get(r::TDBAL), 0x2345_6000);
    assert_eq!(m.get(r::TDBAH), 1);
    assert_eq!(m.get(r::TDLEN), 4096);
    assert_eq!(m.get(r::TDH), 0);
    assert_eq!(m.get(r::TDT), 0);
    assert_eq!(m.get(r::TXDCTL), 8 | 1 << 8 | r::TXDCTL_QUEUE_ENABLE);
    let t = m.get(r::TCTL);
    assert!(
        t & (r::TCTL_EN | r::TCTL_PSP | r::TCTL_RTLC) == r::TCTL_EN | r::TCTL_PSP | r::TCTL_RTLC
    );
    assert_eq!((t & r::TCTL_CT) >> r::CT_SHIFT, 15);

    c.rx_init(bus, 4096);
    assert_eq!(m.get(r::RDBAL), 0x2345_6000);
    assert_eq!(m.get(r::RDBAH), 1);
    assert_eq!(m.get(r::RDLEN), 4096);
    assert_eq!(m.get(r::SRRCTL), 2 | r::SRRCTL_DESCTYPE_ADV_ONEBUF);
    let rxdctl = m.get(r::RXDCTL);
    assert_eq!(rxdctl & r::RXDCTL_QUEUE_ENABLE, r::RXDCTL_QUEUE_ENABLE);
    assert_eq!(rxdctl & 0x1f, 8);
    assert_eq!((rxdctl >> 8) & 0x1f, 8);
    assert_eq!((rxdctl >> 16) & 0x1f, 4);
    let rctl = m.get(r::RCTL);
    assert!(
        rctl & (r::RCTL_EN | r::RCTL_BAM | r::RCTL_SECRC)
            == r::RCTL_EN | r::RCTL_BAM | r::RCTL_SECRC
    );
    assert_eq!(rctl & (r::RCTL_SBP | r::RCTL_LPE | r::RCTL_VFE), 0);
    assert_eq!(
        m.get(r::RXCSUM) & (r::RXCSUM_CRCOFL | r::RXCSUM_IPPCSE),
        r::RXCSUM_CRCOFL | r::RXCSUM_IPPCSE
    );
    // The receiver was turned off while the ring was set up.
    let w = m.0.writes.borrow();
    let off = w
        .iter()
        .position(|&(o, v)| o == r::RCTL && v & r::RCTL_EN == 0)
        .unwrap();
    let base = w.iter().position(|&(o, _)| o == r::RDBAL).unwrap();
    assert!(off < base);
    drop(w);
    c.rx_tail(255);
    assert_eq!(m.get(r::RDT), 255);
    c.tx_tail(7);
    assert_eq!(m.get(r::TDT), 7);
}

#[test]
fn interrupts_are_masked_unmasked_and_read_once() {
    let (c, m) = chip();
    c.unmask();
    assert_eq!(m.get(r::IMS), r::IMS_ENABLE);
    c.mask();
    assert_eq!(m.get(r::IMS), 0);
    m.put(r::ICR, r::ICR_LSC | r::ICR_RXT0);
    assert_eq!(c.ack(), r::ICR_LSC | r::ICR_RXT0);
    assert_eq!(c.ack(), 0);
}

// ---- The rings ----------------------------------------------------------------

/// A ring's descriptors in memory.
struct Mem(RefCell<Vec<u32>>);

impl Mem {
    fn new() -> Self {
        Self(RefCell::new(alloc::vec![0; DESCS * WORDS]))
    }
}

impl Words for Mem {
    fn get(&self, d: usize, w: usize) -> u32 {
        self.0.borrow()[d * WORDS + w]
    }
    fn set(&self, d: usize, w: usize, v: u32) {
        self.0.borrow_mut()[d * WORDS + w] = v;
    }
}

fn bus_of(slot: usize) -> u64 {
    0x8_0000_0000 + (slot * SLOT) as u64
}

/// The controller returns descriptor `d`: the status, errors, and length.
fn write_back(m: &Mem, d: usize, status: u32, len: usize) {
    m.set(d, 2, status);
    m.set(d, 3, len as u32);
}

const GOOD: u32 = r::RXD_STAT_DD | r::RXD_STAT_EOP;

#[test]
fn the_receive_ring_posts_all_but_one_buffer() {
    let m = Mem::new();
    let mut ring = RxRing::new();
    let tail = ring.fill(&m, bus_of);
    assert_eq!(tail as usize, DESCS - 1);
    for d in 0..DESCS - 1 {
        let bus = bus_of(d);
        assert_eq!(m.get(d, 0), bus as u32);
        assert_eq!(m.get(d, 1), (bus >> 32) as u32);
        assert_eq!((m.get(d, 2), m.get(d, 3)), (0, 0));
    }
    assert_eq!(ring.take(&m), None);
    assert_eq!(ring.ready(&m), None);
}

#[test]
fn a_returned_descriptor_is_taken_with_its_length_and_checksums() {
    let m = Mem::new();
    let mut ring = RxRing::new();
    ring.fill(&m, bus_of);
    write_back(&m, 0, GOOD | r::RXD_STAT_IPCS | r::RXD_STAT_TCPCS, 1514);
    assert_eq!(ring.ready(&m), Some(0));
    let t = ring.take(&m).unwrap();
    assert_eq!((t.slot, t.len, t.eop, t.bad), (0, 1514, true, false));
    assert_eq!(
        t.csum,
        RxCsum {
            ip_checked: true,
            ip_valid: true,
            l4_valid: true
        }
    );
    // Taking posts the descriptor at the tail, the one left over.
    let tail = ring.refill(&m, bus_of);
    assert_eq!(tail, 0);
    assert_eq!(m.get(DESCS - 1, 0), bus_of(DESCS - 1) as u32);
    assert_eq!(ring.take(&m), None);
}

#[test]
fn checksums_are_claimed_only_for_a_frame_checked_and_good() {
    assert_eq!(RxCsum::of(GOOD), RxCsum::default());
    // The controller did not check it.
    assert_eq!(
        RxCsum::of(GOOD | r::RXD_STAT_IXSM | r::RXD_STAT_IPCS),
        RxCsum::default()
    );
    // A bad IP or layer 4 checksum claims nothing.
    assert_eq!(
        RxCsum::of(GOOD | r::RXD_STAT_IPCS | r::RXD_ERR_IPE),
        RxCsum::default()
    );
    assert_eq!(
        RxCsum::of(GOOD | r::RXD_STAT_UDPCS | r::RXD_ERR_L4E),
        RxCsum::default()
    );
    let udp = RxCsum::of(GOOD | r::RXD_STAT_IPCS | r::RXD_STAT_UDPCS);
    assert!(udp.ip_valid && udp.l4_valid);
}

#[test]
fn the_ring_goes_round_many_times_without_losing_a_buffer() {
    let m = Mem::new();
    let mut ring = RxRing::new();
    ring.fill(&m, bus_of);
    let mut tail = DESCS - 1;
    // The controller fills what it is given (head to tail), the driver
    // takes each and gives one back.
    let mut head = 0usize;
    for n in 0..DESCS * 5 {
        assert_ne!(head, tail, "the controller has no descriptor to fill");
        write_back(&m, head, GOOD, 60 + n % 1400);
        head = (head + 1) % DESCS;
        let t = ring.take(&m).unwrap();
        assert_eq!(t.slot, n % DESCS);
        assert_eq!(t.len, 60 + n % 1400);
        tail = ring.refill(&m, bus_of) as usize;
        // What the controller may fill never includes a stale descriptor.
        assert_eq!(
            m.get((tail + DESCS - 1) % DESCS, 0),
            bus_of((tail + DESCS - 1) % DESCS) as u32
        );
    }
}

#[test]
fn a_frame_in_several_descriptors_is_put_together() {
    let mut a = Assembler::default();
    let part = |slot, eop| crate::ring::Taken {
        slot,
        len: 4,
        eop,
        bad: false,
        csum: RxCsum::default(),
    };
    assert_eq!(a.add(&part(0, false), b"abcd"), Assembled::More);
    assert_eq!(a.add(&part(1, false), b"efgh"), Assembled::More);
    match a.add(&part(2, true), b"ij\0\0") {
        Assembled::Frame(f, _) => assert_eq!(f, b"abcdefghij\0\0"),
        other => panic!("{other:?}"),
    }
    // The next frame starts afresh.
    match a.add(&part(3, true), b"xy\0\0") {
        Assembled::Frame(f, _) => assert_eq!(f.len(), 4),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_frame_with_a_receive_error_is_dropped_and_the_next_is_not() {
    let m = Mem::new();
    let mut ring = RxRing::new();
    ring.fill(&m, bus_of);
    write_back(&m, 0, GOOD | r::RXD_ERR_RXE, 100);
    write_back(&m, 1, GOOD, 100);
    let mut a = Assembler::default();
    let t = ring.take(&m).unwrap();
    assert!(t.bad);
    assert_eq!(a.add(&t, &[0; 100]), Assembled::Dropped);
    let t = ring.take(&m).unwrap();
    assert!(matches!(a.add(&t, &[0; 100]), Assembled::Frame(f, _) if f.len() == 100));
}

#[test]
fn a_transmitted_frame_is_one_descriptor_with_the_commands_and_its_length() {
    let m = Mem::new();
    let mut ring = TxRing::new();
    ring.init(&m);
    assert_eq!(ring.free(), DESCS - 1);
    let (first, tail) = ring.push(&m, &[(0x9_0000_1000, 1514)]).unwrap();
    assert_eq!((first, tail), (0, 1));
    assert_eq!(m.get(0, 0), 0x0000_1000);
    assert_eq!(m.get(0, 1), 9);
    let cmd = m.get(0, 2);
    assert_eq!(cmd & 0xffff, 1514);
    let want = r::ADVTXD_DTYP_DATA
        | r::ADVTXD_DCMD_IFCS
        | r::ADVTXD_DCMD_DEXT
        | r::ADVTXD_DCMD_EOP
        | r::ADVTXD_DCMD_RS;
    assert_eq!(cmd & !0xffff, want);
    assert_eq!(m.get(0, 3), 1514 << r::ADVTXD_PAYLEN_SHIFT);
    assert_eq!(ring.in_flight(), 1);
    assert_eq!(ring.free(), DESCS - 2);
    // Nothing is reaped until the controller says done.
    assert_eq!(ring.reap(&m), None);
    m.set(0, 3, m.get(0, 3) | r::TXD_STAT_DD);
    assert_eq!(ring.reap(&m), Some(0));
    assert_eq!(ring.in_flight(), 0);
    assert_eq!(ring.free(), DESCS - 1);
}

#[test]
fn a_frame_in_several_buffers_reports_status_only_on_its_last() {
    let m = Mem::new();
    let mut ring = TxRing::new();
    ring.init(&m);
    let (first, tail) = ring
        .push(&m, &[(0x1000, 100), (0x2000, 200), (0x3000, 300)])
        .unwrap();
    assert_eq!((first, tail), (0, 3));
    for d in 0..2 {
        assert_eq!(m.get(d, 2) & (r::ADVTXD_DCMD_EOP | r::ADVTXD_DCMD_RS), 0);
    }
    assert_eq!(
        m.get(2, 2) & (r::ADVTXD_DCMD_EOP | r::ADVTXD_DCMD_RS),
        r::ADVTXD_DCMD_EOP | r::ADVTXD_DCMD_RS
    );
    // The payload length is on the first only.
    assert_eq!(m.get(0, 3) >> r::ADVTXD_PAYLEN_SHIFT, 600);
    assert_eq!(m.get(1, 3), 0);
    // Done on a middle descriptor does not finish the frame.
    m.set(1, 3, r::TXD_STAT_DD);
    assert_eq!(ring.reap(&m), None);
    m.set(2, 3, r::TXD_STAT_DD);
    assert_eq!(ring.reap(&m), Some(0));
    assert_eq!(ring.free(), DESCS - 1);
}

#[test]
fn the_transmit_ring_fills_and_wraps() {
    let m = Mem::new();
    let mut ring = TxRing::new();
    ring.init(&m);
    for _ in 0..DESCS - 1 {
        ring.push(&m, &[(0x1000, 60)]).unwrap();
    }
    assert_eq!(ring.free(), 0);
    assert_eq!(ring.push(&m, &[(0x1000, 60)]), Err(Full));
    // Complete the oldest and queue again past the end of the ring.
    m.set(0, 3, r::TXD_STAT_DD);
    assert_eq!(ring.reap(&m), Some(0));
    let (first, tail) = ring.push(&m, &[(0x2000, 80)]).unwrap();
    assert_eq!((first, tail), (DESCS - 1, 0));
    assert_eq!(ring.next(), 0);
    // Many frames round the ring, each reaped as the controller sends it.
    while ring.in_flight() > 0 {
        let q = (0..DESCS)
            .find(|&d| m.get(d, 2) != 0 && m.get(d, 3) & r::TXD_STAT_DD == 0)
            .unwrap();
        m.set(q, 3, m.get(q, 3) | r::TXD_STAT_DD);
        ring.reap(&m);
    }
    for n in 0..DESCS * 3 {
        let d = ring.next();
        let (first, tail) = ring.push(&m, &[(0x3000, 64)]).unwrap();
        assert_eq!(first, d);
        assert_eq!(tail as usize, (d + 1) % DESCS, "frame {n}");
        m.set(d, 3, m.get(d, 3) | r::TXD_STAT_DD);
        assert_eq!(ring.reap(&m), Some(d));
    }
    assert_eq!(ring.in_flight(), 0);
}

#[test]
fn an_empty_frame_and_one_too_large_for_the_ring_are_refused() {
    let m = Mem::new();
    let mut ring = TxRing::new();
    ring.init(&m);
    assert_eq!(ring.push(&m, &[]), Err(Full));
    let many: Vec<(u64, u32)> = (0..DESCS).map(|_| (0x1000, 60)).collect();
    assert_eq!(ring.push(&m, &many), Err(Full));
    assert_eq!(ring.free(), DESCS - 1);
}

#[test]
fn the_largest_frame_fits_a_buffer() {
    const { assert!(MAX_FRAME <= SLOT) };
    assert_eq!(MAX_FRAME, 1500 + 14 + 4);
}
