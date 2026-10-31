// SPDX-License-Identifier: BSD-2-Clause
//! `netisr`: the queues a packet waits on between an interface's output
//! and a protocol's input where FreeBSD defers rather than dispatching
//! directly (a loopback interface's output, so a send never recurses into
//! a receive). Each VNET has its queues; the protocols register one
//! handler each, for every VNET.
//!
//! The kernel's `netisr` thread drains them: a queue going from empty to
//! not calls the kick hook, and the thread calls [`drain`].
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

static HANDLERS: SpinLock<[Option<Handler>; PROTOS]> = SpinLock::new([None; PROTOS]);
static KICK: SpinLock<Option<fn()>> = SpinLock::new(None);

/// Registers `proto`'s input (`netisr_register`).
pub fn register(proto: Proto, h: Handler) {
    HANDLERS.lock()[proto as usize] = Some(h);
}

/// Sets what is called when a queue gets work.
pub fn set_kick(f: fn()) {
    *KICK.lock() = Some(f);
}

/// A VNET's queues (`struct netisr_workstream`'s, one per protocol).
#[derive(Debug)]
pub struct Queues {
    q: SpinLock<VecDeque<(Proto, Packet)>>,
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
            q: SpinLock::new(VecDeque::new()),
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
            let mut q = self.q.lock();
            if q.iter().filter(|(p, _)| *p == proto).count() >= QLIMIT {
                self.qdrops.fetch_add(1, Ordering::Relaxed);
                return Err(m);
            }
            q.push_back((proto, m));
            q.len() == 1
        };
        self.queued.fetch_add(1, Ordering::Relaxed);
        if was_empty && let Some(k) = *KICK.lock() {
            k();
        }
        Ok(())
    }

    fn pop(&self) -> Option<(Proto, Packet)> {
        self.q.lock().pop_front()
    }

    /// Packets waiting.
    #[must_use]
    pub fn len(&self) -> usize {
        self.q.lock().len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Hands `m` to `proto`'s input now (`netisr_dispatch` with the direct
/// policy, as Ethernet input does).
pub fn dispatch(vnet: &Arc<Vnet>, proto: Proto, m: Packet) {
    let h = HANDLERS.lock()[proto as usize];
    match h {
        Some(h) => {
            h(vnet, m);
            vnet.netisr.handled.fetch_add(1, Ordering::Relaxed);
        }
        None => {
            vnet.netisr.qdrops.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Hands everything queued in `vnet` to its protocol, in order; returns
/// how many packets there were.
pub fn drain(vnet: &Arc<Vnet>) -> usize {
    let mut n = 0;
    while let Some((proto, m)) = vnet.netisr.pop() {
        n += 1;
        let h = HANDLERS.lock()[proto as usize];
        match h {
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
