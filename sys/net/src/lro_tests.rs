// SPDX-License-Identifier: BSD-2-Clause
//! Software LRO against frames built here with their checksums.
#![allow(clippy::cast_possible_truncation, reason = "test frames are small")]

extern crate std;

use std::vec;
use std::vec::Vec;

use mbuf::{Chain, Packet, PktHdr, csum};

use crate::lro::Lro;

const A: [u8; 4] = [10, 0, 0, 1];
const B: [u8; 4] = [10, 0, 0, 2];

fn sum(b: &[u8]) -> u32 {
    b.chunks(2)
        .map(|w| u32::from(u16::from_be_bytes([w[0], *w.get(1).unwrap_or(&0)])))
        .sum()
}

fn fold(mut s: u32) -> u16 {
    while s >> 16 != 0 {
        s = (s & 0xffff) + (s >> 16);
    }
    s as u16
}

fn tcp_sum(ip: &[u8]) -> u16 {
    let tcp = &ip[20..];
    let len = tcp.len() as u32;
    !fold(sum(&ip[12..20]) + 6 + len + sum(tcp))
}

struct Seg<'a> {
    sport: u16,
    seq: u32,
    ack: u32,
    win: u16,
    flags: u8,
    ts: Option<(u32, u32)>,
    data: &'a [u8],
}

fn seg(sport: u16, seq: u32, ack: u32, data: &[u8]) -> Seg<'_> {
    Seg {
        sport,
        seq,
        ack,
        win: 1000,
        flags: 0x10,
        ts: Some((seq, 7)),
        data,
    }
}

fn frame(s: &Seg<'_>) -> Packet {
    let thlen = if s.ts.is_some() { 32 } else { 20 };
    let total = 20 + thlen + s.data.len();
    let mut f = vec![0u8; 14];
    f[12] = 8;
    let mut ip = vec![0x45, 0, 0, 0, 0, 1, 0x40, 0, 64, 6, 0, 0];
    ip[2..4].copy_from_slice(&(total as u16).to_be_bytes());
    ip.extend_from_slice(&A);
    ip.extend_from_slice(&B);
    let ipsum = !fold(sum(&ip));
    ip[10..12].copy_from_slice(&ipsum.to_be_bytes());
    let mut th = Vec::new();
    th.extend_from_slice(&s.sport.to_be_bytes());
    th.extend_from_slice(&80u16.to_be_bytes());
    th.extend_from_slice(&s.seq.to_be_bytes());
    th.extend_from_slice(&s.ack.to_be_bytes());
    th.push(((thlen / 4) as u8) << 4);
    th.push(s.flags);
    th.extend_from_slice(&s.win.to_be_bytes());
    th.extend_from_slice(&[0, 0, 0, 0]);
    if let Some((v, e)) = s.ts {
        th.extend_from_slice(&[1, 1, 8, 10]);
        th.extend_from_slice(&v.to_be_bytes());
        th.extend_from_slice(&e.to_be_bytes());
    }
    ip.extend_from_slice(&th);
    ip.extend_from_slice(s.data);
    let ts = tcp_sum(&ip);
    ip[36..38].copy_from_slice(&ts.to_be_bytes());
    f.extend_from_slice(&ip);
    while f.len() < 60 {
        f.push(0xee);
    }
    let hdr = PktHdr {
        csum_flags: csum::IP_CHECKED | csum::IP_VALID | csum::DATA_VALID | csum::PSEUDO_HDR,
        csum_data: 0xffff,
        ..PktHdr::default()
    };
    Packet {
        hdr,
        data: Chain::from_slice(&f).unwrap(),
    }
}

fn bytes(m: &Packet) -> Vec<u8> {
    let mut v = vec![0u8; m.len()];
    m.data.copy_out(0, &mut v);
    v
}

/// A frame that went up: its IP and TCP checksums verify from scratch.
fn check(m: &Packet) -> (u32, u32, Vec<u8>) {
    let b = bytes(m);
    let ip = &b[14..];
    let total = usize::from(u16::from_be_bytes([ip[2], ip[3]]));
    let ip = &ip[..total];
    assert_eq!(fold(sum(&ip[..20])), 0xffff, "IP checksum");
    assert_eq!(
        fold(sum(&ip[12..20]) + 6 + (total as u32 - 20) + sum(&ip[20..])),
        0xffff,
        "TCP checksum"
    );
    let thlen = usize::from(ip[32] >> 4) * 4;
    let seq = u32::from_be_bytes(ip[24..28].try_into().unwrap());
    let ack = u32::from_be_bytes(ip[28..32].try_into().unwrap());
    (seq, ack, ip[20 + thlen..].to_vec())
}

fn run(lro: &mut Lro, segs: &[Seg<'_>]) -> Vec<Packet> {
    let mut out = Vec::new();
    for s in segs {
        lro.rx(frame(s), 0, &mut |m| out.push(m));
    }
    lro.flush_all(&mut |m| out.push(m));
    out
}

#[test]
fn in_order_segments_merge_with_valid_checksums() {
    let d: Vec<u8> = (0..1448u32 * 5).map(|i| (i * 7 + i / 3) as u8).collect();
    // Odd lengths too, so a segment's sum lands at an odd offset.
    let cuts = [0, 1448, 2001, 3449, 5000, d.len()];
    let segs: Vec<Seg<'_>> = cuts
        .windows(2)
        .map(|w| seg(1, 100 + w[0] as u32, 9 + w[0] as u32, &d[w[0]..w[1]]))
        .collect();
    let mut lro = Lro::new();
    let out = run(&mut lro, &segs);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].hdr.lro_nsegs, 5);
    let (seq, ack, data) = check(&out[0]);
    assert_eq!(seq, 100);
    assert_eq!(ack, 9 + 5000);
    assert_eq!(data, d);
}

#[test]
fn a_lone_frame_goes_up_untouched() {
    let mut lro = Lro::new();
    let f = frame(&seg(1, 5, 6, b"hello"));
    let want = bytes(&f)[..14 + 20 + 32 + 5].to_vec();
    let out = run(&mut lro, &[seg(1, 5, 6, b"hello")]);
    assert_eq!(out.len(), 1);
    assert_eq!(bytes(&out[0]), want, "padding cut, nothing else");
    assert_eq!(out[0].hdr.lro_nsegs, 0);
}

#[test]
fn fin_gap_and_dup_ack_keep_order() {
    let d = [7u8; 100];
    let mut fin = seg(1, 300, 9, &d);
    fin.flags = 0x11;
    let segs = [
        seg(1, 100, 9, &d),
        seg(1, 200, 9, &d),
        fin,
        seg(1, 500, 9, &d), // a gap
        seg(1, 600, 9, &d),
        seg(2, 0, 50, &[]), // another connection's ACK
        seg(2, 0, 50, &[]), // its duplicate
    ];
    let mut lro = Lro::new();
    let out = run(&mut lro, &segs);
    let got: Vec<(u32, u32, usize, u16)> = out
        .iter()
        .map(|m| {
            let (s, a, d) = check(m);
            (s, a, d.len(), m.hdr.lro_nsegs)
        })
        .collect();
    let one: Vec<_> = got.iter().filter(|g| g.1 == 9).collect();
    assert_eq!(*one[0], (100, 9, 200, 2));
    assert_eq!(*one[1], (300, 9, 100, 0), "the FIN after what was held");
    assert_eq!(*one[2], (500, 9, 200, 2));
    let two: Vec<_> = got.iter().filter(|g| g.1 == 50).collect();
    assert_eq!(two.len(), 2, "a duplicate ACK is not folded");
}

#[test]
fn acks_fold_into_what_is_held() {
    let d = [3u8; 10];
    let mut lro = Lro::new();
    // After data: the ACKs fold into it (`tcp_lro_condense`).
    let out = run(
        &mut lro,
        &[
            seg(1, 0, 10, &d),
            seg(1, 10, 10, &d),
            seg(1, 20, 20, &[]),
            seg(1, 20, 30, &[]),
        ],
    );
    let got: Vec<(u32, u32, usize)> = out
        .iter()
        .map(|m| {
            let (s, a, d) = check(m);
            (s, a, d.len())
        })
        .collect();
    assert_eq!(got, [(0, 30, 20)]);
    // ACKs alone: one frame, the newest, and an older one dropped.
    let out = run(
        &mut lro,
        &[
            seg(2, 0, 100, &[]),
            seg(2, 0, 200, &[]),
            seg(2, 0, 150, &[]),
            seg(2, 0, 300, &[]),
        ],
    );
    let got: Vec<(u32, u32, usize)> = out
        .iter()
        .map(|m| {
            let (s, a, d) = check(m);
            (s, a, d.len())
        })
        .collect();
    assert_eq!(got, [(0, 300, 0)]);
}

#[test]
fn rounds_flush_acks_at_once_and_data_by_age() {
    let d = [1u8; 64];
    let mut lro = Lro::new();
    let mut out = Vec::new();
    lro.rx(frame(&seg(1, 0, 1, &d)), 0, &mut |m| out.push(m));
    lro.rx(frame(&seg(2, 0, 1, &[])), 0, &mut |m| out.push(m));
    lro.flush_round(10, 100, &mut |m| out.push(m));
    assert_eq!(out.len(), 1, "the ACK went, the data waits");
    lro.rx(frame(&seg(1, 64, 1, &d)), 50, &mut |m| out.push(m));
    lro.flush_round(99, 100, &mut |m| out.push(m));
    assert_eq!(out.len(), 1);
    lro.flush_round(100, 100, &mut |m| out.push(m));
    assert_eq!(out.len(), 2);
    assert_eq!(out[1].hdr.lro_nsegs, 2);
    assert!(lro.is_empty());
}

#[test]
fn unverified_or_odd_frames_pass() {
    let mut lro = Lro::new();
    let mut f = frame(&seg(1, 0, 1, b"x"));
    f.hdr.csum_flags = 0;
    let mut out = Vec::new();
    lro.rx(f, 0, &mut |m| out.push(m));
    let mut no_ts = seg(1, 0, 1, b"y");
    no_ts.ts = None;
    lro.rx(frame(&no_ts), 0, &mut |m| out.push(m));
    let mut ts = seg(1, 1, 1, b"z");
    ts.ts = Some((5, 5));
    lro.rx(frame(&ts), 0, &mut |m| out.push(m));
    lro.flush_all(&mut |m| out.push(m));
    assert_eq!(out.len(), 3, "no merge across a timestamp mismatch");
}

#[test]
fn a_push_goes_up_at_the_round() {
    let d = [1u8; 64];
    let mut lro = Lro::new();
    let mut out = Vec::new();
    let mut psh = seg(1, 0, 1, &d);
    psh.flags = 0x18;
    lro.rx(frame(&psh), 0, &mut |m| out.push(m));
    lro.flush_round(1, 100, &mut |m| out.push(m));
    assert_eq!(out.len(), 1);
}
