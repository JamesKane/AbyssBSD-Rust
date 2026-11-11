// SPDX-License-Identifier: BSD-2-Clause
//! `tun`, `tap`, and `vmnet`: interfaces whose other end is a program.
//! What the stack sends out of one waits on its queue until the program
//! reads it from `/dev/tunN` (or `tapN`, `vmnetN`); what the program
//! writes there arrives on the interface. A `tun` carries IP packets on
//! a point-to-point link, with the family before each if asked
//! (`TUNSIFHEAD`); a `tap` carries Ethernet frames, and a `vmnet` is a
//! `tap` that keeps its addresses when closed.
//!
//! This is the interface's side and the device's state; the device node
//! (open, read, write, `poll`, the requests) is the kernel's, which
//! [`set_hooks`] tells of each device made and gone.
//!
//! Behavior reference: sys/net/if_tuntap.c.

use alloc::collections::{BTreeSet, VecDeque};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicI32, Ordering};

use abi::errno::Errno;
use abi::inet::{AF_INET, AF_INET6};
use abi::net::{IFT_ETHER, IFT_PPP, LINK_STATE_DOWN, LINK_STATE_UNKNOWN, LINK_STATE_UP};
use abi::net::{ifcap, iff, nd6_iff};
use mbuf::{Chain, NoBufs, Packet};
use sync::PreemptLock;

use crate::bpf::{Tap, Way};
use crate::clone::Ctx;
use crate::ifnet::{Ifnet, Stats};
use crate::link::{self, Link, LinkRef, Media, ifm};
use crate::netisr::Proto;
use crate::{Family, Vnet};

/// `IF_MAXUNIT`.
const MAXUNIT: i32 = 0x7fff;
/// `IFQ_MAXLEN` (`net.link.ifqmaxlen`): a device's queue of packets for
/// its reader.
pub const QLEN: usize = 50;
/// `ETHER_ALIGN`: room before a written frame so its IP header is
/// aligned.
const ETHER_ALIGN: usize = 2;

/// `net.link.tun.devfs_cloning`.
pub static TUN_DEVFS_CLONING: AtomicI32 = AtomicI32::new(1);
/// `net.link.tap.devfs_cloning`, `net.link.tap.user_open`,
/// `net.link.tap.up_on_open`.
pub static TAP_DEVFS_CLONING: AtomicI32 = AtomicI32::new(1);
pub static TAP_USER_OPEN: AtomicI32 = AtomicI32::new(0);
pub static TAP_UP_ON_OPEN: AtomicI32 = AtomicI32::new(0);
/// `debug.if_tun_debug` (and `net.link.tap.debug`, the same variable).
pub static DEBUG: AtomicI32 = AtomicI32::new(0);

/// Which driver: the three share everything but their names and layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    Tun,
    Tap,
    Vmnet,
}

impl Kind {
    /// Every one, as `tuntap_drivers` lists them.
    pub const ALL: [Self; 3] = [Self::Tun, Self::Tap, Self::Vmnet];

    /// Its device and interface name (`d_name`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Tun => "tun",
            Self::Tap => "tap",
            Self::Vmnet => "vmnet",
        }
    }

    /// Whether it carries Ethernet frames (`TUN_L2`).
    #[must_use]
    pub const fn l2(self) -> bool {
        !matches!(self, Self::Tun)
    }

    /// `tuntap_name2info` with `dev_stdclone`: the driver `name` is one
    /// of, and its unit if it has one. A unit has no leading zero and
    /// nothing after it.
    #[must_use]
    pub fn parse(name: &[u8]) -> Option<(Self, Option<i32>)> {
        Self::ALL.into_iter().find_map(|k| {
            let rest = name.strip_prefix(k.name().as_bytes())?;
            if rest.is_empty() {
                return Some((k, None));
            }
            if !rest.iter().all(u8::is_ascii_digit) || (rest[0] == b'0' && rest.len() > 1) {
                return None;
            }
            let mut u: i32 = 0;
            for &c in rest {
                u = u.checked_mul(10)?.checked_add(i32::from(c - b'0'))?;
            }
            // `dev_stdclone` takes units up to 0xffffff.
            (u <= 0xff_ffff).then_some((k, Some(u)))
        })
    }
}

/// What the kernel does for a device: makes and removes its node
/// (`make_dev`, `destroy_dev`) and its alias after a rename
/// (`tunrename`), and wakes its readers (`tunstart`).
#[derive(Clone, Copy, Debug)]
pub struct Hooks {
    pub made: fn(&Arc<Tun>),
    pub gone: fn(&Tun),
    /// The interface took a new name; the device's alias by the old one,
    /// if any, is the hook's to remove ([`Tun::set_alias`]).
    pub renamed: fn(&Tun, &str),
}

static HOOKS: sync::Once<Hooks> = sync::Once::new();

/// Sets the kernel's hooks.
pub fn set_hooks(h: Hooks) {
    let _ = HOOKS.set(h);
}

/// Something to wake when a packet waits to be read or the device
/// closes: readers, `select` and `kevent` waiters, and `FIOASYNC`'s
/// signal.
pub trait Wake: Send + Sync {
    fn wake(&self);
}

/// The units each driver has handed out, across every VNET, as each
/// device node is the system's (`drv->unrhdr`).
static UNITS: PreemptLock<BTreeSet<(Kind, i32)>> = PreemptLock::new(BTreeSet::new());

/// The device's state (`tun_flags` and the rest of the softc).
#[derive(Debug, Default)]
struct State {
    /// `TUN_OPEN`, `TUN_LMODE`, `TUN_IFHEAD`, `TUN_TRANSIENT`,
    /// `TUN_DYING`, `TUN_ASYNC`.
    open: bool,
    lmode: bool,
    ifhead: bool,
    transient: bool,
    dying: bool,
    async_io: bool,
    /// The process that has it open, or claimed it (`tun_pid`).
    pid: i32,
    /// Packets for the reader (`if_snd`).
    q: VecDeque<Packet>,
    /// A `tap`'s other end's address (`tun_ether`).
    remote: [u8; 6],
    /// The virtio-net header before each frame (`tun_vhdrlen`).
    vhdrlen: usize,
    /// The device's alias, the interface's name since a rename
    /// (`tun_alias`).
    alias: Option<String>,
}

/// One device and its interface (`struct tuntap_softc`).
pub struct Tun {
    pub kind: Kind,
    pub unit: i32,
    stats: Arc<Stats>,
    tap: Arc<Tap>,
    st: PreemptLock<State>,
    wake: sync::Once<Arc<dyn Wake>>,
}

impl core::fmt::Debug for Tun {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Tun")
            .field("kind", &self.kind)
            .field("unit", &self.unit)
            .finish_non_exhaustive()
    }
}

/// What a write was, once its length is checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WriteLen {
    /// Ignored: the interface is down, or nothing was written.
    Ignore,
    /// A virtio-net header of this length, then the packet.
    Packet { vhdrlen: usize },
}

impl Tun {
    /// Its device's name (`si_name`), which stays when the interface is
    /// renamed.
    #[must_use]
    pub fn devname(&self) -> String {
        format!("{}{}", self.kind.name(), self.unit)
    }

    /// Its interface's VNET and index.
    #[must_use]
    pub fn home(&self) -> (Option<Arc<Vnet>>, u16) {
        self.tap.home()
    }

    /// Sets what to wake (once, when its node is made).
    pub fn set_wake(&self, w: Arc<dyn Wake>) {
        let _ = self.wake.set(w);
    }

    fn wake(&self) {
        if let Some(w) = self.wake.get() {
            w.wake();
        }
    }

    /// Whether `FIOASYNC` is on.
    #[must_use]
    pub fn async_io(&self) -> bool {
        self.st.lock().async_io
    }

    /// Records the device's alias, returning the one it replaces.
    pub fn set_alias(&self, alias: Option<String>) -> Option<String> {
        core::mem::replace(&mut self.st.lock().alias, alias)
    }

    /// Whether it is being destroyed (`TUN_DYING`).
    #[must_use]
    pub fn dying(&self) -> bool {
        self.st.lock().dying
    }

    /// `tunopen` by process `pid`: one open at a time. The interface's
    /// link comes up, and a `tap` runs (and with `net.link.tap.up_on_open`
    /// comes up).
    ///
    /// # Errors
    ///
    /// `EBUSY` if it is open or going, `ENXIO` if its interface is gone.
    pub fn open(&self, pid: i32) -> Result<(), Errno> {
        let (Some(v), index) = self.home() else {
            return Err(Errno::ENXIO);
        };
        let mut n = v.net.lock();
        {
            let mut st = self.st.lock();
            if st.open || st.dying {
                return Err(Errno::EBUSY);
            }
            st.open = true;
            st.pid = pid;
        }
        if self.kind.l2() {
            let up = TAP_UP_ON_OPEN.load(Ordering::Relaxed) != 0;
            let mut ll = None;
            n.with_index(index, |i| {
                ll = i.lladdr;
                i.flags |= iff::RUNNING;
                i.flags &= !iff::OACTIVE;
                if up {
                    i.flags |= iff::UP;
                }
            });
            if let Some(ll) = ll {
                self.st.lock().remote = ll;
            }
        }
        n.link_state_change(index, LINK_STATE_UP);
        Ok(())
    }

    /// `tundtor`, the open's last close, by process `pid` (`comm` its
    /// name): what waits to be read goes; unless it is a `vmnet` (or a
    /// `tap` with `link0`), the interface goes down and loses its
    /// addresses; its link goes down. Returns whether it is transient and
    /// so to be destroyed now.
    pub fn close(&self, pid: i32, comm: &str) -> bool {
        let (owner, transient) = {
            let mut st = self.st.lock();
            st.q.clear();
            (st.pid, st.transient)
        };
        if owner != pid {
            crate::log(
                6,
                &format!(
                    "pid {pid} ({comm}), {}: tun/tap protocol violation, non-controlling process closed last.\n",
                    self.devname()
                ),
            );
        }
        if let (Some(v), index) = self.home() {
            let mut n = v.net.lock();
            let (flags, name) = n
                .by_index(index)
                .map_or((0, String::new()), |i| (i.flags, i.name.clone()));
            let keep = self.kind == Kind::Vmnet || (self.kind.l2() && flags & iff::LINK0 != 0);
            if !keep {
                if flags & iff::UP != 0 {
                    let _ = n.set_flags(name.as_bytes(), flags & !iff::UP);
                }
                let running = n
                    .by_index(index)
                    .is_some_and(|i| i.flags & iff::RUNNING != 0);
                if running {
                    n.with_index(index, |i| i.flags &= !iff::RUNNING);
                    n.purge_addrs(index);
                }
            }
            n.link_state_change(index, LINK_STATE_DOWN);
        }
        {
            let mut st = self.st.lock();
            st.open = false;
            st.pid = 0;
            st.async_io = false;
            st.vhdrlen = 0;
            if transient {
                st.dying = true;
            }
        }
        if self.kind.l2() {
            self.vnethdr_caps(0);
        }
        self.wake();
        transient
    }

    /// The packet a `read` returns, if one waits (`tunread`).
    ///
    /// # Errors
    ///
    /// `EHOSTDOWN` if the device is not open, `ENXIO` if it is gone.
    pub fn dequeue(&self) -> Result<Option<Packet>, Errno> {
        let mut st = self.st.lock();
        if st.dying {
            return Err(Errno::ENXIO);
        }
        if !st.open {
            return Err(Errno::EHOSTDOWN);
        }
        Ok(st.q.pop_front())
    }

    /// The virtio-net header length reads and writes carry.
    #[must_use]
    pub fn vhdrlen(&self) -> usize {
        self.st.lock().vhdrlen
    }

    /// How many packets wait (`ifq_len`), for `kevent`.
    #[must_use]
    pub fn queued(&self) -> usize {
        self.st.lock().q.len()
    }

    /// The first waiting packet's length (`FIONREAD`).
    #[must_use]
    pub fn first_len(&self) -> usize {
        self.st.lock().q.front().map_or(0, Packet::len)
    }

    /// Its interface's MTU (`kevent`'s write filter).
    #[must_use]
    pub fn mtu(&self) -> u32 {
        let (Some(v), index) = self.home() else {
            return 0;
        };
        v.net.lock().by_index(index).map_or(0, |i| i.mtu)
    }

    /// `tunwrite`'s checks on a write of `resid` bytes.
    ///
    /// # Errors
    ///
    /// `EIO` for more than the device takes, `ENXIO` if it is gone.
    pub fn write_len(&self, resid: usize) -> Result<WriteLen, Errno> {
        let (Some(v), index) = self.home() else {
            return Err(Errno::ENXIO);
        };
        let up = v
            .net
            .lock()
            .by_index(index)
            .is_some_and(|i| i.flags & iff::UP != 0);
        // Ignored silently while down, as an empty write is.
        if !up || resid == 0 {
            return Ok(WriteLen::Ignore);
        }
        let (vhdrlen, ifhead) = {
            let st = self.st.lock();
            (st.vhdrlen, st.ifhead)
        };
        let mru = if self.kind.l2() {
            abi::tun::TAPMRU + vhdrlen
        } else if ifhead {
            abi::tun::TUNMRU + 4
        } else {
            abi::tun::TUNMRU
        };
        if resid > mru {
            return Err(Errno::EIO);
        }
        Ok(WriteLen::Packet { vhdrlen })
    }

    /// The room a written packet's first buffer keeps before its data.
    #[must_use]
    pub const fn write_lead(&self) -> usize {
        if self.kind.l2() { ETHER_ALIGN } else { 0 }
    }

    /// A written packet, after its virtio-net header `vhdr` if any
    /// (`tunwrite_l2`, `tunwrite_l3`): a frame into Ethernet input if the
    /// interface would have received it, a packet to its protocol.
    ///
    /// # Errors
    ///
    /// `EAFNOSUPPORT` for a `tun` packet of another family, `ENOBUFS`
    /// without buffers, `ENXIO` if the interface is gone.
    pub fn write(&self, vhdr: Option<&[u8]>, data: Chain) -> Result<(), Errno> {
        let (Some(v), index) = self.home() else {
            return Err(Errno::ENXIO);
        };
        let mut m = Packet {
            hdr: mbuf::PktHdr::default(),
            data,
        };
        m.hdr.rcvif = index;
        m.hdr.vnet = v.id;
        let (flags, lladdr, capenable, fib) = {
            let section = crate::view::enter();
            let Some(i) = v.net.view(&section).by_index(index) else {
                return Err(Errno::ENXIO);
            };
            (i.flags, i.lladdr, i.capenable, i.fib)
        };
        if self.kind.l2() {
            self.write_l2(&v, m, vhdr, flags, lladdr, capenable);
            return Ok(());
        }
        let family = if self.st.lock().ifhead {
            let mut h = [0u8; 4];
            if m.data.copy_out(0, &mut h) != 4 {
                return Err(Errno::ENOBUFS);
            }
            m.data.trim_front(4);
            u32::from_be_bytes(h)
        } else {
            u32::from(AF_INET)
        };
        self.tap.mtap2(&family.to_ne_bytes(), &m, Way::In);
        let proto = match u8::try_from(family) {
            Ok(AF_INET) => Proto::Ip,
            Ok(AF_INET6) => Proto::Ipv6,
            _ => return Err(Errno::EAFNOSUPPORT),
        };
        self.stats.ibytes.add(m.len() as u64);
        self.stats.ipackets.add(1);
        m.hdr.fib = fib;
        crate::netisr::dispatch(&v, proto, m);
        Ok(())
    }

    fn write_l2(
        &self,
        v: &Arc<Vnet>,
        mut m: Packet,
        vhdr: Option<&[u8]>,
        flags: u32,
        lladdr: Option<[u8; 6]>,
        capenable: u32,
    ) {
        let mut h = [0u8; crate::ether::HDR_LEN];
        if m.data.copy_out(0, &mut h) != h.len() {
            return;
        }
        // Only what real hardware would have received.
        let dst: [u8; 6] = [h[0], h[1], h[2], h[3], h[4], h[5]];
        if flags & iff::PROMISC == 0
            && !crate::ether::is_multicast(&dst)
            && lladdr.is_some_and(|l| l != dst)
        {
            return;
        }
        let ty = u16::from_be_bytes([h[12], h[13]]);
        let valid = mbuf::csum::IP_CHECKED
            | mbuf::csum::IP_VALID
            | mbuf::csum::DATA_VALID
            | mbuf::csum::PSEUDO_HDR;
        match vhdr {
            // `virtio_net_rx_csum`: the sender's checksum, partial or
            // checked, is taken as done.
            Some(vh) => {
                let f = vh.first().copied().unwrap_or(0);
                // `VIRTIO_NET_HDR_F_NEEDS_CSUM`, `VIRTIO_NET_HDR_F_DATA_VALID`.
                if f & 0x3 != 0 {
                    m.hdr.csum_flags |= valid;
                    m.hdr.csum_data = 0xffff;
                }
            }
            None => match ty {
                crate::ether::ty::IP if capenable & ifcap::RXCSUM != 0 => {
                    m.hdr.csum_flags |= valid;
                    m.hdr.csum_data = 0xffff;
                }
                crate::ether::ty::IPV6 if capenable & ifcap::RXCSUM_IPV6 != 0 => {
                    m.hdr.csum_flags |= mbuf::csum::DATA_VALID | mbuf::csum::PSEUDO_HDR;
                    m.hdr.csum_data = 0xffff;
                }
                _ => {}
            },
        }
        // `ether_input` counts the bytes, the device the packet.
        self.stats.ibytes.add(m.len() as u64);
        crate::ether::input(v, m);
        self.stats.ipackets.add(1);
    }

    /// Queues `m` for the reader (`IFQ_HANDOFF`), waking it.
    fn enqueue(&self, m: Packet) -> Result<(), NoBufs> {
        let len = m.len() as u64;
        let mcast = m.hdr.flags & mbuf::flags::MCAST != 0;
        {
            let mut st = self.st.lock();
            if st.q.len() >= QLEN {
                self.stats.oqdrops.add(1);
                return Err(NoBufs);
            }
            st.q.push_back(m);
        }
        self.stats.obytes.add(len);
        if mcast {
            self.stats.omcasts.add(1);
        }
        self.stats.opackets.add(1);
        self.wake();
        Ok(())
    }

    /// `tunoutput`: an IP packet of `family`, its family tapped before
    /// it, and before it on the queue its destination (`TUNSLMODE`) or
    /// family (`TUNSIFHEAD`); without either only IPv4 goes.
    fn output_l3(&self, family: Option<Family>, mut m: Packet) -> Result<(), NoBufs> {
        let af = family.map_or(0, Family::af);
        let (open, lmode, ifhead) = {
            let st = self.st.lock();
            (st.open, st.lmode, st.ifhead)
        };
        if !open {
            return Err(NoBufs);
        }
        self.tap.mtap2(&u32::from(af).to_ne_bytes(), &m, Way::Out);
        if lmode {
            let sa = dest_sockaddr(family, &m);
            match m.prepend(sa.len()) {
                Ok(b) => b.copy_from_slice(&sa),
                Err(e) => {
                    self.stats.iqdrops.add(1);
                    self.stats.oerrors.add(1);
                    return Err(e);
                }
            }
        }
        if ifhead {
            match m.prepend(4) {
                Ok(b) => b.copy_from_slice(&u32::from(af).to_be_bytes()),
                Err(e) => {
                    self.stats.iqdrops.add(1);
                    self.stats.oerrors.add(1);
                    return Err(e);
                }
            }
        } else if family != Some(Family::Inet) {
            return Err(NoBufs);
        }
        self.enqueue(m)
    }

    /// `tap_transmit` and `tunstart_l2`: a frame, tapped, for the reader;
    /// dropped as an error unless the device is open (a `vmnet` keeps
    /// it).
    fn output_l2(&self, m: Packet) -> Result<(), NoBufs> {
        self.tap.mtap(&m, Way::Out);
        let ready = {
            let st = self.st.lock();
            st.open || self.kind == Kind::Vmnet
        };
        if !ready {
            self.stats.oerrors.add(1);
            return Ok(());
        }
        self.enqueue(m)
    }

    /// `tun_vnethdr_set`'s capabilities: the virtio-net header's offloads
    /// are offered while it is on.
    fn vnethdr_caps(&self, len: usize) {
        const CAPS: u32 = ifcap::HWCSUM
            | ifcap::HWCSUM_IPV6
            | ifcap::VLAN_HWCSUM
            | ifcap::TSO
            | ifcap::LRO
            | ifcap::VLAN_HWTSO;
        let (Some(v), index) = self.home() else {
            return;
        };
        v.net.lock().with_index(index, |i| {
            if len == 0 {
                i.capabilities &= !CAPS;
            } else {
                i.capabilities |= CAPS;
            }
            i.capenable &= i.capabilities;
        });
    }

    /// The device's requests that need nothing of the kernel's (`tunioctl`):
    /// `Ok(true)` if `req` was one, `Ok(false)` if not; `pid` is the
    /// caller's.
    ///
    /// # Errors
    ///
    /// What the request fails with.
    pub fn ioctl(&self, req: u32, arg: &mut [u8], pid: i32) -> Result<bool, Errno> {
        use abi::tun as t;
        let int = |arg: &[u8]| {
            arg.get(..4)
                .and_then(|b| b.try_into().ok())
                .map_or(0, i32::from_ne_bytes)
        };
        let put = |arg: &mut [u8], v: i32| {
            if let Some(a) = arg.get_mut(..4) {
                a.copy_from_slice(&v.to_ne_bytes());
            }
        };
        let (Some(v), index) = self.home() else {
            return Err(Errno::ENXIO);
        };
        if self.kind.l2() {
            match req {
                t::VMIO_SIOCSIFFLAGS => {
                    // Straight into the flags, as VMware's tools expect.
                    let f = u32::from_ne_bytes(int(arg).to_ne_bytes()) & 0x0fff & !iff::CANTCHANGE
                        | iff::UP;
                    v.net
                        .lock()
                        .with_index(index, |i| i.flags = f | (i.flags & iff::CANTCHANGE));
                    return Ok(true);
                }
                abi::net::sio::SIOCGIFADDR => {
                    let r = self.st.lock().remote;
                    if let Some(a) = arg.get_mut(..6) {
                        a.copy_from_slice(&r);
                    }
                    return Ok(true);
                }
                abi::net::sio::SIOCSIFADDR => {
                    let mut st = self.st.lock();
                    if let Some(a) = arg.get(..6) {
                        st.remote.copy_from_slice(a);
                    }
                    return Ok(true);
                }
                t::TAPSVNETHDR => {
                    let len = usize::try_from(int(arg)).map_err(|_| Errno::EINVAL)?;
                    if len != 0 && len != t::VNET_HDR_LEN && len != t::VNET_HDR_MRG_LEN {
                        return Err(Errno::EINVAL);
                    }
                    self.st.lock().vhdrlen = len;
                    self.vnethdr_caps(len);
                    return Ok(true);
                }
                t::TAPGVNETHDR => {
                    put(arg, i32::try_from(self.vhdrlen()).unwrap_or(0));
                    return Ok(true);
                }
                _ => {}
            }
        } else {
            match req {
                t::TUNSLMODE => {
                    let mut st = self.st.lock();
                    st.lmode = int(arg) != 0;
                    if st.lmode {
                        st.ifhead = false;
                    }
                    return Ok(true);
                }
                t::TUNSIFHEAD => {
                    let mut st = self.st.lock();
                    st.ifhead = int(arg) != 0;
                    if st.ifhead {
                        st.lmode = false;
                    }
                    return Ok(true);
                }
                t::TUNGIFHEAD => {
                    put(arg, i32::from(self.st.lock().ifhead));
                    return Ok(true);
                }
                t::TUNSIFMODE => {
                    let want = u32::from_ne_bytes(int(arg).to_ne_bytes());
                    let mut n = v.net.lock();
                    if n.by_index(index).is_some_and(|i| i.flags & iff::UP != 0) {
                        return Err(Errno::EBUSY);
                    }
                    match want & !iff::MULTICAST {
                        iff::POINTOPOINT | iff::BROADCAST => {
                            n.with_index(index, |i| {
                                i.flags &= !(iff::BROADCAST | iff::POINTOPOINT | iff::MULTICAST);
                                i.flags |= want;
                            });
                        }
                        _ => return Err(Errno::EINVAL),
                    }
                    return Ok(true);
                }
                t::TUNSIFPID => {
                    self.st.lock().pid = pid;
                    return Ok(true);
                }
                _ => {}
            }
        }
        match req {
            t::TUNGIFNAME => {
                let name = v
                    .net
                    .lock()
                    .by_index(index)
                    .map(|i| i.name.clone())
                    .unwrap_or_default();
                let n = name.len().min(abi::net::IFNAMSIZ - 1);
                if let Some(a) = arg.get_mut(..abi::net::IFNAMSIZ) {
                    a.fill(0);
                    a[..n].copy_from_slice(&name.as_bytes()[..n]);
                }
            }
            t::TUNGIFINFO => {
                let (mtu, ty, baud) = v
                    .net
                    .lock()
                    .by_index(index)
                    .map_or((0, 0, 0), |i| (i.mtu, i.ty, i.baudrate));
                if arg.len() >= 8 {
                    arg[0..4].copy_from_slice(&i32::try_from(baud).unwrap_or(0).to_ne_bytes());
                    arg[4..6].copy_from_slice(&u16::try_from(mtu).unwrap_or(0).to_ne_bytes());
                    arg[6] = ty;
                }
            }
            t::TUNSTRANSIENT => self.st.lock().transient = int(arg) != 0,
            t::TUNGTRANSIENT => put(arg, i32::from(self.st.lock().transient)),
            t::TUNSDEBUG => DEBUG.store(int(arg), Ordering::Relaxed),
            t::TUNGDEBUG => put(arg, DEBUG.load(Ordering::Relaxed)),
            abi::ioctl::FIONBIO => {}
            abi::ioctl::FIOASYNC => self.st.lock().async_io = int(arg) != 0,
            abi::ioctl::FIONREAD => put(arg, i32::try_from(self.first_len()).unwrap_or(i32::MAX)),
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// `FIOASYNC` set or cleared through `fcntl`.
    pub fn set_async(&self, on: bool) {
        self.st.lock().async_io = on;
    }

    /// `TUNSIFINFO`, once the caller may change the MTU: the type must be
    /// the interface's; the MTU and speed are set.
    ///
    /// # Errors
    ///
    /// `EPROTOTYPE` for another type, `EINVAL` for an MTU out of range.
    pub fn set_info(&self, info: &abi::tun::Tuninfo) -> Result<(), Errno> {
        let (Some(v), index) = self.home() else {
            return Err(Errno::ENXIO);
        };
        let mut n = v.net.lock();
        let i = n.by_index(index).ok_or(Errno::ENXIO)?;
        if i.ty != info.r#type {
            return Err(Errno::EPROTOTYPE);
        }
        if i.mtu != u32::from(info.mtu) {
            // `ifhwioctl`'s `SIOCSIFMTU`.
            if !(72..=65535).contains(&u32::from(info.mtu)) {
                return Err(Errno::EINVAL);
            }
            if i.bridge.is_some() {
                return Err(Errno::EOPNOTSUPP);
            }
            let name = i.name.clone();
            n.set_mtu(name.as_bytes(), u32::from(info.mtu))?;
        }
        let baud = u64::try_from(info.baudrate).unwrap_or(0);
        n.with_index(index, |i| i.baudrate = baud);
        Ok(())
    }

    /// `TUNGIFINFO`'s and `TUNSIFINFO`'s view of whether the caller must be
    /// privileged: only to change the MTU.
    #[must_use]
    pub fn mtu_changes(&self, info: &abi::tun::Tuninfo) -> bool {
        self.mtu() != u32::from(info.mtu)
    }
}

/// The destination of IP packet `m` as a `sockaddr` (`TUNSLMODE`'s
/// prefix, where FreeBSD puts `if_output`'s `dst`).
fn dest_sockaddr(family: Option<Family>, m: &Packet) -> Vec<u8> {
    match family {
        Some(Family::Inet6) => {
            let mut a = [0u8; 16];
            m.data.copy_out(24, &mut a);
            crate::addr::sockaddr_in6(&a, 0, 0)
        }
        _ => {
            let mut a = [0u8; 4];
            m.data.copy_out(16, &mut a);
            crate::addr::sockaddr_in(a, 0)
        }
    }
}

impl Link for Tun {
    fn transmit(
        &self,
        _vnet: &Arc<Vnet>,
        _index: u16,
        family: Option<Family>,
        m: Packet,
    ) -> Result<(), NoBufs> {
        if self.kind.l2() {
            self.output_l2(m)
        } else {
            self.output_l3(family, m)
        }
    }

    /// `tunoutput` refuses while no one has the device open.
    fn ready(&self) -> Result<(), Errno> {
        if !self.kind.l2() && !self.st.lock().open {
            return Err(Errno::EHOSTDOWN);
        }
        Ok(())
    }

    fn status(&self) -> Option<String> {
        let pid = self.st.lock().pid;
        Some(if pid != 0 {
            format!("\tOpened by PID {pid}\n")
        } else {
            String::new()
        })
    }

    /// A `tap` is gigabit Ethernet, active while open; a `tun` has no
    /// media.
    fn media(&self) -> Option<Media> {
        if !self.kind.l2() {
            return None;
        }
        let word = ifm::ETHER | ifm::FDX | ifm::T1000;
        let active = if self.st.lock().open { ifm::ACTIVE } else { 0 };
        Some(Media {
            current: word,
            active: word,
            status: ifm::AVALID | active,
            supported: ifm::ETHER,
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// `tun_clone_create` and `tuncreate`: device `kind` with `unit` (or the
/// lowest free) and its interface in `vnet`; returns the interface's
/// name.
///
/// # Errors
///
/// `EEXIST` if the unit is taken, `ENOSPC` for no unit left.
pub fn create(vnet: &Arc<Vnet>, kind: Kind, unit: Option<i32>, ctx: &Ctx) -> Result<String, Errno> {
    let named = |u: i32| {
        vnet.net
            .lock()
            .by_name(format!("{}{u}", kind.name()).as_bytes())
            .is_some()
    };
    let u = {
        let mut units = UNITS.lock();
        let u = match unit {
            Some(u) if u > MAXUNIT => return Err(Errno::ENOSPC),
            Some(u) if units.contains(&(kind, u)) => return Err(Errno::EEXIST),
            Some(u) => u,
            None => (0..=MAXUNIT)
                .find(|u| !units.contains(&(kind, *u)) && !named(*u))
                .ok_or(Errno::ENOSPC)?,
        };
        units.insert((kind, u));
        u
    };
    let name = format!("{}{u}", kind.name());
    let tap = if kind.l2() { Tap::ether() } else { Tap::null() };
    let tun = Arc::new(Tun {
        kind,
        unit: u,
        stats: Arc::new(Stats::default()),
        tap: Arc::clone(&tap),
        st: PreemptLock::new(State::default()),
        wake: sync::Once::new(),
    });
    let caps = ifcap::LINKSTATE | ifcap::MEXTPG;
    let (ty, flags, capabilities, lladdr) = if kind.l2() {
        let ll = link::gen_addr(&ctx.hostuuid, &name, &ctx.jail).unwrap_or_else(|| {
            let r = &ctx.random;
            link::random_addr([r[0], r[1], r[2], r[3], r[4], r[5]])
        });
        (
            IFT_ETHER,
            iff::BROADCAST | iff::SIMPLEX | iff::MULTICAST,
            caps | ifcap::RXCSUM | ifcap::RXCSUM_IPV6 | ifcap::LRO,
            Some(ll),
        )
    } else {
        (IFT_PPP, iff::POINTOPOINT | iff::MULTICAST, caps, None)
    };
    let mut n = vnet.net.lock();
    let index = n.next_index();
    let i = Ifnet {
        index,
        name: name.clone(),
        dname: kind.name(),
        dunit: u,
        home: vnet.id,
        fib: 0,
        flags,
        mtu: abi::tun::TUNMTU,
        metric: 0,
        ty,
        link_state: LINK_STATE_UNKNOWN,
        capabilities,
        capenable: caps,
        nd6_flags: nd6_iff::PERFORMNUD | nd6_iff::IFDISABLED | nd6_iff::AUTO_LINKLOCAL,
        nd: crate::ifnet::NdInfo::DEFAULT,
        description: None,
        groups: alloc::vec![String::from(kind.name())],
        addrs: Vec::new(),
        epoch: ctx.now,
        lastchange: (ctx.now, 0),
        stats: Arc::clone(&tun.stats),
        lladdr,
        baudrate: 0,
        link: LinkRef(Some(Arc::clone(&tun) as Arc<dyn Link>)),
        bpf: Some(tap),
        pcount: 0,
        maddrs: Vec::new(),
        pcp: crate::ifnet::PCP_NONE,
        bridge: None,
    };
    n.attach(i);
    drop(n);
    if let Some(h) = HOOKS.get() {
        (h.made)(&tun);
    }
    Ok(name)
}

/// `tun_destroy`: the device goes, its unit free again; the interface is
/// the caller's to detach.
pub(crate) fn destroy(tun: &Tun) {
    {
        let mut st = tun.st.lock();
        st.dying = true;
        st.q.clear();
    }
    tun.wake();
    if let Some(h) = HOOKS.get() {
        (h.gone)(tun);
    }
    UNITS.lock().remove(&(tun.kind, tun.unit));
}

/// `tunrename`: interface `name` was renamed; a device of ours gets an
/// alias by the new name.
pub(crate) fn renamed(link: &LinkRef, name: &str) {
    if let Some(t) = link
        .0
        .as_ref()
        .and_then(|l| l.as_any().downcast_ref::<Tun>())
        && let Some(h) = HOOKS.get()
    {
        (h.renamed)(t, name);
    }
}

/// The units handed out, for tests.
#[cfg(test)]
pub(crate) fn unit_taken(kind: Kind, unit: i32) -> bool {
    UNITS.lock().contains(&(kind, unit))
}
