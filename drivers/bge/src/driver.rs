// SPDX-License-Identifier: BSD-2-Clause
//! `bgeN`: the PCI driver and its interface.
//!
//! - attach maps BAR0, identifies the chip (the 57765 family only), resets
//!   it (the bootcode's handshake), runs `bge_chipinit`, reads the hardware
//!   configuration and the station address, finds the PHY, and allocates
//!   the rings, the status block and the buffer slots, all below 4 GiB (the
//!   chip's `BGE_FLAG_4G_BNDRY_BUG`: nothing it reads may cross a 4 GiB
//!   line). It takes one MSI.
//! - start installs the interrupt and runs `bge_init_locked`'s sequence:
//!   stop, reset, chip and block init, rings filled, MAC started, filter,
//!   interrupts on, autonegotiation. The chip runs while the driver is
//!   attached, as `rge`'s does.
//! - The interrupt is MSI with tagged status, one-shot (the chip masks it
//!   until acknowledged): its thread reads the status block, follows a
//!   link change, acknowledges with the block's tag, takes received frames
//!   and reaps sent ones (`bge_intr_task`).
//! - A thread ticks once a second: it polls the PHY (FreeBSD polls only
//!   while the link is down and relies on the chip's link attention
//!   afterwards; polling always loses nothing if the attention never
//!   comes), reaps, and counts the transmit watchdog down; a stalled
//!   transmitter reinitializes the chip.
//! - Frames are copied: into the send descriptor's own slot (padded to 60
//!   bytes), and out of the receive slot the chip returns, which is then
//!   posted again. TX checksum offload is off, as FreeBSD's default; the
//!   chip's receive checksums are used.
//!
//! Behavior reference: sys/dev/bge/if_bge.c.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicU8, Ordering};

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
use pci::bus::{ConfigState, Messages, PciChild};
use regs::Bytes;
use sched::lock::{LockClass, Mutex};
use sched::{IrqCx, Sleepable, Thread, WaitQueue};
use sync::SpinLock;
use vm::dma::{DmaCoherent, DmaConstraints};

use crate::hw::{Chip, HwError, LinkMode, Regs, Rings, multicast_hash};
use crate::phy::{Mii, Phy, Speed, Status as PhyStatus};
use crate::regs as r;
use crate::ring::{self, ReturnRing, StdRing, TxRing};
use nicwatch::TxWatchdog;

/// Slots per DMA chunk (32 KiB).
const SLOTS_PER_CHUNK: usize = 16;
/// Frames taken per pass of the interrupt thread.
const RX_BUDGET: usize = 256;
/// `ETHERMTU`.
const ETHERMTU: u32 = 1500;
/// The longest frame sent: a VLAN-tagged one without its CRC.
const MAX_FRAME: usize = 1518;
/// The tick's period.
const TICK_NS: u64 = 1_000_000_000;

static CORE_LOCK: LockClass = LockClass::new("bge core", 500);

/// Whether the chip runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum State {
    Running = 0,
    /// Being reinitialized, or failed to be: the tick tries again.
    Down = 1,
    /// Detaching.
    Stopped = 2,
}

impl State {
    const fn of(v: u8) -> Self {
        match v {
            0 => Self::Running,
            1 => Self::Down,
            _ => Self::Stopped,
        }
    }
}

/// BAR0 and configuration space.
pub struct Bar {
    mmio: MmioGrant<Bytes>,
    child: Arc<PciChild>,
    saved: SpinLock<Option<ConfigState>>,
}

impl core::fmt::Debug for Bar {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Bar").finish_non_exhaustive()
    }
}

impl Regs for Bar {
    fn read32(&self, off: usize) -> u32 {
        self.mmio.read32(off)
    }
    fn write32(&self, off: usize, v: u32) {
        self.mmio.write32(off, v);
    }
    fn cfg_read32(&self, off: u16) -> u32 {
        self.child.read32(off)
    }
    fn cfg_write32(&self, off: u16, v: u32) {
        let _ = self.child.write32(off, v);
    }
    fn cfg_read16(&self, off: u16) -> u16 {
        self.child.read16(off)
    }
    fn cfg_write16(&self, off: u16, v: u16) {
        let _ = self.child.write16(off, v);
    }
    fn save_config(&self) {
        *self.saved.lock() = Some(self.child.save_state());
    }
    fn restore_config(&self) {
        if let Some(s) = self.saved.lock().take() {
            self.child.restore_state(&s);
        }
    }
    fn delay_us(&self, us: u32) {
        bus::manager().delay_us(u64::from(us));
    }
}

/// DMA memory as host-order words.
struct Words(DmaCoherent<u32>);

impl ring::Words for Words {
    fn get(&self, w: usize) -> u32 {
        u32::from_le(self.0.read(w))
    }
    fn set(&self, w: usize, v: u32) {
        self.0.write(w, v.to_le());
    }
}

/// The kernel's barriers.
struct Hal;

impl ring::Barriers for Hal {
    fn write(&self) {
        hal::dma::write_barrier();
    }
    fn read(&self) {
        hal::dma::read_barrier();
    }
}

fn slot_bus(chunks: &[DmaCoherent<u8>], slot: usize) -> u64 {
    chunks[slot / SLOTS_PER_CHUNK].bus_addr() + ((slot % SLOTS_PER_CHUNK) * ring::SLOT) as u64
}

struct Rx {
    std: StdRing,
    std_descs: Words,
    ret: ReturnRing,
    ret_descs: Words,
    chunks: Vec<DmaCoherent<u8>>,
}

struct Tx {
    ring: TxRing,
    descs: Words,
    slots: Vec<DmaCoherent<u8>>,
    wd: TxWatchdog,
}

/// What the `core` mutex holds.
struct Core {
    /// The link as last seen.
    link: PhyStatus,
    /// Reinitializations failed in a row.
    failures: u32,
}

/// What the interface and the interrupt share.
pub struct Shared {
    chip: Chip<Bar>,
    phy: Phy,
    rx: SpinLock<Rx>,
    tx: SpinLock<Tx>,
    status: Words,
    rings: Rings,
    port: Arc<Port>,
    filter: SpinLock<(u32, Vec<[u8; 6]>)>,
    unit: u32,
    lladdr: [u8; 6],
    state: AtomicU8,
    /// The link as the `core` lock holder last saw it, for senders and
    /// media queries, which do not take that lock.
    link: SpinLock<PhyStatus>,
    core: Mutex<Core>,
    wake: WaitQueue,
}

impl core::fmt::Debug for Shared {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Shared")
            .field("chip", &self.chip.id)
            .finish_non_exhaustive()
    }
}

/// MII access through the chip.
struct ChipMii<'a>(&'a Chip<Bar>);

impl Mii for ChipMii<'_> {
    fn read(&self, reg: u8) -> Option<u16> {
        self.0.mii_read(reg).ok()
    }
    fn write(&self, reg: u8, v: u16) -> Option<()> {
        self.0.mii_write(reg, v).ok()
    }
    fn delay_us(&self, us: u32) {
        self.0.regs.delay_us(us);
    }
}

/// A received frame's packet header: what the chip checked.
fn rx_hdr(c: ring::RxCsum) -> PktHdr {
    let mut hdr = PktHdr::default();
    if c.ip_checked {
        hdr.csum_flags |= csum::IP_CHECKED;
    }
    if c.ip_valid {
        hdr.csum_flags |= csum::IP_VALID;
    }
    if let Some(v) = c.l4 {
        hdr.csum_flags |= csum::DATA_VALID | csum::PSEUDO_HDR;
        hdr.csum_data = u32::from(v);
    }
    hdr
}

impl Shared {
    fn state(&self) -> State {
        State::of(self.state.load(Ordering::Acquire))
    }

    fn set_state(&self, _tx: &mut Tx, s: State) -> State {
        State::of(self.state.swap(s as u8, Ordering::AcqRel))
    }

    fn log(&self, msg: core::fmt::Arguments<'_>) {
        bus::manager().log(format_args!("bge{}: {msg}", self.unit));
    }

    /// Programs the receive filter (`bge_setpromisc`, `bge_setmulti`).
    fn program_filter(&self, (fl, groups): &(u32, Vec<[u8; 6]>)) {
        let promisc = fl & iff::PROMISC != 0;
        let hash = if promisc || fl & iff::ALLMULTI != 0 {
            [u32::MAX; 4]
        } else {
            multicast_hash(groups)
        };
        self.chip.set_filter(promisc, hash);
    }

    fn apply_filter(&self) {
        let f = self.filter.lock();
        if self.state() == State::Running {
            self.program_filter(&f);
        }
    }

    /// `bge_init_locked` (5411-5586), from any state: stop, reset, chip and
    /// block init, fresh rings, the MAC, the filter, interrupts, and
    /// autonegotiation.
    fn init(&self, core: &mut Core) -> Result<(), HwError> {
        {
            let mut tx = self.tx.lock();
            if self.state() == State::Stopped {
                return Ok(());
            }
            self.set_state(&mut tx, State::Down);
            tx.wd.clear();
        }
        self.chip.stop();
        if let Err(e) = self.chip.reset() {
            self.log(format_args!("{e}"));
        }
        self.chip.chip_init(true);
        self.chip.block_init(&self.rings, self.lladdr)?;
        {
            let mut rx = self.rx.lock();
            let Rx {
                std,
                std_descs,
                ret,
                chunks,
                ..
            } = &mut *rx;
            let prod = std.fill(std_descs, &Hal, |s| slot_bus(chunks, s));
            ret.cons = 0;
            self.chip.regs.write32(r::MBX_RX_STD_PROD, prod);
        }
        {
            let mut tx = self.tx.lock();
            tx.ring.reset();
            tx.wd.clear();
            self.chip.regs.write32(r::MBX_TX_HOST_PROD0, 0);
            self.chip.regs.write32(r::MBX_TX_NIC_PROD0, 0);
        }
        self.chip.start_mac(self.lladdr, ETHERMTU);
        self.program_filter(&self.filter.lock());
        self.chip.enable_interrupts();
        core.link = PhyStatus::Down;
        *self.link.lock() = PhyStatus::Down;
        let mii = ChipMii(&self.chip);
        if self.phy.autonegotiate(&mii, false).is_err() {
            self.log(format_args!("PHY did not answer"));
        }
        self.chip.kick();
        {
            let mut tx = self.tx.lock();
            if self.state() == State::Down {
                self.set_state(&mut tx, State::Running);
            }
        }
        self.apply_filter();
        Ok(())
    }

    fn reinit(&self, core: &mut Core) {
        match self.init(core) {
            Ok(()) => core.failures = 0,
            Err(e) => {
                core.failures += 1;
                if nicwatch::report(u64::from(core.failures)) {
                    self.log(format_args!(
                        "{}: {e:?} ({} in a row)",
                        nicwatch::MSG_INIT_FAILED,
                        core.failures
                    ));
                }
            }
        }
    }

    /// Reads the PHY and, on a change, sets the MAC up for the link and
    /// tells the stack (`bge_link_upd`, `bge_miibus_statchg`).
    fn link_update(&self, core: &mut Core) -> Option<bool> {
        let mii = ChipMii(&self.chip);
        let st = self.phy.status(&mii).ok()?;
        self.chip.ack_link();
        if st == core.link {
            return None;
        }
        core.link = st;
        *self.link.lock() = st;
        if let PhyStatus::Up {
            speed,
            full_duplex,
            tx_pause,
            rx_pause,
            ..
        } = st
        {
            self.chip.set_link(LinkMode {
                gigabit: speed == Speed::Thousand,
                full_duplex,
                tx_pause,
                rx_pause,
            });
        }
        Some(matches!(st, PhyStatus::Up { .. }))
    }

    /// The interrupt thread (`bge_intr_task`).
    fn service(&self, s: &Sleepable) {
        let mut g = self.core.lock(s);
        if self.state() != State::Running {
            return;
        }
        let st = ring::take_status(&self.status, &Hal);
        let link = if st.flags & ring::status::LINKSTATE_CHANGED != 0 {
            self.link_update(&mut g)
        } else {
            None
        };
        self.chip.ack(st.tag);
        let frames = self.rx_take(st.rx_prod);
        self.reap(st.tx_cons);
        drop(g);
        if let Some(up) = link {
            self.port.link_state(up);
        }
        self.deliver(frames);
    }

    fn reap(&self, cons: usize) {
        let mut tx = self.tx.lock();
        let n = tx.ring.reap(cons);
        let pending = tx.ring.in_flight > 0;
        tx.wd.reaped(n > 0, pending);
    }

    /// Takes returned frames up to the chip's producer (`bge_rxeof`).
    fn rx_take(&self, prod: usize) -> Vec<Packet> {
        let mut frames = Vec::new();
        let mut rx = self.rx.lock();
        let Rx {
            std,
            std_descs,
            ret,
            ret_descs,
            chunks,
        } = &mut *rx;
        ring::Barriers::read(&Hal);
        let mut posted = false;
        for p in ret.take(ret_descs, prod, RX_BUDGET) {
            if p.slot >= ring::STD_RX {
                self.port.stats.ierrors.add(1);
                continue;
            }
            let bus = slot_bus(chunks, p.slot);
            let ok = p.flags & ring::rx_flags::ERROR == 0
                && p.len > ring::CRC_LEN
                && p.len <= ring::RX_LEN;
            if ok {
                let at = (p.slot % SLOTS_PER_CHUNK) * ring::SLOT + ring::RX_ALIGN;
                let chunk = &chunks[p.slot / SLOTS_PER_CHUNK];
                let made = Chain::fill(p.len - ring::CRC_LEN, |b, off| {
                    chunk.copy_to_slice(at + off, b);
                    Ok::<usize, NoBufs>(b.len())
                });
                match made {
                    Ok(data) => frames.push(Packet {
                        hdr: rx_hdr(p.csum()),
                        data,
                    }),
                    Err(_) => self.port.stats.iqdrops.add(1),
                }
            } else {
                self.port.stats.ierrors.add(1);
            }
            std.repost(std_descs, p.slot, bus);
            posted = true;
        }
        ring::Barriers::write(&Hal);
        self.chip
            .regs
            .write32(r::MBX_RX_CONS0, u32::try_from(ret.cons).unwrap_or(0));
        if posted {
            self.chip.regs.write32(r::MBX_RX_STD_PROD, std.producer());
        }
        frames
    }

    fn deliver(&self, frames: Vec<Packet>) {
        if frames.is_empty() {
            return;
        }
        let batch = net::defer::begin();
        for m in frames {
            self.port.stats.ipackets.add(1);
            self.port.stats.ibytes.add(m.len() as u64);
            self.port.input(m);
        }
        drop(batch);
    }

    /// The once-a-second tick (`bge_tick`).
    fn tick(&self, s: &Sleepable) {
        let mut g = self.core.lock(s);
        match self.state() {
            State::Running => {}
            State::Down => {
                self.reinit(&mut g);
                return;
            }
            State::Stopped => return,
        }
        let link = self.link_update(&mut g);
        let fire = {
            let mut tx = self.tx.lock();
            tx.wd.tick()
        };
        if fire {
            self.log(format_args!("{}", nicwatch::MSG_TX_TIMEOUT));
            self.port.stats.oerrors.add(1);
            self.reinit(&mut g);
        }
        drop(g);
        if let Some(up) = link {
            self.port.link_state(up);
        }
    }

    /// Sends `m` (`bge_encap`, with the frame copied into its descriptor's
    /// slot).
    fn send(&self, m: Packet) -> Result<(), NoBufs> {
        let len = m.len();
        if len > MAX_FRAME {
            return Err(NoBufs);
        }
        let mcast = m.hdr.flags & (flags::BCAST | flags::MCAST) != 0;
        self.port.tap.mtap(&m, net::bpf::Way::Out);
        {
            let mut tx = self.tx.lock();
            if self.state() != State::Running || !link_is_up(&self.core_link()) {
                return Err(NoBufs);
            }
            if !tx.ring.has_room() {
                return Err(NoBufs);
            }
            let d = tx.ring.push();
            let chunk = &tx.slots[d / SLOTS_PER_CHUNK];
            let mut at = (d % SLOTS_PER_CHUNK) * ring::SLOT;
            for seg in m.data.segments() {
                chunk.copy_from_slice(at, seg);
                at += seg.len();
            }
            if len < ring::MIN_FRAME {
                chunk.copy_from_slice(at, &[0u8; ring::MIN_FRAME][..ring::MIN_FRAME - len]);
            }
            let padded = len.max(ring::MIN_FRAME);
            let bus = slot_bus(&tx.slots, d);
            let padded = u16::try_from(padded).unwrap_or(u16::MAX);
            ring::post_tx(&tx.descs, d, bus, padded, ring::tx_flags::END);
            ring::Barriers::write(&Hal);
            self.chip.regs.write32(
                r::MBX_TX_HOST_PROD0,
                u32::try_from(tx.ring.prod).unwrap_or(0),
            );
            tx.wd.queued();
        }
        let st = &self.port.stats;
        st.opackets.add(1);
        st.obytes.add(len as u64);
        if mcast {
            st.omcasts.add(1);
        }
        Ok(())
    }

    /// The link as the `core` lock last saw it, without taking that lock
    /// (a sender must not wait for the interrupt thread).
    fn core_link(&self) -> PhyStatus {
        *self.link.lock()
    }

    fn media(&self) -> Media {
        let st = self.core_link();
        let mut active = ifm::ETHER;
        let up = link_is_up(&st);
        if let PhyStatus::Up {
            speed, full_duplex, ..
        } = st
        {
            active |= if full_duplex { ifm::FDX } else { ifm::HDX };
            active |= match speed {
                Speed::Ten => ifm::T10,
                Speed::Hundred => ifm::TX100,
                Speed::Thousand => ifm::T1000,
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

fn link_is_up(s: &PhyStatus) -> bool {
    matches!(s, PhyStatus::Up { .. })
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
        Some(link_is_up(&self.0.core_link()))
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

/// A `bgeN` instance.
pub struct Bge {
    shared: Arc<Shared>,
    line: SpinLock<Option<IrqLine>>,
    ticker: SpinLock<Option<Arc<Thread>>>,
    _messages: Messages,
    info: Ether,
}

impl core::fmt::Debug for Bge {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Bge").finish_non_exhaustive()
    }
}

fn filter(_: &Bge, _: usize, _: &IrqCx) -> Filter {
    // One-shot MSI: the chip sends no other until acknowledged.
    Filter::Schedule
}

fn handler(d: &Bge, _: usize, s: &Sleepable) {
    d.shared.service(s);
}

/// The 57765 family (`bge_devs`, the devices whose chip ID is in
/// `GEN15_PRODID_ASICREV`).
const IDS: &[PciId] = &[
    PciId::device(0x14e4, 0x16b0), // BCM57761
    PciId::device(0x14e4, 0x1682), // BCM57762
    PciId::device(0x14e4, 0x16b4), // BCM57765
    PciId::device(0x14e4, 0x1686), // BCM57766
    PciId::device(0x14e4, 0x16b1), // BCM57781
    PciId::device(0x14e4, 0x16b7), // BCM57782
    PciId::device(0x14e4, 0x16b5), // BCM57785
    PciId::device(0x14e4, 0x16b3), // BCM57786
    PciId::device(0x14e4, 0x16b2), // BCM57791
    PciId::device(0x14e4, 0x16b6), // BCM57795
];

impl Driver for Bge {
    const NAME: &'static str = "bge";
    const IDS: Ids = Ids::Pci(IDS);
    const CONCURRENCY: bus::Concurrency = bus::Concurrency::Concurrent;

    fn describe(_: &Ident) -> String {
        "Broadcom NetXtreme Gigabit Ethernet".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let child = cx.bus_data_arc::<PciChild>().ok_or(AttachError::Decline)?;
        let mmio = cx.mmio_bytes(0)?;
        child.enable_memory();
        child.set_bus_master(true);
        let expcap = pci::caps::find(&child.function.caps, pci::caps::id::PCIE)
            .map(|c| u16::from(c.offset))
            .ok_or(AttachError::Device("not PCI Express"))?;
        let unit = cx.unit();
        let log = |a: core::fmt::Arguments<'_>| bus::manager().log(a);
        let bar = Bar {
            mmio,
            child: Arc::clone(&child),
            saved: SpinLock::new(None),
        };
        let hw = |e: HwError| {
            AttachError::Device(match e {
                HwError::NotSupported(_) => "not a BCM57765-family chip",
                HwError::NoAddress => "no Ethernet address",
                HwError::NoHwConfig => "failed to read EEPROM",
                HwError::Fiber => "fiber is not supported",
                _ => "chip did not start",
            })
        };
        let chip = Chip::new(bar, expcap).map_err(hw)?;
        log(format_args!(
            "bge{unit}: CHIP ID {:#010x}; ASIC REV {:#x}; {}",
            chip.id.0,
            chip.id.asicrev(),
            chip.id.name()
        ));
        if let Err(e) = chip.reset() {
            log(format_args!("bge{unit}: {e}"));
        }
        chip.chip_init(true);
        let cfg = chip.hw_config().map_err(hw)?;
        if cfg.fiber() {
            return Err(hw(HwError::Fiber));
        }
        if cfg.asf() {
            log(format_args!(
                "bge{unit}: ASF firmware present; the driver does not hand off to it"
            ));
        }
        let lladdr = chip.station_address().map_err(hw)?;
        let phy = Phy::attach(&ChipMii(&chip)).map_err(|_| AttachError::Device("no PHY"))?;
        log(format_args!(
            "bge{unit}: PHY OUI {:#08x} model {:#x} rev {}",
            phy.id.oui, phy.id.model, phy.id.rev
        ));
        // Everything the chip reads below 4 GiB, so nothing crosses a 4 GiB
        // line; rings and the status block page-aligned.
        let tag = cx.dma_tag(DmaConstraints {
            addr_bits: 32,
            align: 4096,
            ..DmaConstraints::UNLIMITED
        })?;
        let nomem = |_| AttachError::NoMemory;
        let words = |n: usize| tag.coherent::<u32>(n).map(Words).map_err(nomem);
        let std_descs = words(ring::STD_RX * ring::RX_WORDS)?;
        let ret_descs = words(ring::RETURN * ring::RX_WORDS)?;
        let tx_descs = words(ring::TX * ring::TX_WORDS)?;
        let status = words(ring::STATUS_WORDS)?;
        let chunks = |n: usize| {
            (0..n / SLOTS_PER_CHUNK)
                .map(|_| tag.coherent::<u8>(ring::SLOT * SLOTS_PER_CHUNK))
                .collect::<Result<Vec<_>, _>>()
                .map_err(nomem)
        };
        let rx_chunks = chunks(ring::STD_RX)?;
        let tx_slots = chunks(ring::TX)?;
        let rings = Rings {
            std_rx: std_descs.0.bus_addr(),
            // Programmed disabled, never read.
            jumbo_rx: 0,
            rx_return: ret_descs.0.bus_addr(),
            tx: tx_descs.0.bus_addr(),
            status: status.0.bus_addr(),
        };
        let (line, messages) = pci::bus::msi_line(cx).map_err(|_| AttachError::Device("no MSI"))?;
        let caps = ifcap::RXCSUM | ifcap::VLAN_MTU | ifcap::LINKSTATE;
        let info = Ether {
            dname: "bge",
            unit: i32::try_from(unit).unwrap_or(0),
            lladdr,
            mtu: ETHERMTU,
            capabilities: caps,
            capenable: caps,
            hwassist: 0,
            baudrate: 1_000_000_000,
        };
        Ok(Self {
            shared: Arc::new(Shared {
                chip,
                phy,
                rx: SpinLock::new(Rx {
                    std: StdRing::default(),
                    std_descs,
                    ret: ReturnRing::default(),
                    ret_descs,
                    chunks: rx_chunks,
                }),
                tx: SpinLock::new(Tx {
                    ring: TxRing::default(),
                    descs: tx_descs,
                    slots: tx_slots,
                    wd: TxWatchdog::default(),
                }),
                status,
                rings,
                port: Port::new(),
                filter: SpinLock::new((0, Vec::new())),
                unit,
                lladdr,
                state: AtomicU8::new(State::Down as u8),
                link: SpinLock::new(PhyStatus::Down),
                core: Mutex::new(
                    &CORE_LOCK,
                    Core {
                        link: PhyStatus::Down,
                        failures: 0,
                    },
                ),
                wake: WaitQueue::new(),
            }),
            line: SpinLock::new(Some(line)),
            ticker: SpinLock::new(None),
            _messages: messages,
            info,
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        if let Some(line) = self.line.lock().take() {
            cx.interrupt(line, Some(filter), Some(handler), 0)?;
        }
        let sh = &self.shared;
        let s = cx.sleepable();
        {
            let mut g = sh.core.lock(s);
            sh.init(&mut g)
                .map_err(|_| AttachError::Device("chip did not start"))?;
        }
        let name = net::netif::attach_ether(&self.info, &sh.port, Arc::new(Iface(Arc::clone(sh))));
        let m = self.info.lladdr;
        bus::manager().log(format_args!(
            "{name}: Ethernet address: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            m[0], m[1], m[2], m[3], m[4], m[5]
        ));
        let tick = Arc::clone(sh);
        let t = sched::Builder::new(&alloc::format!("{name} tick"))
            .priority(sched::Priority::KERNEL)
            .spawn(move |s| {
                let stopped = || tick.state() == State::Stopped;
                loop {
                    let mut done = stopped;
                    bus::manager().wait_timeout(s, &tick.wake, &mut done, TICK_NS);
                    if stopped() {
                        break;
                    }
                    tick.tick(s);
                }
            })
            .map_err(|_| AttachError::NoMemory)?;
        *self.ticker.lock() = Some(t);
        Ok(())
    }

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        let s = cx.sleepable();
        let sh = &self.shared;
        {
            let mut tx = sh.tx.lock();
            sh.set_state(&mut tx, State::Stopped);
        }
        sh.wake.wake_all();
        if let Some(t) = self.ticker.lock().take() {
            t.join(s);
        }
        net::netif::detach(&sh.port);
        let _core = sh.core.lock(s);
        sh.chip.stop();
        sh.tx.lock().ring.reset();
    }
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<Bge>();
