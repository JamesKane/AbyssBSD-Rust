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

/// `ISN_BYTES_PER_SECOND`: how fast the ISN space advances.
const ISN_BYTES_PER_SECOND: u64 = 1_048_576;

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

impl Secrets {
    /// `tcp_new_isn`: a hash of the endpoints, moving on with the clock
    /// (`now` in ticks of a millisecond).
    #[must_use]
    pub fn new_isn(&self, inc: &Inc, now: u64) -> u32 {
        let mut b = [0u8; 40];
        let n = endpoints(inc, true, &mut b);
        let h = siphash24(&self.isn, &b[..n]) as u32;
        h.wrapping_add((now * ISN_BYTES_PER_SECOND / 1000) as u32)
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
        let i1 = s.new_isn(&a, 0);
        assert_eq!(s.new_isn(&a, 1000).wrapping_sub(i1), 1_048_576);
        a.fport = 1001;
        assert_ne!(s.new_isn(&a, 0), i1);
        // Without per-connection offsets the ports do not count.
        let o = s.new_ts_offset(&a, false);
        a.fport = 1000;
        assert_eq!(s.new_ts_offset(&a, false), o);
    }
}
