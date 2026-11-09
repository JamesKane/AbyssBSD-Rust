// SPDX-License-Identifier: BSD-2-Clause
//! What the packet paths read of a VNET's interfaces and addresses,
//! without its lock: a copy made when a writer changes them, published
//! for readers inside the network epoch (docs/NETWORKING.md §6). FreeBSD
//! reads `ifnet`, `in_ifaddr` and its address hash under `NET_EPOCH` the
//! same way; its writers change the objects in place under the lock,
//! where these build a new copy, so a reader never sees half a change.
//!
//! A view goes when a newer one replaces it, after every section that
//! might still read it has ended (`epoch_call`).
//!
//! Behavior reference: sys/net/if.c (`ifnet_byindex`, under the epoch),
//! sys/netinet/in.c (`in_localip`, `INADDR_HASH`).

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicPtr, Ordering};

use crate::Family;
use crate::addr::In6;
use crate::addr::v4_of;
use crate::ifnet::{Ifaddr, Net, Route, Stats};
use crate::link::LinkRef;

/// The network epoch (`net_epoch_preempt`).
pub static NET_EPOCH: epoch::Epoch = epoch::Epoch::new("net");

/// A read section of the network epoch (`NET_EPOCH_ENTER`).
pub type Section = epoch::Section<'static>;

/// Enters the network epoch.
pub fn enter() -> Section {
    NET_EPOCH.enter()
}

/// An interface as the packet paths see it.
#[derive(Debug)]
pub struct IfView {
    pub index: u16,
    pub flags: u32,
    pub fib: u16,
    pub ty: u8,
    pub mtu: u32,
    pub capenable: u32,
    pub lladdr: Option<[u8; 6]>,
    pub bpf: Option<Arc<crate::bpf::Tap>>,
    pub stats: Arc<Stats>,
    pub addrs: Vec<Ifaddr>,
    /// The driver's side (`if_transmit`).
    pub link: LinkRef,
}

impl IfView {
    /// Its IPv4 addresses.
    pub fn addrs4(&self) -> impl Iterator<Item = &Ifaddr> {
        self.addrs.iter().filter(|a| a.family == Family::Inet)
    }
}

/// A VNET's interfaces and addresses at one moment.
#[derive(Debug)]
pub struct View {
    /// In the order of [`Net::ifaces`].
    pub ifaces: Vec<IfView>,
    /// The routes, with their next hops (the FIBs' `nhop_object`s).
    pub routes: Vec<Route>,
    /// `net.route.hash_outbound`.
    pub hash_outbound: bool,
}

impl View {
    /// A copy of what `n` has now.
    #[must_use]
    pub fn of(n: &Net) -> Self {
        Self {
            ifaces: n
                .ifaces
                .iter()
                .map(|i| IfView {
                    index: i.index,
                    flags: i.flags,
                    fib: i.fib,
                    ty: i.ty,
                    mtu: i.mtu,
                    capenable: i.capenable,
                    lladdr: i.lladdr,
                    bpf: i.bpf.clone(),
                    stats: Arc::clone(&i.stats),
                    addrs: i.addrs.clone(),
                    link: LinkRef(i.link.0.clone()),
                })
                .collect(),
            routes: n.routes.clone(),
            hash_outbound: n.hash_outbound,
        }
    }

    /// The interface with index `index` (`ifnet_byindex`).
    #[must_use]
    pub fn by_index(&self, index: u16) -> Option<&IfView> {
        self.ifaces.iter().find(|i| i.index == index)
    }

    /// The route to `dst` in table `fib` (`fib4_lookup`), as
    /// [`Net::route_in`].
    #[must_use]
    pub fn route_in(&self, fib: u16, family: Family, dst: &In6) -> Option<&Route> {
        crate::ifnet::route_in(&self.routes, fib, family, dst)
    }

    /// Whether `a` is an IPv4 address of some interface (`in_localip`).
    #[must_use]
    pub fn is_local4(&self, a: &[u8; 4]) -> bool {
        self.ifaces
            .iter()
            .any(|i| i.addrs4().any(|x| v4_of(&x.addr) == *a))
    }
}

/// Where a VNET's current view is published.
#[derive(Debug)]
pub(crate) struct Published(AtomicPtr<View>);

impl Published {
    pub(crate) fn new(v: View) -> Self {
        Self(AtomicPtr::new(Box::into_raw(Box::new(v))))
    }

    /// The current view, for as long as the section `_s` lasts.
    pub(crate) fn get<'s>(&self, _s: &'s Section) -> &'s View {
        // Acquire: pairs with the release in `replace`; the view was
        // built before it was published.
        let p = self.0.load(Ordering::Acquire);
        // SAFETY: never null (set at `new` and by `replace`); a view is
        // freed only by `retire`, after every section that began before
        // it was unpublished has ended, and `_s` began before any
        // unpublishing of the one loaded here can have been waited out.
        unsafe { &*p }
    }

    /// Publishes `v` in place of the current view, which goes once no
    /// section can still see it.
    pub(crate) fn replace(&self, v: View) {
        // Release: the view's contents before the pointer.
        let old = self.0.swap(Box::into_raw(Box::new(v)), Ordering::AcqRel);
        retire(old);
    }
}

impl Drop for Published {
    fn drop(&mut self) {
        retire(*self.0.get_mut());
    }
}

/// Frees `old` after a grace period (`epoch_call`).
fn retire(old: *mut View) {
    let old = SendPtr(old);
    NET_EPOCH.call(Box::new(move || {
        let p = old;
        // SAFETY: unpublished before this call was queued, so after the
        // grace period no section can still read it; made by
        // `Box::into_raw`, freed once.
        drop(unsafe { Box::from_raw(p.0) });
    }));
}

/// A view's pointer, carried to the deferred free.
struct SendPtr(*mut View);

// SAFETY: a view is `Send` (its parts are), and the pointer is only used
// to free it once, on whichever CPU runs the call.
unsafe impl Send for SendPtr {}

/// Waits until no section can still see an interface just taken out,
/// and frees the views that held it (`if_detach_internal`'s
/// `epoch_drain_callbacks(net_epoch_preempt)`): its device goes only
/// after this. Thread context, holding no lock.
pub fn drain() {
    NET_EPOCH.drain(&mut core::hint::spin_loop);
}

/// Frees the views whose grace period has passed (the epoch's callback
/// task): call from a thread that may wait, holding no lock; `pause`
/// yields while it waits.
pub fn reclaim(pause: &mut dyn FnMut()) {
    NET_EPOCH.reclaim(pause);
}
