// SPDX-License-Identifier: BSD-2-Clause
//! The host cache (`tcp_hostcache.c`): what past connections learned
//! about a peer (its path MTU, RTT, slow-start threshold, window), which
//! a new connection to it starts from.
//!
//! Buckets keep their entries most recently used first; a full bucket
//! (or a full cache) gives up its least recently used. Entries expire an
//! hour after their last update, checked every five minutes.
//!
//! Behavior reference: sys/netinet/tcp_hostcache.c.

use alloc::vec::Vec;
use core::sync::atomic::{AtomicI32, AtomicU32, Ordering};

use sync::SpinLock;

use crate::stats::{Stats, tcps};

/// `TCP_HOSTCACHE_HASHSIZE`.
pub const HASHSIZE: usize = 512;
/// `TCP_HOSTCACHE_BUCKETLIMIT`.
pub const BUCKETLIMIT: usize = 30;
/// `TCP_HOSTCACHE_EXPIRE`, in seconds.
pub const EXPIRE: i32 = 60 * 60;
/// `TCP_HOSTCACHE_PRUNE`, in seconds.
pub const PRUNE: i32 = 5 * 60;

/// A peer: its address (IPv4 in the first four bytes) and, for a scoped
/// IPv6 address, its zone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Peer {
    pub addr: [u8; 16],
    pub v6: bool,
    pub zone: u32,
}

/// `struct hc_metrics_lite`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Metrics {
    pub mtu: u32,
    pub ssthresh: u32,
    pub rtt: u32,
    pub rttvar: u32,
    pub cwnd: u32,
    pub sendpipe: u32,
    pub recvpipe: u32,
}

/// One entry (`struct hc_metrics`), as `net.inet.tcp.hostcache.list`
/// shows it.
#[derive(Clone, Copy, Debug)]
pub struct Entry {
    pub peer: Peer,
    pub m: Metrics,
    /// Seconds left (`hc_expire`).
    pub expire: i32,
    /// `hc_hits`, `hc_updates`.
    pub hits: u32,
    pub updates: u32,
}

/// A VNET's host cache (`V_tcp_hostcache`).
#[derive(Debug)]
pub struct HostCache {
    buckets: SpinLock<Vec<Vec<Entry>>>,
    count: AtomicU32,
    /// `net.inet.tcp.hostcache.enable`, `expire`, `prune`, `cachelimit`,
    /// `bucketlimit`.
    pub enable: AtomicI32,
    pub expire: AtomicI32,
    pub prune: AtomicI32,
    pub cache_limit: AtomicU32,
    pub bucket_limit: AtomicU32,
}

impl Default for HostCache {
    fn default() -> Self {
        Self::new()
    }
}

fn hash(p: &Peer) -> usize {
    // FNV-1a over the address; FreeBSD hashes with a secret.
    let mut h: u32 = 0x811c_9dc5;
    let n = if p.v6 { 16 } else { 4 };
    for b in &p.addr[..n] {
        h ^= u32::from(*b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h as usize % HASHSIZE
}

impl HostCache {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buckets: SpinLock::new(Vec::new()),
            count: AtomicU32::new(0),
            enable: AtomicI32::new(1),
            expire: AtomicI32::new(EXPIRE),
            prune: AtomicI32::new(PRUNE),
            cache_limit: AtomicU32::new((HASHSIZE * BUCKETLIMIT) as u32),
            bucket_limit: AtomicU32::new(BUCKETLIMIT as u32),
        }
    }

    fn enabled(&self) -> bool {
        // Relaxed: a setting.
        self.enable.load(Ordering::Relaxed) != 0
    }

    /// `tcp_hc_get`: what is known of `peer`, zero if nothing.
    #[must_use]
    pub fn get(&self, peer: &Peer) -> Metrics {
        if !self.enabled() {
            return Metrics::default();
        }
        let mut b = self.buckets.lock();
        let Some(bucket) = b.get_mut(hash(peer)) else {
            return Metrics::default();
        };
        match bucket.iter_mut().find(|e| e.peer == *peer) {
            Some(e) => {
                e.hits += 1;
                // Relaxed: a setting.
                e.expire = self.expire.load(Ordering::Relaxed);
                e.m
            }
            None => Metrics::default(),
        }
    }

    /// `tcp_hc_getmtu`.
    #[must_use]
    pub fn mtu(&self, peer: &Peer) -> u32 {
        self.get(peer).mtu
    }

    /// `tcp_hc_updatemtu`.
    pub fn update_mtu(&self, peer: &Peer, mtu: u32, stats: &Stats) {
        self.update(
            peer,
            &Metrics {
                mtu,
                ..Metrics::default()
            },
            stats,
        );
    }

    /// `tcp_hc_update`: folds `m`'s non-zero fields in (the path MTU
    /// replaced, the rest averaged with what was there).
    pub fn update(&self, peer: &Peer, m: &Metrics, stats: &Stats) {
        if !self.enabled() {
            return;
        }
        // Relaxed: settings and a count read under the lock.
        let expire = self.expire.load(Ordering::Relaxed);
        let blimit = self.bucket_limit.load(Ordering::Relaxed) as usize;
        let climit = self.cache_limit.load(Ordering::Relaxed);
        let mut b = self.buckets.lock();
        if b.is_empty() {
            b.resize_with(HASHSIZE, Vec::new);
        }
        let bucket = &mut b[hash(peer)];
        let at = match bucket.iter().position(|e| e.peer == *peer) {
            Some(i) => {
                bucket[i].expire = expire;
                bucket[i].updates += 1;
                i
            }
            None => {
                if bucket.len() >= blimit || self.count.load(Ordering::Relaxed) >= climit {
                    if bucket.pop().is_none() {
                        return;
                    }
                    self.count.fetch_sub(1, Ordering::Relaxed);
                    stats.inc(tcps::HC_BUCKETOVERFLOW);
                }
                bucket.insert(
                    0,
                    Entry {
                        peer: *peer,
                        m: Metrics::default(),
                        expire,
                        hits: 0,
                        updates: 1,
                    },
                );
                self.count.fetch_add(1, Ordering::Relaxed);
                stats.inc(tcps::HC_ADDED);
                0
            }
        };
        let e = &mut bucket[at].m;
        let avg = |old: u32, new: u32| {
            if old == 0 {
                new
            } else {
                ((u64::from(old) + u64::from(new)) / 2) as u32
            }
        };
        if m.mtu != 0 {
            e.mtu = m.mtu;
        }
        if m.rtt != 0 {
            e.rtt = avg(e.rtt, m.rtt);
            stats.inc(tcps::CACHEDRTT);
        }
        if m.rttvar != 0 {
            e.rttvar = avg(e.rttvar, m.rttvar);
            stats.inc(tcps::CACHEDRTTVAR);
        }
        if m.ssthresh != 0 {
            e.ssthresh = avg(e.ssthresh, m.ssthresh);
            stats.inc(tcps::CACHEDSSTHRESH);
        }
        if m.cwnd != 0 {
            e.cwnd = avg(e.cwnd, m.cwnd);
        }
        if m.sendpipe != 0 {
            e.sendpipe = avg(e.sendpipe, m.sendpipe);
        }
        if m.recvpipe != 0 {
            e.recvpipe = avg(e.recvpipe, m.recvpipe);
        }
        // Most recently used first.
        let e = bucket.remove(at);
        bucket.insert(0, e);
    }

    /// `tcp_hc_purge_internal`: removes the expired entries (all of them
    /// with `all`, as a write to `net.inet.tcp.hostcache.purgenow` does)
    /// and ages the rest by the prune interval.
    pub fn purge(&self, all: bool) {
        // Relaxed: a setting.
        let step = self.prune.load(Ordering::Relaxed);
        let mut b = self.buckets.lock();
        let mut gone = 0;
        for bucket in b.iter_mut() {
            bucket.retain_mut(|e| {
                if all || e.expire <= 0 {
                    gone += 1;
                    return false;
                }
                e.expire -= step;
                true
            });
        }
        // Relaxed: a count changed under the lock.
        self.count.fetch_sub(gone, Ordering::Relaxed);
    }

    /// The entries (`net.inet.tcp.hostcache.list`).
    #[must_use]
    pub fn entries(&self) -> Vec<Entry> {
        self.buckets.lock().iter().flatten().copied().collect()
    }

    /// `net.inet.tcp.hostcache.count`.
    #[must_use]
    pub fn count(&self) -> u32 {
        // Relaxed: a statistic.
        self.count.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(n: u8) -> Peer {
        Peer {
            addr: [10, 0, 0, n, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            v6: false,
            zone: 0,
        }
    }

    #[test]
    fn updates_average_and_the_mtu_replaces() {
        let hc = HostCache::new();
        let st = Stats::new();
        let m = |mtu, rtt| Metrics {
            mtu,
            rtt,
            ..Metrics::default()
        };
        hc.update(&peer(1), &m(1500, 100), &st);
        hc.update(&peer(1), &m(1400, 300), &st);
        assert_eq!(hc.get(&peer(1)), m(1400, 200));
        assert_eq!(hc.get(&peer(2)), Metrics::default());
        assert_eq!(hc.count(), 1);
        assert_eq!(st.get(tcps::HC_ADDED), 1);
    }

    #[test]
    fn entries_expire() {
        let hc = HostCache::new();
        let st = Stats::new();
        hc.update_mtu(&peer(1), 1500, &st);
        for _ in 0..(EXPIRE / PRUNE) {
            hc.purge(false);
        }
        // A lookup restarts its hour.
        assert_eq!(hc.mtu(&peer(1)), 1500);
        for _ in 0..=(EXPIRE / PRUNE) {
            hc.purge(false);
        }
        assert_eq!(hc.count(), 0);
        assert_eq!(hc.mtu(&peer(1)), 0);
    }
}
