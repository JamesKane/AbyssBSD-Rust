// SPDX-License-Identifier: BSD-2-Clause

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use abi::bpf::{BpfInsn, dir, dlt, op, ts};
use abi::errno::Errno;
use abi::net::iff;
use mbuf::{Chain, Packet};

use super::{Desc, Notify, Step, Which};
use crate::Vnet;
use crate::clone::{self, Ctx};

#[derive(Default)]
struct Count(AtomicUsize);

impl Notify for Count {
    fn wake(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

fn ctx() -> Ctx {
    Ctx {
        now: 0,
        hostuuid: "6b0c3f6e-1111-2222-3333-444455556666".into(),
        jail: "0".into(),
        random: [7; 12],
    }
}

fn up(v: &Vnet, name: &[u8]) {
    let f = v.net.lock().by_name(name).unwrap().flags;
    v.net.lock().set_flags(name, f | iff::UP).unwrap();
}

fn frame(ty: u16, len: usize) -> Packet {
    let mut f = std::vec![0u8; len];
    f[..6].copy_from_slice(&[0xff; 6]);
    f[6..12].copy_from_slice(&[2, 0, 0, 0, 0, 1]);
    f[12..14].copy_from_slice(&ty.to_be_bytes());
    for (i, b) in f[14..].iter_mut().enumerate() {
        *b = u8::try_from(i % 256).unwrap();
    }
    Packet {
        hdr: mbuf::PktHdr::default(),
        data: Chain::from_slice(&f).unwrap(),
    }
}

fn send(v: &Arc<Vnet>, name: &[u8], m: Packet) {
    let (index, link) = {
        let n = v.net.lock();
        let i = n.by_name(name).unwrap();
        (i.index, i.link.0.clone().unwrap())
    };
    let _ = link.transmit(v, index, None, m);
}

/// A captured packet: caplen, datalen, hdrlen, bytes.
type Record = (u32, u32, u16, Vec<u8>);

/// The packets in a read buffer.
fn records(buf: &[u8], len: usize) -> Vec<Record> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < len {
        let caplen = u32::from_ne_bytes(buf[at + 16..at + 20].try_into().unwrap());
        let datalen = u32::from_ne_bytes(buf[at + 20..at + 24].try_into().unwrap());
        let hdrlen = u16::from_ne_bytes(buf[at + 24..at + 26].try_into().unwrap());
        let s = at + usize::from(hdrlen);
        out.push((
            caplen,
            datalen,
            hdrlen,
            buf[s..s + caplen as usize].to_vec(),
        ));
        at = abi::bpf::wordalign(s + caplen as usize);
    }
    out
}

fn read_now(d: &Desc) -> Result<Vec<Record>, Errno> {
    let timed_out = d.read_begin(d.bufsize() as usize)?;
    match d.read_step(true, timed_out, false)? {
        Step::Data(b, n) => {
            let r = records(&b, n);
            d.read_done(b);
            Ok(r)
        }
        s => panic!("{s:?}"),
    }
}

fn pair() -> Arc<Vnet> {
    let v = Vnet::new(0);
    clone::create(&v, b"epair", &ctx()).unwrap();
    up(&v, b"epair0a");
    up(&v, b"epair0b");
    v
}

#[test]
fn captures_both_ways_with_ethernet_headers() {
    let v = pair();
    let n = Arc::new(Count::default());
    let d = Desc::new(n.clone(), true);
    assert_eq!(d.dlt(), Err(Errno::EINVAL));
    d.set_interface(&v, b"epair0b").unwrap();
    assert_eq!(d.dlt(), Ok(dlt::EN10MB));
    d.set_immediate(true);
    // The other end's frame waits in `netisr` for Ethernet input, which
    // taps it coming in.
    send(&v, b"epair0a", frame(0x0806, 60));
    assert_eq!(n.0.load(Ordering::Relaxed), 0);
    // This end's own is tapped as it goes out, and wakes the reader in
    // immediate mode.
    send(&v, b"epair0b", frame(0x0800, 100));
    assert_eq!(n.0.load(Ordering::Relaxed), 1);
    let r = read_now(&d).unwrap();
    assert_eq!(r.len(), 1);
    assert_eq!((r[0].0, r[0].1, r[0].2), (100, 100, 26));
    assert_eq!(&r[0].3[12..14], &[8, 0]);
    assert_eq!(d.stats(), (1, 0));
}

#[test]
fn direction_and_filters_choose() {
    let v = pair();
    let d = Desc::new(Arc::new(Count::default()), true);
    d.set_interface(&v, b"epair0a").unwrap();
    d.set_immediate(true);
    d.set_direction(dir::IN).unwrap();
    assert_eq!(d.set_direction(7), Err(Errno::EINVAL));
    send(&v, b"epair0a", frame(0x0800, 60));
    assert_eq!(d.nread(), 0);
    assert_eq!(d.stats(), (0, 0));
    d.set_direction(dir::OUT).unwrap();
    // `tcpdump -dd arp` with a snapshot of 20 bytes.
    let arp = std::vec![
        BpfInsn {
            code: op::LD | op::H | op::ABS,
            jt: 0,
            jf: 0,
            k: 12
        },
        BpfInsn {
            code: op::JMP | op::JEQ | op::K,
            jt: 0,
            jf: 1,
            k: 0x0806
        },
        BpfInsn {
            code: op::RET | op::K,
            jt: 0,
            jf: 0,
            k: 20
        },
        BpfInsn {
            code: op::RET | op::K,
            jt: 0,
            jf: 0,
            k: 0
        },
    ];
    d.set_filter(Which::Read, Some(arp)).unwrap();
    send(&v, b"epair0a", frame(0x0800, 60));
    send(&v, b"epair0a", frame(0x0806, 60));
    let r = read_now(&d).unwrap();
    assert_eq!(r.len(), 1);
    assert_eq!((r[0].0, r[0].1), (20, 60));
    assert_eq!(d.stats(), (2, 0));
    // A program that fails validation is refused, the old one kept.
    let bad = std::vec![BpfInsn {
        code: op::LD | op::IMM,
        jt: 0,
        jf: 0,
        k: 0
    }];
    assert_eq!(d.set_filter(Which::Read, Some(bad)), Err(Errno::EINVAL));
}

#[test]
fn a_write_only_open_reads_after_its_real_filter() {
    let v = pair();
    let d = Desc::new(Arc::new(Count::default()), false);
    d.set_interface(&v, b"epair0a").unwrap();
    d.set_immediate(true);
    send(&v, b"epair0a", frame(0x0800, 60));
    assert_eq!(d.stats(), (0, 0));
    // pcap_open_live's snapshot filter, twice: the second upgrades.
    let snap = || {
        std::vec![BpfInsn {
            code: op::RET | op::K,
            jt: 0,
            jf: 0,
            k: 262_144
        }]
    };
    d.set_filter(Which::Read, Some(snap())).unwrap();
    send(&v, b"epair0a", frame(0x0800, 60));
    assert_eq!(d.stats(), (0, 0));
    d.set_filter(Which::Read, Some(snap())).unwrap();
    send(&v, b"epair0a", frame(0x0800, 60));
    assert_eq!(d.stats(), (1, 0));
}

#[test]
fn full_buffers_rotate_then_drop() {
    let v = pair();
    let d = Desc::new(Arc::new(Count::default()), true);
    assert_eq!(d.set_bufsize(10), Ok(32));
    assert_eq!(d.set_bufsize(1 << 30), Ok(super::MAXBUFSIZE));
    d.set_bufsize(512).unwrap();
    d.set_interface(&v, b"epair0a").unwrap();
    assert_eq!(d.set_bufsize(1024), Err(Errno::EINVAL));
    // 26 + 200 bytes a packet, word-aligned: two fit in 512.
    for _ in 0..5 {
        send(&v, b"epair0a", frame(0x0800, 200));
    }
    // Two in the hold buffer, two in the store, one dropped.
    assert_eq!(d.stats(), (5, 1));
    assert_eq!(d.nread(), 2 * (232 + 226));
    let r = read_now(&d).unwrap();
    assert_eq!(r.len(), 2);
    // Not immediate: the store waits for a timeout or to fill, unless
    // the read does not block.
    let r = read_now(&d).unwrap();
    assert_eq!(r.len(), 2);
    assert_eq!(read_now(&d).unwrap_err(), Errno::EAGAIN);
    // Blocking, it waits.
    d.read_begin(512).unwrap();
    assert!(matches!(
        d.read_step(false, false, false),
        Ok(Step::Wait(0))
    ));
    assert!(matches!(d.read_step(false, false, true), Ok(Step::Empty)));
    assert_eq!(d.read_begin(100), Err(Errno::EINVAL));
}

#[test]
fn timeouts_and_stamps_are_freebsds() {
    let d = Desc::new(Arc::new(Count::default()), true);
    d.set_timeout(1, 500_000).unwrap();
    assert_eq!(d.timeout(), (1, 500_000));
    d.set_timeout(0, 1500).unwrap();
    assert_eq!(d.timeout(), (0, 2000));
    assert_eq!(d.set_timeout(0, 1_000_000), Err(Errno::EINVAL));
    assert_eq!(d.set_tstamp(ts::NANOTIME | ts::MONOTONIC), Ok(()));
    assert_eq!(d.set_tstamp(0x4), Err(Errno::EINVAL));
    assert_eq!(
        super::stamp(ts::MICROTIME, (1_500_000_000, 10_000_000_000)),
        (11, 500_000)
    );
    assert_eq!(
        super::stamp(ts::NANOTIME | ts::MONOTONIC, (1_500_000_007, 9)),
        (1, 500_000_007)
    );
    assert_eq!(super::stamp(ts::NONE, (5, 5)), (0, 0));
    // On 64-bit machines the extended header is as long as the old one.
    let v = pair();
    d.set_interface(&v, b"epair0a").unwrap();
    d.set_immediate(true);
    send(&v, b"epair0a", frame(0x0800, 60));
    let r = read_now(&d).unwrap();
    assert_eq!(r[0].2, 26);
}

#[test]
fn locked_descriptors_refuse_changes() {
    use abi::bpf::bioc;
    let d = Desc::new(Arc::new(Count::default()), true);
    assert!(!d.forbids(bioc::SETF));
    d.lock();
    assert!(d.forbids(bioc::SETF));
    assert!(d.forbids(bioc::SETIF));
    assert!(!d.forbids(bioc::GSTATS));
    assert!(!d.forbids(bioc::IMMEDIATE));
}

#[test]
fn writes_build_frames_as_freebsd() {
    let v = pair();
    let d = Desc::new(Arc::new(Count::default()), true);
    assert_eq!(d.write(&[0; 60]).unwrap_err(), Errno::ENXIO);
    d.set_interface(&v, b"epair0a").unwrap();
    let mut f = std::vec![0xffu8; 60];
    f[6..12].copy_from_slice(&[9; 6]);
    let out = d.write(&f).unwrap().unwrap();
    let mut b = [0u8; 14];
    out.frame.data.copy_out(0, &mut b);
    let ll = v.net.lock().by_name(b"epair0a").unwrap().lladdr.unwrap();
    assert_eq!(&b[6..12], &ll);
    assert_ne!(out.frame.hdr.flags & mbuf::flags::BCAST, 0);
    d.set_hdrcmplt(true);
    let out = d.write(&f).unwrap().unwrap();
    out.frame.data.copy_out(0, &mut b);
    assert_eq!(&b[6..12], &[9; 6]);
    assert_eq!(d.write(&[0; 10]).unwrap_err(), Errno::EMSGSIZE);
    assert_eq!(d.write(&[0; 1515]).unwrap_err(), Errno::EMSGSIZE);
    assert!(d.write(&[]).unwrap().is_none());
    let refuse = std::vec![BpfInsn {
        code: op::RET | op::K,
        jt: 0,
        jf: 0,
        k: 0
    }];
    d.set_filter(Which::Write, Some(refuse)).unwrap();
    assert_eq!(d.write(&f).unwrap_err(), Errno::EPERM);
    let f2 = v.net.lock().by_name(b"epair0a").unwrap().flags;
    v.net.lock().set_flags(b"epair0a", f2 & !iff::UP).unwrap();
    assert_eq!(d.write(&f).unwrap_err(), Errno::ENETDOWN);
}

#[test]
fn promiscuous_mode_counts_takers() {
    let v = pair();
    let flags = || v.net.lock().by_name(b"epair0a").unwrap().flags;
    let a = Desc::new(Arc::new(Count::default()), true);
    let b = Desc::new(Arc::new(Count::default()), true);
    assert_eq!(a.promisc(), Err(Errno::EINVAL));
    a.set_interface(&v, b"epair0a").unwrap();
    b.set_interface(&v, b"epair0a").unwrap();
    a.promisc().unwrap();
    a.promisc().unwrap();
    b.promisc().unwrap();
    assert_ne!(flags() & iff::PROMISC, 0);
    a.close();
    assert_ne!(flags() & iff::PROMISC, 0);
    // SIOCSIFFLAGS keeps it while taken.
    let f = flags();
    v.net.lock().set_flags(b"epair0a", f).unwrap();
    assert_ne!(flags() & iff::PROMISC, 0);
    b.close();
    assert_eq!(flags() & iff::PROMISC, 0);
}

#[test]
fn loopback_traffic_passes_lo0s_tap() {
    let v = Vnet::new(0);
    up(&v, b"lo0");
    clone::create(&v, b"lo", &ctx()).unwrap();
    up(&v, b"lo1");
    let d = Desc::new(Arc::new(Count::default()), true);
    d.set_interface(&v, b"lo0").unwrap();
    assert_eq!(d.dlt(), Ok(dlt::NULL));
    d.set_immediate(true);
    let m = Packet {
        hdr: mbuf::PktHdr::default(),
        data: Chain::from_slice(&[0x45; 20]).unwrap(),
    };
    let index = v.net.lock().by_name(b"lo1").unwrap().index;
    crate::clone::output(&v, index, crate::Family::Inet, m).unwrap();
    let r = read_now(&d).unwrap();
    assert_eq!((r[0].0, r[0].2), (24, 28));
    assert_eq!(&r[0].3[..4], &u32::from(abi::inet::AF_INET).to_ne_bytes());
}

#[test]
fn moving_or_destroying_detaches() {
    let v = pair();
    let j = Vnet::new(0);
    let n = Arc::new(Count::default());
    let d = Desc::new(n.clone(), true);
    d.set_interface(&v, b"epair0b").unwrap();
    clone::vmove(&v, b"epair0b", &j).unwrap();
    assert_eq!(d.dlt(), Err(Errno::EINVAL));
    assert_eq!(read_now(&d).unwrap_err(), Errno::ENXIO);
    d.set_interface(&v, b"epair0a").unwrap();
    clone::destroy(&v, b"epair0a").unwrap();
    assert_eq!(d.dlt(), Err(Errno::EINVAL));
}
