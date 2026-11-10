// SPDX-License-Identifier: BSD-2-Clause
//! `rgeN`: the PCI driver and its interface.
//!
//! - attach maps the registers (the first memory BAR), identifies the
//!   revision, allocates the rings (descriptors below 4 GiB, as FreeBSD's
//!   32-bit descriptor tags put them), and runs `rge_init_locked`'s
//!   sequence: chip init, station address, rings, PHY microcode and
//!   configuration, MAC setup, receive filter. It takes one MSI.
//! - start installs the interrupt, starts the receiver and transmitter,
//!   and attaches the interface. The chip runs while the driver is
//!   attached, as `vtnet` does.
//! - The interrupt's filter masks the controller; its thread acknowledges
//!   what is pending, takes received frames, reaps sent ones, follows the
//!   link, and unmasks it.
//! - A frame to send is copied into one buffer, padded to 60 bytes (the
//!   controller pads nothing), and mapped; its segments become its
//!   descriptors.
//!
//! Received frames are copied out of their slots, so DMA memory holds
//! only device payload.
//!
//! Behavior reference: sys/dev/rge/if_rge.c.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;

use abi::net::{ifcap, iff};
use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, IrqLine, MmioGrant, StartCx, StopCx,
    StopReason,
};
use devtree::ids::PciId;
use irq::Filter;
use mbuf::{Chain, NoBufs, Packet, PktHdr, csum, flags};
use net::link::{Link, Media, ifm};
use net::netif::{Ether, Port};
use net::{Family, Vnet};
use pci::bus::{Messages, PciChild};
use regs::Bytes;
use sched::{IrqCx, Sleepable};
use sync::SpinLock;
use vm::dma::{DmaCoherent, DmaConstraints, DmaTag, Mapped, dir};

use crate::hw::{Chip, MacType, Regs};
use crate::regs as r;
use crate::ring::{self, Assembled, Assembler, DESCS, RxRing, SLOT, TxRing, WORDS};

/// Receive slots per DMA chunk (32 KiB each).
const SLOTS_PER_CHUNK: usize = 16;
/// Frames taken per pass of the interrupt thread.
const RX_BUDGET: usize = 256;
/// `ETHERMTU`.
const ETHERMTU: u32 = 1500;
/// The longest frame sent: a VLAN-tagged jumbo frame without its CRC.
const MAX_FRAME: usize = r::JUMBO_FRAMELEN as usize - ring::CRC_LEN;

/// The registers, shared by the chip programming and the interrupt path.
#[derive(Clone, Debug)]
pub struct Bar(Arc<MmioGrant<Bytes>>);

impl Regs for Bar {
    fn read8(&self, off: usize) -> u8 {
        self.0.read8(off)
    }
    fn read16(&self, off: usize) -> u16 {
        self.0.read16(off)
    }
    fn read32(&self, off: usize) -> u32 {
        self.0.read32(off)
    }
    fn write8(&self, off: usize, v: u8) {
        self.0.write8(off, v);
    }
    fn write16(&self, off: usize, v: u16) {
        self.0.write16(off, v);
    }
    fn write32(&self, off: usize, v: u32) {
        self.0.write32(off, v);
    }
    fn delay_us(&self, us: u32) {
        bus::manager().delay_us(u64::from(us));
    }
}

/// A ring's descriptors in DMA memory.
struct Descs(DmaCoherent<u32>);

impl ring::Words for Descs {
    fn get(&self, d: usize, w: usize) -> u32 {
        u32::from_le(self.0.read(d * WORDS + w))
    }
    fn set(&self, d: usize, w: usize, v: u32) {
        self.0.write(d * WORDS + w, v.to_le());
    }
}

struct Rx {
    ring: RxRing,
    descs: Descs,
    chunks: Vec<DmaCoherent<u8>>,
    asm: Assembler,
}

impl Rx {
    fn slot_bus(&self, slot: usize) -> u64 {
        self.chunks[slot / SLOTS_PER_CHUNK].bus_addr() + ((slot % SLOTS_PER_CHUNK) * SLOT) as u64
    }

    fn slot_copy(&self, slot: usize, dst: &mut [u8]) {
        self.chunks[slot / SLOTS_PER_CHUNK].copy_to_slice((slot % SLOTS_PER_CHUNK) * SLOT, dst);
    }
}

struct Tx {
    ring: TxRing,
    descs: Descs,
    /// Each in-flight frame's buffer, by its first descriptor.
    sent: Vec<Option<Mapped<Vec<u8>, dir::ToDevice>>>,
}

impl Tx {
    /// Frees what the controller has sent (`rge_txeof`).
    fn reap(&mut self, port: &Port) {
        while let Some((first, ok)) = self.ring.reap(&self.descs, &ring::Hal) {
            if let Some(m) = self.sent[first].take() {
                drop(m.unmap());
            }
            if !ok {
                port.stats.oerrors.add(1);
            }
        }
    }
}

/// What the interface and the interrupt share.
pub struct Shared {
    chip: Chip<Bar>,
    tag: DmaTag,
    rx: SpinLock<Rx>,
    tx: SpinLock<Tx>,
    port: Arc<Port>,
    /// The interface's flags and multicast groups, for the filter.
    filter: SpinLock<(u32, Vec<[u8; 6]>)>,
}

impl core::fmt::Debug for Shared {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Shared")
            .field("mac", &self.chip.mac)
            .finish_non_exhaustive()
    }
}

impl Shared {
    /// Programs the receive filter from the interface's state.
    fn apply_filter(&self) {
        let (fl, groups) = &*self.filter.lock();
        let promisc = fl & iff::PROMISC != 0;
        let hash = if promisc || fl & iff::ALLMULTI != 0 {
            [u32::MAX; 2]
        } else {
            crate::multicast_hash(groups)
        };
        self.chip.set_filter(promisc, hash);
    }

    /// Takes a pass of received frames; whether more may be waiting.
    fn rx_pass(&self) -> bool {
        let mut frames: Vec<Packet> = Vec::new();
        {
            let mut rx = self.rx.lock();
            let mut buf = alloc::vec![0u8; SLOT];
            while frames.len() < RX_BUDGET {
                let Some(p) = ({
                    let Rx { ring, descs, .. } = &mut *rx;
                    ring.take(descs, &ring::Hal)
                }) else {
                    break;
                };
                rx.slot_copy(p.slot, &mut buf[..p.len]);
                let bus = rx.slot_bus(p.slot);
                RxRing::post(&rx.descs, &ring::Hal, p.slot, bus);
                match rx.asm.add(&p, &buf[..p.len]) {
                    Assembled::More => {}
                    Assembled::Dropped => self.port.stats.ierrors.add(1),
                    Assembled::Frame(f, c) => {
                        let Ok(data) = Chain::from_slice(&f) else {
                            self.port.stats.iqdrops.add(1);
                            continue;
                        };
                        let mut hdr = PktHdr::default();
                        if c.ip_checked {
                            hdr.csum_flags |= csum::IP_CHECKED;
                        }
                        if c.ip_valid {
                            hdr.csum_flags |= csum::IP_VALID;
                        }
                        if c.l4_valid {
                            hdr.csum_flags |= csum::DATA_VALID | csum::PSEUDO_HDR;
                            hdr.csum_data = 0xffff;
                        }
                        frames.push(Packet { hdr, data });
                    }
                }
            }
        }
        let more = frames.len() >= RX_BUDGET;
        let batch = net::defer::begin();
        for m in frames {
            self.port.stats.ipackets.add(1);
            self.port.stats.ibytes.add(m.len() as u64);
            self.port.input(m);
        }
        drop(batch);
        more
    }

    /// Sends frame `m` (`rge_encap`, with the frame copied).
    fn send(&self, m: Packet) -> Result<(), NoBufs> {
        let len = m.len();
        if len > MAX_FRAME {
            return Err(NoBufs);
        }
        let mut buf = Vec::with_capacity(len.max(ring::MIN_FRAME));
        for seg in m.data.segments() {
            buf.extend_from_slice(seg);
        }
        // The controller does not pad short frames.
        buf.resize(len.max(ring::MIN_FRAME), 0);
        let mcast = m.hdr.flags & (flags::BCAST | flags::MCAST) != 0;
        self.port.tap.mtap(&m, net::bpf::Way::Out);
        drop(m);
        let data = self.tag.map::<dir::ToDevice, _>(buf).map_err(|_| NoBufs)?;
        let segs: Vec<(u64, u32)> = data
            .segments()
            .iter()
            .map(|s| (s.bus, u32::try_from(s.len).unwrap_or(u32::MAX)))
            .collect();
        let mut tx = self.tx.lock();
        if tx.ring.free() < segs.len() {
            tx.reap(&self.port);
        }
        let first = {
            let Tx { ring, descs, .. } = &mut *tx;
            ring.push(descs, &ring::Hal, &segs, 0)
        };
        let Ok(first) = first else {
            drop(tx);
            drop(data.unmap());
            return Err(NoBufs);
        };
        tx.sent[first] = Some(data);
        self.chip.kick_tx();
        drop(tx);
        let st = &self.port.stats;
        st.opackets.add(1);
        st.obytes.add(len as u64);
        if mcast {
            st.omcasts.add(1);
        }
        Ok(())
    }

    /// The media words `rge_ifmedia_sts` reports.
    fn media(&self) -> Media {
        let st = self.chip.phystat();
        let up = st & r::PHYSTAT_LINK != 0;
        let mut active = ifm::ETHER;
        if up {
            let fast = r::PHYSTAT_1000MBPS | r::PHYSTAT_2500MBPS | r::PHYSTAT_5000MBPS;
            active |= if st & (r::PHYSTAT_FDX | fast) != 0 {
                ifm::FDX
            } else {
                ifm::HDX
            };
            active |= if st & r::PHYSTAT_10MBPS != 0 {
                ifm::T10
            } else if st & r::PHYSTAT_100MBPS != 0 {
                ifm::TX100
            } else if st & r::PHYSTAT_1000MBPS != 0 {
                ifm::T1000
            } else if st & r::PHYSTAT_2500MBPS != 0 {
                ifm::T2500
            } else {
                ifm::T5000
            };
        } else {
            active |= ifm::NONE;
        }
        Media {
            current: ifm::ETHER | ifm::AUTO,
            active,
            status: ifm::AVALID | if up { ifm::ACTIVE } else { 0 },
            supported: ifm::ETHER | ifm::AUTO,
        }
    }
}

/// The interface's device.
struct Iface(Arc<Shared>);

impl Link for Iface {
    fn transmit(
        &self,
        _vnet: &Arc<Vnet>,
        _index: u16,
        _family: Option<Family>,
        m: Packet,
    ) -> Result<(), NoBufs> {
        self.0
            .send(m)
            .inspect_err(|_| self.0.port.stats.oerrors.add(1))
    }

    fn media(&self) -> Option<Media> {
        Some(self.0.media())
    }

    fn moved(&self, vnet: &Arc<Vnet>, index: u16) {
        self.0.port.moved(vnet, index);
    }

    fn runs_while_up(&self) -> bool {
        true
    }

    fn link_status(&self) -> Option<bool> {
        Some(self.0.chip.link_up())
    }

    fn flags_changed(&self, fl: u32) {
        self.0.filter.lock().0 = fl;
        self.0.apply_filter();
    }

    fn multicast_changed(&self, groups: &[[u8; 6]]) {
        self.0.filter.lock().1 = groups.to_vec();
        self.0.apply_filter();
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// An `rgeN` instance.
pub struct Rge {
    shared: Arc<Shared>,
    line: SpinLock<Option<IrqLine>>,
    _messages: Messages,
    info: Ether,
}

impl core::fmt::Debug for Rge {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Rge").finish_non_exhaustive()
    }
}

fn filter(d: &Rge, _: usize, _: &IrqCx) -> Filter {
    d.shared.chip.mask();
    Filter::Schedule
}

fn handler(d: &Rge, _: usize, _: &Sleepable) {
    let sh = &d.shared;
    loop {
        let st = sh.chip.ack();
        if st & r::INTRS == 0 {
            break;
        }
        while sh.rx_pass() {
            sched::yield_now();
        }
        sh.tx.lock().reap(&sh.port);
        if st & r::ISR_LINKCHG != 0 {
            sh.port.link_state(sh.chip.link_up());
        }
        if st & r::ISR_SYSTEM_ERR != 0 {
            bus::manager().log(format_args!(
                "{}{}: system error interrupt",
                d.info.dname, d.info.unit
            ));
        }
    }
    sh.chip.unmask();
}

/// A valid unicast station address, or a local one from the unit
/// (`ether_gen_addr`'s role).
pub(crate) fn station(a: [u8; 6], unit: u32) -> [u8; 6] {
    if a != [0; 6] && a[0] & 1 == 0 {
        return a;
    }
    let u = unit.to_be_bytes();
    net::link::random_addr([0x58, 0x9c, u[0], u[1], u[2], u[3]])
}

impl Driver for Rge {
    const NAME: &'static str = "rge";
    // The RTL8126; the RTL8125 family and the RTL8127 come with their
    // programming (the X870E's NIC is an RTL8125-family part).
    const IDS: Ids = Ids::Pci(&[PciId::device(0x10ec, 0x8126)]);
    const CONCURRENCY: bus::Concurrency = bus::Concurrency::Concurrent;

    fn describe(_: &Ident) -> String {
        "RTL8126".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let child = cx.bus_data_arc::<PciChild>().ok_or(AttachError::Decline)?;
        let bar = Bar(Arc::new(cx.mmio_bytes(0)?));
        child.enable_decoding();
        child.set_bus_master(true);
        let hwrev = bar.read32(r::TXCFG) & r::TXCFG_HWREV;
        let log = |a: core::fmt::Arguments<'_>| bus::manager().log(a);
        let unit = cx.unit();
        let mac = MacType::of(hwrev).ok_or(AttachError::Device("unknown chip revision"))?;
        log(format_args!(
            "rge{unit}: chip rev: {} ({hwrev:#010x})",
            mac.name()
        ));
        if !mac.supported() {
            return Err(AttachError::Device("chip revision not supported yet"));
        }
        // Descriptor rings below 4 GiB, 256-byte aligned; buffers anywhere.
        let ring_tag = cx.dma_tag(DmaConstraints {
            addr_bits: 32,
            align: 256,
            ..DmaConstraints::UNLIMITED
        })?;
        let tag = cx.dma_tag(DmaConstraints::UNLIMITED)?;
        let nomem = |_| AttachError::NoMemory;
        let rx_descs = Descs(ring_tag.coherent::<u32>(DESCS * WORDS).map_err(nomem)?);
        let tx_descs = Descs(ring_tag.coherent::<u32>(DESCS * WORDS).map_err(nomem)?);
        let chunks = (0..DESCS / SLOTS_PER_CHUNK)
            .map(|_| tag.coherent::<u8>(SLOT * SLOTS_PER_CHUNK))
            .collect::<Result<Vec<_>, _>>()
            .map_err(nomem)?;
        let mut chip = Chip::new(bar, mac);
        let hw = |_| AttachError::Device("controller did not respond");
        chip.chip_init().map_err(hw)?;
        let lladdr = station(chip.get_macaddr(), unit);
        // `rge_init_locked`: from a stopped chip, with fresh rings.
        chip.stop().map_err(hw)?;
        chip.set_macaddr(lladdr);
        let mut rx = Rx {
            ring: RxRing::new(),
            descs: rx_descs,
            chunks,
            asm: Assembler::default(),
        };
        {
            let Rx {
                ring,
                descs,
                chunks,
                ..
            } = &mut rx;
            ring.fill(descs, &ring::Hal, |s| {
                chunks[s / SLOTS_PER_CHUNK].bus_addr() + ((s % SLOTS_PER_CHUNK) * SLOT) as u64
            });
        }
        let mut tx = Tx {
            ring: TxRing::new(),
            descs: tx_descs,
            sent: (0..DESCS).map(|_| None).collect(),
        };
        tx.ring.init(&tx.descs);
        chip.chip_init().map_err(hw)?;
        chip.phy_config()
            .map_err(|_| AttachError::Device("PHY did not reset"))?;
        chip.configure(rx.descs.0.bus_addr(), tx.descs.0.bus_addr(), true);
        chip.set_filter(false, [0; 2]);
        let (line, messages) = pci::bus::msi_line(cx).map_err(|_| AttachError::Device("no MSI"))?;
        let caps = ifcap::RXCSUM | ifcap::VLAN_MTU | ifcap::LINKSTATE;
        let info = Ether {
            dname: "rge",
            unit: i32::try_from(unit).unwrap_or(0),
            lladdr,
            mtu: ETHERMTU,
            capabilities: caps,
            capenable: caps,
            baudrate: 5_000_000_000,
        };
        Ok(Self {
            shared: Arc::new(Shared {
                chip,
                tag,
                rx: SpinLock::new(rx),
                tx: SpinLock::new(tx),
                port: Port::new(),
                filter: SpinLock::new((0, Vec::new())),
            }),
            line: SpinLock::new(Some(line)),
            _messages: messages,
            info,
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        if let Some(line) = self.line.lock().take() {
            cx.interrupt(line, Some(filter), Some(handler), 0)?;
        }
        let sh = &self.shared;
        sh.chip.start();
        let name = net::netif::attach_ether(&self.info, &sh.port, Arc::new(Iface(Arc::clone(sh))));
        let m = self.info.lladdr;
        bus::manager().log(format_args!(
            "{name}: Ethernet address: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            m[0], m[1], m[2], m[3], m[4], m[5]
        ));
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {
        let sh = &self.shared;
        net::netif::detach(&sh.port);
        let _ = sh.chip.stop();
        let mut tx = sh.tx.lock();
        for m in tx.sent.iter_mut().filter_map(Option::take) {
            drop(m.unmap());
        }
        sh.rx.lock().asm.clear();
    }
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<Rge>();
