// SPDX-License-Identifier: BSD-2-Clause
//! `vtnet0` on QEMU's user-mode network: a DHCP discover written to
//! `/dev/bpf` brings the network's offer back through it,
//! and ARP resolves the router once the interface has an address. Without
//! a `vtnet0` (bhyve's kernel tests have no network) the test says so
//! and passes.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use abi::bpf::bioc;
use abi::net::iff;
use ktest::ktest;
use net::{Family, ifnet::Ifaddr};
use vfs::{ReadBuf, WriteBuf};

use crate::devctl::ktests::cx;

/// The Internet checksum of `b`.
fn cksum(b: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    for c in b.chunks(2) {
        let w = u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]);
        sum += u32::from(w);
    }
    while sum > 0xffff {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    #[allow(clippy::cast_possible_truncation, reason = "folded to 16 bits")]
    !(sum as u16)
}

/// A broadcast DHCPDISCOVER from `mac` with transaction `xid`.
fn discover(mac: [u8; 6], xid: u32) -> Vec<u8> {
    let mut bootp = alloc::vec![0u8; 300];
    bootp[0] = 1; // BOOTREQUEST
    bootp[1] = 1; // Ethernet
    bootp[2] = 6;
    bootp[4..8].copy_from_slice(&xid.to_be_bytes());
    bootp[10] = 0x80; // broadcast replies
    bootp[28..34].copy_from_slice(&mac);
    bootp[236..240].copy_from_slice(&[99, 130, 83, 99]);
    bootp[240..243].copy_from_slice(&[53, 1, 1]); // DHCPDISCOVER
    bootp[243] = 255;
    let udp_len = 8 + bootp.len();
    let ip_len = 20 + udp_len;
    let mut f = Vec::with_capacity(14 + ip_len);
    f.extend_from_slice(&[0xff; 6]);
    f.extend_from_slice(&mac);
    f.extend_from_slice(&0x0800u16.to_be_bytes());
    let mut ip = [0u8; 20];
    ip[0] = 0x45;
    ip[2..4].copy_from_slice(&u16::try_from(ip_len).unwrap_or(0).to_be_bytes());
    ip[8] = 64;
    ip[9] = 17;
    ip[16..20].copy_from_slice(&[255; 4]);
    let c = cksum(&ip);
    ip[10..12].copy_from_slice(&c.to_be_bytes());
    f.extend_from_slice(&ip);
    f.extend_from_slice(&68u16.to_be_bytes());
    f.extend_from_slice(&67u16.to_be_bytes());
    f.extend_from_slice(&u16::try_from(udp_len).unwrap_or(0).to_be_bytes());
    f.extend_from_slice(&[0, 0]);
    f.extend_from_slice(&bootp);
    f
}

/// The address a DHCP offer for `xid` makes, from the frames in a read.
fn offer(buf: &[u8], len: usize, xid: u32) -> Option<[u8; 4]> {
    let mut at = 0;
    while at + 26 <= len {
        let caplen = u32::from_ne_bytes(buf[at + 16..at + 20].try_into().ok()?) as usize;
        let hdrlen = usize::from(u16::from_ne_bytes(buf[at + 24..at + 26].try_into().ok()?));
        let f = buf.get(at + hdrlen..at + hdrlen + caplen)?;
        at = abi::bpf::wordalign(at + hdrlen + caplen);
        // IPv4, UDP to port 68, a BOOTREPLY for this transaction.
        let ihl = usize::from(f.get(14)? & 0xf) * 4;
        let udp = 14 + ihl;
        let bootp = udp + 8;
        if f.get(12..14)? == [8, 0]
            && *f.get(23)? == 17
            && f.get(udp + 2..udp + 4)? == 68u16.to_be_bytes()
            && *f.get(bootp)? == 2
            && f.get(bootp + 4..bootp + 8)? == xid.to_be_bytes()
        {
            return f.get(bootp + 16..bootp + 20)?.try_into().ok();
        }
    }
    None
}

ktest! {
    fn vtnet_carries_dhcp_and_arp() {
        let s = cx();
        let v = net::vnet0();
        let Some((index, flags, mac)) = v
            .net
            .lock()
            .by_name(b"vtnet0")
            .map(|i| (i.index, i.flags, i.lladdr.unwrap_or_default()))
        else {
            kprintln!("ktest: no vtnet0; the network test is skipped");
            return;
        };
        v.net.lock().set_flags(b"vtnet0", flags | iff::UP).unwrap();
        let f = v.net.lock().by_index(index).unwrap().flags;
        assert_eq!(f & (iff::UP | iff::RUNNING), iff::UP | iff::RUNNING);

        // DHCP through `/dev/bpf`, as `dhclient` uses it: a frame
        // written, the offer read back by a read that sleeps for it.
        let dev = crate::devctl::ktests::open(&s, b"bpf").unwrap();
        let mut ifr = [0u8; 32];
        ifr[..6].copy_from_slice(b"vtnet0");
        dev.ioctl(&s, bioc::SETIF, &mut ifr).unwrap();
        let mut one = 1u32.to_ne_bytes();
        dev.ioctl(&s, bioc::IMMEDIATE, &mut one).unwrap();
        let mut w = [0u8; 4];
        dev.ioctl(&s, bioc::VERSION, &mut w).unwrap();
        assert_eq!(w, [1, 0, 1, 0]);
        dev.ioctl(&s, bioc::GDLT, &mut w).unwrap();
        assert_eq!(u32::from_ne_bytes(w), abi::bpf::dlt::EN10MB);
        dev.ioctl(&s, bioc::GBLEN, &mut w).unwrap();
        let blen = u32::from_ne_bytes(w) as usize;
        assert_eq!(blen, 4096);
        // Three seconds at most per read.
        let mut tv = [0u8; 16];
        tv[..8].copy_from_slice(&3i64.to_ne_bytes());
        dev.ioctl(&s, bioc::SRTIMEOUT, &mut tv).unwrap();
        let xid = 0x4142_4344;
        let frame = discover(mac, xid);
        let n = dev.write(&s, &mut WriteBuf::new(&frame), 0, false).unwrap();
        assert_eq!(n, frame.len());
        let mut got = None;
        let mut buf = alloc::vec![0u8; blen];
        for _ in 0..5 {
            let mut r = ReadBuf::new(&mut buf);
            let n = dev.read(&s, &mut r, 0, false).unwrap();
            if n == 0 {
                break;
            }
            got = offer(r.filled(), n, xid);
            if got.is_some() {
                break;
            }
        }
        assert_eq!(got, Some([10, 0, 2, 15]), "no DHCP offer");
        let mut st = [0u8; 8];
        dev.ioctl(&s, bioc::GSTATS, &mut st).unwrap();
        let (recv, dropped) = (
            u32::from_ne_bytes(st[..4].try_into().unwrap()),
            u32::from_ne_bytes(st[4..].try_into().unwrap()),
        );
        assert!(recv >= 2 && dropped == 0, "bpf saw {recv}, dropped {dropped}");
        // Nothing more: a read that does not wait says so.
        let mut r = ReadBuf::new(&mut buf);
        assert_eq!(dev.read(&s, &mut r, 0, true), Err(abi::errno::Errno::EAGAIN));
        drop(dev);

        // ARP: with the offered address, the router resolves.
        let a = Ifaddr {
            family: Family::Inet,
            addr: net::addr::from_v4([10, 0, 2, 15]),
            plen: 24,
            dst: None,
            broadcast: Some(net::addr::from_v4([10, 0, 2, 255])),
            flags6: 0,
            vltime: u32::MAX,
            pltime: u32::MAX,
            vhid: 0,
        };
        v.net.lock().add_addr(b"vtnet0", a).unwrap();
        let now = super::uptime_s();
        let r = net::arp::resolve(&mut v.net.lock(), index, false, None, [10, 0, 2, 2], now);
        assert_eq!(r, Ok(net::arp::Resolved::Held));
        let router = net::addr::from_v4([10, 0, 2, 2]);
        let mut resolved = false;
        for _ in 0..300 {
            if v.net
                .lock()
                .lle(index, Family::Inet, &router)
                .is_some_and(|e| e.flags & net::lltable::lle::VALID != 0)
            {
                resolved = true;
                break;
            }
            crate::time::sleep_ns(&s, 10_000_000);
        }
        assert!(resolved, "the router did not answer ARP");
        let st = v.net.lock().arpstat;
        assert!(st.rxreplies >= 1 && st.txrequests >= 2);
        // As it was: no address, down.
        v.net
            .lock()
            .delete_addr(b"vtnet0", Family::Inet, None)
            .unwrap();
        v.net.lock().set_flags(b"vtnet0", flags & !iff::UP).unwrap();
        let st = v.net.lock().by_index(index).map(|i| Arc::clone(&i.stats)).unwrap();
        assert!(st.ipackets.load(Ordering::Relaxed) >= 2);
        assert!(st.opackets.load(Ordering::Relaxed) >= 3);
    }
}
