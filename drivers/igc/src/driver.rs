// SPDX-License-Identifier: BSD-2-Clause
//! `igcN`: the PCI driver and its interface.
//!
//! - attach maps the registers (the first memory BAR), resets the
//!   controller, takes the station address its NVM loaded (or makes a
//!   local one), allocates the two rings below 4 GiB and a buffer for each
//!   descriptor, sets the link going (`igc_init_hw_base`), starts the
//!   queues, and takes one MSI.
//! - start installs the interrupt and attaches the interface. The chip
//!   runs while the driver is attached, as `vtnet` and `rge` do.
//! - The interrupt's filter masks the controller; its thread reads what is
//!   pending (which clears it), takes received frames, reaps sent ones,
//!   follows the link, unmasks, and takes once more what arrived while it
//!   was masked. A device reset or a fatal error reinitializes the chip.
//! - A thread ticks once a second: it follows the link, reaps sent frames,
//!   counts the transmit watchdog down ([`nicwatch`]), takes received
//!   frames that waited a whole second for an interrupt, and stops
//!   touching a controller whose registers read all ones.
//! - The `core` mutex serializes the interrupt thread, the tick, and a
//!   reinitialization; senders take only the transmit ring's lock.
//! - A frame to send is copied into the buffer of the descriptor it takes
//!   (the controller pads short ones), and received frames are copied out
//!   of theirs, so DMA memory holds only device payload.
//!
//! Not done yet: transmit checksum offload and TSO (the stack computes
//! the checksums), more than one queue, MSI-X, jumbo frames, VLAN tag
//! offload, wake on LAN.
//!
//! Behavior reference: sys/dev/igc/if_igc.c.

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
use nicwatch::{self as health, RxWatch, TxWatchdog};
use pci::bus::{Messages, PciChild};
use regs::Bytes;
use sched::lock::{LockClass, Mutex};
use sched::{IrqCx, Sleepable, Thread, WaitQueue};
use sync::SpinLock;
use vm::dma::{DmaCoherent, DmaConstraints, DmaTag};

use crate::hw::{Chip, HwError, LinkState, Regs, Speed};
use crate::regs as r;
use crate::ring::{self, Assembled, Assembler, DESCS, RxRing, SLOT, TxRing, WORDS};

/// Slots per DMA chunk (32 KiB each), on either ring.
const SLOTS_PER_CHUNK: usize = 16;
/// Frames taken per pass of the interrupt thread.
const RX_BUDGET: usize = 256;
/// `ETHERMTU`.
const ETHERMTU: u32 = 1500;
/// The tick's period.
const TICK_NS: u64 = 1_000_000_000;
/// The longest wait between attempts to reinitialize a chip that failed
/// to: 2^6 = 64 ticks.
const RETRY_SHIFT: u64 = 6;

static CORE_LOCK: LockClass = LockClass::new("igc core", 500);

/// Whether the chip runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum State {
    /// Frames flow.
    Running = 0,
    /// Being reinitialized, or failed to be: the tick tries again.
    Down = 1,
    /// Gone from the bus: nothing touches it again.
    Gone = 2,
    /// Detaching.
    Stopped = 3,
}

impl State {
    const fn of(v: u8) -> Self {
        match v {
            0 => Self::Running,
            1 => Self::Down,
            2 => Self::Gone,
            _ => Self::Stopped,
        }
    }
}

/// The registers.
#[derive(Clone, Debug)]
pub struct Bar(Arc<MmioGrant<Bytes>>);

impl Regs for Bar {
    fn read32(&self, off: usize) -> u32 {
        self.0.read32(off)
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

/// Slot `slot`'s bus address.
fn slot_bus(chunks: &[DmaCoherent<u8>], slot: usize) -> u64 {
    chunks[slot / SLOTS_PER_CHUNK].bus_addr() + ((slot % SLOTS_PER_CHUNK) * SLOT) as u64
}

struct Rx {
    ring: RxRing,
    descs: Descs,
    chunks: Vec<DmaCoherent<u8>>,
    asm: Assembler,
    /// Descriptors taken so far, for the stall check.
    taken: u64,
}

impl Rx {
    /// Posts the buffers again and forgets a frame in progress; the tail to
    /// write.
    fn reset(&mut self) -> u32 {
        let Self {
            ring,
            descs,
            chunks,
            asm,
            ..
        } = self;
        let tail = ring.fill(descs, |s| slot_bus(chunks, s));
        asm.clear();
        tail
    }

    fn slot_copy(&self, slot: usize, dst: &mut [u8]) {
        self.chunks[slot / SLOTS_PER_CHUNK].copy_to_slice((slot % SLOTS_PER_CHUNK) * SLOT, dst);
    }
}

struct Tx {
    ring: TxRing,
    descs: Descs,
    /// Each descriptor's buffer: a frame is copied into its first
    /// descriptor's.
    slots: Vec<DmaCoherent<u8>>,
    wd: TxWatchdog,
}

impl Tx {
    /// Frees what the controller has sent and rearms or disarms the
    /// watchdog.
    fn reap(&mut self) {
        let mut progress = false;
        while self.ring.reap(&self.descs).is_some() {
            progress = true;
        }
        self.wd.reaped(progress, self.ring.in_flight() > 0);
    }

    /// Empties the ring, the controller stopped.
    fn reset(&mut self) {
        self.ring.init(&self.descs);
        self.wd.clear();
    }
}

/// What the `core` mutex holds: the tick's memory.
struct Core {
    rx: RxWatch,
    /// Reinitializations failed in a row, and ticks until the next try.
    failures: u64,
    retry_in: u64,
}

/// What the interface and the interrupt share.
pub struct Shared {
    chip: Chip<Bar>,
    rx: SpinLock<Rx>,
    tx: SpinLock<Tx>,
    port: Arc<Port>,
    /// The interface's flags and multicast groups, for the filter.
    filter: SpinLock<(u32, Vec<[u8; 6]>)>,
    unit: u32,
    lladdr: [u8; 6],
    rx_bus: u64,
    tx_bus: u64,
    /// A [`State`]; changed under the transmit ring's lock.
    state: AtomicU8,
    /// Serializes the interrupt thread, the tick, and reinitialization.
    core: Mutex<Core>,
    /// Wakes the tick thread to stop.
    wake: WaitQueue,
}

impl core::fmt::Debug for Shared {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Shared")
            .field("lladdr", &self.lladdr)
            .finish_non_exhaustive()
    }
}

impl Shared {
    fn state(&self) -> State {
        State::of(self.state.load(Ordering::Acquire))
    }

    /// Sets the state, the transmit ring's lock held (`_tx`), so no sender
    /// is between its check and the doorbell; the old state.
    fn set_state(&self, _tx: &mut Tx, s: State) -> State {
        State::of(self.state.swap(s as u8, Ordering::AcqRel))
    }

    fn log(&self, msg: core::fmt::Arguments<'_>) {
        bus::manager().log(format_args!("igc{}: {msg}", self.unit));
    }

    /// Programs the receive filter from the interface's state.
    fn apply_filter(&self) {
        let f = self.filter.lock();
        if self.state() == State::Running {
            self.program_filter(&f);
        }
    }

    fn program_filter(&self, (fl, groups): &(u32, Vec<[u8; 6]>)) {
        self.chip
            .set_filter(fl & iff::PROMISC != 0, fl & iff::ALLMULTI != 0, groups);
    }

    /// Whether the link is up, without touching a chip gone from the bus.
    fn link_up(&self) -> bool {
        matches!(self.state(), State::Running | State::Down) && self.chip.link().up
    }

    /// The controller left the bus: says so once, and nothing touches it
    /// again.
    fn gone(&self) {
        let was = {
            let mut tx = self.tx.lock();
            let was = self.state();
            if matches!(was, State::Running | State::Down) {
                self.set_state(&mut tx, State::Gone);
            }
            was
        };
        if matches!(was, State::Running | State::Down) {
            self.log(format_args!("{}", health::MSG_GONE));
        }
    }

    /// Brings the chip up from reset with fresh rings (`igc_if_init`).
    fn bring_up(&self) -> Result<(), HwError> {
        self.chip.reset()?;
        self.chip.init_hw(self.lladdr)?;
        self.tx.lock().reset();
        let tail = self.rx.lock().reset();
        self.chip.tx_init(self.tx_bus, ring::RING_BYTES);
        self.chip.rx_init(self.rx_bus, ring::RING_BYTES);
        self.chip.rx_tail(tail);
        Ok(())
    }

    /// Reinitializes the chip, the `core` lock held: the interrupt thread
    /// and the tick wait, senders see it down and send nothing.
    fn reinit(&self, core: &mut Core) {
        {
            let mut tx = self.tx.lock();
            if !matches!(self.state(), State::Running | State::Down) {
                return;
            }
            self.set_state(&mut tx, State::Down);
            tx.wd.clear();
        }
        core.rx.clear();
        if !self.chip.responding() {
            self.gone();
            return;
        }
        if let Err(e) = self.bring_up() {
            if !self.chip.responding() {
                self.gone();
                return;
            }
            core.failures += 1;
            core.retry_in = 1u64 << core.failures.min(RETRY_SHIFT);
            if health::report(core.failures) {
                self.log(format_args!(
                    "{}: {e:?} ({} in a row)",
                    health::MSG_INIT_FAILED,
                    core.failures
                ));
            }
            return;
        }
        core.failures = 0;
        self.program_filter(&self.filter.lock());
        {
            // Unless detach began meanwhile.
            let mut tx = self.tx.lock();
            if self.state() == State::Down {
                self.set_state(&mut tx, State::Running);
            }
        }
        self.chip.unmask();
        self.apply_filter();
    }

    /// The interrupt thread's work: reads what is pending, takes received
    /// frames (delivered with the `core` lock let go), reaps sent ones,
    /// follows the link, reinitializes on a device reset or a fatal error,
    /// unmasks, and takes what arrived before the unmask. A controller that
    /// stopped running stays masked.
    fn service(&self, s: &Sleepable) {
        let mut g = self.core.lock(s);
        let mut link = false;
        loop {
            if self.state() != State::Running {
                break;
            }
            let st = self.chip.ack();
            if !health::responding(st) {
                self.gone();
                link = true;
                break;
            }
            if st & (r::IMS_ENABLE | r::ICR_RXO | r::ICR_DRSTA | r::ICR_FER) == 0 {
                self.chip.unmask();
                // Collects again once unmasked: a frame that arrived after
                // the last read, while masked, raises no interrupt when the
                // mask is lifted, and would wait for the next one.
                let (frames, more) = self.rx_take();
                self.tx.lock().reap();
                if frames.is_empty() {
                    break;
                }
                drop(g);
                self.deliver(frames);
                g = self.core.lock(s);
                if !more {
                    break;
                }
                self.chip.mask();
                continue;
            }
            loop {
                let (frames, more) = self.rx_take();
                drop(g);
                self.deliver(frames);
                if more {
                    sched::yield_now();
                }
                g = self.core.lock(s);
                if !more || self.state() != State::Running {
                    break;
                }
            }
            if self.state() != State::Running {
                break;
            }
            self.tx.lock().reap();
            if st & (r::ICR_LSC | r::ICR_RXSEQ) != 0 {
                self.link_changed();
                link = true;
            }
            if st & (r::ICR_DRSTA | r::ICR_FER) != 0 {
                self.log(format_args!("{}", health::MSG_SYSTEM_ERR));
                self.reinit(&mut g);
                link = true;
            }
        }
        drop(g);
        if link {
            self.port.link_state(self.link_up());
        }
    }

    /// A link change: the collision distance, which FreeBSD sets at each
    /// link up.
    fn link_changed(&self) {
        if self.chip.link().up {
            self.chip.link_up_config();
        }
    }

    /// The once-a-second tick: retries a failed reinitialization, checks
    /// the controller still answers, follows the link, reaps sent frames and
    /// counts the watchdog down, and takes received frames that waited a
    /// whole tick.
    fn tick(&self, s: &Sleepable) {
        let mut g = self.core.lock(s);
        match self.state() {
            State::Running => {}
            State::Down => {
                if g.retry_in > 1 {
                    g.retry_in -= 1;
                } else {
                    self.reinit(&mut g);
                }
                return;
            }
            State::Gone | State::Stopped => return,
        }
        if !self.chip.responding() {
            self.gone();
            drop(g);
            self.port.link_state(false);
            return;
        }
        let fire = {
            let mut tx = self.tx.lock();
            tx.reap();
            tx.wd.tick()
        };
        let (waiting, taken) = {
            let rx = self.rx.lock();
            (rx.ring.ready(&rx.descs), rx.taken)
        };
        let stall = g.rx.check(waiting, taken);
        if fire {
            self.log(format_args!("{}", health::MSG_TX_TIMEOUT));
            self.port.stats.oerrors.add(1);
            self.reinit(&mut g);
        }
        drop(g);
        self.port.link_state(self.link_up());
        if let Some(n) = stall
            && !fire
        {
            if health::report(n) {
                self.log(format_args!("{} ({n} so far)", health::MSG_RX_STALL));
            }
            self.service(s);
        }
    }

    /// Takes up to a budget of received frames; whether more may be
    /// waiting.
    fn rx_take(&self) -> (Vec<Packet>, bool) {
        let mut frames: Vec<Packet> = Vec::new();
        {
            let mut rx = self.rx.lock();
            let mut buf = alloc::vec![0u8; SLOT];
            while frames.len() < RX_BUDGET {
                let Some(t) = ({
                    let Rx { ring, descs, .. } = &mut *rx;
                    ring.take(descs)
                }) else {
                    break;
                };
                rx.taken += 1;
                let len = t.len.min(SLOT);
                rx.slot_copy(t.slot, &mut buf[..len]);
                // The buffer is free to go back to the controller.
                let tail = {
                    let Rx {
                        ring,
                        descs,
                        chunks,
                        ..
                    } = &mut *rx;
                    ring.refill(descs, |s| slot_bus(chunks, s))
                };
                self.chip.rx_tail(tail);
                match rx.asm.add(&t, &buf[..len]) {
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
        (frames, more)
    }

    /// Hands received frames to the stack (`if_input`).
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

    /// Sends frame `m`, copied into the buffer of the descriptor it takes.
    fn send(&self, m: Packet) -> Result<(), NoBufs> {
        let len = m.len();
        if len > ring::MAX_FRAME {
            return Err(NoBufs);
        }
        let mcast = m.hdr.flags & (flags::BCAST | flags::MCAST) != 0;
        self.port.tap.mtap(&m, net::bpf::Way::Out);
        let mut tx = self.tx.lock();
        // Nothing reaches a chip that is not running.
        if self.state() != State::Running {
            return Err(NoBufs);
        }
        if tx.ring.free() == 0 {
            tx.reap();
            if tx.ring.free() == 0 {
                return Err(NoBufs);
            }
        }
        // The next descriptor is free, and so is its buffer.
        let d = tx.ring.next();
        let chunk = &tx.slots[d / SLOTS_PER_CHUNK];
        let mut at = (d % SLOTS_PER_CHUNK) * SLOT;
        for seg in m.data.segments() {
            chunk.copy_from_slice(at, seg);
            at += seg.len();
        }
        let seg = (
            slot_bus(&tx.slots, d),
            u32::try_from(len).unwrap_or(u32::MAX),
        );
        let (_, tail) = {
            let Tx { ring, descs, .. } = &mut *tx;
            ring.push(descs, &[seg]).map_err(|_| NoBufs)?
        };
        tx.wd.queued();
        self.chip.tx_tail(tail);
        drop(tx);
        let st = &self.port.stats;
        st.opackets.add(1);
        st.obytes.add(len as u64);
        if mcast {
            st.omcasts.add(1);
        }
        Ok(())
    }

    /// The media words `igc_if_media_status` reports.
    fn media(&self) -> Media {
        let l = if self.state() == State::Gone {
            LinkState {
                up: false,
                speed: Speed::M10,
                full_duplex: false,
            }
        } else {
            self.chip.link()
        };
        let mut active = ifm::ETHER;
        if l.up {
            active |= if l.full_duplex { ifm::FDX } else { ifm::HDX };
            active |= match l.speed {
                Speed::M10 => ifm::T10,
                Speed::M100 => ifm::TX100,
                Speed::M1000 => ifm::T1000,
                Speed::M2500 => ifm::T2500,
            };
        } else {
            active |= ifm::NONE;
        }
        Media {
            current: ifm::ETHER | ifm::AUTO,
            active,
            status: ifm::AVALID | if l.up { ifm::ACTIVE } else { 0 },
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
        Some(self.0.link_up())
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

/// An `igcN` instance.
pub struct Igc {
    shared: Arc<Shared>,
    line: SpinLock<Option<IrqLine>>,
    /// The tick thread.
    ticker: SpinLock<Option<Arc<Thread>>>,
    _messages: Messages,
    info: Ether,
}

impl core::fmt::Debug for Igc {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Igc").finish_non_exhaustive()
    }
}

fn filter(d: &Igc, _: usize, _: &IrqCx) -> Filter {
    if d.shared.state() == State::Gone {
        return Filter::Handled;
    }
    d.shared.chip.mask();
    Filter::Schedule
}

fn handler(d: &Igc, _: usize, s: &Sleepable) {
    d.shared.service(s);
}

/// A valid unicast station address, or a local one from the unit.
pub(crate) fn station(a: [u8; 6], unit: u32) -> [u8; 6] {
    if a != [0; 6] && a[0] & 1 == 0 {
        return a;
    }
    let u = unit.to_be_bytes();
    net::link::random_addr([0x58, 0x9c, u[0], u[1], u[2], u[3]])
}

/// The device IDs FreeBSD's `igc` takes, by PCI device ID.
const DEVICES: [(u16, &str); 16] = [
    (0x15f2, "Intel(R) Ethernet Controller I225-LM"),
    (0x15f3, "Intel(R) Ethernet Controller I225-V"),
    (0x3100, "Intel(R) Ethernet Controller I225-K"),
    (0x15f8, "Intel(R) Ethernet Controller I225-IT"),
    (0x15f7, "Intel(R) Ethernet Controller I220-V"),
    (0x3101, "Intel(R) Ethernet Controller I225-K(2)"),
    (0x5502, "Intel(R) Ethernet Controller I225-LMvP(2)"),
    (0x3102, "Intel(R) Ethernet Controller I226-K"),
    (0x5503, "Intel(R) Ethernet Controller I226-LMvP"),
    (0x0d9f, "Intel(R) Ethernet Controller I225-IT(2)"),
    (0x125b, "Intel(R) Ethernet Controller I226-LM"),
    (0x125c, "Intel(R) Ethernet Controller I226-V"),
    (0x125d, "Intel(R) Ethernet Controller I226-IT"),
    (0x125e, "Intel(R) Ethernet Controller I221-V"),
    (0x125f, "Intel(R) Ethernet Controller I226(blankNVM)"),
    (0x15fd, "Intel(R) Ethernet Controller I225(blankNVM)"),
];

impl Driver for Igc {
    const NAME: &'static str = "igc";
    const IDS: Ids = Ids::Pci(&[
        PciId::device(0x8086, 0x15f2),
        PciId::device(0x8086, 0x15f3),
        PciId::device(0x8086, 0x3100),
        PciId::device(0x8086, 0x15f8),
        PciId::device(0x8086, 0x15f7),
        PciId::device(0x8086, 0x3101),
        PciId::device(0x8086, 0x5502),
        PciId::device(0x8086, 0x3102),
        PciId::device(0x8086, 0x5503),
        PciId::device(0x8086, 0x0d9f),
        PciId::device(0x8086, 0x125b),
        PciId::device(0x8086, 0x125c),
        PciId::device(0x8086, 0x125d),
        PciId::device(0x8086, 0x125e),
        PciId::device(0x8086, 0x125f),
        PciId::device(0x8086, 0x15fd),
    ]);
    const CONCURRENCY: bus::Concurrency = bus::Concurrency::Concurrent;

    fn describe(ident: &Ident) -> String {
        match ident {
            Ident::Pci(i, _, _) => DEVICES
                .iter()
                .find(|(d, _)| *d == i.device)
                .map_or("Intel(R) Ethernet Controller I225", |(_, n)| n)
                .into(),
            _ => "Intel(R) Ethernet Controller I225".into(),
        }
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let child = cx.bus_data_arc::<PciChild>().ok_or(AttachError::Decline)?;
        let bar = Bar(Arc::new(cx.mmio_bytes(0)?));
        child.enable_memory();
        child.set_bus_master(true);
        let unit = cx.unit();
        let chip = Chip::new(bar);
        let hw = |e: HwError| {
            AttachError::Device(match e {
                HwError::NotResponding => "controller does not respond",
                HwError::Semaphore => "PHY semaphore not got",
                HwError::Phy => "PHY did not answer",
            })
        };
        chip.reset().map_err(hw)?;
        let lladdr = station(chip.mac_address(), unit);
        // Descriptor rings below 4 GiB, 128-byte aligned; buffers anywhere.
        let ring_tag = cx.dma_tag(DmaConstraints {
            addr_bits: 32,
            align: 128,
            ..DmaConstraints::UNLIMITED
        })?;
        let tag: DmaTag = cx.dma_tag(DmaConstraints::UNLIMITED)?;
        let nomem = |_| AttachError::NoMemory;
        let rx_descs = Descs(ring_tag.coherent::<u32>(DESCS * WORDS).map_err(nomem)?);
        let tx_descs = Descs(ring_tag.coherent::<u32>(DESCS * WORDS).map_err(nomem)?);
        let slots = || {
            (0..DESCS / SLOTS_PER_CHUNK)
                .map(|_| tag.coherent::<u8>(SLOT * SLOTS_PER_CHUNK))
                .collect::<Result<Vec<_>, _>>()
                .map_err(nomem)
        };
        let chunks = slots()?;
        let tx_slots = slots()?;
        let rx = Rx {
            ring: RxRing::new(),
            descs: rx_descs,
            chunks,
            asm: Assembler::default(),
            taken: 0,
        };
        let tx = Tx {
            ring: TxRing::new(),
            descs: tx_descs,
            slots: tx_slots,
            wd: TxWatchdog::default(),
        };
        let (rx_bus, tx_bus) = (rx.descs.0.bus_addr(), tx.descs.0.bus_addr());
        let (line, messages) = pci::bus::msi_line(cx).map_err(|_| AttachError::Device("no MSI"))?;
        let caps = ifcap::RXCSUM | ifcap::VLAN_MTU | ifcap::LINKSTATE;
        let info = Ether {
            dname: "igc",
            unit: i32::try_from(unit).unwrap_or(0),
            lladdr,
            mtu: ETHERMTU,
            capabilities: caps,
            capenable: caps,
            hwassist: 0,
            baudrate: 2_500_000_000,
        };
        let shared = Arc::new(Shared {
            chip,
            rx: SpinLock::new(rx),
            tx: SpinLock::new(tx),
            port: Port::new(),
            filter: SpinLock::new((0, Vec::new())),
            unit,
            lladdr,
            rx_bus,
            tx_bus,
            state: AtomicU8::new(State::Running as u8),
            core: Mutex::new(
                &CORE_LOCK,
                Core {
                    rx: RxWatch::default(),
                    failures: 0,
                    retry_in: 0,
                },
            ),
            wake: WaitQueue::new(),
        });
        // `igc_if_init`: from the reset chip, with fresh rings.
        shared.bring_up().map_err(hw)?;
        shared.chip.set_filter(false, false, &[]);
        Ok(Self {
            shared,
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
        sh.chip.unmask();
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
                    bus::manager().wait_timeout(s, &tick.wake, &mut { stopped }, TICK_NS);
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
        let was = {
            let mut tx = sh.tx.lock();
            sh.set_state(&mut tx, State::Stopped)
        };
        sh.wake.wake_all();
        let t = self.ticker.lock().take();
        if let Some(t) = t {
            t.join(s);
        }
        net::netif::detach(&sh.port);
        // The interrupt thread done with the chip.
        let _core = sh.core.lock(s);
        if was != State::Gone {
            let _ = sh.chip.stop();
        }
        sh.tx.lock().reset();
        sh.rx.lock().asm.clear();
    }
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<Igc>();
