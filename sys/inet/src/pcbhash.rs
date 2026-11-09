// SPDX-License-Identifier: BSD-2-Clause
//! The connected PCBs' hash for the packet paths (`ipi_hashbase` read as
//! `in_pcblookup_hash_smr` reads it): a fixed array of buckets, each an
//! immutable list published for readers inside the network epoch. The
//! table's writers, under its lock, copy a bucket, change the copy and
//! publish it; the old list goes after a grace period. A reader finds a
//! connection's socket with no lock, as FreeBSD's SMR lookup does.
//!
//! What a reader finds may have just been taken out (a closing
//! connection, seen within the grace period); the caller checks the
//! connection under its own lock, as FreeBSD checks `INP_FREED`.
//!
//! Behavior reference: sys/netinet/in_pcb.c (`in_pcblookup_hash_smr`,
//! `INP_PCBHASH`, `in_pcbinshash`, `in_pcbremhash_locked`).

use alloc::boxed::Box;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicPtr, Ordering};

use abi::inet::{INP_IPV4, INP_IPV6};

use crate::Socket;
use crate::addr::{In6, v4_of};

/// Buckets in a hash (a power of two).
const BUCKETS: usize = 1024;

/// A connected PCB as the lookups match it.
#[derive(Clone, Debug)]
pub(crate) struct Conn {
    pub(crate) id: u64,
    pub(crate) vnet: u32,
    pub(crate) lport: u16,
    pub(crate) fport: u16,
    pub(crate) laddr: In6,
    pub(crate) faddr: In6,
    pub(crate) vflag: u8,
    pub(crate) so: Weak<Socket>,
}

impl Conn {
    fn same(&self, o: &Self) -> bool {
        self.id == o.id
            && self.vnet == o.vnet
            && self.lport == o.lport
            && self.fport == o.fport
            && self.laddr == o.laddr
            && self.faddr == o.faddr
            && self.vflag == o.vflag
            && Weak::ptr_eq(&self.so, &o.so)
    }
}

type Bucket = Vec<Conn>;

/// A table's connected hash.
pub(crate) struct Exact {
    buckets: [AtomicPtr<Bucket>; BUCKETS],
}

impl core::fmt::Debug for Exact {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Exact").finish_non_exhaustive()
    }
}

/// `INP_PCBHASH`: the foreign address's low word and both ports, mixed.
fn bucket_of(vnet: u32, faddr: &In6, lport: u16, fport: u16) -> usize {
    let a = u32::from_ne_bytes(v4_of(faddr));
    let mut h = a ^ (u32::from(lport) << 16 | u32::from(fport)) ^ vnet.rotate_left(7);
    h ^= h >> 16;
    h = h.wrapping_mul(0x045d_9f3b);
    h ^= h >> 16;
    h as usize & (BUCKETS - 1)
}

impl Exact {
    pub(crate) const fn new() -> Self {
        Self {
            buckets: [const { AtomicPtr::new(core::ptr::null_mut()) }; BUCKETS],
        }
    }

    /// The bucket `i` as of now, for as long as the section `_s` lasts.
    fn read<'s>(&self, i: usize, _s: &'s net::view::Section) -> &'s [Conn] {
        // Acquire: pairs with the release in `publish`.
        let p = self.buckets[i].load(Ordering::Acquire);
        // SAFETY: null or a bucket published by `publish`, which frees one
        // only through `defer_drop` after unpublishing it, so it outlives
        // every section begun before that; `_s` is such a section.
        unsafe { p.as_ref() }.map_or(&[], Vec::as_slice)
    }

    /// Puts `b` in as bucket `i`; the one it replaces goes after a grace
    /// period. Under the table's lock (one writer at a time).
    fn publish(&self, i: usize, b: Bucket) {
        let new = if b.is_empty() {
            core::ptr::null_mut()
        } else {
            Box::into_raw(Box::new(b))
        };
        // AcqRel: the list's contents before the pointer; the old one's
        // reads before its retirement.
        let old = self.buckets[i].swap(new, Ordering::AcqRel);
        if !old.is_null() {
            // SAFETY: made by `Box::into_raw` above and now unpublished;
            // freed once, after the grace period.
            let old = unsafe { Box::from_raw(old) };
            net::view::defer_drop(*old);
        }
    }

    /// The current bucket `i`, to copy. Under the table's lock.
    fn current(&self, i: usize) -> Bucket {
        let s = net::view::enter();
        self.read(i, &s).to_vec()
    }

    /// Makes `c` the entry for its PCB (in place of `was`, if it had one
    /// elsewhere or different); under the table's lock.
    pub(crate) fn set(&self, was: Option<&Conn>, c: Conn) {
        if let Some(w) = was {
            if w.same(&c) {
                return;
            }
            self.remove(w);
        }
        let i = bucket_of(c.vnet, &c.faddr, c.lport, c.fport);
        let mut b = self.current(i);
        b.push(c);
        self.publish(i, b);
    }

    /// Takes PCB `c.id`'s entry out; under the table's lock.
    pub(crate) fn remove(&self, c: &Conn) {
        let i = bucket_of(c.vnet, &c.faddr, c.lport, c.fport);
        let mut b = self.current(i);
        let n = b.len();
        b.retain(|x| x.id != c.id);
        if b.len() != n {
            self.publish(i, b);
        }
    }

    /// Tests: whether `c` is published as it is.
    #[cfg(test)]
    pub(crate) fn holds(&self, c: &Conn) -> bool {
        let s = net::view::enter();
        self.read(bucket_of(c.vnet, &c.faddr, c.lport, c.fport), &s)
            .iter()
            .any(|x| x.same(c))
    }

    /// The socket of the connected IPv4 PCB for the four-tuple
    /// (`in_pcblookup_hash_smr`'s exact match), without the table's lock.
    pub(crate) fn lookup4(
        &self,
        s: &net::view::Section,
        vnet: u32,
        faddr: [u8; 4],
        fport: u16,
        laddr: [u8; 4],
        lport: u16,
    ) -> Option<Arc<Socket>> {
        let f = crate::addr::v4mapped(faddr);
        self.read(bucket_of(vnet, &f, lport, fport), s)
            .iter()
            .find(|c| {
                c.vnet == vnet
                    && c.lport == lport
                    && c.fport == fport
                    && c.vflag & INP_IPV4 != 0
                    && v4_of(&c.faddr) == faddr
                    && v4_of(&c.laddr) == laddr
            })
            .and_then(|c| c.so.upgrade())
    }

    /// [`Exact::lookup4`] for IPv6.
    pub(crate) fn lookup6(
        &self,
        s: &net::view::Section,
        vnet: u32,
        faddr: &In6,
        fport: u16,
        laddr: &In6,
        lport: u16,
    ) -> Option<Arc<Socket>> {
        self.read(bucket_of(vnet, faddr, lport, fport), s)
            .iter()
            .find(|c| {
                c.vnet == vnet
                    && c.lport == lport
                    && c.fport == fport
                    && c.vflag & INP_IPV6 != 0
                    && c.faddr == *faddr
                    && c.laddr == *laddr
            })
            .and_then(|c| c.so.upgrade())
    }
}
