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

use sync::{SpinGuard, SpinLock};

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
pub struct NetLock {
    net: SpinLock<Net>,
    listeners: SpinLock<Vec<Weak<dyn Listener>>>,
}

impl core::fmt::Debug for NetLock {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NetLock").finish_non_exhaustive()
    }
}

impl NetLock {
    #[must_use]
    pub const fn new(net: Net) -> Self {
        Self {
            net: SpinLock::new(net),
            listeners: SpinLock::new(Vec::new()),
        }
    }

    /// Takes the writers' lock.
    pub fn lock(&self) -> NetGuard<'_> {
        NetGuard {
            g: Some(self.net.lock()),
            owner: self,
        }
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
    g: Option<SpinGuard<'a, Net>>,
    owner: &'a NetLock,
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
        self.g
            .as_mut()
            .unwrap_or_else(|| unreachable!("held until dropped"))
    }
}

impl Drop for NetGuard<'_> {
    fn drop(&mut self) {
        let (events, tx, vnet) = match self.g.as_mut() {
            Some(g) => {
                let tx: Vec<_> = core::mem::take(&mut g.txq)
                    .into_iter()
                    .filter_map(|(index, m)| {
                        let i = g.by_index(index)?;
                        Some((index, i.flags, i.link.0.clone()?, m))
                    })
                    .collect();
                let vnet = if tx.is_empty() {
                    None
                } else {
                    g.owner.upgrade()
                };
                (core::mem::take(&mut g.outbox), tx, vnet)
            }
            None => (Vec::new(), Vec::new(), None),
        };
        drop(self.g.take());
        self.owner.deliver(&events);
        // Frames queued under the lock go out now, in order.
        if let Some(v) = vnet {
            for (index, flags, link, m) in tx {
                let _ = crate::ether::output_frame(&v, index, &link, flags, m);
            }
        }
    }
}
