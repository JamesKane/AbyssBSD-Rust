// SPDX-License-Identifier: BSD-2-Clause
//! The TCP header (`struct tcphdr`) and its options (`struct tcpopt`):
//! parsed from untrusted bytes into typed values, and written back.
//!
//! Behavior reference: sys/netinet/tcp.h, sys/netinet/tcp_var.h
//! (`struct tcpopt`), sys/netinet/tcp_input.c (`tcp_dooptions`),
//! sys/netinet/tcp_output.c (`tcp_addoptions`).

use crate::seq::Seq;

/// The header's length without options.
pub const HDR_LEN: usize = 20;
/// `TCP_MAXOLEN`: the most option bytes a header holds.
pub const MAXOLEN: usize = 40;
/// `TCP_MAX_WINSHIFT`.
pub const MAX_WINSHIFT: u8 = 14;
/// `TCP_MAXWIN`: the largest unscaled window.
pub const MAXWIN: u32 = 65535;
/// `MAX_SACK_BLKS`: SACK blocks a receiver keeps and sends.
pub const MAX_SACK_BLKS: usize = 6;
/// `TCP_MAX_SACK`: SACK blocks one header carries (with timestamps).
pub const MAX_SACK: usize = 4;

/// `th_flags` bits (`TH_*`), the 12 of `tcp_get_flags`.
pub mod th {
    pub const FIN: u16 = 0x01;
    pub const SYN: u16 = 0x02;
    pub const RST: u16 = 0x04;
    pub const PUSH: u16 = 0x08;
    pub const ACK: u16 = 0x10;
    pub const URG: u16 = 0x20;
    pub const ECE: u16 = 0x40;
    pub const CWR: u16 = 0x80;
    pub const AE: u16 = 0x100;
    /// `TH_RES1`..`TH_RES3`.
    pub const RES: u16 = 0xe00;
}

/// A parsed header (`struct tcphdr`), in host order.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Th {
    pub sport: u16,
    pub dport: u16,
    pub seq: Seq,
    pub ack: Seq,
    /// The header's length in bytes (`th_off << 2`).
    pub off: usize,
    pub flags: u16,
    pub win: u16,
    pub sum: u16,
    pub urp: u16,
}

impl Th {
    /// Parses the fixed part of the header at the start of `b`; `None`
    /// if `b` is short or the data offset is impossible (`tcps_rcvshort`,
    /// `tcps_rcvbadoff`, which the caller tells apart by length).
    #[must_use]
    pub fn parse(b: &[u8]) -> Option<Self> {
        let h = b.get(..HDR_LEN)?;
        let off = usize::from(h[12] >> 4) << 2;
        if off < HDR_LEN {
            return None;
        }
        Some(Self {
            sport: u16::from_be_bytes([h[0], h[1]]),
            dport: u16::from_be_bytes([h[2], h[3]]),
            seq: u32::from_be_bytes([h[4], h[5], h[6], h[7]]),
            ack: u32::from_be_bytes([h[8], h[9], h[10], h[11]]),
            off,
            flags: (u16::from(h[12] & 0x0f) << 8) | u16::from(h[13]),
            win: u16::from_be_bytes([h[14], h[15]]),
            sum: u16::from_be_bytes([h[16], h[17]]),
            urp: u16::from_be_bytes([h[18], h[19]]),
        })
    }

    /// Writes the fixed part into `b` (20 bytes), with the checksum
    /// field as given.
    pub fn write(&self, b: &mut [u8]) {
        b[0..2].copy_from_slice(&self.sport.to_be_bytes());
        b[2..4].copy_from_slice(&self.dport.to_be_bytes());
        b[4..8].copy_from_slice(&self.seq.to_be_bytes());
        b[8..12].copy_from_slice(&self.ack.to_be_bytes());
        // The offset fits: at most 60 bytes.
        b[12] = (((self.off >> 2) as u8) << 4) | ((self.flags >> 8) as u8 & 0x0f);
        b[13] = self.flags as u8;
        b[14..16].copy_from_slice(&self.win.to_be_bytes());
        b[16..18].copy_from_slice(&self.sum.to_be_bytes());
        b[18..20].copy_from_slice(&self.urp.to_be_bytes());
    }

    #[inline]
    #[must_use]
    pub const fn has(&self, f: u16) -> bool {
        self.flags & f != 0
    }
}

/// `TCPOPT_*` and their lengths.
pub mod opt {
    pub const EOL: u8 = 0;
    pub const NOP: u8 = 1;
    pub const MAXSEG: u8 = 2;
    pub const WINDOW: u8 = 3;
    pub const SACK_PERMITTED: u8 = 4;
    pub const SACK: u8 = 5;
    pub const TIMESTAMP: u8 = 8;
    pub const SIGNATURE: u8 = 19;
    pub const FAST_OPEN: u8 = 34;
    pub const LEN_MAXSEG: usize = 4;
    pub const LEN_WINDOW: usize = 3;
    pub const LEN_SACK_PERMITTED: usize = 2;
    pub const LEN_SACKHDR: usize = 2;
    pub const LEN_SACK: usize = 8;
    pub const LEN_TIMESTAMP: usize = 10;
    /// `TCPOLEN_TSTAMP_APPA`: a timestamp with two NOPs before it.
    pub const LEN_TSTAMP_APPA: usize = 12;
    pub const LEN_SIGNATURE: usize = 18;
}

/// `to_flags` bits (`TOF_*`).
pub mod tof {
    pub const MSS: u32 = 0x0001;
    pub const SCALE: u32 = 0x0002;
    pub const SACKPERM: u32 = 0x0004;
    pub const TS: u32 = 0x0010;
    pub const SIGNATURE: u32 = 0x0040;
    pub const SACK: u32 = 0x0080;
    pub const FASTOPEN: u32 = 0x0100;
    pub const MAXOPT: u32 = 0x0200;
}

/// A SACK block (`struct sackblk`): `[start, end)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SackBlk {
    pub start: Seq,
    pub end: Seq,
}

/// The options of one segment (`struct tcpopt`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Opts {
    pub flags: u32,
    pub tsval: u32,
    pub tsecr: u32,
    pub mss: u16,
    pub wscale: u8,
    pub nsacks: u8,
    pub sacks: [SackBlk; MAX_SACK],
}

/// What `tcp_dooptions` may accept: SYN-only options need `syn`, and SACK
/// permission needs `sack` (`net.inet.tcp.sack.enable`).
#[derive(Clone, Copy, Debug)]
pub struct Accept {
    pub syn: bool,
    pub sack: bool,
}

impl Opts {
    #[inline]
    #[must_use]
    pub const fn has(&self, f: u32) -> bool {
        self.flags & f != 0
    }

    /// `tcp_dooptions`: the options in `b`, which never fail; a malformed
    /// option ends the parse, an unknown or misplaced one is skipped.
    #[must_use]
    pub fn parse(mut b: &[u8], acc: Accept) -> Self {
        let mut to = Self::default();
        while let Some(&kind) = b.first() {
            if kind == opt::EOL {
                break;
            }
            let len = if kind == opt::NOP {
                1
            } else {
                let Some(&l) = b.get(1) else { break };
                let l = usize::from(l);
                if l < 2 || l > b.len() {
                    break;
                }
                l
            };
            let o = &b[..len];
            b = &b[len..];
            match kind {
                opt::MAXSEG if len == opt::LEN_MAXSEG && acc.syn => {
                    to.flags |= tof::MSS;
                    to.mss = u16::from_be_bytes([o[2], o[3]]);
                }
                opt::WINDOW if len == opt::LEN_WINDOW && acc.syn => {
                    to.flags |= tof::SCALE;
                    to.wscale = o[2].min(MAX_WINSHIFT);
                }
                opt::TIMESTAMP if len == opt::LEN_TIMESTAMP => {
                    to.flags |= tof::TS;
                    to.tsval = u32::from_be_bytes([o[2], o[3], o[4], o[5]]);
                    to.tsecr = u32::from_be_bytes([o[6], o[7], o[8], o[9]]);
                }
                opt::SIGNATURE if len == opt::LEN_SIGNATURE => {
                    to.flags |= tof::SIGNATURE;
                }
                opt::SACK_PERMITTED if len == opt::LEN_SACK_PERMITTED && acc.syn && acc.sack => {
                    to.flags |= tof::SACKPERM;
                }
                opt::SACK
                    if len > opt::LEN_SACKHDR
                        && (len - opt::LEN_SACKHDR).is_multiple_of(opt::LEN_SACK)
                        && !acc.syn =>
                {
                    to.flags |= tof::SACK;
                    let n = ((len - opt::LEN_SACKHDR) / opt::LEN_SACK).min(MAX_SACK);
                    to.nsacks = n as u8;
                    for (i, s) in to.sacks.iter_mut().take(n).enumerate() {
                        let p = &o[2 + i * 8..10 + i * 8];
                        s.start = u32::from_be_bytes([p[0], p[1], p[2], p[3]]);
                        s.end = u32::from_be_bytes([p[4], p[5], p[6], p[7]]);
                    }
                }
                _ => {}
            }
        }
        to
    }

    /// The SACK blocks it carries.
    #[must_use]
    pub fn sacks(&self) -> &[SackBlk] {
        &self.sacks[..usize::from(self.nsacks).min(MAX_SACK)]
    }

    /// `tcp_addoptions`: writes the options into `b` (at least
    /// [`MAXOLEN`] bytes) in FreeBSD's order and alignment, padded to a
    /// multiple of four; returns their length. SACK blocks beyond what
    /// fits are left out.
    pub fn write(&self, b: &mut [u8]) -> usize {
        let mut n = 0;
        let put = |b: &mut [u8], n: &mut usize, v: u8| {
            b[*n] = v;
            *n += 1;
        };
        let mut mask = 1;
        while mask < tof::MAXOPT {
            let f = self.flags & mask;
            mask <<= 1;
            if f == 0 {
                continue;
            }
            if n == MAXOLEN {
                break;
            }
            match f {
                tof::MSS => {
                    while n % 4 != 0 {
                        put(b, &mut n, opt::NOP);
                    }
                    if MAXOLEN - n < opt::LEN_MAXSEG {
                        continue;
                    }
                    put(b, &mut n, opt::MAXSEG);
                    put(b, &mut n, opt::LEN_MAXSEG as u8);
                    let m = self.mss.to_be_bytes();
                    put(b, &mut n, m[0]);
                    put(b, &mut n, m[1]);
                }
                tof::SCALE => {
                    while n == 0 || n % 2 != 1 {
                        put(b, &mut n, opt::NOP);
                    }
                    if MAXOLEN - n < opt::LEN_WINDOW {
                        continue;
                    }
                    put(b, &mut n, opt::WINDOW);
                    put(b, &mut n, opt::LEN_WINDOW as u8);
                    put(b, &mut n, self.wscale);
                }
                tof::SACKPERM => {
                    while n % 2 != 0 {
                        put(b, &mut n, opt::NOP);
                    }
                    if MAXOLEN - n < opt::LEN_SACK_PERMITTED {
                        continue;
                    }
                    put(b, &mut n, opt::SACK_PERMITTED);
                    put(b, &mut n, opt::LEN_SACK_PERMITTED as u8);
                }
                tof::TS => {
                    while n == 0 || n % 4 != 2 {
                        put(b, &mut n, opt::NOP);
                    }
                    if MAXOLEN - n < opt::LEN_TIMESTAMP {
                        continue;
                    }
                    put(b, &mut n, opt::TIMESTAMP);
                    put(b, &mut n, opt::LEN_TIMESTAMP as u8);
                    for x in self
                        .tsval
                        .to_be_bytes()
                        .into_iter()
                        .chain(self.tsecr.to_be_bytes())
                    {
                        put(b, &mut n, x);
                    }
                }
                tof::SACK => {
                    while n == 0 || n % 4 != 2 {
                        put(b, &mut n, opt::NOP);
                    }
                    if MAXOLEN - n < opt::LEN_SACKHDR + opt::LEN_SACK {
                        continue;
                    }
                    let k = self
                        .sacks()
                        .len()
                        .min((MAXOLEN - n - opt::LEN_SACKHDR) / opt::LEN_SACK);
                    put(b, &mut n, opt::SACK);
                    put(b, &mut n, (opt::LEN_SACKHDR + k * opt::LEN_SACK) as u8);
                    for s in &self.sacks()[..k] {
                        for x in s.start.to_be_bytes().into_iter().chain(s.end.to_be_bytes()) {
                            put(b, &mut n, x);
                        }
                    }
                }
                // Signatures and Fast Open are not sent.
                _ => {}
            }
        }
        if n % 4 != 0 {
            put(b, &mut n, opt::EOL);
        }
        while n % 4 != 0 {
            put(b, &mut n, 0);
        }
        n
    }

    /// The length [`Opts::write`] would give.
    #[must_use]
    pub fn len(&self) -> usize {
        let mut b = [0u8; MAXOLEN + 4];
        self.write(&mut b)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.flags == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SYN: Accept = Accept {
        syn: true,
        sack: true,
    };

    #[test]
    fn a_syn_s_options_are_laid_out_as_freebsd_s() {
        let to = Opts {
            flags: tof::MSS | tof::SCALE | tof::SACKPERM | tof::TS,
            mss: 1460,
            wscale: 8,
            tsval: 0x0102_0304,
            tsecr: 0,
            ..Opts::default()
        };
        let mut b = [0u8; 44];
        let n = to.write(&mut b);
        // mss 1460, nop, wscale 8, sackOK, TS (tcpdump shows FreeBSD's SYN so).
        assert_eq!(
            &b[..n],
            &[
                2, 4, 0x05, 0xb4, 1, 3, 3, 8, 4, 2, 8, 10, 1, 2, 3, 4, 0, 0, 0, 0
            ]
        );
        assert_eq!(Opts::parse(&b[..n], SYN), to);
    }

    #[test]
    fn timestamps_alone_use_appendix_a() {
        let to = Opts {
            flags: tof::TS,
            tsval: 7,
            tsecr: 9,
            ..Opts::default()
        };
        let mut b = [0u8; 44];
        assert_eq!(to.write(&mut b), opt::LEN_TSTAMP_APPA);
        assert_eq!(&b[..4], &[1, 1, 8, 10]);
    }

    #[test]
    fn sack_blocks_fit_beside_timestamps() {
        let mut to = Opts {
            flags: tof::TS | tof::SACK,
            nsacks: 4,
            ..Opts::default()
        };
        for (i, s) in to.sacks.iter_mut().enumerate() {
            *s = SackBlk {
                start: i as u32 * 100,
                end: i as u32 * 100 + 50,
            };
        }
        let mut b = [0u8; 44];
        let n = to.write(&mut b);
        assert!(n <= MAXOLEN);
        let back = Opts::parse(
            &b[..n],
            Accept {
                syn: false,
                sack: true,
            },
        );
        // 12 for the timestamp, 2 + 3 * 8 for SACK: three blocks fit.
        assert_eq!(back.nsacks, 3);
        assert_eq!(back.sacks[2], to.sacks[2]);
    }

    #[test]
    fn malformed_options_end_the_parse_without_panicking() {
        for b in [
            &[2u8][..],
            &[2, 1],
            &[2, 40, 0],
            &[5, 3, 1, 2, 3],
            &[8, 10, 1],
        ] {
            let _ = Opts::parse(b, SYN);
        }
        // An MSS outside a SYN is ignored.
        let to = Opts::parse(
            &[2, 4, 1, 0],
            Accept {
                syn: false,
                sack: true,
            },
        );
        assert!(!to.has(tof::MSS));
    }

    #[test]
    fn headers_round_trip() {
        let th = Th {
            sport: 1,
            dport: 2,
            seq: 3,
            ack: 4,
            off: 32,
            flags: th::SYN | th::ACK | th::AE,
            win: 5,
            sum: 6,
            urp: 7,
        };
        let mut b = [0u8; 20];
        th.write(&mut b);
        assert_eq!(Th::parse(&b), Some(th));
        b[12] = 0x40;
        assert_eq!(Th::parse(&b), None);
    }
}
