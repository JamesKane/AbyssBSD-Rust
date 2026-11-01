// SPDX-License-Identifier: BSD-2-Clause
//! IPv6 reassembly: fragments queued by packet (source, destination,
//! identification) until they make it whole, with RFC 8200's rules (an
//! overlap drops the whole packet, an exact duplicate only itself) and
//! FreeBSD's limits: `net.inet6.ip6.maxfrags` fragments in all,
//! `maxfragpackets` packets and `maxfragbucketsize` per hash bucket in a
//! VNET, `maxfragsperpacket`, and `fraglifetime_ms` to finish, counted in
//! half-second ticks. Atomic fragments (RFC 6946) pass straight through.
//!
//! Behavior reference: sys/netinet6/frag6.c (`frag6_input`,
//! `frag6_freef`, `frag6_slowtimo`, `frag6_drain`).

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI32, Ordering};

use mbuf::{Packet, csum, flags};
use net::Vnet;
use net::addr::In6;
use net::ifnet::in6_ifs;

use crate::icmp6;
use crate::ip6::{self, HDR_LEN, Ip6, MAXPACKET};
use crate::state::{State, ip6s, val};

/// `IP6REASS_NHASH`.
const NHASH: u32 = 1024;
/// `IP6_CALLOUT_INTERVAL_MS`: the queues' timer ticks every half second.
pub const TICK_MS: u64 = 500;

/// `net.inet6.ip6.maxfrags` (`ip6_maxfrags`, `nmbclusters / 32`), across
/// VNETs: 0 refuses fragments, -1 takes any number.
pub static MAXFRAGS: AtomicI32 = AtomicI32::new(-1);

/// Sets the limits from the cluster count (`frag6_change`).
pub fn set_nmbclusters(n: u64) {
    // Relaxed: a setting.
    MAXFRAGS.store(i32::try_from(n / 32).unwrap_or(i32::MAX), Ordering::Relaxed);
}

/// `net.inet6.ip6.frag6_nfrags` (`frag6_nfrags`): the fragments held in
/// every VNET's queues; a VNET that goes takes its own with it.
#[must_use]
pub fn nfrags() -> u32 {
    let mut n = 0usize;
    for v in net::Vnet::all() {
        if v.inet.get().is_some() {
            n += crate::state::of(&v)
                .frag6
                .lock()
                .q
                .iter()
                .map(|q| q.frags.len())
                .sum::<usize>();
        }
    }
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// `IP6_MAXFRAGPACKETS` and `frag6_set_bucketsize`: a VNET's packet
/// limit and the bucket size that follows from it.
pub(crate) fn init_limits(st: &State) {
    // Relaxed: settings.
    let max = MAXFRAGS.load(Ordering::Relaxed);
    let packets = if max < 0 {
        1024 * 50
    } else {
        max.min(1024 * 50)
    };
    st.conf6.maxfragpackets.store(packets, Ordering::Relaxed);
    set_bucketsize(st);
}

/// `frag6_set_bucketsize`.
pub fn set_bucketsize(st: &State) {
    let i = val(&st.conf6.maxfragpackets);
    if i > 0 {
        #[allow(clippy::cast_possible_wrap, reason = "512")]
        let b = (i / (NHASH as i32 / 2)).max(1);
        // Relaxed: a setting.
        st.conf6.maxfragbucketsize.store(b, Ordering::Relaxed);
    }
}

/// One fragment (`struct ip6asfrag`): where its data sits in the
/// datagram (`ip6af_off`) and in its packet (`ip6af_offset`).
#[derive(Debug)]
struct Frag {
    off: usize,
    frglen: usize,
    offset: usize,
    more: bool,
    m: Packet,
}

/// A packet being reassembled (`struct ip6q`).
#[derive(Debug)]
struct Q {
    ident: u32,
    src: In6,
    dst: In6,
    bucket: u32,
    /// Half-second ticks left (`ip6q_ttl`).
    ttl: u32,
    ecn: u8,
    /// The unfragmentable part's length once the first fragment came
    /// (`ip6q_unfrglen`), and its next header.
    unfrglen: Option<usize>,
    nxt: u8,
    frags: Vec<Frag>,
    /// The interface the fragments came in on, for the error.
    rcvif: u16,
}

/// A VNET's reassembly queues, most recently active first.
#[derive(Debug, Default)]
pub struct Queues {
    q: Vec<Q>,
    seed: u32,
}

impl Queues {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            q: Vec::new(),
            seed: 0,
        }
    }

    /// `frag6_nfragpackets`: the packets being reassembled.
    #[must_use]
    pub fn len(&self) -> usize {
        self.q.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.q.is_empty()
    }

    fn bucket_count(&self, b: u32) -> usize {
        self.q.iter().filter(|q| q.bucket == b).count()
    }
}

/// The bucket of a packet's key.
fn bucket(seed: u32, src: &In6, dst: &In6, ident: u32) -> u32 {
    let mut h = seed ^ 0x811c_9dc5;
    for b in src.iter().chain(dst).chain(&ident.to_ne_bytes()) {
        h = (h ^ u32::from(*b)).wrapping_mul(0x0100_0193);
    }
    h % NHASH
}

/// `IPV6_ECN`.
const fn ecn_of(ip6: &Ip6) -> u8 {
    ((ip6.flow >> 20) & 3) as u8
}

/// `IPTOS_ECN_NOTECT`, `IPTOS_ECN_CE`.
const ECN_NOTECT: u8 = 0;
const ECN_CE: u8 = 3;

/// Counts a reassembly failure on the destination's interface.
fn reass_fail(dstif: Option<&Arc<net::ifnet::Stats>>) {
    if let Some(s) = dstif {
        s.in6_inc(in6_ifs::REASS_FAIL);
    }
}

/// `frag6_input`: the fragment header at `off` of `m`. An atomic
/// fragment loses its fragment header and goes on; another joins its
/// packet's queue, and the whole packet goes on once it is complete:
/// the packet, the offset of the header after the fragment header, and
/// that header's number.
pub(crate) fn input(
    vnet: &Arc<Vnet>,
    st: &State,
    mut m: Packet,
    off: usize,
) -> Option<(Packet, usize, u8)> {
    let mut fh = [0u8; 8];
    if m.data.copy_out(off, &mut fh) != 8 {
        st.ip6stat.inc(ip6s::EXTHDRTOOLONG);
        return None;
    }
    let ip6 = m.header::<Ip6>(0).ok()?;
    // The destination's interface, for its counters.
    let dstif = {
        let n = vnet.net.lock();
        crate::in6::ifa_with(&n, &ip6.dst, m.hdr.rcvif)
            .and_then(|(i, _)| n.by_index(i).map(|i| Arc::clone(&i.stats)))
    };
    let offlg = u16::from_be_bytes([fh[2], fh[3]]);
    let more = offlg & 1 != 0;
    let plen = usize::from(ip6.plen);
    // Every fragment but the last a multiple of 8 bytes.
    if more && (plen.wrapping_sub(off)) & 7 != 0 {
        icmp6::error(
            vnet,
            m,
            icmp6::ty::PARAM_PROB,
            icmp6::code::PARAMPROB_HEADER,
            4,
        );
        reass_fail(dstif.as_ref());
        return None;
    }
    st.ip6stat.inc(ip6s::FRAGMENTS);
    if let Some(s) = &dstif {
        s.in6_inc(in6_ifs::REASS_REQD);
    }
    // An atomic fragment: offset and more both 0.
    if offlg & !6 == 0 {
        st.ip6stat.inc(ip6s::ATOMICFRAGS);
        let nxt = fh[0];
        let prev = ip6::prevhdr(&m, off);
        let _ = m.data.copy_in(prev, &[nxt]);
        let mut h = ip6;
        h.plen -= 8;
        if delete_fraghdr(&mut m, off, &h).is_err() {
            st.ip6stat.inc(ip6s::FRAGDROPPED);
            reass_fail(dstif.as_ref());
            return None;
        }
        if let Some(s) = &dstif {
            s.in6_inc(in6_ifs::REASS_OK);
        }
        return Some((m, off, nxt));
    }
    let offset = off + 8;
    let frgpartlen = (HDR_LEN + plen).saturating_sub(offset);
    if frgpartlen == 0 {
        icmp6::error(
            vnet,
            m,
            icmp6::ty::PARAM_PROB,
            icmp6::code::PARAMPROB_HEADER,
            4,
        );
        reass_fail(dstif.as_ref());
        st.ip6stat.inc(ip6s::FRAGDROPPED);
        return None;
    }
    // The system's limit on fragments held.
    // Relaxed: a limit read as FreeBSD reads it.
    let maxfrags = MAXFRAGS.load(Ordering::Relaxed);
    if maxfrags >= 0 && nfrags() >= maxfrags.cast_unsigned() {
        reass_fail(dstif.as_ref());
        st.ip6stat.inc(ip6s::FRAGDROPPED);
        return None;
    }
    let fragoff = usize::from(offlg & 0xfff8);
    let ident = u32::from_be_bytes([fh[4], fh[5], fh[6], fh[7]]);
    let mut qs = st.frag6.lock();
    if qs.seed == 0 {
        qs.seed = crate::random() | 1;
    }
    let b = bucket(qs.seed, &ip6.src, &ip6.dst, ident);
    let mut errors: Vec<(Packet, usize)> = Vec::new();
    let found =
        qs.q.iter()
            .position(|q| q.ident == ident && q.src == ip6.src && q.dst == ip6.dst);
    let only_frag = found.is_none();
    let qi = if let Some(i) = found {
        i
    } else {
        let maxpk = val(&st.conf6.maxfragpackets);
        if maxpk >= 0
            && (qs.bucket_count(b)
                >= usize::try_from(val(&st.conf6.maxfragbucketsize)).unwrap_or(0)
                || qs.q.len() >= usize::try_from(maxpk).unwrap_or(0))
        {
            drop(qs);
            reass_fail(dstif.as_ref());
            st.ip6stat.inc(ip6s::FRAGDROPPED);
            return None;
        }
        let ttl_ms = u64::try_from(val(&st.conf6.fraglifetime_ms)).unwrap_or(60_000);
        qs.q.insert(
            0,
            Q {
                ident,
                src: ip6.src,
                dst: ip6.dst,
                bucket: b,
                ttl: u32::try_from(ttl_ms / TICK_MS).unwrap_or(u32::MAX),
                ecn: ecn_of(&ip6),
                unfrglen: None,
                nxt: 0,
                frags: Vec::new(),
                rcvif: m.hdr.rcvif,
            },
        );
        // `frag6_slowtimo` runs while queues wait.
        crate::nd6::arm(crate::now() + TICK_MS * 1_000_000);
        0
    };
    let q = &mut qs.q[qi];
    if fragoff == 0 && q.unfrglen.is_none() {
        q.unfrglen = Some(offset - HDR_LEN - 8);
        q.nxt = fh[0];
    }
    // The packet put together may not exceed 65535 bytes.
    let too_big = match q.unfrglen {
        Some(u) => u + fragoff + frgpartlen > MAXPACKET,
        None => fragoff + frgpartlen > MAXPACKET,
    };
    if too_big {
        if only_frag {
            qs.q.remove(qi);
        }
        drop(qs);
        icmp6::error(
            vnet,
            m,
            icmp6::ty::PARAM_PROB,
            icmp6::code::PARAMPROB_HEADER,
            offset - 8 + 2,
        );
        return None;
    }
    // The first fragment checks those already held.
    if fragoff == 0 && !only_frag {
        let u = q.unfrglen.unwrap_or(0);
        let mut i = 0;
        while i < q.frags.len() {
            if u + q.frags[i].off + q.frags[i].frglen > MAXPACKET {
                let f = q.frags.remove(i);
                let mut em = f.m;
                em.hdr.rcvif = m.hdr.rcvif;
                let _ = em.data.copy_in(8, &q.src);
                let _ = em.data.copy_in(24, &q.dst);
                errors.push((em, f.offset - 8 + 2));
            } else {
                i += 1;
            }
        }
    }
    let new = Frag {
        off: fragoff,
        frglen: frgpartlen,
        offset,
        more,
        m,
    };
    let mut dropped = false;
    if !only_frag {
        // ECN: CE is kept, and CE mixed with not-ECT drops it.
        let ecn = ecn_of(&ip6);
        if ecn == ECN_CE {
            if q.ecn == ECN_NOTECT {
                dropped = true;
            } else {
                q.ecn = ECN_CE;
            }
        }
        if ecn == ECN_NOTECT && q.ecn != ECN_NOTECT {
            dropped = true;
        }
    }
    if dropped {
        drop(qs);
        send_errors(vnet, errors);
        reass_fail(dstif.as_ref());
        st.ip6stat.inc(ip6s::FRAGDROPPED);
        return None;
    }
    // Where it goes; an overlap drops the queue unless it is an exact
    // duplicate, which drops only itself.
    let at = q
        .frags
        .iter()
        .position(|f| f.off > new.off)
        .unwrap_or(q.frags.len());
    let overlaps = |o: &Frag| o.off != new.off || o.frglen != new.frglen;
    let mut freeq = false;
    let mut dup = false;
    if at > 0 {
        let p = &q.frags[at - 1];
        if p.off + p.frglen > new.off {
            dup = true;
            freeq = overlaps(p);
        }
    }
    if !dup
        && let Some(nx) = q.frags.get(at)
        && new.off + new.frglen > nx.off
    {
        dup = true;
        freeq = overlaps(nx);
    }
    if dup {
        if freeq {
            let gone = qs.q.remove(qi);
            errors.extend(freef(st, gone));
        }
        drop(qs);
        send_errors(vnet, errors);
        reass_fail(dstif.as_ref());
        st.ip6stat.inc(ip6s::FRAGDROPPED);
        return None;
    }
    q.frags.insert(at, new);
    // Complete? Each part where the last ended, the last without more.
    let maxper = usize::try_from(val(&st.conf6.maxfragsperpacket)).unwrap_or(0);
    let mut next = 0;
    let mut gap = false;
    for f in &q.frags {
        if f.off != next {
            gap = true;
            break;
        }
        next += f.frglen;
    }
    if gap || q.frags.last().is_some_and(|f| f.more) {
        if q.frags.len() > maxper {
            st.ip6stat.add(ip6s::FRAGDROPPED, q.frags.len() as u64);
            let gone = qs.q.remove(qi);
            errors.extend(freef(st, gone));
        }
        drop(qs);
        send_errors(vnet, errors);
        return None;
    }
    let q = qs.q.remove(qi);
    drop(qs);
    send_errors(vnet, errors);
    let plen = next;
    let mut frags = q.frags.into_iter();
    let first = frags.next()?;
    let offset = first.offset - 8;
    if plen + offset - HDR_LEN > MAXPACKET {
        reass_fail(dstif.as_ref());
        st.ip6stat.inc(ip6s::FRAGDROPPED);
        return None;
    }
    let mut whole = first.m;
    let _ = whole.data.split_off(first.offset + first.frglen);
    let mut csum_flags = whole.hdr.csum_flags;
    let mut csum_data = whole.hdr.csum_data;
    for f in frags {
        csum_flags &= f.m.hdr.csum_flags;
        csum_data = csum_data.wrapping_add(f.m.hdr.csum_data);
        let mut d = f.m.data;
        d.trim_front(f.offset);
        let _ = d.split_off(f.frglen);
        whole.data.append(d);
    }
    while csum_data & 0xffff_0000 != 0 {
        csum_data = (csum_data & 0xffff) + (csum_data >> 16);
    }
    whole.hdr.csum_flags = csum_flags;
    whole.hdr.csum_data = csum_data;
    let mut h = whole.header::<Ip6>(0).ok()?;
    #[allow(clippy::cast_possible_truncation, reason = "checked under 64 KiB")]
    {
        h.plen = (plen + offset - HDR_LEN) as u16;
    }
    if q.ecn == ECN_CE {
        h.flow |= u32::from(ECN_CE) << 20;
    }
    if delete_fraghdr(&mut whole, offset, &h).is_err() {
        return None;
    }
    let prev = ip6::prevhdr(&whole, offset);
    let _ = whole.data.copy_in(prev, &[q.nxt]);
    whole.hdr.flags |= flags::FRAGMENTED;
    whole.hdr.rcvif = q.rcvif;
    // A checksum the device computed over a part no longer stands.
    if whole.hdr.csum_flags & csum::PSEUDO_HDR == 0 {
        whole.hdr.csum_flags &= !csum::DATA_VALID;
    }
    st.ip6stat.inc(ip6s::REASSEMBLED);
    if let Some(s) = &dstif {
        s.in6_inc(in6_ifs::REASS_OK);
    }
    Some((whole, offset, q.nxt))
}

/// `ip6_deletefraghdr`: takes out the 8-byte fragment header at `off`,
/// the headers before it (IPv6's set to `h`) moving up over it.
fn delete_fraghdr(m: &mut Packet, off: usize, h: &Ip6) -> Result<(), ()> {
    let mut before = alloc::vec![0u8; off];
    m.data.copy_out(0, &mut before);
    before[..HDR_LEN].copy_from_slice(&h.bytes());
    m.data.trim_front(off + 8);
    m.prepend(off).map_err(|_| ())?.copy_from_slice(&before);
    m.hdr.flags |= flags::FRAGMENTED;
    Ok(())
}

/// `frag6_freef`: a queue's fragments go; the first one's packet comes
/// back for its time exceeded error (with the queue's addresses).
fn freef(st: &State, q: Q) -> Vec<(Packet, usize)> {
    let _ = st;
    let mut out = Vec::new();
    for f in q.frags {
        if f.off == 0 && f.m.hdr.rcvif != 0 {
            let mut m = f.m;
            let _ = m.data.copy_in(8, &q.src);
            let _ = m.data.copy_in(24, &q.dst);
            out.push((m, usize::MAX));
        }
    }
    out
}

/// Sends the errors gathered under the queues' lock: a parameter problem
/// at an offset, or (`usize::MAX`) time exceeded in reassembly.
fn send_errors(vnet: &Arc<Vnet>, errors: Vec<(Packet, usize)>) {
    for (m, at) in errors {
        if at == usize::MAX {
            icmp6::error(
                vnet,
                m,
                icmp6::ty::TIME_EXCEEDED,
                icmp6::code::TIME_EXCEED_REASSEMBLY,
                0,
            );
        } else {
            icmp6::error(
                vnet,
                m,
                icmp6::ty::PARAM_PROB,
                icmp6::code::PARAMPROB_HEADER,
                at,
            );
        }
    }
}

/// `frag6_slowtimo`: one half-second tick; queues whose time ran out go,
/// and queues over a lowered limit, oldest first.
pub fn tick(vnet: &Arc<Vnet>, st: &State) {
    let mut errors = Vec::new();
    {
        let mut qs = st.frag6.lock();
        let mut i = 0;
        while i < qs.q.len() {
            qs.q[i].ttl = qs.q[i].ttl.saturating_sub(1);
            if qs.q[i].ttl == 0 {
                let q = qs.q.remove(i);
                st.ip6stat.add(ip6s::FRAGTIMEOUT, q.frags.len() as u64);
                errors.extend(freef(st, q));
            } else {
                i += 1;
            }
        }
        let maxpk = val(&st.conf6.maxfragpackets);
        let bsize = usize::try_from(val(&st.conf6.maxfragbucketsize)).unwrap_or(0);
        loop {
            let over = if maxpk == 0 {
                qs.q.last().map(|q| q.bucket)
            } else if maxpk > 0 {
                qs.q.iter()
                    .rev()
                    .map(|q| q.bucket)
                    .find(|b| qs.bucket_count(*b) > bsize)
            } else {
                None
            };
            let Some(b) = over else {
                break;
            };
            let Some(i) = qs.q.iter().rposition(|q| q.bucket == b) else {
                break;
            };
            let q = qs.q.remove(i);
            st.ip6stat.add(ip6s::FRAGOVERFLOW, q.frags.len() as u64);
            errors.extend(freef(st, q));
        }
        while maxpk >= 0 && qs.q.len() > usize::try_from(maxpk).unwrap_or(0) {
            let Some(q) = qs.q.pop() else {
                break;
            };
            st.ip6stat.add(ip6s::FRAGOVERFLOW, q.frags.len() as u64);
            errors.extend(freef(st, q));
        }
    }
    send_errors(vnet, errors);
}

/// `frag6_drain_one`: every queue goes (memory is short, or the VNET is).
pub fn drain(vnet: &Arc<Vnet>, st: &State) {
    let gone: Vec<Q> = core::mem::take(&mut st.frag6.lock().q);
    let mut errors = Vec::new();
    for q in gone {
        st.ip6stat.inc(ip6s::FRAGDROPPED);
        errors.extend(freef(st, q));
    }
    send_errors(vnet, errors);
}
