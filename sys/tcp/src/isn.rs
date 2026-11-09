// SPDX-License-Identifier: BSD-2-Clause
//! Keyed hashes of a connection's endpoints: initial sequence numbers
//! (RFC 6528), timestamp offsets (RFC 7323 §5.4), and SYN cookies, all
//! with SipHash-2-4 under per-VNET secrets.
//!
//! FreeBSD's `tcp_new_isn` hashes with MD5; any keyed hash gives the
//! property RFC 6528 asks for, so SipHash serves all three here.
//!
//! Behavior reference: sys/netinet/tcp_subr.c (`tcp_new_isn`,
//! `tcp_new_ts_offset`), sys/crypto/siphash/siphash.c (the algorithm,
//! from its paper).

use crate::tcb::Inc;

/// `ISN_BYTES_PER_SECOND`: how fast the ISN space advances at least.
const ISN_BYTES_PER_SECOND: u32 = 1_048_576;
/// `ISN_STATIC_INCREMENT`, `ISN_RANDOM_INCREMENT`: what each new ISN
/// adds to the offset, so that a four-tuple used again soon still starts
/// past the sequence numbers its last connection used.
const ISN_STATIC_INCREMENT: u32 = 4096;
const ISN_RANDOM_INCREMENT: u32 = 4096 - 1;
/// The clock's ticks per second (`hz`: [`Ctx::now`](crate::Ctx) counts
/// milliseconds).
const HZ: u32 = 1000;

/// SipHash-2-4 of `data` under `key`.
#[must_use]
pub fn siphash24(key: &[u8; 16], data: &[u8]) -> u64 {
    let k0 = u64::from_le_bytes(key[0..8].try_into().unwrap_or_default());
    let k1 = u64::from_le_bytes(key[8..16].try_into().unwrap_or_default());
    let mut v = [
        k0 ^ 0x736f_6d65_7073_6575,
        k1 ^ 0x646f_7261_6e64_6f6d,
        k0 ^ 0x6c79_6765_6e65_7261,
        k1 ^ 0x7465_6462_7974_6573,
    ];
    let round = |v: &mut [u64; 4]| {
        v[0] = v[0].wrapping_add(v[1]);
        v[1] = v[1].rotate_left(13) ^ v[0];
        v[0] = v[0].rotate_left(32);
        v[2] = v[2].wrapping_add(v[3]);
        v[3] = v[3].rotate_left(16) ^ v[2];
        v[0] = v[0].wrapping_add(v[3]);
        v[3] = v[3].rotate_left(21) ^ v[0];
        v[2] = v[2].wrapping_add(v[1]);
        v[1] = v[1].rotate_left(17) ^ v[2];
        v[2] = v[2].rotate_left(32);
    };
    let (chunks, rest) = data.as_chunks::<8>();
    for c in chunks {
        let m = u64::from_le_bytes(*c);
        v[3] ^= m;
        round(&mut v);
        round(&mut v);
        v[0] ^= m;
    }
    let mut last = [0u8; 8];
    last[..rest.len()].copy_from_slice(rest);
    last[7] = data.len() as u8;
    let m = u64::from_le_bytes(last);
    v[3] ^= m;
    round(&mut v);
    round(&mut v);
    v[0] ^= m;
    v[2] ^= 0xff;
    for _ in 0..4 {
        round(&mut v);
    }
    v[0] ^ v[1] ^ v[2] ^ v[3]
}

/// The endpoints as hashed: foreign then local address, then the ports.
fn endpoints(inc: &Inc, ports: bool, out: &mut [u8; 40]) -> usize {
    let n = if inc.v6 { 16 } else { 4 };
    let mut at = 0;
    out[at..at + n].copy_from_slice(&inc.faddr[..n]);
    at += n;
    out[at..at + n].copy_from_slice(&inc.laddr[..n]);
    at += n;
    if ports {
        out[at..at + 2].copy_from_slice(&inc.fport.to_be_bytes());
        out[at + 2..at + 4].copy_from_slice(&inc.lport.to_be_bytes());
        at += 4;
    }
    at
}

/// A VNET's secrets for ISNs and timestamp offsets, which the caller
/// fills from its random source at the VNET's start.
#[derive(Clone, Copy, Debug, Default)]
pub struct Secrets {
    pub isn: [u8; 16],
    pub ts: [u8; 16],
}

/// A VNET's ISN offset (`V_isn_offset`, `V_isn_offset_old`,
/// `V_isn_last`), under its lock (`isn_mtx`).
#[derive(Debug)]
pub struct Offset(sync::SpinLock<OffsetState>);

#[derive(Debug)]
struct OffsetState {
    offset: u32,
    old: u32,
    last: u64,
}

impl Offset {
    #[must_use]
    pub const fn new() -> Self {
        Self(sync::SpinLock::new(OffsetState {
            offset: 0,
            old: 0,
            last: 0,
        }))
    }
}

impl Default for Offset {
    fn default() -> Self {
        Self::new()
    }
}

impl Secrets {
    /// `tcp_new_isn`: a hash of the endpoints plus the VNET's offset,
    /// which every ISN moves on by 4 KiB and a random part (`random`), and
    /// the clock (`now`, in ticks of a millisecond) by at least
    /// `ISN_BYTES_PER_SECOND`.
    #[must_use]
    pub fn new_isn(&self, inc: &Inc, now: u64, offset: &Offset, random: u32) -> u32 {
        let mut b = [0u8; 40];
        let n = endpoints(inc, true, &mut b);
        let h = siphash24(&self.isn, &b[..n]) as u32;
        let mut o = offset.0.lock();
        o.offset = o
            .offset
            .wrapping_add(ISN_STATIC_INCREMENT + (random & ISN_RANDOM_INCREMENT));
        if now != o.last {
            // Ticks are 32 bits in FreeBSD's arithmetic too.
            let ticks = now.wrapping_sub(o.last) as u32;
            let projected = o
                .old
                .wrapping_add((ISN_BYTES_PER_SECOND / HZ).wrapping_mul(ticks));
            if crate::seq::gt(projected, o.offset) {
                o.offset = projected;
            }
            o.old = o.offset;
            o.last = now;
        }
        h.wrapping_add(o.offset)
    }

    /// `tcp_new_ts_offset`: a hash of the addresses (and the ports, with
    /// `net.inet.tcp.ts_offset_per_conn`).
    #[must_use]
    pub fn new_ts_offset(&self, inc: &Inc, per_conn: bool) -> u32 {
        let mut b = [0u8; 40];
        let n = endpoints(inc, per_conn, &mut b);
        siphash24(&self.ts, &b[..n]) as u32
    }
}

/// `syncookie_mac`: the hash a cookie carries, of the endpoints, the
/// peer's ISN, the cookie's flags, and the bucket.
#[must_use]
pub(crate) fn syncookie_mac(key: &[u8; 16], inc: &Inc, irs: u32, flags: u8, bucket: u32) -> u32 {
    let mut b = [0u8; 52];
    let mut ep = [0u8; 40];
    let n = endpoints(inc, true, &mut ep);
    b[..n].copy_from_slice(&ep[..n]);
    b[n..n + 4].copy_from_slice(&irs.to_be_bytes());
    b[n + 4] = flags;
    b[n + 5..n + 9].copy_from_slice(&bucket.to_le_bytes());
    let h = siphash24(key, &b[..n + 9]);
    (h as u32) ^ ((h >> 32) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn siphash_matches_the_paper_s_vector() {
        // Appendix A of the SipHash paper: key 00..0f, message 00..0e.
        let key: [u8; 16] = core::array::from_fn(|i| i as u8);
        let msg: [u8; 15] = core::array::from_fn(|i| i as u8);
        assert_eq!(siphash24(&key, &msg), 0xa129_ca61_49be_45e5);
    }

    #[test]
    fn isns_differ_by_connection_and_advance() {
        let s = Secrets {
            isn: [7; 16],
            ts: [9; 16],
        };
        let mut a = Inc {
            fport: 1000,
            lport: 80,
            ..Inc::default()
        };
        let o = Offset::new();
        let i1 = s.new_isn(&a, 0, &o, 0);
        // The same tick: past the last by the static increment at least.
        let i2 = s.new_isn(&a, 0, &o, 7);
        assert_eq!(i2.wrapping_sub(i1), 4096 + 7);
        // A second on: the clock's rate (1048 a tick) from the offset as
        // of the last tick change (none yet: 0), past `i1`'s 4096.
        let i3 = s.new_isn(&a, 1000, &o, 0);
        assert_eq!(i3.wrapping_sub(i1), 1048 * 1000 - 4096);
        // Other ports, another hash.
        let i4 = s.new_isn(&a, 1000, &o, 0);
        a.fport = 1001;
        assert_ne!(s.new_isn(&a, 1000, &o, 0), i4.wrapping_add(4096));
        // Without per-connection offsets the ports do not count.
        let o = s.new_ts_offset(&a, false);
        a.fport = 1000;
        assert_eq!(s.new_ts_offset(&a, false), o);
    }
}
