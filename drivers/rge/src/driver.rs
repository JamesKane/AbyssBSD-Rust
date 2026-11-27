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
//!   link, unmasks it, and takes once more what arrived while it was
//!   masked. A system error reinitializes the chip.
//! - A thread ticks once a second (`rge_tick`): it follows the link,
//!   reaps sent frames, and counts the transmit watchdog down
//!   ([`crate::health`]); a stalled transmitter is reported and the chip
//!   reinitialized (`rge_init_locked`). It also takes received frames
//!   that waited a whole second for an interrupt, and stops touching a
//!   controller whose registers read all ones (gone from the bus).
//! - The `core` mutex serializes the interrupt thread, the tick, and a
//!   reinitialization; senders take only the transmit ring's lock, and
//!   send nothing unless the chip runs ([`State`]), which a reset changes
//!   under that lock first.
//! - A frame to send is copied, padded to 60 bytes (the controller pads
//!   nothing), into the slot of the descriptor it takes: each transmit
//!   descriptor has one, mapped at attach as the receive slots are, so
//!   sending maps nothing. A longer one (jumbo) gets a buffer of its own,
//!   mapped for it, whose segments become its descriptors.
//! - The controller computes the IPv4, TCP, and UDP checksums the stack
//!   leaves to it (`if_hwassist`, `RGE_CSUM_FEATURES`), while transmit
//!   checksums are on, except in a frame shorter than 60 bytes, where
//!   its sums are wrong: those the driver computes before padding.
//!
//! Received frames are copied out of their slots, so DMA memory holds
//! only device payload.
//!
//! Behavior reference: sys/dev/rge/if_rge.c.

use crate::ring::RxCsum;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

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
use sched::lock::{LockClass, Mutex};
use sched::{IrqCx, Sleepable, Thread, WaitQueue};
use sync::SpinLock;
use vm::dma::{DmaCoherent, DmaConstraints, DmaTag, Mapped, dir};

use crate::health::{self, RxWatch, TxWatchdog};
use crate::hw::{Chip, HwError, MacType, Regs};
use crate::regs as r;
use crate::ring::{self, Assembled, Assembler, DESCS, RxRing, SLOT, TxRing, WORDS};

/// Slots per DMA chunk (32 KiB each), on either ring.
const SLOTS_PER_CHUNK: usize = 16;
/// Frames taken per pass of the interrupt thread.
const RX_BUDGET: usize = 256;
/// `ETHERMTU`.
const ETHERMTU: u32 = 1500;
/// The longest frame sent: a VLAN-tagged jumbo frame without its CRC.
const MAX_FRAME: usize = r::JUMBO_FRAMELEN as usize - ring::CRC_LEN;
/// The tick's period (`callout_reset(..., hz, rge_tick, sc)`).
const TICK_NS: u64 = 1_000_000_000;
/// Receive mitigation: an interrupt that finds frames within
/// [`POLL_GAP_NS`] of the last one's end keeps the controller masked and
/// looks at the ring every [`POLL_NS`] (at most [`POLL_MAX`] times, then
/// reads the status again) while frames keep coming. A busy receiver then
/// takes a few frames at a time for one interrupt's cost in register
/// reads, as a controller's interrupt timer would give.
const POLL_NS: u64 = 30_000;
const POLL_GAP_NS: u64 = 40_000;
const POLL_MAX: u32 = 64;
/// How often the doorbell watch looks while frames are in flight, and how
/// old the oldest may be before it rings again. A frame takes some tens of
/// microseconds to go; one older than this has been left in the ring.
const WATCH_NS: u64 = 1_000_000;
/// The longest wait between attempts to reinitialize a chip that failed
/// to: 2^6 = 64 ticks.
const RETRY_SHIFT: u64 = 6;

static CORE_LOCK: LockClass = LockClass::new("rge core", 500);

/// Whether the chip runs (`IFF_DRV_RUNNING`, `sc_stopped`).
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
    /// A slot's bytes on their way out of the DMA memory, kept between
    /// calls: allocating (and zeroing) one for every call cost more than
    /// the frames it took (about 1.8 µs against 0.4 µs a frame).
    scratch: Vec<u8>,
    /// Descriptors taken so far, for the stall check.
    taken: u64,
}

/// Slot `slot`'s bus address.
fn slot_bus(chunks: &[DmaCoherent<u8>], slot: usize) -> u64 {
    chunks[slot / SLOTS_PER_CHUNK].bus_addr() + ((slot % SLOTS_PER_CHUNK) * SLOT) as u64
}

impl Rx {
    /// Posts every slot again and forgets a frame in progress
    /// (`rge_rx_list_init`).
    fn reset(&mut self) {
        let Self {
            ring,
            descs,
            chunks,
            asm,
            ..
        } = self;
        ring.fill(descs, &ring::Hal, |s| slot_bus(chunks, s));
        asm.clear();
    }

    fn slot_copy(&self, slot: usize, dst: &mut [u8]) {
        self.chunks[slot / SLOTS_PER_CHUNK].copy_to_slice((slot % SLOTS_PER_CHUNK) * SLOT, dst);
    }
}

/// A received frame's packet header: what the controller checked.
fn rx_hdr(c: &RxCsum) -> PktHdr {
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
    hdr
}

struct Tx {
    ring: TxRing,
    descs: Descs,
    /// Each descriptor's slot, mapped for the binding's life: a frame
    /// that fits is copied into its first descriptor's.
    slots: Vec<DmaCoherent<u8>>,
    /// Each in-flight longer frame's buffer, by its first descriptor.
    sent: Vec<Option<Mapped<Vec<u8>, dir::ToDevice>>>,
    /// `sc_watchdog`.
    wd: TxWatchdog,
    /// Which push each in-flight frame was, by its first descriptor, for
    /// the doorbell watch (a count, not a clock read, for every packet).
    stamp: Vec<u64>,
    /// Frames pushed so far.
    pushes: u64,
}

impl Tx {
    /// Frees what the controller has sent, and rearms or disarms the
    /// watchdog (`rge_txeof`).
    fn reap(&mut self, port: &Port) {
        let mut progress = false;
        while let Some((first, ok)) = self.ring.reap(&self.descs, &ring::Hal) {
            progress = true;
            if let Some(m) = self.sent[first].take() {
                drop(m.unmap());
            }
            if !ok {
                port.stats.oerrors.add(1);
            }
        }
        self.wd.reaped(progress, self.ring.in_flight() > 0);
    }

    /// Frees every frame and empties the ring (`rge_stop_locked`,
    /// `rge_tx_list_init`), the controller stopped.
    fn reset(&mut self) {
        for m in self.sent.iter_mut().filter_map(Option::take) {
            drop(m.unmap());
        }
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
    tag: DmaTag,
    rx: SpinLock<Rx>,
    tx: SpinLock<Tx>,
    port: Arc<Port>,
    /// The interface's flags and multicast groups, for the filter.
    filter: SpinLock<(u32, Vec<[u8; 6]>)>,
    unit: u32,
    lladdr: [u8; 6],
    /// The rings' bus addresses.
    rx_bus: u64,
    tx_bus: u64,
    /// A [`State`]; changed under the transmit ring's lock.
    state: AtomicU8,
    /// Serializes the interrupt thread, the tick, and reinitialization.
    core: Mutex<Core>,
    /// Wakes the tick thread to stop.
    wake: WaitQueue,
    /// The interrupt thread's wait between looks at the ring.
    pollq: WaitQueue,
    /// When the last service ended (`sched::now_ns`).
    last_end: core::sync::atomic::AtomicU64,
    /// Frames may be in flight that the controller has not been told of
    /// (or has missed): the tick thread looks every [`WATCH_NS`].
    watch: AtomicBool,
    /// Frames the watch had to ring for again.
    rekicks: AtomicU64,
    /// The oldest frame in flight at the watch's last look (its first
    /// descriptor and push count), to tell a stuck frame from a busy ring.
    seen: SpinLock<Option<(usize, u64)>>,
}

impl core::fmt::Debug for Shared {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Shared")
            .field("mac", &self.chip.mac)
            .finish_non_exhaustive()
    }
}

impl Shared {
    fn state(&self) -> State {
        State::of(self.state.load(Ordering::Acquire))
    }

    /// Sets the state, the transmit ring's lock held (`_tx`), so no
    /// sender is between its check and the doorbell; the old state.
    fn set_state(&self, _tx: &mut Tx, s: State) -> State {
        State::of(self.state.swap(s as u8, Ordering::AcqRel))
    }

    /// Prints `msg` after the device's name, as `device_printf`.
    fn log(&self, msg: core::fmt::Arguments<'_>) {
        bus::manager().log(format_args!("rge{}: {msg}", self.unit));
    }

    /// Programs the receive filter from the interface's state, the chip
    /// running.
    fn apply_filter(&self) {
        let f = self.filter.lock();
        if self.state() == State::Running {
            self.program_filter(&f);
        }
    }

    /// Programs the receive filter (`rge_iff_locked`).
    fn program_filter(&self, (fl, groups): &(u32, Vec<[u8; 6]>)) {
        let promisc = fl & iff::PROMISC != 0;
        let hash = if promisc || fl & iff::ALLMULTI != 0 {
            [u32::MAX; 2]
        } else {
            crate::multicast_hash(groups)
        };
        self.chip.set_filter(promisc, hash);
    }

    /// Whether the link is up, without touching a chip gone from the bus.
    fn link_up(&self) -> bool {
        matches!(self.state(), State::Running | State::Down) && self.chip.link_up()
    }

    /// The controller left the bus: says so once, and nothing touches it
    /// again. The caller then reports the link down, the `core` lock let
    /// go.
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

    /// Reinitializes the chip with fresh rings (`rge_init_locked`), the
    /// `core` lock held: the interrupt thread and the tick wait, senders
    /// see it down and send nothing.
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
        let r = self
            .chip
            .init_locked(self.lladdr, self.rx_bus, self.tx_bus, true, || {
                self.tx.lock().reset();
                self.rx.lock().reset();
            });
        if let Err(e) = r {
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
        self.chip.start();
        {
            // Unless detach began meanwhile.
            let mut tx = self.tx.lock();
            if self.state() == State::Down {
                self.set_state(&mut tx, State::Running);
            }
        }
        // Again, for a change made while the chip was down.
        self.apply_filter();
    }

    /// The interrupt thread's work (`rge_intr`), which the tick also runs
    /// for frames that waited too long: acknowledges what is pending,
    /// takes received frames (delivered with the `core` lock let go),
    /// reaps sent ones, reinitializes on a system error, unmasks the
    /// controller, and takes what arrived before the unmask. A controller
    /// that stopped running stays masked.
    fn service(&self, s: &Sleepable) {
        let mut g = self.core.lock(s);
        let mut link = false;
        let begin = sched::now_ns();
        let busy = begin.saturating_sub(self.last_end.load(Ordering::Relaxed)) < POLL_GAP_NS;
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
            if st & r::INTRS == 0 {
                self.chip.unmask();
                // Collects again once unmasked (`rge_intr_msi`, "to
                // avoid the possible race introduced by changing
                // interrupt masks"): a frame that arrived after the last
                // acknowledgement, while masked, raises no interrupt when
                // the mask is lifted, and would wait for the next one.
                let (frames, more) = self.rx_take();
                self.tx.lock().reap(&self.port);
                if frames.is_empty() {
                    break;
                }
                drop(g);
                self.deliver(frames);
                g = self.core.lock(s);
                if !more {
                    break;
                }
                // A full budget: service the rest masked, as above.
                self.chip.mask();
                continue;
            }
            let mut took = 0;
            loop {
                let (frames, more) = self.rx_take();
                took += frames.len();
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
            self.tx.lock().reap(&self.port);
            link |= st & r::ISR_LINKCHG != 0;
            if st & r::ISR_SYSTEM_ERR != 0 {
                self.log(format_args!("{}", health::MSG_SYSTEM_ERR));
                self.reinit(&mut g);
                link = true;
            } else if st & r::ISR_LINKCHG == 0 {
                if took > 0 && busy {
                    // Masked still: look again shortly, without a status
                    // read or an unmask in between.
                    for _ in 0..POLL_MAX {
                        drop(g);
                        let mut stop = || self.state() != State::Running;
                        bus::manager().wait_timeout(s, &self.pollq, &mut stop, POLL_NS);
                        g = self.core.lock(s);
                        if self.state() != State::Running {
                            break;
                        }
                        let (frames, more) = self.rx_take();
                        let got = frames.len();
                        drop(g);
                        self.deliver(frames);
                        g = self.core.lock(s);
                        self.tx.lock().reap(&self.port);
                        if got == 0 {
                            break;
                        }
                        if more {
                            sched::yield_now();
                        }
                    }
                }
            }
        }
        drop(g);
        self.last_end.store(sched::now_ns(), Ordering::Relaxed);
        if link {
            self.port.link_state(self.link_up());
        }
    }

    /// A frame was queued and the doorbell rung: the tick thread is to
    /// look at it shortly (a doorbell written as the controller finishes
    /// polling its ring can be lost, and the frame then waits for the
    /// next one: a connection attempt's one SYN, a second).
    fn arm_watch(&self) {
        // Relaxed: the flag is a hint, and the lock held by the caller
        // orders the frame itself.
        if !self.watch.load(Ordering::Relaxed) && !self.watch.swap(true, Ordering::Relaxed) {
            self.wake.wake_all();
        }
    }

    /// The doorbell watch, once every [`WATCH_NS`] while frames are in
    /// flight: rings again if the oldest is the one it saw last time (so
    /// nothing has taken it for that long), and stands down with the ring
    /// empty.
    fn watch_doorbell(&self) {
        let mut tx = self.tx.lock();
        tx.reap(&self.port);
        let Some(first) = tx.ring.oldest() else {
            // A sender queuing a frame sets the flag again under this
            // lock.
            self.watch.store(false, Ordering::Relaxed);
            *self.seen.lock() = None;
            return;
        };
        // A busy ring's oldest frame changes from look to look.
        let me = (first, tx.stamp[first]);
        if self.seen.lock().replace(me) == Some(me) {
            self.chip.kick_tx();
            let n = self.rekicks.fetch_add(1, Ordering::Relaxed) + 1;
            if n <= 8 {
                self.log(format_args!(
                    "transmit doorbell rung again for a frame left in the ring ({n} so far)"
                ));
            }
        }
    }

    /// The once-a-second tick (`rge_tick`): retries a failed
    /// reinitialization, checks the controller still answers, follows
    /// the link, reaps sent frames and counts the watchdog down, and
    /// takes received frames that waited a whole tick.
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
            tx.reap(&self.port);
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
            let mut buf = core::mem::take(&mut rx.scratch);
            while frames.len() < RX_BUDGET {
                let Some(p) = ({
                    let Rx { ring, descs, .. } = &mut *rx;
                    ring.take(descs, &ring::Hal)
                }) else {
                    break;
                };
                rx.taken += 1;
                // A frame in one piece (the usual): from its slot straight
                // into mbufs, one copy and no buffer between.
                if p.first && p.last && !p.error && p.len > ring::CRC_LEN && !rx.asm.open() {
                    let at = (p.slot % SLOTS_PER_CHUNK) * SLOT;
                    let chunk = &rx.chunks[p.slot / SLOTS_PER_CHUNK];
                    let made = Chain::fill(p.len - ring::CRC_LEN, |b, off| {
                        chunk.copy_to_slice(at + off, b);
                        Ok::<usize, NoBufs>(b.len())
                    });
                    let bus = slot_bus(&rx.chunks, p.slot);
                    RxRing::post(&rx.descs, &ring::Hal, p.slot, bus);
                    match made {
                        Ok(data) => frames.push(Packet {
                            hdr: rx_hdr(&RxCsum::of(p.ext)),
                            data,
                        }),
                        Err(_) => self.port.stats.iqdrops.add(1),
                    }
                    continue;
                }
                rx.slot_copy(p.slot, &mut buf[..p.len]);
                let bus = slot_bus(&rx.chunks, p.slot);
                RxRing::post(&rx.descs, &ring::Hal, p.slot, bus);
                match rx.asm.add(&p, &buf[..p.len]) {
                    Assembled::More => {}
                    Assembled::Dropped => self.port.stats.ierrors.add(1),
                    Assembled::Frame(f, c) => {
                        let Ok(data) = Chain::from_slice(&f) else {
                            self.port.stats.iqdrops.add(1);
                            continue;
                        };
                        frames.push(Packet {
                            hdr: rx_hdr(&c),
                            data,
                        });
                    }
                }
            }
            rx.scratch = buf;
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

    /// Sends frame `m` (`rge_encap`, with the frame copied).
    fn send(&self, m: Packet) -> Result<(), NoBufs> {
        let len = m.len();
        if len > MAX_FRAME {
            return Err(NoBufs);
        }
        let mcast = m.hdr.flags & (flags::BCAST | flags::MCAST) != 0;
        self.port.tap.mtap(&m, net::bpf::Way::Out);
        if m.hdr.csum_flags & csum::IP_TSO != 0 {
            self.send_tso(m)?;
        } else if len <= SLOT {
            self.send_slotted(&m)?;
        } else {
            let ext = ring::tx::csum_ext(m.hdr.csum_flags);
            self.send_mapped(m, ext, 0)?;
        }
        let st = &self.port.stats;
        st.opackets.add(1);
        st.obytes.add(len as u64);
        if mcast {
            st.omcasts.add(1);
        }
        Ok(())
    }

    /// Sends `m`, no longer than a slot, from the slot of the descriptor
    /// it takes: one copy, and no mapping.
    fn send_slotted(&self, m: &Packet) -> Result<(), NoBufs> {
        let len = m.len();
        // The controller does not pad short frames.
        let padded = len.max(ring::MIN_FRAME);
        let mut tx = self.tx.lock();
        // Nothing reaches a chip that is not running (`sc_stopped`).
        if self.state() != State::Running {
            return Err(NoBufs);
        }
        if tx.ring.free() == 0 {
            tx.reap(&self.port);
            if tx.ring.free() == 0 {
                return Err(NoBufs);
            }
        }
        // The next descriptor is free, and so is its slot.
        let d = tx.ring.next();
        let chunk = &tx.slots[d / SLOTS_PER_CHUNK];
        let mut at = (d % SLOTS_PER_CHUNK) * SLOT;
        let mut ext = ring::tx::csum_ext(m.hdr.csum_flags);
        if len < ring::MIN_FRAME {
            // The controller's checksums are wrong below 60 bytes: done
            // here, then padded.
            let mut b = [0u8; ring::MIN_FRAME];
            m.data.copy_out(0, &mut b[..len]);
            if ext != 0 {
                ring::short_csum(&mut b[..len], m.hdr.csum_flags, m.hdr.csum_data);
                ext = 0;
            }
            chunk.copy_from_slice(at, &b);
        } else {
            for seg in m.data.segments() {
                chunk.copy_from_slice(at, seg);
                at += seg.len();
            }
        }
        let seg = (
            slot_bus(&tx.slots, d),
            u32::try_from(padded).unwrap_or(u32::MAX),
        );
        let Tx { ring, descs, .. } = &mut *tx;
        let first = ring
            .push(descs, &ring::Hal, &[seg], ext, 0)
            .map_err(|_| NoBufs)?;
        tx.pushes += 1;
        tx.stamp[first] = tx.pushes;
        tx.wd.queued();
        self.chip.kick_tx();
        self.arm_watch();
        Ok(())
    }

    /// Sends a TCP segment of IPv4 that the controller cuts into frames
    /// (`CSUM_TSO`; its TCP checksum field holds the pseudo-header's sum
    /// without the length, which the stack leaves for TSO): the command
    /// and extended status words of every descriptor carry the TCP
    /// header's offset and the segment size. A frame the controller
    /// cannot take is dropped.
    fn send_tso(&self, m: Packet) -> Result<(), NoBufs> {
        // Ethernet, then an IPv4 header whose length gives the TCP
        // header's offset (a VLAN tag the device inserts would move it).
        let mut h = [0u8; 34];
        if m.len() < h.len() || m.hdr.flags & flags::VLANTAG != 0 {
            return Err(NoBufs);
        }
        m.data.copy_out(0, &mut h);
        let ip_hlen = usize::from(h[14] & 0x0f) * 4;
        let is_tcp4 = h[12..14] == [0x08, 0x00] && h[14] >> 4 == 4 && h[23] == 6;
        let Some((cmd, ext)) = is_tcp4
            .then(|| ring::tx::tso4(14 + ip_hlen, u32::from(m.hdr.tso_segsz)))
            .flatten()
        else {
            return Err(NoBufs);
        };
        self.send_mapped(m, ext, cmd)
    }

    /// Sends a longer `m` (a jumbo frame, or a TSO segment) from a
    /// buffer of its own, mapped for it. `ext` and `frame_cmd` go in
    /// every descriptor.
    fn send_mapped(&self, m: Packet, ext: u32, frame_cmd: u32) -> Result<(), NoBufs> {
        let mut buf = Vec::with_capacity(m.len());
        for seg in m.data.segments() {
            buf.extend_from_slice(seg);
        }
        drop(m);
        let data = self.tag.map::<dir::ToDevice, _>(buf).map_err(|_| NoBufs)?;
        let segs: Vec<(u64, u32)> = data
            .segments()
            .iter()
            .map(|s| (s.bus, u32::try_from(s.len).unwrap_or(u32::MAX)))
            .collect();
        let mut tx = self.tx.lock();
        if self.state() != State::Running {
            drop(tx);
            drop(data.unmap());
            return Err(NoBufs);
        }
        if tx.ring.free() < segs.len() {
            tx.reap(&self.port);
        }
        let first = {
            let Tx { ring, descs, .. } = &mut *tx;
            ring.push(descs, &ring::Hal, &segs, ext, frame_cmd)
        };
        let Ok(first) = first else {
            drop(tx);
            drop(data.unmap());
            return Err(NoBufs);
        };
        tx.sent[first] = Some(data);
        tx.pushes += 1;
        tx.stamp[first] = tx.pushes;
        tx.wd.queued();
        self.chip.kick_tx();
        self.arm_watch();
        Ok(())
    }

    /// The media words `rge_ifmedia_sts` reports.
    fn media(&self) -> Media {
        let st = if self.state() == State::Gone {
            0
        } else {
            self.chip.phystat()
        };
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

/// An `rgeN` instance.
pub struct Rge {
    shared: Arc<Shared>,
    line: SpinLock<Option<IrqLine>>,
    /// The tick thread.
    ticker: SpinLock<Option<Arc<Thread>>>,
    _messages: Messages,
    info: Ether,
}

impl core::fmt::Debug for Rge {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Rge").finish_non_exhaustive()
    }
}

fn filter(d: &Rge, _: usize, _: &IrqCx) -> Filter {
    if d.shared.state() == State::Gone {
        return Filter::Handled;
    }
    d.shared.chip.mask();
    Filter::Schedule
}

fn handler(d: &Rge, _: usize, s: &Sleepable) {
    d.shared.service(s);
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
    // The RTL8126 and the RTL8125 (the X870E's NIC is an RTL8125D, hardware
    // revision 0x688). Attach says which revisions it can program; the
    // other RTL8125 revisions and the RTL8127 come with their programming.
    const IDS: Ids = Ids::Pci(&[PciId::device(0x10ec, 0x8126), PciId::device(0x10ec, 0x8125)]);
    const CONCURRENCY: bus::Concurrency = bus::Concurrency::Concurrent;

    fn describe(ident: &Ident) -> String {
        match ident {
            Ident::Pci(i, _, _) if i.device == 0x8125 => "RTL8125".into(),
            _ => "RTL8126".into(),
        }
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let child = cx.bus_data_arc::<PciChild>().ok_or(AttachError::Decline)?;
        let bar = Bar(Arc::new(cx.mmio_bytes(0)?));
        child.enable_memory();
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
        let slots = || {
            (0..DESCS / SLOTS_PER_CHUNK)
                .map(|_| tag.coherent::<u8>(SLOT * SLOTS_PER_CHUNK))
                .collect::<Result<Vec<_>, _>>()
                .map_err(nomem)
        };
        let chunks = slots()?;
        let tx_slots = slots()?;
        let mut chip = Chip::new(bar, mac);
        let hw = |e| {
            AttachError::Device(match e {
                HwError::PhyResetTimeout => "PHY did not reset",
                HwError::ResetTimeout => "controller did not respond",
            })
        };
        chip.chip_init().map_err(hw)?;
        let lladdr = station(chip.get_macaddr(), unit);
        let mut rx = Rx {
            ring: RxRing::new(),
            descs: rx_descs,
            chunks,
            asm: Assembler::default(),
            scratch: alloc::vec![0u8; SLOT],
            taken: 0,
        };
        let mut tx = Tx {
            ring: TxRing::new(),
            descs: tx_descs,
            slots: tx_slots,
            sent: (0..DESCS).map(|_| None).collect(),
            wd: TxWatchdog::default(),
            stamp: alloc::vec![0u64; ring::DESCS],
            pushes: 0,
        };
        let (rx_bus, tx_bus) = (rx.descs.0.bus_addr(), tx.descs.0.bus_addr());
        // `rge_init_locked`: from a stopped chip, with fresh rings, as a
        // reinitialization does.
        chip.init_locked(lladdr, rx_bus, tx_bus, true, || {
            rx.reset();
            tx.reset();
        })
        .map_err(hw)?;
        chip.set_filter(false, [0; 2]);
        let (line, messages) = pci::bus::msi_line(cx).map_err(|_| AttachError::Device("no MSI"))?;
        // TSO for IPv4 (never on the first RTL8125, whose users reported
        // transmit timeouts with it).
        let tso = crate::tso_wanted(chip.mac);
        let caps = ifcap::RXCSUM
            | ifcap::TXCSUM
            | ifcap::VLAN_MTU
            | ifcap::LINKSTATE
            | if tso { ifcap::TSO4 } else { 0 };
        let info = Ether {
            dname: "rge",
            unit: i32::try_from(unit).unwrap_or(0),
            lladdr,
            mtu: ETHERMTU,
            capabilities: caps,
            capenable: caps,
            // `RGE_CSUM_FEATURES`.
            hwassist: csum::IP | csum::IP_TCP | csum::IP_UDP | if tso { csum::IP_TSO } else { 0 },
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
                pollq: WaitQueue::new(),
                last_end: core::sync::atomic::AtomicU64::new(0),
                watch: AtomicBool::new(false),
                rekicks: AtomicU64::new(0),
                seen: SpinLock::new(None),
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
        sh.chip.start();
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
                let mut last_tick = sched::now_ns();
                loop {
                    let armed = tick.watch.load(Ordering::Relaxed);
                    let period = if armed { WATCH_NS } else { TICK_NS };
                    // A sleep planned for the tick ends when the watch is
                    // armed: a wake between the decision and the sleep
                    // would otherwise be lost.
                    let mut done = || stopped() || (!armed && tick.watch.load(Ordering::Relaxed));
                    bus::manager().wait_timeout(s, &tick.wake, &mut done, period);
                    if stopped() {
                        break;
                    }
                    let now = sched::now_ns();
                    if tick.watch.load(Ordering::Relaxed) {
                        tick.watch_doorbell();
                    }
                    if now.saturating_sub(last_tick) >= TICK_NS {
                        last_tick = now;
                        tick.tick(s);
                    }
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
pub const DRIVER: DriverDecl = DriverDecl::of::<Rge>();
