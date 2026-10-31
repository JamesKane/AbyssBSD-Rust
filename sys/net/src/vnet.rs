// SPDX-License-Identifier: BSD-2-Clause
//! VNETs: separate network stacks (FreeBSD's `struct vnet`). Each has its
//! own interfaces, addresses, and routes, and its own `lo0`.
//!
//! Behavior reference: sys/net/vnet.c (`vnet_alloc`, `vnet_destroy`),
//! sys/net/if_loop.c (`vnet_loif_init`).

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use sync::{Once, SpinLock};

use crate::announce::NetLock;
use crate::ifnet::{Net, Stats};
use crate::netisr::Queues;

/// A network stack.
#[derive(Debug)]
pub struct Vnet {
    /// Its number, 0 for the system's.
    pub id: u32,
    /// Its interfaces and routes, under the writers' lock, with their
    /// listeners.
    pub net: NetLock,
    /// `lo0`'s counters.
    pub lo: Arc<Stats>,
    /// Its `netisr` queues.
    pub netisr: Queues,
    /// The units its cloners have handed out (`ifc_unrhdr`).
    pub(crate) units: SpinLock<Vec<(&'static str, i32)>>,
}

static NEXT: AtomicU32 = AtomicU32::new(1);
/// Every VNET, for the `netisr` thread and jail lookups.
static ALL: SpinLock<Vec<Weak<Vnet>>> = SpinLock::new(Vec::new());
static VNET0: Once<Arc<Vnet>> = Once::new();

impl Vnet {
    fn with_id(id: u32, now: i64) -> Arc<Self> {
        let lo = Arc::new(Stats::default());
        let mut net = Net::new();
        let mut lo0 = crate::clone::lo_ifnet(0, net.next_index(), now, Arc::clone(&lo));
        lo0.home = id;
        net.attach(lo0);
        // No one listens yet.
        net.outbox.clear();
        let v = Arc::new(Self {
            id,
            net: NetLock::new(net),
            lo,
            netisr: Queues::new(),
            units: SpinLock::new(alloc::vec![("lo", 0)]),
        });
        let mut all = ALL.lock();
        all.retain(|w| w.strong_count() > 0);
        all.push(Arc::downgrade(&v));
        v
    }

    /// Every VNET alive.
    #[must_use]
    pub fn all() -> Vec<Arc<Self>> {
        ALL.lock().iter().filter_map(Weak::upgrade).collect()
    }

    /// A new VNET (`vnet_alloc`), with `lo0` down and unaddressed until
    /// configured, as a VNET jail's is.
    #[must_use]
    pub fn new(now: i64) -> Arc<Self> {
        Self::with_id(NEXT.fetch_add(1, Ordering::Relaxed), now)
    }
}

/// The system's VNET (`vnet0`), made on first use.
pub fn vnet0() -> &'static Arc<Vnet> {
    if let Some(v) = VNET0.get() {
        return v;
    }
    // A racing first use makes one too; one of them is kept.
    let _ = VNET0.set(Vnet::with_id(0, 0));
    VNET0.get().unwrap_or_else(|| unreachable!("just set"))
}
