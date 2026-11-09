// SPDX-License-Identifier: BSD-2-Clause
//! `netisr`: the queues a packet waits on between an interface's output
//! and a protocol's input where FreeBSD defers rather than dispatching
//! directly (a loopback interface's output, so a send never recurses into
//! a receive). Each VNET has its queues; the protocols register one
//! handler each, for every VNET.
//!
//! The kernel's `netisr` thread drains them: a queue going from empty to
//! not calls the kick hook, and the thread calls [`drain`] on each VNET
//! in turn until all are empty.
//!
//! Behavior reference: sys/net/netisr.c (`netisr_queue_src`,
//! `netisr_process_workstream_proto`, `net.isr.defaultqlimit`).

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use mbuf::Packet;
use sync::SpinLock;

use crate::Vnet;

/// The protocols that take queued packets (`NETISR_*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Proto {
    Ip = 0,
    Arp = 1,
    Ether = 2,
    Ipv6 = 3,
}

const PROTOS: usize = 4;

/// `net.isr.defaultqlimit`.
pub const QLIMIT: usize = 256;

/// A protocol's input.
pub type Handler = fn(&Arc<Vnet>, Packet);

/// The protocols' inputs and the kick hook, each set once at start-up,
/// read without a lock on every packet.
static HANDLERS: [sync::Once<Handler>; PROTOS] = [const { sync::Once::new() }; PROTOS];
static KICK: sync::Once<fn()> = sync::Once::new();

/// Registers `proto`'s input (`netisr_register`); a second registration
/// is ignored.
pub fn register(proto: Proto, h: Handler) {
    let _ = HANDLERS[proto as usize].set(h);
}

/// Sets what is called when a queue gets work (once).
pub fn set_kick(f: fn()) {
    let _ = KICK.set(f);
}

fn handler(proto: Proto) -> Option<Handler> {
    HANDLERS[proto as usize].get().copied()
}

/// A VNET's queued packets, and how many each protocol has
/// (`nw_len`).
#[derive(Debug)]
struct Waiting {
    q: VecDeque<(Proto, Packet)>,
    len: [usize; PROTOS],
}

/// A VNET's queues (`struct netisr_workstream`'s, one per protocol).
#[derive(Debug)]
pub struct Queues {
    q: SpinLock<Waiting>,
    /// Queued, handled, and dropped for a full queue or no handler
    /// (`nw_queued`, `nw_handled`, `nw_qdrops`).
    pub queued: AtomicU64,
    pub handled: AtomicU64,
    pub qdrops: AtomicU64,
}

impl Default for Queues {
    fn default() -> Self {
        Self::new()
    }
}

impl Queues {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            q: SpinLock::new(Waiting {
                q: VecDeque::new(),
                len: [0; PROTOS],
            }),
            queued: AtomicU64::new(0),
            handled: AtomicU64::new(0),
            qdrops: AtomicU64::new(0),
        }
    }

    /// Queues `m` for `proto` (`netisr_queue`).
    ///
    /// # Errors
    ///
    /// The packet back when the queue is full (`ENOBUFS`).
    pub fn queue(&self, proto: Proto, m: Packet) -> Result<(), Packet> {
        let was_empty = {
            let mut w = self.q.lock();
            if w.len[proto as usize] >= QLIMIT {
                self.qdrops.fetch_add(1, Ordering::Relaxed);
                return Err(m);
            }
            w.q.push_back((proto, m));
            w.len[proto as usize] += 1;
            w.q.len() == 1
        };
        self.queued.fetch_add(1, Ordering::Relaxed);
        // The hook runs with no lock held: it may hand packets on at once.
        if was_empty && let Some(k) = KICK.get() {
            k();
        }
        Ok(())
    }

    /// Everything waiting, taken at once.
    fn take(&self) -> VecDeque<(Proto, Packet)> {
        let mut w = self.q.lock();
        w.len = [0; PROTOS];
        core::mem::take(&mut w.q)
    }

    /// Packets waiting.
    #[must_use]
    pub fn len(&self) -> usize {
        self.q.lock().q.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Hands `m` to `proto`'s input now (`netisr_dispatch` with the direct
/// policy, as Ethernet input does).
pub fn dispatch(vnet: &Arc<Vnet>, proto: Proto, m: Packet) {
    match handler(proto) {
        Some(h) => {
            h(vnet, m);
            vnet.netisr.handled.fetch_add(1, Ordering::Relaxed);
        }
        None => {
            vnet.netisr.qdrops.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Hands what was queued in `vnet` when it was called to its protocol,
/// in order; returns how many packets there were. What the handlers
/// queue meanwhile waits for the next call, as
/// `netisr_process_workstream_proto` takes a snapshot of its queue: a
/// VNET whose input keeps queueing more (a loopback flood) cannot starve
/// the others.
pub fn drain(vnet: &Arc<Vnet>) -> usize {
    let batch = vnet.netisr.take();
    let n = batch.len();
    for (proto, m) in batch {
        match handler(proto) {
            Some(h) => {
                h(vnet, m);
                vnet.netisr.handled.fetch_add(1, Ordering::Relaxed);
            }
            None => {
                vnet.netisr.qdrops.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    n
}
