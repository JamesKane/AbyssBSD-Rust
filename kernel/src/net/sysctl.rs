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
use abi::sysctl::ctlflag;
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

/// `net.link.ether.inet`'s ARP settings, each the current VNET's.
/// `net.inet.tcp`'s settings, each the current VNET's.
type Knob = (
    &'static str,
    &'static str,
    fn(&inet::state::State) -> &AtomicI32,
);
const TCP_KNOBS: &[Knob] = &[
    ("sendspace", "Initial send socket buffer size", |s| {
        &s.tcp.conf.sendspace
    }),
    ("recvspace", "Initial receive socket buffer size", |s| {
        &s.tcp.conf.recvspace
    }),
    ("mssdflt", "Default TCP Maximum Segment Size", |s| {
        &s.tcp.conf.mssdflt
    }),
    (
        "v6mssdflt",
        "Default TCP Maximum Segment Size for IPv6",
        |s| &s.tcp.conf.v6mssdflt,
    ),
    ("minmss", "Minimum TCP Maximum Segment Size", |s| {
        &s.tcp.conf.minmss
    }),
    (
        "rfc1323",
        "Enable rfc1323 (high performance TCP) extensions",
        |s| &s.tcp.conf.rfc1323,
    ),
    ("msl", "Maximum segment lifetime", |s| &s.tcp.conf.msl),
    (
        "msl_local",
        "Maximum segment lifetime for local communication",
        |s| &s.tcp.conf.msl_local,
    ),
    (
        "nolocaltimewait",
        "Do not create TCP TIME_WAIT state for local connections",
        |s| &s.tcp.conf.nolocaltimewait,
    ),
    ("keepinit", "time to establish connection", |s| {
        &s.tcp.conf.keepinit
    }),
    ("keepidle", "time before keepalive probes begin", |s| {
        &s.tcp.conf.keepidle
    }),
    ("keepintvl", "time between keepalive probes", |s| {
        &s.tcp.conf.keepintvl
    }),
    ("keepcnt", "Number of keepalive probes to send", |s| {
        &s.tcp.conf.keepcnt
    }),
    (
        "always_keepalive",
        "Assume SO_KEEPALIVE on all TCP connections",
        |s| &s.tcp.conf.always_keepalive,
    ),
    ("delacktime", "Time before a delayed ACK is sent", |s| {
        &s.tcp.conf.delacktime
    }),
    (
        "delayed_ack",
        "Delay ACK to try and piggyback it onto a data packet",
        |s| &s.tcp.conf.delayed_ack,
    ),
    ("rexmit_initial", "Initial Retransmission Timeout", |s| {
        &s.tcp.conf.rexmit_initial
    }),
    ("rexmit_min", "Minimum Retransmission Timeout", |s| {
        &s.tcp.conf.rexmit_min
    }),
    ("rexmit_max", "Maximum Retransmission Timeout", |s| {
        &s.tcp.conf.rexmit_max
    }),
    ("rexmit_slop", "Retransmission Timer Slop", |s| {
        &s.tcp.conf.rexmit_slop
    }),
    (
        "retries",
        "maximum number of consecutive timer based retransmissions",
        |s| &s.tcp.conf.retries,
    ),
    ("persmin", "minimum persistence interval", |s| {
        &s.tcp.conf.persmin
    }),
    ("persmax", "maximum persistence interval", |s| {
        &s.tcp.conf.persmax
    }),
    ("finwait2_timeout", "FIN-WAIT2 timeout", |s| {
        &s.tcp.conf.finwait2_timeout
    }),
    (
        "fast_finwait2_recycle",
        "Recycle closed FIN_WAIT_2 connections faster",
        |s| &s.tcp.conf.fast_finwait2_recycle,
    ),
    (
        "maxunacktime",
        "Maximum time (in ms) that a session can linger without making progress",
        |s| &s.tcp.conf.maxunacktime,
    ),
    ("drop_synfin", "Drop TCP packets with SYN+FIN set", |s| {
        &s.tcp.conf.drop_synfin
    }),
    (
        "blackhole",
        "Do not send RST on segments to closed ports",
        |s| &s.tcp.conf.blackhole,
    ),
    (
        "log_in_vain",
        "Log all incoming TCP segments to closed ports",
        |s| &s.tcp.conf.log_in_vain,
    ),
    (
        "tolerate_missing_ts",
        "Tolerate missing TCP timestamps",
        |s| &s.tcp.conf.tolerate_missing_ts,
    ),
    (
        "ts_offset_per_conn",
        "Initialize TCP timestamps per connection instead of per host pair",
        |s| &s.tcp.conf.ts_offset_per_conn,
    ),
    (
        "insecure_rst",
        "Follow RFC793 instead of RFC5961 criteria for accepting RST packets",
        |s| &s.tcp.conf.insecure_rst,
    ),
    (
        "insecure_syn",
        "Follow RFC793 instead of RFC5961 criteria for accepting SYN packets",
        |s| &s.tcp.conf.insecure_syn,
    ),
    (
        "insecure_ack",
        "Follow RFC793 criteria for validating SEG.ACK",
        |s| &s.tcp.conf.insecure_ack,
    ),
    (
        "icmp_may_rst",
        "Certain ICMP unreachable messages may abort connections in SYN_SENT",
        |s| &s.tcp.conf.icmp_may_rst,
    ),
    (
        "ack_war_timewindow",
        "Time interval in ms used to limit the number (ack_war_cnt) of challenge ACKs sent per TCP connection",
        |s| &s.tcp.conf.ack_war_timewindow,
    ),
    (
        "ack_war_cnt",
        "Maximum number of challenge ACKs sent per TCP connection during the time interval (ack_war_timewindow)",
        |s| &s.tcp.conf.ack_war_cnt,
    ),
    (
        "syncookies",
        "Use TCP SYN cookies if the syncache overflows",
        |s| &s.tcp.conf.syncookies,
    ),
    ("syncookies_only", "Use only TCP SYN cookies", |s| {
        &s.tcp.conf.syncookies_only
    }),
    ("path_mtu_discovery", "Enable Path MTU Discovery", |s| {
        &s.tcp.conf.path_mtu_discovery
    }),
    (
        "pmtud_blackhole_detection",
        "Path MTU Discovery Black Hole Detection Enabled",
        |s| &s.tcp.conf.pmtud_blackhole_detect,
    ),
    (
        "pmtud_blackhole_mss",
        "Path MTU Discovery Black Hole Detection lowered MSS",
        |s| &s.tcp.conf.pmtud_blackhole_mss,
    ),
    (
        "v6pmtud_blackhole_mss",
        "Path MTU Discovery IPv6 Black Hole Detection lowered MSS",
        |s| &s.tcp.conf.v6pmtud_blackhole_mss,
    ),
    ("tso", "Enable TCP Segmentation Offload", |s| {
        &s.tcp.conf.tso
    }),
    ("sendbuf_auto", "Enable automatic send buffer sizing", |s| {
        &s.tcp.conf.sendbuf_auto
    }),
    (
        "sendbuf_inc",
        "Incrementor step size of automatic send buffer",
        |s| &s.tcp.conf.sendbuf_inc,
    ),
    ("sendbuf_max", "Max size of automatic send buffer", |s| {
        &s.tcp.conf.sendbuf_max
    }),
    (
        "sendbuf_auto_lowat",
        "Modify threshold for auto send buffer growth to account for SO_SNDLOWAT",
        |s| &s.tcp.conf.sendbuf_auto_lowat,
    ),
    (
        "recvbuf_auto",
        "Enable automatic receive buffer sizing",
        |s| &s.tcp.conf.recvbuf_auto,
    ),
    ("recvbuf_max", "Max size of automatic receive buffer", |s| {
        &s.tcp.conf.recvbuf_max
    }),
    (
        "initcwnd_segments",
        "Slow-start flight size (initial congestion window) in number of segments",
        |s| &s.tcp.conf.initcwnd_segments,
    ),
    ("rfc3042", "Enable RFC 3042 (Limited Transmit)", |s| {
        &s.tcp.conf.rfc3042
    }),
    (
        "rfc3390",
        "Enable RFC 3390 (Increasing TCP's Initial Congestion Window)",
        |s| &s.tcp.conf.rfc3390,
    ),
    (
        "rfc3465",
        "Enable RFC 3465 (Appropriate Byte Counting)",
        |s| &s.tcp.conf.rfc3465,
    ),
    (
        "abc_l_var",
        "Cap the max cwnd increment during slow-start to this number of segments",
        |s| &s.tcp.conf.abc_l_var,
    ),
    (
        "newcwv",
        "Enable New Congestion Window Validation per RFC7661",
        |s| &s.tcp.conf.newcwv,
    ),
    (
        "do_prr",
        "Enable Proportional Rate Reduction per RFC 6937",
        |s| &s.tcp.conf.do_prr,
    ),
    (
        "isn_reseed_interval",
        "Seconds between reseeding of ISN secret",
        |s| &s.tcp.conf.isn_reseed_interval,
    ),
    (
        "udp_tunneling_port",
        "Tunneling port for tcp over udp",
        |s| &s.tcp.conf.udp_tunneling_port,
    ),
];

/// An opaque per-VNET value from `s`.
fn ctlflag_vnet_opaque(format: &'static str, f: fn(&inet::state::State) -> Vec<u8>) -> Value {
    Value::Opaque {
        format,
        read: Box::new(move |out| out.extend(f(&inet::state::of_arc(&super::curvnet())))),
    }
}

/// A per-VNET internet setting (`CTLFLAG_VNET`), the current thread's
/// VNET's.
fn vknob(
    t: &mut Tree,
    parent: sysctl::NodeId,
    name: &str,
    descr: &str,
    field: fn(&inet::state::State) -> &AtomicI32,
) {
    t.add_writable(
        parent,
        Number::Auto,
        name,
        descr,
        ctlflag::VNET,
        // Relaxed: a setting.
        Value::Int(Box::new(move || {
            field(&inet::state::of_arc(&super::curvnet())).load(Ordering::Relaxed)
        })),
        sysctl::int_writer(move |v| {
            field(&inet::state::of_arc(&super::curvnet())).store(v, Ordering::Relaxed);
            Ok(())
        }),
    )
    .expect("a net.inet sysctl");
}

/// [`vknob`] for an unsigned one.
fn vknob_u32(
    t: &mut Tree,
    parent: sysctl::NodeId,
    name: &str,
    descr: &str,
    field: fn(&inet::state::State) -> &AtomicU32,
) {
    t.add_writable(
        parent,
        Number::Auto,
        name,
        descr,
        ctlflag::VNET,
        // Relaxed: a setting.
        Value::Uint(Box::new(move || {
            field(&inet::state::of_arc(&super::curvnet())).load(Ordering::Relaxed)
        })),
        sysctl::int_writer(move |v| {
            let v = u32::try_from(v).map_err(|_| Errno::EINVAL)?;
            field(&inet::state::of_arc(&super::curvnet())).store(v, Ordering::Relaxed);
            Ok(())
        }),
    )
    .expect("a net.inet sysctl");
}

/// A protocol's statistics structure (`VNET_PCPUSTAT_SYSINIT`), the
/// current VNET's.
fn vstats(
    t: &mut Tree,
    parent: sysctl::NodeId,
    format: &'static str,
    descr: &str,
    bytes: fn(&inet::state::State) -> Vec<u8>,
    clear: fn(&inet::state::State),
) {
    vstats_n(
        t,
        parent,
        Number::Auto,
        "stats",
        format,
        descr,
        bytes,
        clear,
    );
}

/// [`vstats`] with its number and name; a write zeroes them (`netstat
/// -z`, `VNET_PCPUSTAT_SYSINIT`'s handler).
#[allow(clippy::too_many_arguments, reason = "the node's parts")]
fn vstats_n(
    t: &mut Tree,
    parent: sysctl::NodeId,
    number: Number,
    name: &str,
    format: &'static str,
    descr: &str,
    bytes: fn(&inet::state::State) -> Vec<u8>,
    clear: fn(&inet::state::State),
) {
    t.add_writable(
        parent,
        number,
        name,
        descr,
        ctlflag::VNET,
        Value::Opaque {
            format,
            read: Box::new(move |out| out.extend(bytes(&inet::state::of_arc(&super::curvnet())))),
        },
        Arc::new(move |_: &[u8]| {
            clear(&inet::state::of_arc(&super::curvnet()));
            Ok(())
        }),
    )
    .expect("a net.inet stats sysctl");
}

/// [`vknob`] with a fixed number (`IPV6CTL_*`, `ICMPV6CTL_*`), which
/// `rtsold` and `ndp` ask for.
fn vknob_n(
    t: &mut Tree,
    parent: sysctl::NodeId,
    number: i32,
    name: &str,
    descr: &str,
    field: fn(&inet::state::State) -> &AtomicI32,
) {
    t.add_writable(
        parent,
        Number::Fixed(number),
        name,
        descr,
        ctlflag::VNET,
        // Relaxed: a setting.
        Value::Int(Box::new(move || {
            field(&inet::state::of_arc(&super::curvnet())).load(Ordering::Relaxed)
        })),
        sysctl::int_writer(move |v| {
            field(&inet::state::of_arc(&super::curvnet())).store(v, Ordering::Relaxed);
            Ok(())
        }),
    )
    .expect("a net.inet6 sysctl");
}

/// `bind_all_fibs` (`CTLFLAG_RDTUN`, 1): a socket hears every FIB's
/// traffic, so tests of per-FIB binding skip.
fn bind_all_fibs(t: &mut Tree, parent: sysctl::NodeId) {
    t.add_value(
        parent,
        Number::Auto,
        "bind_all_fibs",
        "Bound sockets receive traffic from all FIBs",
        Value::int(1),
    )
    .expect("a bind_all_fibs sysctl");
}

fn arp_knobs(t: &mut Tree, parent: sysctl::NodeId) {
    type Get = fn(&net::arp::Conf) -> i32;
    type Set = fn(&mut net::arp::Conf, i32) -> Result<(), Errno>;
    let list: [(&str, &str, Get, Set); 6] = [
        (
            "garp_rexmit_count",
            "Number of times to retransmit GARP packets; 0 to disable, maximum of 16",
            |c| i32::from(c.garp_rexmit_count),
            // `sysctl_garp_rexmit`: clamped, not refused.
            |c, v| {
                c.garp_rexmit_count =
                    u8::try_from(v.clamp(0, i32::from(net::arp::MAX_GARP_RETRANSMITS)))
                        .unwrap_or(0);
                Ok(())
            },
        ),
        (
            "max_age",
            "ARP entry lifetime in seconds",
            |c| i32::try_from(c.max_age).unwrap_or(i32::MAX),
            // `sysctl_netinet_arp_timers`... a plain `int` here, kept
            // positive.
            |c, v| {
                c.max_age = u64::try_from(v).map_err(|_| Errno::EINVAL)?;
                Ok(())
            },
        ),
        (
            "maxtries",
            "ARP resolution attempts before returning error",
            |c| i32::from(c.maxtries),
            |c, v| {
                c.maxtries = u16::try_from(v).map_err(|_| Errno::EINVAL)?;
                Ok(())
            },
        ),
        (
            "proxyall",
            "Enable proxy ARP for all suitable requests",
            |c| i32::from(c.proxyall),
            |c, v| {
                c.proxyall = v != 0;
                Ok(())
            },
        ),
        (
            "wait",
            "Incomplete ARP entry lifetime in seconds",
            |c| i32::try_from(c.wait).unwrap_or(i32::MAX),
            |c, v| {
                c.wait = u64::try_from(v).map_err(|_| Errno::EINVAL)?;
                Ok(())
            },
        ),
        (
            "maxhold",
            "Number of packets to hold per ARP entry",
            |c| i32::try_from(c.maxhold).unwrap_or(i32::MAX),
            |c, v| {
                c.maxhold = usize::try_from(v).map_err(|_| Errno::EINVAL)?;
                Ok(())
            },
        ),
    ];
    for (name, descr, get, set) in list {
        t.add_writable(
            parent,
            Number::Auto,
            name,
            descr,
            ctlflag::VNET,
            Value::Int(Box::new(move || get(&super::curvnet().net.lock().arp))),
            sysctl::int_writer(move |v| set(&mut super::curvnet().net.lock().arp, v)),
        )
        .expect("an ARP sysctl");
    }
}

/// `net.bpf`: buffer sizes and the program length limit, settable
/// knobs with FreeBSD's values (the descriptors use the defaults).
fn bpf_knobs(t: &mut Tree, net: sysctl::NodeId) {
    let bpf = node(t, net, 0x1000, "bpf", "bpf sysctl");
    knobs(
        t,
        bpf,
        &[
            ("bufsize", "Default capture buffer size in bytes", 4096),
            ("maxbufsize", "Maximum capture buffer in bytes", 524_288),
            ("maxinsns", "Maximum bpf program instructions", 512),
            (
                "zerocopy_enable",
                "Enable new zero-copy BPF buffer sessions",
                0,
            ),
            (
                "optimize_writers",
                "Do not send packets until BPF program is set",
                0,
            ),
        ],
    );
}

/// `net.link.tun`, `net.link.tap`, and `debug.if_tun_debug`
/// (`if_tuntap.c`).
fn tuntap_knobs(t: &mut Tree, root: sysctl::NodeId, link: sysctl::NodeId) {
    use net::tuntap;
    let tun = t
        .add_node(
            link,
            Number::Auto,
            "tun",
            "IP tunnel software network interface",
        )
        .expect("net.link.tun");
    rw_tun(
        t,
        tun,
        "devfs_cloning",
        "Enable legacy devfs interface creation",
        &tuntap::TUN_DEVFS_CLONING,
    );
    let tap = t
        .add_node(
            link,
            Number::Auto,
            "tap",
            "Ethernet tunnel software network interface",
        )
        .expect("net.link.tap");
    rw_i32(
        t,
        tap,
        "user_open",
        "Enable legacy devfs interface creation for all users",
        &tuntap::TAP_USER_OPEN,
        |_| true,
    );
    rw_i32(
        t,
        tap,
        "up_on_open",
        "Bring interface up when /dev/tap is opened",
        &tuntap::TAP_UP_ON_OPEN,
        |_| true,
    );
    rw_tun(
        t,
        tap,
        "devfs_cloning",
        "Enable legacy devfs interface creation",
        &tuntap::TAP_DEVFS_CLONING,
    );
    rw_i32(t, tap, "debug", "", &tuntap::DEBUG, |_| true);
    let debug = t
        .child(root, "debug")
        .or_else(|| t.add_node(root, Number::Auto, "debug", "Debugging").ok());
    if let Some(d) = debug {
        rw_i32(t, d, "if_tun_debug", "", &tuntap::DEBUG, |_| true);
    }
}

/// A `CTLFLAG_RWTUN` `int`.
fn rw_tun(t: &mut Tree, parent: sysctl::NodeId, name: &str, descr: &str, a: &'static AtomicI32) {
    t.add_writable(
        parent,
        Number::Auto,
        name,
        descr,
        ctlflag::TUN,
        // Relaxed: a tunable.
        Value::Int(Box::new(move || a.load(Ordering::Relaxed))),
        sysctl::int_writer(move |v| {
            a.store(v, Ordering::Relaxed);
            Ok(())
        }),
    )
    .expect("a net.link sysctl");
}

/// `net.link.bridge` (`if_bridge.c`): each the current VNET's.
fn bridge_knobs(t: &mut Tree, link: sysctl::NodeId) {
    type Field = fn(&net::bridge::Conf) -> &AtomicI32;
    let bridge = node(t, link, i32::from(abi::net::IFT_BRIDGE), "bridge", "Bridge");
    let knobs: [(&str, &str, u32, bool, Field); 11] = [
        (
            "pfil_onlyip",
            "Only pass IP packets when pfil is enabled",
            ctlflag::TUN,
            false,
            |c| &c.pfil_onlyip,
        ),
        (
            "pfil_bridge",
            "Packet filter on the bridge interface",
            ctlflag::TUN,
            false,
            |c| &c.pfil_bridge,
        ),
        (
            "ipfw_arp",
            "Filter ARP packets through IPFW layer2",
            ctlflag::TUN,
            false,
            |c| &c.ipfw_arp,
        ),
        (
            "pfil_member",
            "Packet filter on the member interface",
            ctlflag::TUN,
            false,
            |c| &c.pfil_member,
        ),
        (
            "pfil_local_phys",
            "Packet filter on the physical interface for locally destined packets",
            ctlflag::TUN,
            false,
            |c| &c.pfil_local_phys,
        ),
        (
            "log_stp",
            "Log STP state changes",
            ctlflag::TUN,
            false,
            |c| &c.log_stp,
        ),
        (
            "inherit_mac",
            "Inherit MAC address from the first bridge member",
            ctlflag::TUN,
            false,
            |c| &c.inherit_mac,
        ),
        (
            "allow_llz_overlap",
            "Allow overlap of link-local scope zones of a bridge interface and the member interfaces",
            0,
            false,
            |c| &c.allow_llz_overlap,
        ),
        (
            "log_mac_flap",
            "Log MAC address port flapping",
            0,
            true,
            |c| &c.log_mac_flap,
        ),
        (
            "member_ifaddrs",
            "Allow layer 3 addresses on bridge members (deprecated)",
            0,
            true,
            |c| &c.member_ifaddrs,
        ),
        ("ipfw", "Layer2 filter with IPFW", 0, false, |c| &c.ipfw),
    ];
    for (name, descr, flags, boolean, field) in knobs {
        let value = if boolean {
            // `SYSCTL_BOOL`.
            Value::U8(Box::new(move || {
                u8::from(field(&super::curvnet().bridge).load(Ordering::Relaxed) != 0)
            }))
        } else {
            Value::Int(Box::new(move || {
                field(&super::curvnet().bridge).load(Ordering::Relaxed)
            }))
        };
        let writer = if boolean {
            sysctl::u8_writer(move |v| {
                // Relaxed: a setting.
                field(&super::curvnet().bridge).store(i32::from(v != 0), Ordering::Relaxed);
                Ok(())
            })
        } else {
            sysctl::int_writer(move |v| {
                let vn = super::curvnet();
                let c = &vn.bridge;
                // Relaxed: settings.
                if name == "ipfw" {
                    // `sysctl_pfil_ipfw`: on, it takes over from pfil.
                    let on = v & 1;
                    if on != c.ipfw.load(Ordering::Relaxed) {
                        c.ipfw.store(on, Ordering::Relaxed);
                        if on != 0 {
                            c.pfil_onlyip.store(0, Ordering::Relaxed);
                            c.pfil_bridge.store(0, Ordering::Relaxed);
                            c.pfil_member.store(0, Ordering::Relaxed);
                        }
                    }
                    return Ok(());
                }
                field(c).store(v, Ordering::Relaxed);
                Ok(())
            })
        };
        t.add_writable(
            bridge,
            Number::Auto,
            name,
            descr,
            ctlflag::VNET | flags,
            value,
            writer,
        )
        .expect("a net.link.bridge sysctl");
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
        // `sysctl_fibs`: tables may be added, never taken away.
        t.add_writable(
            net,
            Number::Auto,
            "fibs",
            "set number of fibs",
            ctlflag::VNET | ctlflag::TUN,
            Value::Uint(Box::new(net::numfibs)),
            sysctl::int_writer(|v| {
                let n = u32::try_from(v).unwrap_or(0).clamp(1, net::MAXFIBS);
                if n < net::numfibs() {
                    return Err(Errno::ENOTCAPABLE);
                }
                net::set_numfibs(n);
                Ok(())
            }),
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
        vknob(
            t,
            ip,
            "forwarding",
            "Enable IP forwarding between interfaces",
            |s| &s.conf.forwarding,
        );
        vknob(t, ip, "redirect", "Enable sending IP redirects", |s| {
            &s.conf.redirect
        });
        vknob(t, ip, "random_id", "Assign random ip_id values", |s| {
            &s.conf.random_id
        });
        vknob(
            t,
            ip,
            "rfc6864",
            "Use constant IP ID for atomic datagrams",
            |s| &s.conf.rfc6864,
        );
        vknob(
            t,
            ip,
            "rfc1122_strong_es",
            "Packet's IP destination address must match address on arrival interface",
            |s| &s.conf.strong_es,
        );
        vknob(
            t,
            ip,
            "source_address_validation",
            "Drop incoming packets with source address that is a local address",
            |s| &s.conf.sav,
        );
        vknob(
            t,
            ip,
            "maxfragsperpacket",
            "Maximum number of IPv4 fragments allowed per packet",
            |s| &s.conf.maxfragsperpacket,
        );
        vknob(
            t,
            ip,
            "fragttl",
            "IP fragment life time on reassembly queue (seconds)",
            |s| &s.conf.fragttl,
        );
        vstats(
            t,
            ip,
            "S,ipstat",
            "IP statistics (struct ipstat, netinet/ip_var.h)",
            |s| s.ipstat.bytes(),
            |s| s.ipstat.clear(),
        );
        knobs(
            t,
            ip,
            &[
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
                (
                    "check_interface",
                    "Verify packet arrives on correct interface",
                    0,
                ),
            ],
        );
        let icmp = node(t, inet, 1, "icmp", "ICMP");
        vknob(
            t,
            icmp,
            "bmcastecho",
            "Reply to multicast ICMP Echo Request and Timestamp packets",
            |s| &s.conf.bmcastecho,
        );
        vknob(t, icmp, "drop_redirect", "Ignore ICMP redirects", |s| {
            &s.conf.drop_redirect
        });
        vknob(
            t,
            icmp,
            "log_redirect",
            "Log ICMP redirects to the console",
            |s| &s.conf.log_redirect,
        );
        vknob_u32(
            t,
            icmp,
            "icmplim",
            "Maximum number of ICMP responses per second",
            |s| &s.conf.icmplim,
        );
        vknob_u32(
            t,
            icmp,
            "icmplim_jitter",
            "Random icmplim jitter adjustment limit",
            |s| &s.conf.icmplim_jitter,
        );
        vknob(
            t,
            icmp,
            "icmplim_output",
            "Enable logging of ICMP response rate limiting",
            |s| &s.conf.icmplim_output,
        );
        vknob(
            t,
            icmp,
            "maskrepl",
            "Reply to ICMP Address Mask Request packets",
            |s| &s.conf.maskrepl,
        );
        vknob(
            t,
            icmp,
            "tstamprepl",
            "Respond to ICMP Timestamp packets",
            |s| &s.conf.tstamprepl,
        );
        vknob(
            t,
            icmp,
            "quotelen",
            "Number of bytes from original packet to quote in ICMP reply",
            |s| &s.conf.quotelen,
        );
        vknob(
            t,
            icmp,
            "reply_from_interface",
            "ICMP reply from incoming interface for non-local packets",
            |s| &s.conf.rfi,
        );
        vstats(
            t,
            icmp,
            "S,icmpstat",
            "ICMP statistics (struct icmpstat, netinet/icmp_var.h)",
            |s| s.icmpstat.bytes(),
            |s| s.icmpstat.clear(),
        );
        let igmp = node(t, inet, 2, "igmp", "IGMP");
        vknob(
            t,
            igmp,
            "sendra",
            "Send IP Router Alert option in IGMPv2/v3 messages",
            |s| &s.conf.igmp_sendra,
        );
        vknob(
            t,
            igmp,
            "sendlocal",
            "Send IGMP membership reports for 224.0.0.0/24 groups",
            |s| &s.conf.igmp_sendlocal,
        );
        vknob(
            t,
            igmp,
            "default_version",
            "Default version of IGMP to run on each interface",
            |s| &s.conf.igmp_version,
        );
        knobs(
            t,
            igmp,
            &[
                (
                    "recvifkludge",
                    "Rewrite IGMPv1/v2 reports from 0.0.0.0 to contain subnet address",
                    1,
                ),
                ("v1enable", "Enable backwards compatibility with IGMPv1", 1),
                ("v2enable", "Enable backwards compatibility with IGMPv2", 1),
                (
                    "legacysupp",
                    "Allow v1/v2 reports to suppress v3 group responses",
                    0,
                ),
                (
                    "gsrdelay",
                    "Rate limit for IGMPv3 Group-and-Source queries in seconds",
                    10,
                ),
            ],
        );
        let tcp = node(t, inet, 6, "tcp", "TCP");
        bind_all_fibs(t, tcp);
        for &(name, descr, f) in TCP_KNOBS {
            vknob(t, tcp, name, descr, f);
        }
        vstats(
            t,
            tcp,
            "S,tcpstat",
            "TCP statistics (struct tcpstat, netinet/tcp_var.h)",
            |s| {
                s.tcp
                    .stats
                    .snapshot()
                    .iter()
                    .flat_map(|w| w.to_ne_bytes())
                    .collect()
            },
            |s| s.tcp.stats.clear(),
        );
        t.add_value(
            tcp,
            Number::Auto,
            "states",
            "TCP connection counts by TCP state",
            ctlflag_vnet_opaque("QU", |s| {
                s.tcp
                    .stats
                    .states()
                    .iter()
                    .flat_map(|w| w.to_ne_bytes())
                    .collect()
            }),
        )
        .expect("net.inet.tcp.states");
        let sack = t
            .add_node(tcp, Number::Auto, "sack", "TCP SACK")
            .expect("net.inet.tcp.sack");
        vknob(t, sack, "enable", "Enable/Disable TCP SACK support", |s| {
            &s.tcp.conf.sack_enable
        });
        vknob(
            t,
            sack,
            "maxholes",
            "Maximum number of TCP SACK holes allowed per connection",
            |s| &s.tcp.conf.sack_maxholes,
        );
        vknob(
            t,
            sack,
            "globalmaxholes",
            "Global maximum number of TCP SACK holes",
            |s| &s.tcp.conf.sack_globalmaxholes,
        );
        t.add_value(
            sack,
            Number::Auto,
            "globalholes",
            "Global number of TCP SACK holes currently allocated",
            Value::Int(Box::new(|| {
                inet::state::of_arc(&super::curvnet())
                    .tcp
                    .stats
                    .sack_globalholes
                    .load(Ordering::Relaxed)
            })),
        )
        .expect("net.inet.tcp.sack.globalholes");
        vknob(
            t,
            sack,
            "lrd",
            "Perform Lost Retransmission Detection",
            |s| &s.tcp.conf.do_lrd,
        );
        let ecn = t
            .add_node(tcp, Number::Auto, "ecn", "TCP ECN")
            .expect("net.inet.tcp.ecn");
        vknob(t, ecn, "enable", "TCP ECN support", |s| {
            &s.tcp.conf.ecn_enable
        });
        vknob(
            t,
            ecn,
            "maxretries",
            "Max retries before giving up on ECN",
            |s| &s.tcp.conf.ecn_maxretries,
        );
        let hc = t
            .add_node(tcp, Number::Auto, "hostcache", "TCP Host cache")
            .expect("net.inet.tcp.hostcache");
        vknob(t, hc, "enable", "Enable the TCP hostcache", |s| {
            &s.tcp.hc.enable
        });
        vknob(
            t,
            hc,
            "expire",
            "Expire time of TCP hostcache entries",
            |s| &s.tcp.hc.expire,
        );
        vknob(t, hc, "prune", "Time between purge runs", |s| {
            &s.tcp.hc.prune
        });
        vknob_u32(
            t,
            hc,
            "cachelimit",
            "Overall entry limit for hostcache",
            |s| &s.tcp.hc.cache_limit,
        );
        vknob_u32(
            t,
            hc,
            "bucketlimit",
            "Per-bucket hash limit for hostcache",
            |s| &s.tcp.hc.bucket_limit,
        );
        t.add_value(
            hc,
            Number::Auto,
            "hashsize",
            "Size of TCP hostcache hashtable",
            Value::Uint(Box::new(|| {
                u32::try_from(tcp::hostcache::HASHSIZE).unwrap_or(0)
            })),
        )
        .expect("net.inet.tcp.hostcache.hashsize");
        t.add_value(
            hc,
            Number::Auto,
            "count",
            "The current number of entries in the host cache",
            Value::Uint(Box::new(|| {
                inet::state::of_arc(&super::curvnet()).tcp.hc.count()
            })),
        )
        .expect("net.inet.tcp.hostcache.count");
        t.add_value(
            hc,
            Number::Auto,
            "list",
            "List of all hostcache entries",
            Value::Str(Box::new(|out| {
                out.push_str(&inet::hostcache_list(&inet::state::of_arc(
                    &super::curvnet(),
                )));
            })),
        )
        .expect("net.inet.tcp.hostcache.list");
        t.add_writable(
            hc,
            Number::Auto,
            "purgenow",
            "Immediately purge all entries",
            ctlflag::VNET,
            Value::Int(Box::new(|| 0)),
            sysctl::int_writer(|v| {
                if v != 0 {
                    inet::state::of_arc(&super::curvnet()).tcp.hc.purge(true);
                }
                Ok(())
            }),
        )
        .expect("net.inet.tcp.hostcache.purgenow");
        let sc = t
            .add_node(tcp, Number::Auto, "syncache", "TCP SYN cache")
            .expect("net.inet.tcp.syncache");
        vknob_u32(
            t,
            sc,
            "bucketlimit",
            "Per-bucket hash limit for syncache",
            |s| &s.tcp.syncache.bucket_limit,
        );
        vknob_u32(
            t,
            sc,
            "cachelimit",
            "Overall entry limit for syncache",
            |s| &s.tcp.syncache.cache_limit,
        );
        vknob_u32(
            t,
            sc,
            "rexmtlimit",
            "Limit on SYN/ACK retransmissions",
            |s| &s.tcp.syncache.rexmt_limit,
        );
        t.add_value(
            sc,
            Number::Auto,
            "count",
            "Current number of entries in syncache",
            Value::Uint(Box::new(|| {
                inet::state::of_arc(&super::curvnet()).tcp.syncache.count()
            })),
        )
        .expect("net.inet.tcp.syncache.count");
        t.add_value(
            sc,
            Number::Auto,
            "hashsize",
            "Size of TCP syncache hashtable",
            Value::Uint(Box::new(|| {
                u32::try_from(tcp::syncache::HASHSIZE).unwrap_or(0)
            })),
        )
        .expect("net.inet.tcp.syncache.hashsize");
        t.add_value(
            tcp,
            Number::Auto,
            "pcblist",
            "List of active TCP connections",
            Value::Opaque {
                format: "S,xtcpcb",
                read: Box::new(|out| {
                    out.extend(inet::export::tcp_pcblist(super::curvnet().id, &|_| true))
                }),
            },
        )
        .expect("net.inet.tcp.pcblist");
        t.add_value(
            tcp,
            Number::Auto,
            "pcbcount",
            "Number of active PCBs",
            Value::Uint(Box::new(|| {
                u32::try_from(inet::export::count(inet::Proto::Tcp, super::curvnet().id))
                    .unwrap_or(u32::MAX)
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
        t.add_writable(
            cc,
            Number::Auto,
            "algorithm",
            "Default congestion control algorithm",
            ctlflag::VNET,
            Value::Str(Box::new(|out| {
                out.push_str(inet::state::of_arc(&super::curvnet()).tcp.cc_algo().name());
            })),
            sysctl::str_writer(16, |b| {
                let a = tcp::cc::Algo::by_name(b).ok_or(Errno::ESRCH)?;
                let i = tcp::cc::Algo::ALL.iter().position(|x| *x == a).unwrap_or(1);
                inet::state::of_arc(&super::curvnet())
                    .tcp
                    .default_cc
                    .store(u8::try_from(i).unwrap_or(1), Ordering::Relaxed);
                Ok(())
            }),
        )
        .expect("net.inet.tcp.cc.algorithm");
        t.add_value(
            cc,
            Number::Auto,
            "available",
            "List available congestion control algorithms",
            Value::Str(Box::new(|out| {
                use core::fmt::Write;
                let d = inet::state::of_arc(&super::curvnet()).tcp.cc_algo();
                out.push_str("CCmod           D PCB count\n");
                for a in tcp::cc::Algo::ALL {
                    let _ = writeln!(
                        out,
                        "{:<15} {} {:>2}",
                        a.name(),
                        if a == d { '*' } else { ' ' },
                        0
                    );
                }
            })),
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
        bind_all_fibs(t, udp);
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
        vknob(t, udp, "log_in_vain", "Log all incoming UDP packets", |s| {
            &s.conf.udp_log_in_vain
        });
        vknob(
            t,
            udp,
            "blackhole",
            "Do not send port unreachables for refused connects",
            |s| &s.conf.udp_blackhole,
        );
        vknob(
            t,
            udp,
            "blackhole_local",
            "Enforce net.inet.udp.blackhole for locally originated packets",
            |s| &s.conf.udp_blackhole_local,
        );
        vknob(t, udp, "checksum", "compute udp checksum", |s| {
            &s.conf.udp_cksum
        });
        vstats(
            t,
            udp,
            "S,udpstat",
            "UDP statistics (struct udpstat, netinet/udp_var.h)",
            |s| s.udpstat.bytes(),
            |s| s.udpstat.clear(),
        );
        t.add_value(
            udp,
            Number::Auto,
            "pcblist",
            "List of active UDP sockets",
            Value::Opaque {
                format: "S,xinpcb",
                read: Box::new(|out| {
                    out.extend(inet::export::udp_pcblist(super::curvnet().id, &|_| true))
                }),
            },
        )
        .expect("net.inet.udp.pcblist");
        let raw = node(t, inet, 255, "raw", "RAW");
        bind_all_fibs(t, raw);
        rw_u64(
            t,
            raw,
            "maxdgram",
            "Maximum outgoing raw IP datagram size",
            &inet::raw::SENDSPACE,
        );
        rw_u64(
            t,
            raw,
            "recvspace",
            "Maximum space for incoming raw IP datagrams",
            &inet::raw::RECVSPACE,
        );
        t.add_value(
            raw,
            Number::Auto,
            "pcblist",
            "List of active raw IP sockets",
            Value::Opaque {
                format: "S,xinpcb",
                read: Box::new(|out| {
                    out.extend(inet::export::raw_pcblist(super::curvnet().id, &|_| true));
                }),
            },
        )
        .expect("net.inet.raw.pcblist");

        // `net.inet6` (`PF_INET6`).
        inet6(t, net);

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
        arp_knobs(t, ether_inet);
        rw_i32(
            t,
            ether_inet,
            "log_arp_movements",
            "log arp replies from MACs different than the one in the cache",
            &net::arp::LOG_MOVEMENTS,
            |_| true,
        );
        rw_i32(
            t,
            ether_inet,
            "log_arp_permanent_modify",
            "log arp replies from MACs different than the one in the permanent arp entry",
            &net::arp::LOG_PERMANENT_MODIFY,
            |_| true,
        );
        rw_i32(
            t,
            ether_inet,
            "max_log_per_second",
            "Maximum number of remotely triggered ARP messages that can be logged per second",
            &net::arp::MAX_LOG_PER_SECOND,
            |_| true,
        );
        knobs(
            t,
            ether_inet,
            &[
                (
                    "log_arp_wrong_iface",
                    "log arp packets arriving on the wrong interface",
                    1,
                ),
                ("allow_multicast", "accept multicast addresses", 0),
            ],
        );
        let ether_arp = node(t, ether, 35, "arp", "");
        t.add_writable(
            ether_arp,
            Number::Auto,
            "stats",
            "ARP statistics (struct arpstat, net/if_arp.h)",
            ctlflag::VNET,
            Value::Opaque {
                format: "S,arpstat",
                read: Box::new(|out| {
                    let st = super::curvnet().net.lock().arpstat;
                    // SAFETY: `Stat` is `repr(C)` and all `u64`s.
                    let b = unsafe {
                        core::slice::from_raw_parts(
                            core::ptr::from_ref(&st).cast::<u8>(),
                            size_of::<net::arp::Stat>(),
                        )
                    };
                    out.extend_from_slice(b);
                }),
            },
            // `netstat -z`.
            Arc::new(|_: &[u8]| {
                super::curvnet().net.lock().arpstat = net::arp::Stat::ZERO;
                Ok(())
            }),
        )
        .expect("net.link.ether.arp.stats");
        rw_i32(
            t,
            ether_arp,
            "log_level",
            "Minimum log(9) level for recording rate limited arp log messages. The higher will be log more (emerg=0, info=6 (default), debug=7).",
            &net::arp::LOG_LEVEL,
            |_| true,
        );
        bpf_knobs(t, net);
        let lo = node(t, link, 24, "loif", "");
        let _ = lo;
        tuntap_knobs(t, root, link);
        bridge_knobs(t, link);

        // `net.routetable` (`PF_ROUTE`): `sysctl_rtsock`.
        t.add_handler(
            net,
            Number::Fixed(17),
            "routetable",
            "Return route tables and interface/address lists",
            Arc::new(rtsock),
        )
        .expect("net.routetable");
        // `net.route`: routes have several paths (`ROUTE_MPATH`, as in
        // GENERIC).
        let route = t
            .add_node(net, Number::Auto, "route", "")
            .expect("net.route");
        vknob(t, route, "multipath", "Enable route multipath", |s| {
            &s.conf.route_multipath
        });
        t.add_value(
            route,
            Number::Auto,
            "hash_outbound",
            "Compute flowid for locally-originated packets",
            Value::Uint(Box::new(|| {
                u32::from(super::curvnet().net.lock().hash_outbound)
            })),
        )
        .expect("net.route.hash_outbound");

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
                // `lltable_sysctl_dumparp`: the ARP or ND entries, of one
                // family.
                if arg & abi::net::rtf::LLINFO == 0 {
                    return Ok(());
                }
                let family = net::Family::from_af(af).ok_or(ReadError::Failed(Errno::EINVAL))?;
                let now = u64::try_from(super::now_s()).unwrap_or(0);
                out.extend(n.lle_dump(family, now, super::uptime_s()));
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
        net_rt::IFMALIST => {
            let g6 = inet::mld6::groups(&super::curvnet());
            out.extend(inet::rtmsg::ifmalist(&n, af, arg, &g6));
        }
        // No next-hop groups to list.
        net_rt::NHGRP => {}
        _ => return Err(ReadError::Failed(Errno::EINVAL)),
    }
    Ok(())
}

/// `net.inet6`: IPv6's, ICMPv6's, and MLD's settings and statistics,
/// each the current VNET's, with FreeBSD's numbers where programs ask by
/// them (sys/netinet6/in6_proto.c, frag6.c, icmp6.c, nd6.c, mld6.c).
fn inet6(t: &mut Tree, net: sysctl::NodeId) {
    let inet6 = node(t, net, 28, "inet6", "Internet6 Family");
    let ip6 = node(t, inet6, 41, "ip6", "IP6");
    vknob_n(
        t,
        ip6,
        1,
        "forwarding",
        "Enable forwarding of IPv6 packets between interfaces",
        |s| &s.conf6.forwarding,
    );
    vknob_n(
        t,
        ip6,
        2,
        "redirect",
        "Send ICMPv6 redirects for unforwardable IPv6 packets",
        |s| &s.conf6.redirect,
    );
    t.add_writable(
        ip6,
        Number::Fixed(3),
        "hlim",
        "Default hop limit to use for outgoing IPv6 packets",
        ctlflag::VNET,
        // Relaxed: a tunable.
        Value::Int(Box::new(|| tune::IP6_HLIM.load(Ordering::Relaxed))),
        sysctl::int_writer(|v| {
            if !(0..=255).contains(&v) {
                return Err(Errno::EINVAL);
            }
            tune::IP6_HLIM.store(v, Ordering::Relaxed);
            Ok(())
        }),
    )
    .expect("net.inet6.ip6.hlim");
    vstats_n(
        t,
        ip6,
        Number::Fixed(6),
        "stats",
        "S,ip6stat",
        "IP6 statistics (struct ip6stat, netinet6/ip6_var.h)",
        |s| s.ip6stat.bytes(),
        |s| s.ip6stat.clear(),
    );
    t.add_writable(
        ip6,
        Number::Fixed(9),
        "maxfragpackets",
        "Default maximum number of outstanding fragmented IPv6 packets. A value of 0 means no fragmented packets will be accepted, while a a value of -1 means no limit",
        ctlflag::VNET,
        Value::Int(Box::new(|| {
            inet::state::val_of(&inet::state::of_arc(&super::curvnet()).conf6.maxfragpackets)
        })),
        sysctl::int_writer(|v| {
            let st = inet::state::of_arc(&super::curvnet());
            st.conf6.maxfragpackets.store(v, Ordering::Relaxed);
            inet::frag6::set_bucketsize(&st);
            Ok(())
        }),
    )
    .expect("net.inet6.ip6.maxfragpackets");
    vknob_n(
        t,
        ip6,
        12,
        "accept_rtadv",
        "Default value of per-interface flag for accepting ICMPv6 RA messages",
        |s| &s.conf6.accept_rtadv,
    );
    vknob_n(
        t,
        ip6,
        15,
        "hdrnestlimit",
        "Default maximum number of IPv6 extension headers permitted on incoming IPv6 packets, 0 for no artificial limit",
        |s| &s.conf6.hdrnestlimit,
    );
    vknob_n(
        t,
        ip6,
        16,
        "dad_count",
        "Number of ICMPv6 NS messages sent during duplicate address detection",
        |s| &s.conf6.dad_count,
    );
    vknob_n(
        t,
        ip6,
        17,
        "auto_flowlabel",
        "Provide an IPv6 flowlabel in outbound packets",
        |s| &s.conf6.auto_flowlabel,
    );
    vknob_n(
        t,
        ip6,
        18,
        "defmcasthlim",
        "Default hop limit for IPv6 multicast packets originating from this node",
        |s| &s.conf6.defmcasthlim,
    );
    t.add_value(
        ip6,
        Number::Fixed(20),
        "kame_version",
        "KAME version string",
        Value::Str(Box::new(|s| s.push_str("FreeBSD"))),
    )
    .expect("net.inet6.ip6.kame_version");
    vknob_n(
        t,
        ip6,
        21,
        "use_deprecated",
        "Allow the use of addresses whose preferred lifetimes have expired",
        |s| &s.conf6.use_deprecated,
    );
    rw_i32(
        t,
        ip6,
        "v6only",
        "Restrict AF_INET6 sockets to IPv6 addresses only",
        &tune::V6ONLY,
        |_| true,
    );
    vknob_n(
        t,
        ip6,
        32,
        "use_tempaddr",
        "Create RFC3041 temporary addresses for autoconfigured addresses",
        |s| &s.conf6.use_tempaddr,
    );
    vstats_n(
        t,
        ip6,
        Number::Fixed(36),
        "rip6stats",
        "S,rip6stat",
        "Raw IP6 statistics (struct rip6stat, netinet6/raw_ip6.h)",
        |s| s.rip6stat.bytes(),
        |s| s.rip6stat.clear(),
    );
    vknob_n(
        t,
        ip6,
        37,
        "prefer_tempaddr",
        "Prefer RFC3041 temporary addresses in source address selection",
        |s| &s.conf6.prefer_tempaddr,
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
    // Relaxed: settings.
    t.add_writable(
        ip6,
        Number::Fixed(41),
        "maxfrags",
        "Maximum allowed number of outstanding IPv6 packet fragments. A value of 0 means no fragmented packets will be accepted, while a a value of -1 means no limit",
        0,
        Value::Int(Box::new(|| inet::frag6::MAXFRAGS.load(Ordering::Relaxed))),
        sysctl::int_writer(|v| {
            inet::frag6::MAXFRAGS.store(v, Ordering::Relaxed);
            Ok(())
        }),
    )
    .expect("net.inet6.ip6.maxfrags");
    vknob_n(
        t,
        ip6,
        48,
        "no_radr",
        "Default value of per-interface flag to control whether routers sending ICMPv6 RA messages on that interface are added into the default router list",
        |s| &s.conf6.no_radr,
    );
    vknob_n(
        t,
        ip6,
        49,
        "norbit_raif",
        "Always set clear the R flag in ICMPv6 NA messages when accepting RA on the interface",
        |s| &s.conf6.norbit_raif,
    );
    vknob_n(
        t,
        ip6,
        50,
        "rfc6204w3",
        "Accept the default router list from ICMPv6 RA messages even when packet forwarding is enabled",
        |s| &s.conf6.rfc6204w3,
    );
    vknob_n(
        t,
        ip6,
        53,
        "maxfragsperpacket",
        "Maximum allowed number of fragments per packet",
        |s| &s.conf6.maxfragsperpacket,
    );
    vknob_n(
        t,
        ip6,
        54,
        "maxfragbucketsize",
        "Maximum number of reassembly queues per hash bucket",
        |s| &s.conf6.maxfragbucketsize,
    );
    vknob(
        t,
        ip6,
        "fraglifetime_ms",
        "Fragment lifetime, in milliseconds",
        |s| &s.conf6.fraglifetime_ms,
    );
    vknob(
        t,
        ip6,
        "source_address_validation",
        "Drop incoming packets with source address that is a local address",
        |s| &s.conf6.sav,
    );
    vknob(
        t,
        ip6,
        "log_cannot_forward",
        "Log packets that cannot be forwarded",
        |s| &s.conf6.log_cannot_forward,
    );
    t.add_value(
        ip6,
        Number::Auto,
        "frag6_nfrags",
        "Global number of IPv6 fragments across all reassembly queues.",
        Value::Uint(Box::new(inet::frag6::nfrags)),
    )
    .expect("net.inet6.ip6.frag6_nfrags");
    t.add_value(
        ip6,
        Number::Auto,
        "frag6_nfragpackets",
        "Per-VNET number of IPv6 fragments across all reassembly queues.",
        Value::Uint(Box::new(|| {
            u32::try_from(inet::state::of_arc(&super::curvnet()).frag6_len()).unwrap_or(u32::MAX)
        })),
    )
    .expect("net.inet6.ip6.frag6_nfragpackets");
    knobs(
        t,
        ip6,
        &[
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
    let icmp6 = node(t, inet6, 58, "icmp6", "ICMP6");
    vstats_n(
        t,
        icmp6,
        Number::Fixed(1),
        "stats",
        "S,icmp6stat",
        "ICMPv6 statistics (struct icmp6stat, netinet/icmp6.h)",
        |s| s.icmp6stat.bytes(),
        |s| s.icmp6stat.clear(),
    );
    vknob_n(
        t,
        icmp6,
        2,
        "rediraccept",
        "Accept ICMPv6 redirect messages",
        |s| &s.conf6.rediraccept,
    );
    vknob_n(
        t,
        icmp6,
        3,
        "redirtimeout",
        "Delay in seconds before expiring redirect route",
        |s| &s.conf6.redirtimeout,
    );
    vknob_n(
        t,
        icmp6,
        6,
        "nd6_prune",
        "Frequency in seconds of checks for expired prefixes and routers",
        |s| &s.conf6.nd6_prune,
    );
    vknob_n(
        t,
        icmp6,
        8,
        "nd6_delay",
        "Delay in seconds before probing for reachability",
        |s| &s.conf6.nd6_delay,
    );
    vknob_n(
        t,
        icmp6,
        9,
        "nd6_umaxtries",
        "Number of ICMPv6 NS messages sent during reachability detection",
        |s| &s.conf6.nd6_umaxtries,
    );
    vknob_n(
        t,
        icmp6,
        10,
        "nd6_mmaxtries",
        "Number of ICMPv6 NS messages sent during address resolution",
        |s| &s.conf6.nd6_mmaxtries,
    );
    vknob_n(
        t,
        icmp6,
        11,
        "nd6_useloopback",
        "Create a loopback route when configuring an IPv6 address",
        |s| &s.conf6.nd6_useloopback,
    );
    vknob_n(
        t,
        icmp6,
        13,
        "nodeinfo",
        "Mask of enabled RFC4620 node information query types",
        |s| &s.conf6.nodeinfo,
    );
    t.add_writable(
        icmp6,
        Number::Fixed(14),
        "errppslimit",
        "Maximum number of ICMPv6 error/reply messages per second",
        ctlflag::VNET,
        Value::Uint(Box::new(|| {
            inet::state::of_arc(&super::curvnet())
                .conf6
                .errppslimit
                .load(Ordering::Relaxed)
        })),
        sysctl::int_writer(|v| {
            let st = inet::state::of_arc(&super::curvnet());
            let v = u32::try_from(v).map_err(|_| Errno::EINVAL)?;
            if v != 0 && v <= st.conf6.icmp6lim_jitter.load(Ordering::Relaxed) {
                return Err(Errno::EINVAL);
            }
            st.conf6.errppslimit.store(v, Ordering::Relaxed);
            Ok(())
        }),
    )
    .expect("net.inet6.icmp6.errppslimit");
    t.add_writable(
        icmp6,
        Number::Auto,
        "icmp6lim_jitter",
        "Random errppslimit jitter adjustment limit",
        ctlflag::VNET,
        Value::Uint(Box::new(|| {
            inet::state::of_arc(&super::curvnet())
                .conf6
                .icmp6lim_jitter
                .load(Ordering::Relaxed)
        })),
        sysctl::int_writer(|v| {
            let st = inet::state::of_arc(&super::curvnet());
            let v = u32::try_from(v).map_err(|_| Errno::EINVAL)?;
            if v >= st.conf6.errppslimit.load(Ordering::Relaxed) {
                return Err(Errno::EINVAL);
            }
            st.conf6.icmp6lim_jitter.store(v, Ordering::Relaxed);
            Ok(())
        }),
    )
    .expect("net.inet6.icmp6.icmp6lim_jitter");
    vknob(
        t,
        icmp6,
        "icmp6lim_output",
        "Enable logging of ICMPv6 response rate limiting",
        |s| &s.conf6.icmp6lim_output,
    );
    vknob_n(t, icmp6, 18, "nd6_debug", "Log NDP debug messages", |s| {
        &s.conf6.nd6_debug
    });
    for (num, name, descr, f) in [
        (
            19,
            "nd6_drlist",
            "NDP default router list",
            inet::nd6_rtr::drlist as fn(&net::Vnet, i64, u64) -> Vec<u8>,
        ),
        (20, "nd6_prlist", "NDP prefix list", inet::nd6_rtr::prlist),
    ] {
        t.add_value(
            icmp6,
            Number::Fixed(num),
            name,
            descr,
            Value::Opaque {
                format: "S,in6_defrouter",
                read: Box::new(move |out| {
                    out.extend(f(&super::curvnet(), super::now_s(), super::uptime_s()));
                }),
            },
        )
        .expect("an nd6 list sysctl");
    }
    vknob_n(
        t,
        icmp6,
        24,
        "nd6_maxqlen",
        "Maximum number of outbound packets to hold during address resolution",
        |s| &s.conf6.nd6_maxqueuelen,
    );
    vknob(
        t,
        icmp6,
        "nd6_gctimer",
        "Interface in seconds between garbage collection passes",
        |s| &s.conf6.nd6_gctimer,
    );
    vknob(
        t,
        icmp6,
        "nd6_maxndopt",
        "Maximum number of options in an ND message",
        |s| &s.conf6.nd6_maxndopt,
    );
    vknob(
        t,
        icmp6,
        "nd6_onlink_ns_rfc4861",
        "Accept 'on-link' ICMPv6 NS messages in compliance with RFC 4861",
        |s| &s.conf6.nd6_onlink_ns_rfc4861,
    );
    vknob(
        t,
        icmp6,
        "dad_enhanced",
        "Enable Enhanced DAD, which adds a random nonce to NS messages for DAD.",
        |s| &s.conf6.dad_enhanced,
    );
    vknob(t, icmp6, "dad_maxtry", "Max DAD retry counts", |s| {
        &s.conf6.dad_maxtry
    });
    vknob(
        t,
        icmp6,
        "nd6_ignore_ipv6_only_ra",
        "Ignore the 'IPv6-Only flag' in RA messages in compliance with draft-ietf-6man-ipv6only-flag",
        |s| &s.conf6.nd6_ignore_ipv6_only_ra,
    );
    let mld = node(t, inet6, 0, "mld", "IPv6 Multicast Listener Discovery");
    t.add_handler(
        mld,
        Number::Auto,
        "ifinfo",
        "Per-interface MLDv2 state",
        Arc::new(|args: &[i32], out: &mut Vec<u8>| {
            let [i] = args else {
                return Err(ReadError::Failed(Errno::EINVAL));
            };
            let i = u16::try_from(*i).map_err(|_| ReadError::Failed(Errno::EINVAL))?;
            let info =
                inet::mld6::ifinfo(&super::curvnet(), i).ok_or(ReadError::Failed(Errno::ENOENT))?;
            for w in info {
                out.extend_from_slice(&w.to_ne_bytes());
            }
            Ok(())
        }),
    )
    .expect("net.inet6.mld.ifinfo");
    let mcast = node(t, ip6, 0, "mcast", "IPv6 multicast");
    t.add_handler(
        mcast,
        Number::Auto,
        "filters",
        "Per-interface stack-wide source filters",
        Arc::new(|args: &[i32], out: &mut Vec<u8>| {
            let [i, g @ ..] = args else {
                return Err(ReadError::Failed(Errno::EINVAL));
            };
            if g.len() != 4 {
                return Err(ReadError::Failed(Errno::EINVAL));
            }
            let i = u16::try_from(*i).map_err(|_| ReadError::Failed(Errno::EINVAL))?;
            let mut group = [0u8; 16];
            for (k, w) in g.iter().enumerate() {
                group[k * 4..k * 4 + 4].copy_from_slice(&w.to_ne_bytes());
            }
            if let Some(mode) = inet::mld6::filter_mode(&super::curvnet(), i, &group) {
                out.extend_from_slice(&mode.to_ne_bytes());
            }
            Ok(())
        }),
    )
    .expect("net.inet6.ip6.mcast.filters");
    vknob(t, mld, "v1enable", "Enable fallback to MLDv1", |s| {
        &s.conf6.mld_v1enable
    });
    vknob(t, mld, "v2enable", "Enable MLDv2", |s| {
        &s.conf6.mld_v2enable
    });
    vknob(
        t,
        mld,
        "use_allow",
        "Use ALLOW/BLOCK for RFC 4604 SSM joins/leaves",
        |s| &s.conf6.mld_use_allow,
    );
}
