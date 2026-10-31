// SPDX-License-Identifier: BSD-2-Clause
//! The net class (docs/NETWORKING.md §5.2): what a network driver's
//! interface is to the stack. A driver makes a [`Port`], attaches it as
//! an Ethernet interface with its device ([`Link`]), hands the frames it
//! receives to [`Port::input`], and reports its link with
//! [`Port::link_state`]. The port follows the interface into whichever
//! VNET it is moved to.
//!
//! Behavior reference: sys/net/if_ethersubr.c (`ether_ifattach`,
//! `ether_ifdetach`), sys/net/if.c (`if_attach`, `if_link_state_change`).

use alloc::string::String;
use alloc::sync::{Arc, Weak};

use abi::net::{IFT_ETHER, LINK_STATE_DOWN, LINK_STATE_UNKNOWN, LINK_STATE_UP, iff, nd6_iff};
use mbuf::Packet;
use sync::SpinLock;

use crate::Vnet;
use crate::announce::Event;
use crate::bpf::Tap;
use crate::ifnet::{Ifnet, Stats};
use crate::link::{Link, LinkRef};

/// A driver's side of its interface.
#[derive(Debug)]
pub struct Port {
    /// The VNET the interface is in, and its index there.
    home: SpinLock<(Weak<Vnet>, u16)>,
    /// Its counters, which the driver keeps.
    pub stats: Arc<Stats>,
    /// Its `bpf` tap, for the driver's transmit side (`ETHER_BPF_MTAP`).
    pub tap: Arc<Tap>,
}

impl Port {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            home: SpinLock::new((Weak::new(), 0)),
            stats: Arc::new(Stats::default()),
            tap: Tap::ether(),
        })
    }

    /// The interface's VNET and index.
    #[must_use]
    pub fn home(&self) -> (Option<Arc<Vnet>>, u16) {
        let h = self.home.lock();
        (h.0.upgrade(), h.1)
    }

    /// Records a move (`Link::moved` passes it here).
    pub fn moved(&self, vnet: &Arc<Vnet>, index: u16) {
        *self.home.lock() = (Arc::downgrade(vnet), index);
    }

    /// A received frame, its Ethernet header first (`if_input`): up
    /// through Ethernet input in the caller's thread.
    pub fn input(&self, mut m: Packet) {
        let (Some(v), index) = self.home() else {
            return;
        };
        m.hdr.rcvif = index;
        m.hdr.vnet = v.id;
        crate::ether::input(&v, m);
    }

    /// The link went up or down (`if_link_state_change`): announced to
    /// the routing sockets and `devd`.
    pub fn link_state(&self, up: bool) {
        let (Some(v), index) = self.home() else {
            return;
        };
        let mut n = v.net.lock();
        let state = if up { LINK_STATE_UP } else { LINK_STATE_DOWN };
        let Some(ifp) = n.ifaces.iter_mut().find(|i| i.index == index) else {
            return;
        };
        if ifp.link_state == state {
            return;
        }
        ifp.link_state = state;
        let snap = ifp.clone();
        n.outbox.push(Event::Link { ifp: snap, up });
        n.bump();
    }
}

/// An Ethernet interface to attach.
#[derive(Clone, Debug)]
pub struct Ether {
    /// The driver's name and unit (`vtnet`, 0).
    pub dname: &'static str,
    pub unit: i32,
    pub lladdr: [u8; 6],
    pub mtu: u32,
    /// `IFCAP_*` available and on.
    pub capabilities: u32,
    pub capenable: u32,
    pub baudrate: u64,
}

/// `ether_ifattach`: attaches `port`'s interface, served by `link`, to
/// the system's VNET, down, its link state unknown until it comes up;
/// returns its name.
pub fn attach_ether(e: &Ether, port: &Arc<Port>, link: Arc<dyn Link>) -> String {
    let v = crate::vnet0();
    let name = alloc::format!("{}{}", e.dname, e.unit);
    let now = crate::realtime();
    let mut n = v.net.lock();
    let index = n.next_index();
    port.moved(v, index);
    let i = Ifnet {
        index,
        name: name.clone(),
        dname: e.dname,
        dunit: e.unit,
        home: v.id,
        fib: 0,
        flags: iff::BROADCAST | iff::SIMPLEX | iff::MULTICAST,
        mtu: e.mtu,
        metric: 0,
        ty: IFT_ETHER,
        // Unknown until the driver starts (`if_attach`).
        link_state: LINK_STATE_UNKNOWN,
        capabilities: e.capabilities,
        capenable: e.capenable,
        // `nd6_ifattach`: IPv6 off until enabled, as `rc.conf` leaves an
        // interface without `ifconfig_<if>_ipv6`.
        nd6_flags: nd6_iff::PERFORMNUD | nd6_iff::IFDISABLED | nd6_iff::AUTO_LINKLOCAL,
        description: None,
        groups: alloc::vec![String::from(e.dname)],
        addrs: alloc::vec::Vec::new(),
        epoch: now,
        lastchange: (now, 0),
        stats: Arc::clone(&port.stats),
        lladdr: Some(e.lladdr),
        baudrate: e.baudrate,
        link: LinkRef(Some(link)),
        bpf: Some(Arc::clone(&port.tap)),
        pcount: 0,
        maddrs: alloc::vec::Vec::new(),
    };
    n.attach(i);
    name
}

/// `ether_ifdetach`: the interface goes, wherever it is.
pub fn detach(port: &Port) {
    let (Some(v), index) = port.home() else {
        return;
    };
    let gone = v.net.lock().detach(index);
    if let Some(i) = gone {
        crate::clone::untap(&i);
    }
}
