// SPDX-License-Identifier: BSD-2-Clause
//! The `net` sysctls of the loopback stack: the tunables `rc` and the
//! daemons read and set (`net.inet.ip.portrange.*`, `net.inet6.ip6.v6only`,
//! `net.inet.tcp.*space`, ...), the PCB lists `netstat` and `sockstat`
//! read, the routing sysctl (`net.routetable`: interface lists and the
//! routing table), the interface count (`net.link.generic.system.ifcount`),
//! and `kern.features.inet` and `inet6`.
//!
//! Nodes that exist on FreeBSD but mean nothing here (forwarding, ICMP
//! policy, logging) are there with FreeBSD's defaults, settable, without
//! effect.
//!
//! Behavior reference: sys/netinet/in_pcb.c (`net.inet.ip.portrange`),
//! sys/netinet/ip_input.c, sys/netinet/tcp_subr.c, sys/netinet/tcp_usrreq.c,
//! sys/netinet/udp_usrreq.c, sys/netinet6/in6_proto.c, sys/net/rtsock.c
//! (`sysctl_rtsock`), sys/net/if_mib.c (`net.link.generic`).

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};

use abi::errno::Errno;
use abi::net::net_rt;
use inet::tune;
use sched::Sleepable;
use sysctl::{Number, ReadError, Tree, Value};

/// An `int` tunable that sets `a` when `ok` allows the value.
fn rw_i32(
    t: &mut Tree,
    parent: sysctl::NodeId,
    name: &str,
    descr: &str,
    a: &'static AtomicI32,
    ok: fn(i32) -> bool,
) {
    t.add_writable(
        parent,
        Number::Auto,
        name,
        descr,
        0,
        // Relaxed: a tunable.
        Value::Int(Box::new(move || a.load(Ordering::Relaxed))),
        sysctl::int_writer(move |v| {
            if !ok(v) {
                return Err(Errno::EINVAL);
            }
            a.store(v, Ordering::Relaxed);
            Ok(())
        }),
    )
    .expect("a net sysctl");
}

/// A `u_int` tunable (seconds and milliseconds alike).
fn rw_u32(t: &mut Tree, parent: sysctl::NodeId, name: &str, descr: &str, a: &'static AtomicU32) {
    t.add_writable(
        parent,
        Number::Auto,
        name,
        descr,
        0,
        Value::Int(Box::new(move || {
            i32::try_from(a.load(Ordering::Relaxed)).unwrap_or(i32::MAX)
        })),
        sysctl::int_writer(move |v| {
            if v < 0 {
                return Err(Errno::EINVAL);
            }
            a.store(v.cast_unsigned(), Ordering::Relaxed);
            Ok(())
        }),
    )
    .expect("a net sysctl");
}

/// A `u_long` tunable.
fn rw_u64(t: &mut Tree, parent: sysctl::NodeId, name: &str, descr: &str, a: &'static AtomicU64) {
    t.add_writable(
        parent,
        Number::Auto,
        name,
        descr,
        0,
        Value::Ulong(Box::new(move || a.load(Ordering::Relaxed))),
        Arc::new(move |b: &[u8]| {
            let v = match b.len() {
                8 => u64::from_ne_bytes(b.try_into().map_err(|_| Errno::EINVAL)?),
                4 => u64::from(u32::from_ne_bytes(b.try_into().map_err(|_| Errno::EINVAL)?)),
                _ => return Err(Errno::EINVAL),
            };
            if v == 0 {
                return Err(Errno::EINVAL);
            }
            a.store(v, Ordering::Relaxed);
            Ok(())
        }),
    )
    .expect("a net sysctl");
}

/// Settable `int`s with no effect here, at FreeBSD's defaults.
fn knobs(t: &mut Tree, parent: sysctl::NodeId, list: &[(&str, &str, i32)]) {
    for &(name, descr, v) in list {
        let cell: &'static AtomicI32 = Box::leak(Box::new(AtomicI32::new(v)));
        rw_i32(t, parent, name, descr, cell, |_| true);
    }
}

fn node(
    t: &mut Tree,
    parent: sysctl::NodeId,
    number: i32,
    name: &str,
    descr: &str,
) -> sysctl::NodeId {
    match t.child(parent, name) {
        Some(n) => n,
        None => t
            .add_node(parent, Number::Fixed(number), name, descr)
            .expect("a net node"),
    }
}

/// Adds the nodes.
pub(crate) fn init(s: &Sleepable) {
    crate::sysctl::with(s, |t| {
        let root = t.root();
        let net = t.child(root, "net").expect("net node");
        t.add_value(
            net,
            Number::Auto,
            "fibs",
            "Number of available FIBs",
            Value::Int(Box::new(|| {
                i32::try_from(net::numfibs()).unwrap_or(i32::MAX)
            })),
        )
        .expect("net.fibs");
        t.add_value(
            net,
            Number::Auto,
            "my_fibnum",
            "default FIB of caller",
            Value::Int(Box::new(|| {
                crate::proc::Process::current().map_or(0, |p| {
                    i32::from(p.fib.load(core::sync::atomic::Ordering::Relaxed))
                })
            })),
        )
        .expect("net.my_fibnum");
        knobs(t, net, &[("add_addr_allfibs", "", 0)]);

        // `net.inet` (`PF_INET`).
        let inet = node(t, net, 2, "inet", "Internet Family");
        let ip = node(t, inet, 0, "ip", "IP");
        let pr = t
            .add_node(ip, Number::Auto, "portrange", "IP Ports")
            .expect("net.inet.ip.portrange");
        let low = |v: i32| (1..1024).contains(&v);
        let high = |v: i32| (1024..=65535).contains(&v);
        rw_i32(t, pr, "lowfirst", "", &tune::LOWFIRST, low);
        rw_i32(t, pr, "lowlast", "", &tune::LOWLAST, low);
        rw_i32(t, pr, "first", "", &tune::FIRST, high);
        rw_i32(t, pr, "last", "", &tune::LAST, high);
        rw_i32(t, pr, "hifirst", "", &tune::HIFIRST, high);
        rw_i32(t, pr, "hilast", "", &tune::HILAST, high);
        rw_i32(t, pr, "reservedhigh", "", &tune::RESERVEDHIGH, |v| {
            (0..=65535).contains(&v)
        });
        rw_i32(t, pr, "reservedlow", "", &tune::RESERVEDLOW, |v| {
            (0..=65535).contains(&v)
        });
        rw_i32(
            t,
            pr,
            "randomized",
            "Enable random port allocation",
            &tune::RANDOMIZED,
            |_| true,
        );
        rw_i32(
            t,
            ip,
            "ttl",
            "Maximum TTL on IP packets",
            &tune::IP_TTL,
            |v| (0..=255).contains(&v),
        );
        knobs(
            t,
            ip,
            &[
                ("forwarding", "Enable IP forwarding between interfaces", 0),
                ("redirect", "Enable sending IP redirects", 1),
                (
                    "sourceroute",
                    "Enable forwarding source routed IP packets",
                    0,
                ),
                (
                    "accept_sourceroute",
                    "Enable accepting source routed IP packets",
                    0,
                ),
                (
                    "connect_inaddr_wild",
                    "Allow connecting to INADDR_ANY or INADDR_BROADCAST for connect(2)",
                    0,
                ),
                ("random_id", "Assign random ip_id values", 0),
                (
                    "check_interface",
                    "Verify packet arrives on correct interface",
                    0,
                ),
                (
                    "rfc1122_strong_es",
                    "Packet's IP destination address must match address on arrival interface",
                    0,
                ),
            ],
        );
        let icmp = node(t, inet, 1, "icmp", "ICMP");
        knobs(
            t,
            icmp,
            &[
                ("bmcastecho", "", 0),
                ("drop_redirect", "Ignore ICMP redirects", 0),
                ("log_redirect", "Log ICMP redirects to the console", 0),
                (
                    "icmplim",
                    "Maximum number of ICMP responses per second",
                    200,
                ),
                ("maskrepl", "Reply to ICMP Address Mask Request packets", 0),
            ],
        );
        let tcp = node(t, inet, 6, "tcp", "TCP");
        rw_u64(
            t,
            tcp,
            "sendspace",
            "Initial send socket buffer size",
            &tune::TCP_SENDSPACE,
        );
        rw_u64(
            t,
            tcp,
            "recvspace",
            "Initial receive socket buffer size",
            &tune::TCP_RECVSPACE,
        );
        rw_u32(
            t,
            tcp,
            "mssdflt",
            "Default TCP Maximum Segment Size",
            &tune::TCP_MSSDFLT,
        );
        rw_u32(
            t,
            tcp,
            "v6mssdflt",
            "Default TCP Maximum Segment Size for IPv6",
            &tune::TCP_V6MSSDFLT,
        );
        rw_u32(t, tcp, "msl", "Maximum segment lifetime", &tune::TCP_MSL);
        rw_u32(
            t,
            tcp,
            "msl_local",
            "Maximum segment lifetime for local communication",
            &tune::TCP_MSL_LOCAL,
        );
        rw_u32(
            t,
            tcp,
            "keepinit",
            "time to establish connection",
            &tune::TCP_KEEPINIT,
        );
        rw_u32(
            t,
            tcp,
            "keepidle",
            "time before keepalive probes begin",
            &tune::TCP_KEEPIDLE,
        );
        rw_u32(
            t,
            tcp,
            "keepintvl",
            "time between keepalive probes",
            &tune::TCP_KEEPINTVL,
        );
        rw_u32(
            t,
            tcp,
            "keepcnt",
            "Number of keepalive probes to send",
            &tune::TCP_KEEPCNT,
        );
        knobs(
            t,
            tcp,
            &[
                (
                    "always_keepalive",
                    "Assume SO_KEEPALIVE on all TCP connections",
                    1,
                ),
                (
                    "log_in_vain",
                    "Log all incoming TCP segments to closed ports",
                    0,
                ),
                ("drop_synfin", "Drop TCP packets with SYN+FIN set", 0),
                (
                    "rfc1323",
                    "Enable rfc1323 (high performance TCP) extensions",
                    1,
                ),
                (
                    "blackhole",
                    "Do not send RST on segments to closed ports",
                    0,
                ),
                (
                    "delayed_ack",
                    "Delay ACK to try and piggyback it onto a data packet",
                    1,
                ),
                ("tolerate_missing_ts", "Tolerate missing TCP timestamps", 1),
            ],
        );
        t.add_value(
            tcp,
            Number::Auto,
            "pcblist",
            "List of active TCP connections",
            Value::Opaque {
                format: "S,xtcpcb",
                read: Box::new(|out| out.extend(inet::export::tcp_pcblist(&|_| true))),
            },
        )
        .expect("net.inet.tcp.pcblist");
        t.add_value(
            tcp,
            Number::Auto,
            "pcbcount",
            "Number of active PCBs",
            Value::Uint(Box::new(|| {
                u32::try_from(inet::export::count(inet::Proto::Tcp)).unwrap_or(u32::MAX)
            })),
        )
        .expect("net.inet.tcp.pcbcount");
        let cc = t
            .add_node(
                tcp,
                Number::Auto,
                "cc",
                "Congestion control related settings",
            )
            .expect("net.inet.tcp.cc");
        t.add_value(
            cc,
            Number::Auto,
            "algorithm",
            "Default congestion control algorithm",
            Value::text("cubic"),
        )
        .expect("net.inet.tcp.cc.algorithm");
        t.add_value(
            cc,
            Number::Auto,
            "available",
            "List available congestion control algorithms",
            Value::text(
                "CCmod           D PCB count\nnewreno            0\ncubic           *  0\n",
            ),
        )
        .expect("net.inet.tcp.cc.available");
        t.add_value(
            tcp,
            Number::Auto,
            "functions_default",
            "Set/get the default TCP functions",
            Value::text("freebsd"),
        )
        .expect("net.inet.tcp.functions_default");
        let udp = node(t, inet, 17, "udp", "UDP");
        rw_u64(
            t,
            udp,
            "maxdgram",
            "Maximum outgoing UDP datagram size",
            &tune::UDP_SENDSPACE,
        );
        rw_u64(
            t,
            udp,
            "recvspace",
            "Maximum space for incoming UDP datagrams",
            &tune::UDP_RECVSPACE,
        );
        knobs(
            t,
            udp,
            &[
                ("log_in_vain", "Log all incoming UDP packets", 0),
                (
                    "blackhole",
                    "Do not send port unreachables for refused connects",
                    0,
                ),
                ("checksum", "compute udp checksum", 1),
            ],
        );
        t.add_value(
            udp,
            Number::Auto,
            "pcblist",
            "List of active UDP sockets",
            Value::Opaque {
                format: "S,xinpcb",
                read: Box::new(|out| out.extend(inet::export::udp_pcblist(&|_| true))),
            },
        )
        .expect("net.inet.udp.pcblist");

        // `net.inet6` (`PF_INET6`).
        let inet6 = node(t, net, 28, "inet6", "Internet6 Family");
        let ip6 = node(t, inet6, 41, "ip6", "IP6");
        rw_i32(
            t,
            ip6,
            "v6only",
            "Restrict AF_INET6 sockets to IPv6 addresses only",
            &tune::V6ONLY,
            |_| true,
        );
        rw_i32(
            t,
            ip6,
            "hlim",
            "Default hop limit to use for outgoing IPv6 packets",
            &tune::IP6_HLIM,
            |v| (0..=255).contains(&v),
        );
        knobs(
            t,
            ip6,
            &[
                (
                    "forwarding",
                    "Enable forwarding of IPv6 packets between interfaces",
                    0,
                ),
                (
                    "redirect",
                    "Send ICMPv6 redirects for unforwardable IPv6 packets",
                    1,
                ),
                (
                    "accept_rtadv",
                    "Default value of per-interface flag for accepting ICMPv6 RA messages",
                    0,
                ),
                (
                    "no_radr",
                    "Default value of per-interface flag to control whether routers sending ICMPv6 RA messages on that interface are added into the default router list",
                    0,
                ),
                (
                    "rfc6204w3",
                    "Accept the default router list from ICMPv6 RA messages even when packet forwarding is enabled",
                    0,
                ),
                (
                    "dad_count",
                    "Number of ICMPv6 NS messages sent during duplicate address detection",
                    1,
                ),
                (
                    "use_tempaddr",
                    "Create RFC3041 temporary addresses for autoconfigured addresses",
                    0,
                ),
                (
                    "prefer_tempaddr",
                    "Prefer RFC3041 temporary addresses in source address selection",
                    0,
                ),
                (
                    "use_defaultzone",
                    "Use the default scope zone when none is specified",
                    0,
                ),
                (
                    "auto_linklocal",
                    "Default value of per-interface flag for automatically adding an IPv6 link-local address to interfaces when attached",
                    1,
                ),
                (
                    "connect_in6addr_wild",
                    "Allow connecting to the unspecified address for connect(2)",
                    1,
                ),
            ],
        );
        // `ip6addrctl(8)` reads it by number.
        t.add_value(
            ip6,
            Number::Fixed(abi::net::IPV6CTL_ADDRCTLPOLICY),
            "addrctlpolicy",
            "",
            Value::Opaque {
                format: "S,in6_addrpolicy",
                read: Box::new(super::ioctl::policies),
            },
        )
        .expect("net.inet6.ip6.addrctlpolicy");
        let icmp6 = node(t, inet6, 58, "icmp6", "ICMP6");
        knobs(
            t,
            icmp6,
            &[
                ("rediraccept", "Accept ICMPv6 redirect messages", 1),
                ("nd6_onlink_ns_rfc4861", "", 0),
            ],
        );

        // `net.link`: interface counts (`if_mib.c`) and the Ethernet
        // knob `rc.d/routing` sets.
        let link = node(t, net, 18, "link", "Link layers");
        let generic = node(t, link, 0, "generic", "Generic link-management");
        let system = node(t, generic, 1, "system", "system-wide statistics");
        t.add_value(
            system,
            Number::Fixed(1),
            "ifcount",
            "Maximum known interface index",
            Value::Int(Box::new(|| {
                i32::from(
                    super::curvnet()
                        .net
                        .lock()
                        .ifaces
                        .iter()
                        .map(|i| i.index)
                        .max()
                        .unwrap_or(0),
                )
            })),
        )
        .expect("net.link.generic.system.ifcount");
        let ether = node(t, link, 6, "ether", "Ethernet");
        let ether_inet = node(t, ether, 2, "inet", "");
        knobs(
            t,
            ether_inet,
            &[
                ("proxyall", "Enable proxy ARP for all suitable requests", 0),
                ("max_age", "", 1200),
            ],
        );
        let lo = node(t, link, 24, "loif", "");
        let _ = lo;

        // `net.routetable` (`PF_ROUTE`): `sysctl_rtsock`.
        t.add_handler(
            net,
            Number::Fixed(17),
            "routetable",
            "Return route tables and interface/address lists",
            Arc::new(rtsock),
        )
        .expect("net.routetable");

        // `FEATURE(inet)`, `FEATURE(inet6)`.
        if let Some(f) = t.name2oid("kern.features").and_then(|o| t.lookup(&o)) {
            let _ = t.add_value(
                f,
                Number::Auto,
                "inet",
                "Internet Protocol version 4",
                Value::int(1),
            );
            let _ = t.add_value(
                f,
                Number::Auto,
                "inet6",
                "Internet Protocol version 6",
                Value::int(1),
            );
        }
    });
}

/// `sysctl_rtsock`: `<protocol 0>.<af>.<op>.<arg>[.<fib>]`.
fn rtsock(r: &[i32], out: &mut Vec<u8>) -> Result<(), ReadError> {
    // The protocol, then the request.
    let r = r.get(1..).ok_or(ReadError::Invalid)?;
    if r.len() < 3 {
        return Err(ReadError::Failed(Errno::EINVAL));
    }
    let op = r[1];
    if matches!(op, net_rt::DUMP | net_rt::NHOP | net_rt::NHGRP) {
        if r.len() > 4 {
            return Err(ReadError::Failed(Errno::ENOTDIR));
        }
        if r.len() == 4 && r[3] != -1 && u32::try_from(r[3]).map_or(true, |f| f >= net::numfibs()) {
            return Err(ReadError::Failed(Errno::EINVAL));
        }
    } else if r.len() != 3 {
        return Err(ReadError::Failed(Errno::ENOTDIR));
    }
    let af = u8::try_from(r[0]).map_err(|_| ReadError::Failed(Errno::EINVAL))?;
    let arg = r[2];
    // The table named (`RT_ALL_FIBS`: every one), or the caller's.
    let fib = match r.get(3) {
        Some(-1) => None,
        Some(&f) => u16::try_from(f).ok(),
        None => Some(
            crate::proc::Process::current()
                .map_or(0, |p| p.fib.load(core::sync::atomic::Ordering::Relaxed)),
        ),
    };
    let vn = super::curvnet();
    let n = vn.net.lock();
    match op {
        net_rt::DUMP | net_rt::FLAGS => {
            if op == net_rt::FLAGS {
                // The ARP and ND caches: loopback has none.
                return Ok(());
            }
            if af != 0 && af != abi::inet::AF_INET && af != abi::inet::AF_INET6 {
                return Err(ReadError::Failed(Errno::EAFNOSUPPORT));
            }
            out.extend(inet::rtmsg::dump(&n, af, fib));
        }
        net_rt::IFLIST | net_rt::IFLISTL => {
            out.extend(inet::rtmsg::iflist(&n, af, arg, op == net_rt::IFLISTL));
        }
        net_rt::NHOP => out.extend(inet::rtmsg::nhops(&n, af, fib)),
        // No multicast memberships or next-hop groups to list.
        net_rt::IFMALIST | net_rt::NHGRP => {}
        _ => return Err(ReadError::Failed(Errno::EINVAL)),
    }
    Ok(())
}
