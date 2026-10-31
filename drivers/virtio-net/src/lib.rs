// SPDX-License-Identifier: BSD-2-Clause
//! The virtio network device (`vtnet`), published as `vtnetN`.
//!
//! Queue pairs: receive queue `2i`, transmit queue `2i + 1`, then the
//! control queue. With `MQ` there are as many pairs as the device allows
//! and the CPUs can use; a packet goes out on the pair its flow hashes to.
//!
//! Receive buffers are fixed slots in DMA memory, each posted to the
//! device as one writable descriptor (with mergeable buffers) or as a
//! header descriptor and a data descriptor (a legacy device without
//! them). A frame the device returns is copied into an mbuf chain, its
//! slot posted again, and the frame handed to Ethernet input in the
//! queue's interrupt thread, a budget of frames per pass.
//!
//! A frame to send is copied into a buffer mapped for the device behind
//! its header slot; completions are reaped on the next send and when the
//! ring fills.
//!
//! The control queue sets promiscuous and all-multicast reception, the
//! address filter, and the number of pairs in use; its commands complete
//! while the driver waits, as FreeBSD's `vtnet_exec_ctrl_cmd` polls.
//!
//! Everything a device writes is untrusted: lengths and buffer counts
//! are checked against the slots posted.
//!
//! Behavior reference: sys/dev/virtio/network/if_vtnet.c,
//! sys/dev/virtio/network/virtio_net.h.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::Ordering;

use abi::net::{ifcap, iff};
use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, IrqLine, StartCx, StopCx, StopReason,
};
use devtree::ids::VirtioId;
use irq::Filter;
use mbuf::{Chain, NoBufs, Packet, PktHdr, csum, flags};
use net::link::{Link, Media, ifm};
use net::netif::{Ether, Port};
use net::{Family, Vnet};
use sched::{IrqCx, Sleepable};
use sync::SpinLock;
use virtio::{InterruptMask, VirtioDevice, Virtqueue, device_type};
use vm::dma::{DmaCoherent, DmaConstraints, DmaTag, Mapped, dir};

/// Feature bits (virtio 1.2 §5.1.3).
mod f {
    pub(crate) const CSUM: u64 = 1 << 0;
    pub(crate) const GUEST_CSUM: u64 = 1 << 1;
    pub(crate) const CTRL_GUEST_OFFLOADS: u64 = 1 << 2;
    pub(crate) const MTU: u64 = 1 << 3;
    pub(crate) const MAC: u64 = 1 << 5;
    pub(crate) const GSO: u64 = 1 << 6;
    pub(crate) const GUEST_TSO4: u64 = 1 << 7;
    pub(crate) const GUEST_TSO6: u64 = 1 << 8;
    pub(crate) const GUEST_ECN: u64 = 1 << 9;
    pub(crate) const HOST_TSO4: u64 = 1 << 11;
    pub(crate) const HOST_TSO6: u64 = 1 << 12;
    pub(crate) const HOST_ECN: u64 = 1 << 13;
    pub(crate) const MRG_RXBUF: u64 = 1 << 15;
    pub(crate) const STATUS: u64 = 1 << 16;
    pub(crate) const CTRL_VQ: u64 = 1 << 17;
    pub(crate) const CTRL_RX: u64 = 1 << 18;
    pub(crate) const CTRL_VLAN: u64 = 1 << 19;
    pub(crate) const MQ: u64 = 1 << 22;
    pub(crate) const CTRL_MAC_ADDR: u64 = 1 << 23;
    pub(crate) const SPEED_DUPLEX: u64 = 1 << 63;
}

/// `VTNET_COMMON_FEATURES`; a legacy device adds `GSO`.
const FEATURES: u64 = f::MAC
    | f::STATUS
    | f::CTRL_GUEST_OFFLOADS
    | f::MTU
    | f::CTRL_VQ
    | f::CTRL_RX
    | f::CTRL_MAC_ADDR
    | f::CTRL_VLAN
    | f::CSUM
    | f::HOST_TSO4
    | f::HOST_TSO6
    | f::HOST_ECN
    | f::GUEST_CSUM
    | f::GUEST_TSO4
    | f::GUEST_TSO6
    | f::GUEST_ECN
    | f::MRG_RXBUF
    | f::MQ
    | f::SPEED_DUPLEX
    | virtio::feature::RING_EVENT_IDX;

/// `struct virtio_net_config` offsets.
const CFG_MAC: u16 = 0;
const CFG_STATUS: u16 = 6;
const CFG_MAX_PAIRS: u16 = 8;
const CFG_MTU: u16 = 10;
const CFG_SPEED: u16 = 12;
/// `VIRTIO_NET_S_LINK_UP`.
const S_LINK_UP: u16 = 1;

/// Header flags and GSO types (`VIRTIO_NET_HDR_*`).
const HDR_F_NEEDS_CSUM: u8 = 1;
const HDR_F_DATA_VALID: u8 = 2;

/// Control classes and commands.
const CTRL_RX: u8 = 0;
const CTRL_RX_PROMISC: u8 = 0;
const CTRL_RX_ALLMULTI: u8 = 1;
const CTRL_MAC: u8 = 1;
const CTRL_MAC_TABLE_SET: u8 = 0;
const CTRL_MQ: u8 = 4;
const CTRL_MQ_VQ_PAIRS_SET: u8 = 0;
const CTRL_OK: u8 = 0;
/// The multicast table's room (`VTNET_MAX_MAC_ENTRIES`).
const MAC_FILTER_MAX: usize = 128;
/// The control command buffer: the class, command, two address tables
/// at their largest, and the acknowledgement.
const CTRL_BUF: usize = 1024;
/// `VIRTIO_NET_CTRL_MQ_VQ_PAIRS_MIN`, `_MAX`.
const PAIRS_MIN: u16 = 1;
const PAIRS_MAX: u16 = 0x8000;

/// `ETHERMTU`, `ETHERMTU_JUMBO`, `VTNET_MIN_MTU`, `VTNET_MAX_MTU`.
const ETHERMTU: u32 = 1500;
const ETHERMTU_JUMBO: u32 = 9000;
const MIN_MTU: u16 = 68;
const MAX_MTU: u32 = 65536;
/// Entries wanted in each queue.
const QUEUE: u16 = 256;
/// A receive slot: `MJUMPAGESIZE`, as FreeBSD sizes mergeable buffers.
const SLOT: usize = 4096;
/// The same, as a descriptor length.
const SLOT_LEN: u32 = 4096;
/// Slots per DMA chunk, so no allocation needs much contiguous memory.
const SLOTS_PER_CHUNK: usize = 16;
/// Frames handled per pass of a receive queue (`hw.vtnet.rx_process_limit`
/// is 1024; a pass yields sooner so transmit completions interleave).
const RX_BUDGET: usize = 512;
/// The longest frame without TSO: an 802.1Q header and the largest MTU.
const MAX_FRAME: usize = 14 + 4 + MAX_MTU as usize;
/// Polls of the control queue before a command is given up on.
const CTRL_SPINS: u32 = 10_000_000;

/// A receive queue.
struct RxQ {
    vq: Virtqueue,
    chunks: Vec<DmaCoherent<u8>>,
    /// The slot each outstanding head holds.
    slot_of: Vec<Option<u32>>,
    /// A frame being gathered from mergeable buffers: its data, and the
    /// buffers still to come.
    partial: Option<(Chain, u16)>,
    /// That frame's virtio header.
    partial_hdr: Option<[u8; 12]>,
}

impl RxQ {
    fn slot_bus(&self, slot: u32) -> u64 {
        let s = slot as usize;
        self.chunks[s / SLOTS_PER_CHUNK].bus_addr() + ((s % SLOTS_PER_CHUNK) * SLOT) as u64
    }

    fn slot_copy(&self, slot: u32, off: usize, dst: &mut [u8]) {
        let s = slot as usize;
        self.chunks[s / SLOTS_PER_CHUNK].copy_to_slice((s % SLOTS_PER_CHUNK) * SLOT + off, dst);
    }

    /// Posts `slot` to the device.
    fn post(&mut self, slot: u32, hdr_len: usize, separate: bool) -> bool {
        let b = self.slot_bus(slot);
        let r = if separate {
            let h = u32::try_from(hdr_len).unwrap_or(0);
            self.vq
                .push(&[], &[(b, h), (b + u64::from(h), SLOT_LEN - h)])
        } else {
            self.vq.push(&[], &[(b, SLOT_LEN)])
        };
        match r {
            Ok(head) => {
                self.slot_of[usize::from(head)] = Some(slot);
                true
            }
            Err(_) => false,
        }
    }
}

/// A frame on the device.
struct Sent {
    data: Mapped<Vec<u8>, dir::ToDevice>,
}

/// A transmit queue.
struct TxQ {
    vq: Virtqueue,
    /// Each head's header slot.
    headers: DmaCoherent<u8>,
    sent: Vec<Option<Sent>>,
}

impl TxQ {
    /// Frees what the device has finished sending (`vtnet_txq_eof`).
    fn reap(&mut self) -> usize {
        let mut n = 0;
        while let Ok(Some(u)) = self.vq.pop() {
            if let Some(s) = self
                .sent
                .get_mut(usize::from(u.head))
                .and_then(Option::take)
            {
                drop(s.data.unmap());
                n += 1;
            }
        }
        n
    }
}

/// The control queue and its command buffer.
struct CtrlQ {
    vq: Virtqueue,
    buf: DmaCoherent<u8>,
}

/// The shared state the interface and the interrupt handlers use.
pub struct Shared {
    dev: Arc<VirtioDevice>,
    features: u64,
    tag: DmaTag,
    hdr_len: usize,
    mrg: bool,
    /// Header and data in separate receive descriptors.
    separate: bool,
    rx: Vec<SpinLock<RxQ>>,
    tx: Vec<SpinLock<TxQ>>,
    ctrl: Option<SpinLock<CtrlQ>>,
    port: Arc<Port>,
    /// The receive filter last set: promiscuous, all-multicast.
    filter: SpinLock<(bool, bool)>,
}

impl core::fmt::Debug for Shared {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Shared")
            .field("pairs", &self.rx.len())
            .finish_non_exhaustive()
    }
}

impl Shared {
    fn has(&self, feature: u64) -> bool {
        self.features & feature != 0
    }

    fn link_up(&self) -> bool {
        !self.has(f::STATUS) || self.dev.config16(CFG_STATUS) & S_LINK_UP != 0
    }

    /// Runs a control command (`vtnet_exec_ctrl_cmd`): `class`, `cmd`,
    /// and its data; whether the device acknowledged it.
    fn ctrl(&self, class: u8, cmd: u8, data: &[u8]) -> bool {
        let Some(c) = &self.ctrl else {
            return false;
        };
        let mut c = c.lock();
        let n = 2 + data.len();
        if n + 1 > c.buf.len() {
            return false;
        }
        c.buf.write(0, class);
        c.buf.write(1, cmd);
        c.buf.copy_from_slice(2, data);
        c.buf.write(n, 0xff);
        let b = c.buf.bus_addr();
        let len = u32::try_from(n).unwrap_or(0);
        if c.vq.push(&[(b, len)], &[(b + u64::from(len), 1)]).is_err() {
            return false;
        }
        c.vq.kick();
        for _ in 0..CTRL_SPINS {
            if let Ok(Some(_)) = c.vq.pop() {
                return c.buf.read(n) == CTRL_OK;
            }
            core::hint::spin_loop();
        }
        false
    }

    /// `vtnet_rx_filter`: promiscuous and all-multicast reception as the
    /// interface's flags ask.
    fn rx_filter(&self, flags: u32) {
        if !self.has(f::CTRL_RX) {
            return;
        }
        let want = (flags & iff::PROMISC != 0, flags & iff::ALLMULTI != 0);
        let mut have = self.filter.lock();
        if want.0 != have.0 && self.ctrl(CTRL_RX, CTRL_RX_PROMISC, &[u8::from(want.0)]) {
            have.0 = want.0;
        }
        if want.1 != have.1 && self.ctrl(CTRL_RX, CTRL_RX_ALLMULTI, &[u8::from(want.1)]) {
            have.1 = want.1;
        }
    }

    /// `vtnet_rx_filter_mac`: no extra unicast addresses, and the
    /// interface's multicast groups; past the table's room, all
    /// multicast frames instead.
    fn mac_filter(&self, groups: &[[u8; 6]]) {
        if !self.has(f::CTRL_RX) {
            return;
        }
        let over = groups.len() > MAC_FILTER_MAX;
        let list: &[[u8; 6]] = if over { &[] } else { groups };
        let mut t = Vec::with_capacity(8 + 6 * list.len());
        t.extend_from_slice(&0u32.to_le_bytes());
        t.extend_from_slice(&u32::try_from(list.len()).unwrap_or(0).to_le_bytes());
        for g in list {
            t.extend_from_slice(g);
        }
        let _ = self.ctrl(CTRL_MAC, CTRL_MAC_TABLE_SET, &t);
        let mut have = self.filter.lock();
        if over != have.1 && self.ctrl(CTRL_RX, CTRL_RX_ALLMULTI, &[u8::from(over)]) {
            have.1 = over;
        }
    }

    /// Takes what the device returned on receive queue `q`, a budget of
    /// frames; whether more may be waiting.
    fn rx_pass(&self, q: usize) -> bool {
        let mut frames: Vec<Packet> = Vec::new();
        let more = {
            let mut r = self.rx[q].lock();
            r.vq.set_interrupts(false);
            let mut more = false;
            while frames.len() < RX_BUDGET {
                let used = match r.vq.pop() {
                    Ok(Some(u)) => u,
                    Ok(None) => {
                        if r.vq.rearm() {
                            break;
                        }
                        r.vq.set_interrupts(false);
                        continue;
                    }
                    Err(_) => break,
                };
                let Some(slot) = r
                    .slot_of
                    .get_mut(usize::from(used.head))
                    .and_then(Option::take)
                else {
                    continue;
                };
                if let Some(m) = self.rx_buf(&mut r, slot, used.len as usize) {
                    frames.push(m);
                }
                let separate = self.separate;
                r.post(slot, self.hdr_len, separate);
            }
            if frames.len() >= RX_BUDGET {
                more = true;
            }
            r.vq.kick();
            more
        };
        for m in frames {
            self.port.stats.ipackets.fetch_add(1, Ordering::Relaxed);
            self.port
                .stats
                .ibytes
                .fetch_add(m.len() as u64, Ordering::Relaxed);
            self.port.input(m);
        }
        more
    }

    /// One used buffer of `len` bytes in `slot`: a whole frame, or part of
    /// one with mergeable buffers.
    fn rx_buf(&self, r: &mut RxQ, slot: u32, len: usize) -> Option<Packet> {
        let len = len.min(SLOT);
        if let Some((mut chain, left)) = r.partial.take() {
            let mut b = alloc::vec![0u8; len];
            r.slot_copy(slot, 0, &mut b);
            if chain.extend_from_slice(&b).is_err() {
                self.port.stats.iqdrops.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            if left > 1 {
                r.partial = Some((chain, left - 1));
                return None;
            }
            return Some(self.frame(chain, r.partial_hdr.take()));
        }
        if len < self.hdr_len {
            self.port.stats.ierrors.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let mut h = [0u8; 12];
        r.slot_copy(slot, 0, &mut h[..self.hdr_len]);
        let mut b = alloc::vec![0u8; len - self.hdr_len];
        r.slot_copy(slot, self.hdr_len, &mut b);
        let Ok(chain) = Chain::from_slice(&b) else {
            self.port.stats.iqdrops.fetch_add(1, Ordering::Relaxed);
            return None;
        };
        let nbufs = if self.mrg {
            u16::from_le_bytes([h[10], h[11]]).max(1)
        } else {
            1
        };
        if nbufs > 1 {
            r.partial = Some((chain, nbufs - 1));
            r.partial_hdr = Some(h);
            return None;
        }
        Some(self.frame(chain, Some(h)))
    }

    /// A received frame with its virtio header's checksum results
    /// (`vtnet_rxq_csum`).
    fn frame(&self, data: Chain, h: Option<[u8; 12]>) -> Packet {
        let mut hdr = PktHdr::default();
        if let Some(h) = h
            && self.has(f::GUEST_CSUM)
            && h[0] & (HDR_F_NEEDS_CSUM | HDR_F_DATA_VALID) != 0
        {
            // The host vouches for the data (a partial checksum came from
            // a local sender and never crossed a wire).
            hdr.csum_flags |= csum::DATA_VALID | csum::PSEUDO_HDR;
            hdr.csum_data = 0xffff;
        }
        Packet { hdr, data }
    }

    /// Sends frame `m` on transmit queue `q` (`vtnet_txq_encap`).
    fn send(&self, q: usize, m: Packet) -> Result<(), NoBufs> {
        let len = m.len();
        if len > MAX_FRAME {
            return Err(NoBufs);
        }
        let mut buf = alloc::vec![0u8; len];
        m.data.copy_out(0, &mut buf);
        let mcast = m.hdr.flags & (flags::BCAST | flags::MCAST) != 0;
        self.port.tap.mtap(&m, net::bpf::Way::Out);
        drop(m);
        let data = self.tag.map::<dir::ToDevice, _>(buf).map_err(|_| NoBufs)?;
        let mut t = self.tx[q].lock();
        if usize::from(t.vq.ring().free()) < 1 + data.segments().len() {
            t.reap();
        }
        let Some(head) = t.vq.ring().next_head() else {
            drop(data.unmap());
            return Err(NoBufs);
        };
        let at = usize::from(head) * 16;
        for i in 0..self.hdr_len {
            t.headers.write(at + i, 0);
        }
        let hb = t.headers.bus_addr() + at as u64;
        let mut out = Vec::with_capacity(1 + data.segments().len());
        out.push((hb, u32::try_from(self.hdr_len).unwrap_or(0)));
        for s in data.segments() {
            out.push((s.bus, u32::try_from(s.len).unwrap_or(u32::MAX)));
        }
        match t.vq.push(&out, &[]) {
            Ok(h) => {
                t.sent[usize::from(h)] = Some(Sent { data });
                t.vq.kick();
            }
            Err(_) => {
                drop(data.unmap());
                return Err(NoBufs);
            }
        }
        drop(t);
        let st = &self.port.stats;
        st.opackets.fetch_add(1, Ordering::Relaxed);
        st.obytes.fetch_add(len as u64, Ordering::Relaxed);
        if mcast {
            st.omcasts.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }
}

/// The interface's device: transmit, media, and the receive filter.
struct Iface(Arc<Shared>);

impl Link for Iface {
    fn transmit(
        &self,
        _vnet: &Arc<Vnet>,
        _index: u16,
        _family: Option<Family>,
        m: Packet,
    ) -> Result<(), NoBufs> {
        // The flow's pair (`vtnet_txq_mq_start`: the flow ID, or the CPU).
        let n = self.0.tx.len();
        let q = if m.hdr.hash_type != 0 {
            m.hdr.flowid as usize % n
        } else {
            hal::cpu_id().0 as usize % n
        };
        self.0.send(q, m).inspect_err(|_| {
            self.0.port.stats.oerrors.fetch_add(1, Ordering::Relaxed);
        })
    }

    fn media(&self) -> Option<Media> {
        let up = self.0.link_up();
        Some(Media {
            current: ifm::ETHER | ifm::AUTO,
            active: ifm::ETHER | if up { ifm::T10G | ifm::FDX } else { ifm::NONE },
            status: ifm::AVALID | if up { ifm::ACTIVE } else { 0 },
            supported: ifm::ETHER | ifm::AUTO,
        })
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

    fn flags_changed(&self, flags: u32) {
        self.0.rx_filter(flags);
    }

    fn multicast_changed(&self, groups: &[[u8; 6]]) {
        self.0.mac_filter(groups);
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A `vtnetN` instance.
pub struct VtNet {
    shared: Arc<Shared>,
    /// The interrupt lines, each with the queues it serves.
    lines: SpinLock<Vec<(IrqLine, usize)>>,
    /// Each interrupt index's queues: receive queues, transmit queues,
    /// and whether configuration changes come on it.
    routes: Vec<(usize, Vec<usize>, Vec<usize>, bool)>,
    masks: Vec<InterruptMask>,
    info: Ether,
}

impl core::fmt::Debug for VtNet {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VtNet").finish_non_exhaustive()
    }
}

fn route(d: &VtNet, irq: usize) -> Option<&(usize, Vec<usize>, Vec<usize>, bool)> {
    d.routes.iter().find(|r| r.0 == irq)
}

fn filter(d: &VtNet, irq: usize, _: &IrqCx) -> Filter {
    if d.shared.dev.shared_line() && d.shared.dev.isr() == 0 {
        return Filter::Stray;
    }
    if let Some((_, rx, _, _)) = route(d, irq) {
        for &q in rx {
            d.masks[q].suppress();
        }
    }
    Filter::Schedule
}

fn handler(d: &VtNet, irq: usize, s: &Sleepable) {
    let Some((_, rx, tx, config)) = route(d, irq) else {
        return;
    };
    let _ = s;
    for &q in rx {
        while d.shared.rx_pass(q) {
            sched::yield_now();
        }
    }
    for &q in tx {
        d.shared.tx[q].lock().reap();
    }
    if *config {
        d.shared.port.link_state(d.shared.link_up());
    }
}

/// The MAC address: the device's, or a random-looking local one
/// (`vtnet_get_macaddr`'s fallback, from the unit here).
fn mac(dev: &VirtioDevice, features: u64, unit: u32) -> [u8; 6] {
    if features & f::MAC != 0 {
        let mut m = [0u8; 6];
        for (i, b) in m.iter_mut().enumerate() {
            *b = dev.config8(CFG_MAC + u16::try_from(i).unwrap_or(0));
        }
        return m;
    }
    let u = unit.to_be_bytes();
    [0xb2, 0x2b, u[0], u[1], u[2], u[3]]
}

impl Driver for VtNet {
    const NAME: &'static str = "vtnet";
    const IDS: Ids = Ids::Virtio(&[VirtioId::new(device_type::NETWORK)]);
    const CONCURRENCY: bus::Concurrency = bus::Concurrency::Concurrent;

    fn describe(_: &Ident) -> String {
        "VirtIO Networking Adapter".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let dev = cx
            .bus_data_arc::<VirtioDevice>()
            .ok_or(AttachError::Decline)?;
        let wanted = if dev.is_modern() {
            FEATURES
        } else {
            FEATURES | f::GSO
        };
        let mut features = dev
            .negotiate(wanted)
            .map_err(|_| AttachError::Device("feature negotiation failed"))?;
        // A device's bad MTU or pair count turns the feature off
        // (`vtnet_negotiate_features` renegotiates; the values are
        // ignored here instead).
        if features & f::MTU != 0 && dev.config16(CFG_MTU) < MIN_MTU {
            features &= !f::MTU;
        }
        let max_pairs = if features & f::MQ != 0 && features & f::CTRL_VQ != 0 {
            let p = dev.config16(CFG_MAX_PAIRS);
            if (PAIRS_MIN..=PAIRS_MAX).contains(&p) {
                p
            } else {
                1
            }
        } else {
            1
        };
        let built = (|| -> Result<_, AttachError> {
            let tag = cx.dma_tag(DmaConstraints {
                addr_bits: if dev.is_modern() { 64 } else { 44 },
                ..DmaConstraints::UNLIMITED
            })?;
            let mrg = features & f::MRG_RXBUF != 0;
            let hdr_len = if mrg || dev.is_modern() { 12 } else { 10 };
            let separate = !mrg && !dev.is_modern();
            let pairs = usize::from(max_pairs)
                .min(sched::online_cpus().max(1))
                .max(1);
            let mut rx = Vec::new();
            let mut tx = Vec::new();
            let mut masks = Vec::new();
            for p in 0..pairs {
                let i = u16::try_from(2 * p).unwrap_or(0);
                let rvq = dev
                    .queue(&tag, i, QUEUE)
                    .map_err(|_| AttachError::Device("receive queue"))?;
                let size = usize::from(rvq.ring().size());
                masks.push(rvq.interrupt_mask());
                let nslots = size;
                let chunks = (0..nslots.div_ceil(SLOTS_PER_CHUNK))
                    .map(|_| tag.coherent::<u8>(SLOT * SLOTS_PER_CHUNK))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|_| AttachError::NoMemory)?;
                let mut q = RxQ {
                    vq: rvq,
                    chunks,
                    slot_of: (0..size).map(|_| None).collect(),
                    partial: None,
                    partial_hdr: None,
                };
                let posts = if separate { size / 2 } else { size };
                for s in 0..posts {
                    q.post(u32::try_from(s).unwrap_or(0), hdr_len, separate);
                }
                rx.push(SpinLock::new(q));
                let tvq = dev
                    .queue(&tag, i + 1, QUEUE)
                    .map_err(|_| AttachError::Device("transmit queue"))?;
                let tsize = usize::from(tvq.ring().size());
                // Transmit completions are reaped on send, not signalled.
                tvq.set_interrupts(false);
                let headers = tag
                    .coherent::<u8>(tsize * 16)
                    .map_err(|_| AttachError::NoMemory)?;
                tx.push(SpinLock::new(TxQ {
                    vq: tvq,
                    headers,
                    sent: (0..tsize).map(|_| None).collect(),
                }));
            }
            let ctrl = if features & f::CTRL_VQ != 0 {
                let i = if features & f::MQ != 0 {
                    2 * max_pairs
                } else {
                    2
                };
                let vq = dev
                    .queue(&tag, i, 16)
                    .map_err(|_| AttachError::Device("control queue"))?;
                vq.set_interrupts(false);
                let buf = tag
                    .coherent::<u8>(CTRL_BUF)
                    .map_err(|_| AttachError::NoMemory)?;
                Some(SpinLock::new(CtrlQ { vq, buf }))
            } else {
                None
            };
            // Each interrupt index: the queues it serves.
            let mut routes: Vec<(usize, Vec<usize>, Vec<usize>, bool)> = Vec::new();
            fn at(routes: &mut Vec<(usize, Vec<usize>, Vec<usize>, bool)>, irq: usize) -> usize {
                if let Some(k) = routes.iter().position(|r| r.0 == irq) {
                    k
                } else {
                    routes.push((irq, Vec::new(), Vec::new(), false));
                    routes.len() - 1
                }
            }
            let mut lines = Vec::new();
            for p in 0..pairs {
                let i = u16::try_from(2 * p).unwrap_or(0);
                if let Some(irq) = dev.queue_irq(i) {
                    let k = at(&mut routes, irq);
                    routes[k].1.push(p);
                }
                if let Some(irq) = dev.queue_irq(i + 1) {
                    let k = at(&mut routes, irq);
                    routes[k].2.push(p);
                }
            }
            if let Some(irq) = dev.config_irq() {
                let k = at(&mut routes, irq);
                routes[k].3 = true;
            }
            for r in &routes {
                lines.push((cx.irq(r.0)?, r.0));
            }
            let max_mtu = if features & f::MTU != 0 {
                u32::from(dev.config16(CFG_MTU))
            } else {
                MAX_MTU
            };
            let mut caps = ifcap::VLAN_MTU | ifcap::HWSTATS;
            if features & f::STATUS != 0 {
                caps |= ifcap::LINKSTATE;
            }
            if features & f::CSUM != 0 {
                caps |= ifcap::TXCSUM | ifcap::TXCSUM_IPV6;
                let gso = features & f::GSO != 0;
                if gso || features & f::HOST_TSO4 != 0 {
                    caps |= ifcap::TSO4;
                }
                if gso || features & f::HOST_TSO6 != 0 {
                    caps |= ifcap::TSO6;
                }
                if caps & (ifcap::TSO4 | ifcap::TSO6) != 0 {
                    caps |= ifcap::VLAN_HWTSO;
                }
            }
            if features & f::GUEST_CSUM != 0 {
                caps |= ifcap::RXCSUM | ifcap::RXCSUM_IPV6 | ifcap::LRO;
            }
            if caps & (ifcap::TXCSUM | ifcap::RXCSUM | ifcap::TXCSUM_IPV6 | ifcap::RXCSUM_IPV6) != 0
            {
                caps |= ifcap::VLAN_HWTAGGING | ifcap::VLAN_HWCSUM;
            }
            if max_mtu >= ETHERMTU_JUMBO {
                caps |= ifcap::JUMBO_MTU;
            }
            let capenable = caps;
            if features & f::CTRL_VLAN != 0 {
                caps |= ifcap::VLAN_HWFILTER;
            }
            let baudrate = if features & f::SPEED_DUPLEX != 0 {
                match dev.config32(CFG_SPEED) {
                    u32::MAX => 10_000_000_000,
                    s => u64::from(s) * 1_000_000,
                }
            } else {
                10_000_000_000
            };
            let info = Ether {
                dname: "vtnet",
                unit: i32::try_from(cx.unit()).unwrap_or(0),
                lladdr: mac(&dev, features, cx.unit()),
                mtu: ETHERMTU,
                capabilities: caps,
                capenable,
                baudrate,
            };
            let shared = Shared {
                dev: Arc::clone(&dev),
                features,
                tag,
                hdr_len,
                mrg,
                separate,
                rx,
                tx,
                ctrl,
                port: Port::new(),
                filter: SpinLock::new((false, false)),
            };
            Ok((shared, lines, routes, masks, info))
        })();
        let (shared, lines, routes, masks, info) = built.inspect_err(|_| dev.reset())?;
        if lines.is_empty() {
            dev.reset();
            return Err(AttachError::Device("no interrupt"));
        }
        Ok(Self {
            shared: Arc::new(shared),
            lines: SpinLock::new(lines),
            routes,
            masks,
            info,
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let lines = core::mem::take(&mut *self.lines.lock());
        for (line, irq) in lines {
            cx.interrupt(line, Some(filter), Some(handler), irq)?;
        }
        self.shared.dev.ready();
        let sh = &self.shared;
        if sh.rx.len() > 1 {
            let n = u16::try_from(sh.rx.len()).unwrap_or(1);
            let _ = sh.ctrl(CTRL_MQ, CTRL_MQ_VQ_PAIRS_SET, &n.to_le_bytes());
        }
        sh.mac_filter(&[]);
        for q in &sh.rx {
            q.lock().vq.kick();
        }
        let info = self.info.clone();
        let name = net::netif::attach_ether(&info, &sh.port, Arc::new(Iface(Arc::clone(sh))));
        let m = info.lladdr;
        bus::manager().log(format_args!(
            "{name}: Ethernet address: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            m[0], m[1], m[2], m[3], m[4], m[5]
        ));
        Ok(())
    }

    fn stop(&self, _cx: &mut StopCx<'_>, _: StopReason) {
        net::netif::detach(&self.shared.port);
        self.shared.dev.reset();
        for t in &self.shared.tx {
            let mut t = t.lock();
            for s in t.sent.iter_mut().filter_map(Option::take) {
                drop(s.data.unmap());
            }
        }
    }
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<VtNet>();
