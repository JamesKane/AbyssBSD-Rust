// SPDX-License-Identifier: BSD-2-Clause
//! IPv4 reassembly: fragments queued by datagram (source, destination,
//! protocol, identification) until they make it whole, overlaps trimmed,
//! with FreeBSD's limits: `net.inet.ip.maxfragsperpacket` per datagram,
//! `maxfragpackets` datagrams and `maxfrags` fragments in all, and
//! `net.inet.ip.fragttl` seconds to finish.
//!
//! Behavior reference: sys/netinet/ip_reass.c (`ip_reass`, `ipq_timeout`,
//! `ipq_drop`, `ipreass_slowtimo`'s callout, `IP_MAXFRAGS`).

use alloc::vec::Vec;
use core::sync::atomic::{AtomicI32, AtomicU32, Ordering};

use mbuf::Packet;

use crate::ip::{Ip, MAXPACKET, MF, OFFMASK};
use crate::state::{State, ips, on};

/// `maxfrags` (`nmbclusters / 32`), across VNETs; -1 for no limit.
pub static MAXFRAGS: AtomicI32 = AtomicI32::new(-1);
/// `curfrags` (`nfrags`).
pub static NFRAGS: AtomicU32 = AtomicU32::new(0);
/// `net.inet.ip.maxfragpackets`, per VNET (the zone's limit).
pub static MAXFRAGPACKETS: AtomicI32 = AtomicI32::new(-1);

/// Sets the limits from the cluster count (`ipreass_zone_change`).
pub fn set_nmbclusters(n: u64) {
    let max = i32::try_from(n / 32).unwrap_or(i32::MAX);
    // Relaxed: settings.
    MAXFRAGS.store(max, Ordering::Relaxed);
    // `IP_MAXFRAGPACKETS`: no more than 50 per hash bucket's half.
    MAXFRAGPACKETS.store(max.min(1024 * 50), Ordering::Relaxed);
}

/// One fragment: its data (the header off) at byte offset `off`.
#[derive(Debug)]
struct Frag {
    off: usize,
    len: usize,
    more: bool,
    m: Packet,
}

/// A datagram being reassembled (`struct ipq`).
#[derive(Debug)]
struct Ipq {
    src: [u8; 4],
    dst: [u8; 4],
    p: u8,
    id: u16,
    /// When it is given up (uptime seconds).
    expire: u64,
    /// Where the last fragment ends, once seen.
    maxoff: Option<usize>,
    /// The first fragment's header (with options), from the one at
    /// offset 0 once it comes.
    hdr: Vec<u8>,
    tos: u8,
    frags: Vec<Frag>,
    nfrags: u32,
}

/// A VNET's reassembly queues.
#[derive(Debug, Default)]
pub struct Queues {
    q: Vec<Ipq>,
}

impl Queues {
    #[must_use]
    pub const fn new() -> Self {
        Self { q: Vec::new() }
    }

    /// Datagrams waiting (`net.inet.ip.fragpackets`).
    #[must_use]
    pub fn len(&self) -> usize {
        self.q.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.q.is_empty()
    }
}

fn release(q: &Ipq) {
    // Relaxed: a count of fragments held.
    NFRAGS.fetch_sub(q.nfrags, Ordering::Relaxed);
}

/// `ip_reass`: fragment `m` (header `ip`) joins its datagram's queue;
/// the whole datagram if this completed it, its header first with the
/// total length and no fragment bits.
pub(crate) fn reass(st: &State, mut m: Packet, ip: &Ip) -> Option<Packet> {
    // Relaxed: limits and a count, as FreeBSD reads them unlocked.
    let max = MAXFRAGS.load(Ordering::Relaxed);
    let perpacket = st.conf.maxfragsperpacket.load(Ordering::Relaxed);
    if perpacket == 0
        || (max >= 0 && NFRAGS.load(Ordering::Relaxed) >= u32::try_from(max).unwrap_or(0))
    {
        st.ipstat.inc(ips::FRAGMENTS);
        st.ipstat.inc(ips::FRAGDROPPED);
        return None;
    }
    let hlen = usize::from(ip.hl);
    let mut len = usize::from(ip.len) - hlen;
    let more = ip.off & MF != 0;
    if len == 0 || (more && len % 8 != 0) {
        st.ipstat.inc(ips::TOOSMALL);
        st.ipstat.inc(ips::FRAGDROPPED);
        return None;
    }
    let mut off = usize::from(ip.off & OFFMASK) << 3;
    if off + len > MAXPACKET {
        st.ipstat.inc(ips::TOOLONG);
        st.ipstat.inc(ips::FRAGDROPPED);
        return None;
    }
    st.ipstat.inc(ips::FRAGMENTS);
    let mut hdr = alloc::vec![0u8; hlen];
    m.data.copy_out(0, &mut hdr);
    m.data.trim_front(hlen);
    let now = net::uptime();
    let ttl = u64::try_from(st.conf.fragttl.load(Ordering::Relaxed).max(1)).unwrap_or(30);
    let mut qs = st.reass.lock();
    let at =
        qs.q.iter()
            .position(|q| q.id == ip.id && q.src == ip.src && q.dst == ip.dst && q.p == ip.p);
    let Some(at) = at else {
        let limit = MAXFRAGPACKETS.load(Ordering::Relaxed);
        if limit >= 0 && qs.q.len() >= usize::try_from(limit).unwrap_or(0) {
            // `ipq_reuse`: the oldest gives way.
            if qs.q.is_empty() {
                st.ipstat.inc(ips::FRAGDROPPED);
                return None;
            }
            let old = qs.q.remove(0);
            st.ipstat.add(ips::FRAGDROPPED, u64::from(old.nfrags));
            release(&old);
        }
        NFRAGS.fetch_add(1, Ordering::Relaxed);
        qs.q.push(Ipq {
            src: ip.src,
            dst: ip.dst,
            p: ip.p,
            id: ip.id,
            expire: now + ttl,
            maxoff: (!more).then_some(off + len),
            hdr,
            tos: ip.tos,
            frags: alloc::vec![Frag { off, len, more, m }],
            nfrags: 1,
        });
        return None;
    };
    let q = &mut qs.q[at];
    let drop_q = |qs: &mut Queues, at: usize| {
        let q = qs.q.remove(at);
        st.ipstat.add(ips::FRAGDROPPED, u64::from(q.nfrags));
        release(&q);
    };
    // With the last fragment seen, this one must end before it (or be it).
    if let Some(maxoff) = q.maxoff {
        let end = off + len;
        if (more && end >= maxoff) || (!more && end != maxoff) {
            st.ipstat.inc(ips::FRAGDROPPED);
            return None;
        }
    } else if !more {
        q.maxoff = Some(off + len);
    }
    q.nfrags += 1;
    NFRAGS.fetch_add(1, Ordering::Relaxed);
    // ECN: CE kept; CE with not-ECT, or not-ECT after ECT, refused.
    let (ecn, ecn0) = (ip.tos & 3, q.tos & 3);
    if (ecn == 3 && ecn0 == 0) || (ecn == 0 && ecn0 != 0) {
        q.nfrags -= 1;
        NFRAGS.fetch_sub(1, Ordering::Relaxed);
        st.ipstat.inc(ips::FRAGDROPPED);
        return None;
    }
    if ecn == 3 {
        q.tos |= 3;
    }
    // The first segment beginning after this one.
    let next = q
        .frags
        .iter()
        .position(|f| f.off > off)
        .unwrap_or(q.frags.len());
    // The one before may hold some of this one's data already.
    if next > 0 {
        let p = &q.frags[next - 1];
        let over = (p.off + p.len).saturating_sub(off);
        if over > 0 {
            if over >= len {
                q.nfrags -= 1;
                NFRAGS.fetch_sub(1, Ordering::Relaxed);
                st.ipstat.inc(ips::FRAGDROPPED);
                return None;
            }
            m.data.trim_front(over);
            m.hdr.csum_flags = 0;
            off += over;
            len -= over;
        }
    }
    if off == 0 {
        q.hdr = hdr;
    }
    q.frags.insert(next, Frag { off, len, more, m });
    // Later segments this one covers are trimmed or dropped.
    let end = off + len;
    while let Some(f) = q.frags.get_mut(next + 1) {
        if end <= f.off {
            break;
        }
        let over = end - f.off;
        if over < f.len {
            f.m.data.trim_front(over);
            f.m.hdr.csum_flags = 0;
            f.off += over;
            f.len -= over;
            break;
        }
        q.frags.remove(next + 1);
        q.nfrags -= 1;
        NFRAGS.fetch_sub(1, Ordering::Relaxed);
        st.ipstat.inc(ips::FRAGDROPPED);
    }
    // Complete? Holes, or no last fragment, and it waits (dropped if it
    // has too many fragments already).
    let mut want = 0;
    for f in &q.frags {
        if f.off != want {
            if i64::from(q.nfrags) > i64::from(perpacket) {
                drop_q(&mut qs, at);
            }
            return None;
        }
        want += f.len;
    }
    if q.frags.last().is_some_and(|f| f.more) {
        if i64::from(q.nfrags) > i64::from(perpacket) {
            drop_q(&mut qs, at);
        }
        return None;
    }
    if want + q.hdr.len() > MAXPACKET {
        st.ipstat.inc(ips::TOOLONG);
        drop_q(&mut qs, at);
        return None;
    }
    let q = qs.q.remove(at);
    drop(qs);
    release(&q);
    let mut frags = q.frags.into_iter();
    let first = frags.next()?;
    let mut whole = first.m;
    for f in frags {
        whole.hdr.csum_flags &= f.m.hdr.csum_flags;
        whole.hdr.csum_data = whole.hdr.csum_data.wrapping_add(f.m.hdr.csum_data);
        whole.data.append(f.m.data);
    }
    while whole.hdr.csum_data & 0xffff_0000 != 0 {
        whole.hdr.csum_data = (whole.hdr.csum_data & 0xffff) + (whole.hdr.csum_data >> 16);
    }
    let mut h = q.hdr;
    let total = u16::try_from(h.len() + want).ok()?;
    h[1] = (h[1] & !3) | (q.tos & 3);
    h[2..4].copy_from_slice(&total.to_be_bytes());
    // `ip_reass` shifted the first fragment's offset, flags and all, out
    // of its 16 bits: 0.
    h[6] = 0;
    h[7] = 0;
    whole.prepend(h.len()).ok()?.copy_from_slice(&h);
    whole.hdr.csum_flags |= mbuf::csum::IP_CHECKED | mbuf::csum::IP_VALID;
    st.ipstat.inc(ips::REASSEMBLED);
    Some(whole)
}

/// Gives up the datagrams whose time has run out (`ipreass_callout`).
pub fn tick(st: &State, now: u64) {
    let mut qs = st.reass.lock();
    qs.q.retain(|q| {
        if q.expire > now {
            return true;
        }
        st.ipstat.add(ips::FRAGTIMEOUT, u64::from(q.nfrags));
        release(q);
        false
    });
}

/// Drops every queue (`ipreass_drain`, and a VNET going).
pub fn drain(st: &State) {
    let mut qs = st.reass.lock();
    for q in qs.q.drain(..) {
        st.ipstat.add(ips::FRAGDROPPED, u64::from(q.nfrags));
        release(&q);
    }
}

/// Whether reassembly is refused (`maxfragsperpacket` 0).
#[must_use]
pub fn disabled(st: &State) -> bool {
    !on(&st.conf.maxfragsperpacket)
}
