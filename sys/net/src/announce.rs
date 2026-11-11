// SPDX-License-Identifier: BSD-2-Clause
//! Announcements: what changed in a VNET's interfaces, addresses, and
//! routes, for the routing sockets and netlink sockets listening there
//! (`rt_ifannouncemsg`, `rt_ifmsg`, `rt_newaddrmsg_fib`, `rt_routemsg`,
//! and netlink's `RTNLGRP_*` notifications).
//!
//! A change records its events in the [`Net`] it changes; the guard that
//! held the lock delivers them once it lets go, so a listener never runs
//! under the lock and sees each change complete.
//!
//! Behavior reference: sys/net/rtsock.c, sys/netlink/route/iface.c
//! (`rtnl_handle_ifevent`), sys/netlink/route/rt.c (`rtnl_handle_route_event`).

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

use sync::SpinLock;

use crate::ifnet::{Ifaddr, Ifnet, Net, Route};

/// A change.
#[derive(Clone, Debug)]
pub enum Event {
    /// An interface came or went (`RTM_IFANNOUNCE`, netlink's
    /// `RTM_NEWLINK` and `RTM_DELLINK`), as it was.
    Announce { ifp: Ifnet, arrival: bool },
    /// An interface's flags or link changed (`RTM_IFINFO`): its state
    /// after.
    Info(Ifnet),
    /// An interface's link went up or down (`if_link_state_change`):
    /// `RTM_IFINFO` as [`Event::Info`], and `devd`'s `LINK_UP` or
    /// `LINK_DOWN`.
    Link { ifp: Ifnet, up: bool },
    /// An interface was renamed from `old` (`if_rename`'s `devctl`
    /// notice; its departure and arrival are announced before it).
    Rename {
        ifp: Ifnet,
        old: alloc::string::String,
    },
    /// An address was added or deleted (`RTM_NEWADDR`, `RTM_DELADDR`), on
    /// the interface as it was then.
    Addr { add: bool, ifp: Ifnet, addr: Ifaddr },
    /// A route was added, deleted, or changed (`RTM_ADD`, `RTM_DELETE`,
    /// `RTM_CHANGE`), with its interface, if it still has one.
    /// `rtsock` marks one its reply already announces to routing
    /// sockets.
    Route {
        ty: u8,
        route: Route,
        ifp: Option<Ifnet>,
        rtsock: bool,
    },
    /// A routing socket's reply, which announces what its request did
    /// (`rts_send`'s `rt_dispatch`): for every routing socket of the
    /// family, the sender's only with `SO_USELOOPBACK`.
    Reply {
        family: Option<crate::Family>,
        msg: Vec<u8>,
        /// The sending socket's identity.
        from: usize,
        /// The routing table the request was in.
        fib: u16,
    },
}

/// Something listening for a VNET's changes.
pub trait Listener: Send + Sync {
    /// Called with each change, after the lock is let go.
    fn event(&self, e: &Event);
}

/// A VNET's interfaces and routes, and who listens for their changes.
/// The lock holds off preemption, not interrupts: no interrupt handler
/// takes it (FreeBSD's are sleep mutexes and rmlocks, interrupts on).
pub struct NetLock {
    net: sync::PreemptLock<Net>,
    listeners: SpinLock<Vec<Weak<dyn Listener>>>,
    /// What the packet paths read without the lock, remade when a holder
    /// changes it ([`crate::view`]).
    view: crate::view::Published,
    /// [`Net::hash_outbound`] as of the last change, a plain word for
    /// every packet (`V_fib_hash_outbound`).
    hash_outbound: AtomicBool,
}

impl core::fmt::Debug for NetLock {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NetLock").finish_non_exhaustive()
    }
}

impl NetLock {
    #[must_use]
    pub fn new(net: Net) -> Self {
        Self {
            view: crate::view::Published::new(crate::view::View::of(&net)),
            hash_outbound: AtomicBool::new(net.hash_outbound),
            net: sync::PreemptLock::new(net),
            listeners: SpinLock::new(Vec::new()),
        }
    }

    /// Takes the writers' lock.
    pub fn lock(&self) -> NetGuard<'_> {
        NetGuard {
            g: Some(self.net.lock()),
            owner: self,
            changed: false,
        }
    }

    /// The interfaces and addresses as of the last change, for as long as
    /// the epoch section `s` lasts, without the lock.
    pub fn view<'s>(&self, s: &'s crate::view::Section) -> &'s crate::view::View {
        self.view.get(s)
    }

    /// Whether this host's packets get hashed flows
    /// (`V_fib_hash_outbound`), without the lock.
    pub fn hash_outbound(&self) -> bool {
        // Relaxed: a setting, read as FreeBSD reads its word.
        self.hash_outbound.load(Ordering::Relaxed)
    }

    /// Adds a listener; it goes when it is dropped.
    pub fn listen(&self, l: &Arc<dyn Listener>) {
        let mut ls = self.listeners.lock();
        ls.retain(|w| w.strong_count() > 0);
        ls.push(Arc::downgrade(l));
    }

    fn deliver(&self, events: &[Event]) {
        if events.is_empty() {
            return;
        }
        let ls: Vec<Arc<dyn Listener>> = self
            .listeners
            .lock()
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        for e in events {
            for l in &ls {
                l.event(e);
            }
        }
    }
}

/// The lock held; changes are announced when it goes.
pub struct NetGuard<'a> {
    g: Option<sync::PreemptLockGuard<'a, Net>>,
    owner: &'a NetLock,
    /// Taken for writing: the view is remade when the lock goes.
    changed: bool,
}

impl NetGuard<'_> {
    /// The tables for a packet's way out, which changes only what the
    /// view does not hold (the frame queues, the link-layer table, ARP's
    /// counts): unlike a write through `DerefMut`, the view is not
    /// remade for it.
    pub fn data_path(&mut self) -> &mut Net {
        self.g
            .as_mut()
            .unwrap_or_else(|| unreachable!("held until dropped"))
    }
}

impl core::fmt::Debug for NetGuard<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NetGuard").finish_non_exhaustive()
    }
}

impl Deref for NetGuard<'_> {
    type Target = Net;
    fn deref(&self) -> &Net {
        self.g
            .as_ref()
            .unwrap_or_else(|| unreachable!("held until dropped"))
    }
}

impl DerefMut for NetGuard<'_> {
    fn deref_mut(&mut self) -> &mut Net {
        self.changed = true;
        self.g
            .as_mut()
            .unwrap_or_else(|| unreachable!("held until dropped"))
    }
}

/// What a [`NetGuard`] hands on after the lock goes, the first kept
/// inline: most holds send one frame, and allocate nothing for it.
struct Few<T> {
    first: Option<T>,
    rest: Vec<T>,
}

impl<T> Few<T> {
    const fn new() -> Self {
        Self {
            first: None,
            rest: Vec::new(),
        }
    }

    fn push(&mut self, x: T) {
        if self.first.is_none() {
            self.first = Some(x);
        } else {
            self.rest.push(x);
        }
    }

    fn is_empty(&self) -> bool {
        self.first.is_none()
    }

    fn into_iter(self) -> impl Iterator<Item = T> {
        self.first.into_iter().chain(self.rest)
    }
}

impl Drop for NetGuard<'_> {
    fn drop(&mut self) {
        let mut tx = Few::new();
        let mut isr = Few::new();
        let (events, vnet) = match self.g.as_mut() {
            Some(g) => {
                // Drained, not taken: the queues keep their room.
                let g = &mut **g;
                if self.changed {
                    self.owner.view.replace(crate::view::View::of(g));
                    // Relaxed: see `NetLock::hash_outbound`.
                    self.owner
                        .hash_outbound
                        .store(g.hash_outbound, Ordering::Relaxed);
                }
                for (index, family, mut m) in g.txq.drain(..) {
                    let Some(i) = g.ifaces.iter().find(|i| i.index == index) else {
                        continue;
                    };
                    // `ether_output_frame`'s `ether_do_pcp`.
                    if i.ty == abi::net::IFT_ETHER
                        && i.bridge.is_none()
                        && (i.pcp != crate::ifnet::PCP_NONE
                            || m.hdr.flags & mbuf::flags::VLANTAG != 0)
                    {
                        let hw = i.capenable & abi::net::ifcap::VLAN_HWTAGGING != 0;
                        if crate::ether::set_pcp(&mut m, i.pcp, hw).is_err() {
                            continue;
                        }
                    }
                    let Some(link) = i.link.0.clone() else {
                        continue;
                    };
                    // `ether_output`'s `BRIDGE_OUTPUT`: a member's frames
                    // are bridged.
                    tx.push((index, family, i.flags, link, i.bridge.clone(), m));
                }
                for e in g.isrq.drain(..) {
                    isr.push(e);
                }
                let vnet = if tx.is_empty() && isr.is_empty() {
                    None
                } else {
                    g.owner.upgrade()
                };
                (core::mem::take(&mut g.outbox), vnet)
            }
            None => (Vec::new(), None),
        };
        drop(self.g.take());
        self.owner.deliver(&events);
        // Frames queued under the lock go out now, in order.
        if let Some(v) = vnet {
            for (proto, m) in isr.into_iter() {
                let _ = v.netisr.queue(proto, m);
            }
            for (index, family, flags, link, bridge, m) in tx.into_iter() {
                match bridge {
                    Some(b) => b.output(&v, index, m),
                    None => {
                        let _ = crate::ether::output_frame(&v, index, &link, flags, family, m);
                    }
                }
            }
        }
    }
}
