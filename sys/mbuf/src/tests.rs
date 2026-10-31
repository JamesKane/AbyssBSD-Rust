// SPDX-License-Identifier: BSD-2-Clause

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::zone::{Limits, Zone};
use crate::*;

fn bytes(n: usize) -> Vec<u8> {
    (0..n)
        .map(|i| u8::try_from((i * 7 + 3) % 256).unwrap())
        .collect()
}

fn flat(c: &Chain) -> Vec<u8> {
    c.segments().flatten().copied().collect()
}

#[test]
fn sizes_match_freebsd() {
    assert_eq!((MLEN, MHLEN, MINCLSIZE), (224, 160, 161));
    assert_eq!(MAX_HDR, 76);
}

#[test]
fn carving_follows_mc_get() {
    assert_eq!(carve_next(1), (224, None));
    assert_eq!(carve_next(160), (224, None));
    assert_eq!(carve_next(161), (2048, Some(Cluster::Std)));
    assert_eq!(carve_next(2048), (2048, Some(Cluster::Std)));
    assert_eq!(carve_next(2049), (4096, Some(Cluster::Page)));
    assert_eq!(carve_cost(100), 256);
    assert_eq!(carve_cost(1000), 256 + 2048);
    // 10000: two pages, then 1808 in a 2 KiB cluster.
    assert_eq!(carve_cost(10000), 2 * (256 + 4096) + 256 + 2048);
    let c = Chain::from_slice(&bytes(10000)).unwrap();
    assert_eq!(c.cost(), carve_cost(10000));
    assert_eq!(c.mbufs(), 3);
    assert_eq!(c.len(), 10000);
    assert_eq!(flat(&c), bytes(10000));
}

#[test]
fn fill_stops_at_a_short_copy() {
    let c = Chain::fill(9000, |b, at| {
        let n = if at == 4096 { 10 } else { b.len() };
        b[..n].fill(1);
        Ok::<_, NoBufs>(n)
    })
    .unwrap();
    assert_eq!(c.len(), 4106);
    let e = Chain::fill(100, |_, _| Err::<usize, _>(NoBufs));
    assert!(e.is_err());
    assert!(Chain::from_slice(&[]).unwrap().is_empty());
}

#[test]
fn split_shares_a_cut_cluster_and_copies_inline() {
    let data = bytes(5000);
    let mut c = Chain::from_slice(&data).unwrap();
    let cost = c.cost();
    let rest = c.split_off(1000).unwrap();
    assert_eq!((c.len(), rest.len()), (1000, 4000));
    assert_eq!(flat(&c), data[..1000]);
    assert_eq!(flat(&rest), data[1000..]);
    // The cut page cluster is charged to both halves.
    assert_eq!(c.cost() + rest.cost(), cost + 256 + 4096);
    drop(c);
    assert_eq!(flat(&rest), data[1000..]);

    let mut s = Chain::from_slice(&bytes(100)).unwrap();
    s.set_eor(true);
    let tail = s.split_off(40).unwrap();
    assert!(tail.eor() && !s.eor());
    assert_eq!(
        (s.len(), tail.len(), s.cost(), tail.cost()),
        (40, 60, 256, 256)
    );
    assert_eq!(flat(&tail), bytes(100)[40..]);
    // At the end, or past it: nothing.
    assert!(s.split_off(40).unwrap().is_empty());
    assert!(s.split_off(400).unwrap().is_empty());
    // On an mbuf boundary nothing is shared.
    let mut b = Chain::from_slice(&bytes(4096 + 3000)).unwrap();
    let cost = b.cost();
    let r = b.split_off(4096).unwrap();
    assert_eq!(b.cost() + r.cost(), cost);
}

#[test]
fn take_front_leaves_the_rest() {
    let data = bytes(3000);
    let mut c = Chain::from_slice(&data).unwrap();
    let head = c.take_front(2500).unwrap();
    assert_eq!(flat(&head), data[..2500]);
    assert_eq!(flat(&c), data[2500..]);
    c.trim_front(400);
    assert_eq!(flat(&c), data[2900..]);
}

#[test]
fn copies_by_reference_are_read_only_until_alone() {
    let data = bytes(6000);
    let mut c = Chain::from_slice(&data).unwrap();
    let mut first = c.pop_front().unwrap();
    assert!(first.writable());
    let copy = first.share(10, 100).unwrap();
    assert_eq!(copy.bytes(), &data[10..110]);
    assert!(!first.writable() && !copy.writable());
    assert!(first.bytes_mut().is_none());
    assert_eq!(first.trailing_space(), 0);
    drop(copy);
    assert!(first.writable());
    first.bytes_mut().unwrap()[0] = 9;

    // The first holder goes first: the copy keeps the cluster alive.
    let copy = first.share(0, 50).unwrap();
    let more = copy.share(1, 10).unwrap();
    drop(first);
    assert_eq!(copy.bytes()[0], 9);
    assert_eq!(more.bytes(), &data[1..11]);
    drop(copy);
    assert!(more.writable());
    drop(more);

    let all = c.copy(0, c.len()).unwrap();
    assert_eq!(flat(&all), data[4096..]);
    let part = c.copy(5, 10).unwrap();
    assert_eq!(flat(&part), flat(&c)[5..15]);
}

struct Bytes(Vec<u8>);

impl Pages for Bytes {
    fn bytes(&self) -> &[u8] {
        &self.0
    }
}

#[test]
fn pages_are_shared_and_released() {
    let p = Arc::new(Bytes(bytes(8192)));
    let m = Mbuf::with_pages(p.clone(), 100, 5000, 8192).unwrap();
    assert_eq!(m.bytes(), &bytes(8192)[100..5100]);
    assert!(!m.writable());
    assert_eq!(m.cost(), 256 + 8192);
    let s = m.share(4000, 2000).unwrap();
    assert_eq!(s.len(), 1000);
    assert_eq!(Arc::strong_count(&p), 3);
    drop((m, s));
    assert_eq!(Arc::strong_count(&p), 1);
    let past = Mbuf::with_pages(p.clone(), 9000, 10, 0).unwrap();
    assert!(past.is_empty());
}

#[test]
fn pullup_gathers_into_the_first_mbuf() {
    // Three inline mbufs of 10 bytes each.
    let mut c = Chain::new();
    for i in 0..3u8 {
        let mut m = Mbuf::new().unwrap();
        m.append(&[i; 10]);
        c.push_back(m);
    }
    let h = c.pullup(25).unwrap();
    assert_eq!(&h[..12], &[0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1]);
    assert_eq!(c.len(), 30);
    assert_eq!(c.first().unwrap().len(), 25);
    assert_eq!(c.mbufs(), 2);
    assert_eq!(c.pullup(31), Err(NoBufs));
    assert_eq!(c.len(), 30);

    // A shared cluster can't take more: a new mbuf goes in front.
    let data = bytes(600);
    let src = Chain::from_slice(&data).unwrap();
    let mut s = src.copy(0, 600).unwrap();
    let rest = s.split_off(20).unwrap();
    s.append(rest);
    assert_eq!(s.pullup(64).unwrap(), &data[..64]);
    assert_eq!(flat(&s), data);
    assert_eq!(s.pullup(300), Err(NoBufs));
}

#[test]
fn prepend_uses_leading_space() {
    let mut p = Packet::for_output(100).unwrap();
    p.data.extend_from_slice(b"payload").unwrap();
    assert_eq!(p.data.mbufs(), 1);
    p.prepend(20).unwrap().fill(0x45);
    p.prepend(14).unwrap().fill(0xee);
    assert_eq!(p.data.mbufs(), 1);
    assert_eq!(p.len(), 41);
    // Past the reserve: a new mbuf.
    p.prepend(60).unwrap().fill(1);
    assert_eq!(p.data.mbufs(), 2);
    let mut all = alloc::vec![0; p.len()];
    p.data.copy_out(0, &mut all);
    assert_eq!(&all[60..74], &[0xee; 14]);
    assert_eq!(&all[94..], b"payload");
    assert_eq!(p.prepend(300), Err(NoBufs));
    // A big packet gets a cluster.
    let big = Packet::for_output(1500).unwrap();
    assert_eq!(big.data.cost(), 256 + 2048);
}

#[derive(Debug, PartialEq)]
struct Two(u16);

impl Header for Two {
    const LEN: usize = 2;
    fn parse(b: &[u8]) -> Option<Self> {
        let v = u16::from_be_bytes([b[0], b[1]]);
        (v != 0xffff).then_some(Self(v))
    }
}

#[test]
fn typed_headers() {
    let mut p = Packet {
        hdr: PktHdr::default(),
        data: Chain::from_slice(&[0, 0, 0x12, 0x34, 0xff, 0xff]).unwrap(),
    };
    assert_eq!(p.header::<Two>(2), Ok(Two(0x1234)));
    assert_eq!(p.header::<Two>(4), Err(Pullup::Malformed));
    assert_eq!(p.header::<Two>(5), Err(Pullup::Short));
    assert_eq!(p.header::<Two>(usize::MAX), Err(Pullup::Short));
    let mut d = bytes(400);
    d[300] = 0xab;
    d[301] = 0xcd;
    let mut big = Packet {
        hdr: PktHdr::default(),
        data: Chain::from_slice(&d).unwrap(),
    };
    assert_eq!(big.header::<Two>(300), Ok(Two(0xabcd)));
    let c = big.copy().unwrap();
    let r = big.split_off(100).unwrap();
    assert_eq!((big.len(), r.len(), c.len()), (100, 300, 400));
}

#[test]
fn records_and_compaction() {
    let mut c = Chain::from_slice(b"abc").unwrap();
    c.extend_from_slice(b"def").unwrap();
    assert_eq!((c.mbufs(), c.len()), (1, 6));
    c.set_eor(true);
    assert!(c.eor());
    // A record's end is not appended to.
    c.extend_from_slice(b"g").unwrap();
    assert_eq!(c.mbufs(), 2);
    assert!(!c.eor());
    assert_eq!(flat(&c), b"abcdefg");
}

#[test]
fn zone_limit() {
    static Z: Zone = Zone::new("test", 256);
    Z.set_limit(3);
    let a: Vec<_> = (0..3).map(|_| Z.alloc().unwrap()).collect();
    assert!(Z.alloc().is_none());
    let st = Z.stats();
    assert_eq!((st.used, st.fails), (3, 1));
    // SAFETY: from this zone, freed once.
    unsafe { Z.free(a[0]) };
    let b = Z.alloc().unwrap();
    for p in a[1..].iter().copied().chain([b]) {
        // SAFETY: as above.
        unsafe { Z.free(p) };
    }
    assert_eq!(Z.stats().used, 0);

    // Batches: a CPU's credit counts against the limit.
    static Y: Zone = Zone::new("batched", 256);
    Y.set_limit(100);
    let a: Vec<_> = (0..100).map(|_| Y.alloc().unwrap()).collect();
    assert!(Y.alloc().is_none());
    for p in a {
        // SAFETY: as above.
        unsafe { Y.free(p) };
    }
    let a: Vec<_> = (0..100).map(|_| Y.alloc().unwrap()).collect();
    for p in a {
        // SAFETY: as above.
        unsafe { Y.free(p) };
    }
}

#[test]
fn default_limits() {
    let l = Limits::for_memory(8 << 30);
    assert_eq!(l.maxmbufmem, 4 << 30);
    assert_eq!(l.nmbclusters, (4 << 30) / 2048 / 4);
    assert_eq!(l.nmbjumbop, (4 << 30) / 4096 / 4);
    assert_eq!(l.nmbjumbo9, (4u64 << 30) / 9216 / 6);
    assert_eq!(l.nmbufs, (4 << 30) / 256 / 5);
}
