// SPDX-License-Identifier: BSD-2-Clause
//! Internet sockets (`netinet/in.h`, `netinet6/in6.h`, `netinet/tcp.h`,
//! `netinet/in_pcb.h`, `netinet/tcp_var.h`, `sys/socketvar.h`): the
//! addresses, protocol numbers, and socket options of `AF_INET` and
//! `AF_INET6`, and the records `net.inet.*.pcblist` exports.

use crate::layout::{Arches, ConstValue};

pub const AF_UNSPEC: u8 = 0;
pub const AF_INET: u8 = 2;
pub const AF_ROUTE: u8 = 17;
pub const AF_LINK: u8 = 18;
pub const AF_INET6: u8 = 28;
pub const AF_NETLINK: u8 = 38;
/// `SOCK_RAW`.
pub const SOCK_RAW: u32 = 3;

/// Protocol numbers.
pub const IPPROTO_IP: i32 = 0;
pub const IPPROTO_ICMP: i32 = 1;
pub const IPPROTO_TCP: i32 = 6;
pub const IPPROTO_UDP: i32 = 17;
pub const IPPROTO_IPV6: i32 = 41;
pub const IPPROTO_ICMPV6: i32 = 58;
pub const IPPROTO_RAW: i32 = 255;

/// `INADDR_LOOPBACK`, host order.
pub const INADDR_LOOPBACK: u32 = 0x7f00_0001;

/// Ports.
pub const IPPORT_RESERVED: u16 = 1024;
pub const IPPORT_EPHEMERALFIRST: u16 = 10000;
pub const IPPORT_EPHEMERALLAST: u16 = 65535;
pub const IPPORT_HIFIRSTAUTO: u16 = 49152;
pub const IPPORT_HILASTAUTO: u16 = 65535;
pub const IPPORT_RESERVEDSTART: u16 = 600;

/// `struct sockaddr_in`; the port and address in network order.
#[repr(C, align(4))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SockaddrIn {
    pub sin_len: u8,
    pub sin_family: u8,
    pub sin_port: [u8; 2],
    pub sin_addr: [u8; 4],
    pub sin_zero: [u8; 8],
}

/// `struct sockaddr_in6`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SockaddrIn6 {
    pub sin6_len: u8,
    pub sin6_family: u8,
    pub sin6_port: [u8; 2],
    pub sin6_flowinfo: u32,
    pub sin6_addr: [u8; 16],
    pub sin6_scope_id: u32,
}

/// `IPPROTO_IP` options.
pub mod ip {
    pub const IP_OPTIONS: i32 = 1;
    pub const IP_HDRINCL: i32 = 2;
    pub const IP_TOS: i32 = 3;
    pub const IP_TTL: i32 = 4;
    pub const IP_RECVOPTS: i32 = 5;
    pub const IP_RECVRETOPTS: i32 = 6;
    pub const IP_RECVDSTADDR: i32 = 7;
    pub const IP_RETOPTS: i32 = 8;
    pub const IP_MULTICAST_IF: i32 = 9;
    pub const IP_MULTICAST_TTL: i32 = 10;
    pub const IP_MULTICAST_LOOP: i32 = 11;
    pub const IP_ADD_MEMBERSHIP: i32 = 12;
    pub const IP_DROP_MEMBERSHIP: i32 = 13;
    pub const IP_PORTRANGE: i32 = 19;
    pub const IP_RECVIF: i32 = 20;
    pub const IP_ONESBCAST: i32 = 23;
    pub const IP_BINDANY: i32 = 24;
    pub const IP_ORIGDSTADDR: i32 = 27;
    pub const IP_RECVTTL: i32 = 65;
    pub const IP_MINTTL: i32 = 66;
    pub const IP_DONTFRAG: i32 = 67;
    pub const IP_RECVTOS: i32 = 68;
    pub const IP_FLOWID: i32 = 90;
    pub const IP_FLOWTYPE: i32 = 91;
    pub const IP_RECVFLOWID: i32 = 93;
    pub const IP_PORTRANGE_DEFAULT: i32 = 0;
    pub const IP_PORTRANGE_HIGH: i32 = 1;
    pub const IP_PORTRANGE_LOW: i32 = 2;
    /// `IP_DEFAULT_MULTICAST_TTL`, `IP_DEFAULT_MULTICAST_LOOP`.
    pub const IP_DEFAULT_MULTICAST_TTL: i32 = 1;
    pub const IP_DEFAULT_MULTICAST_LOOP: i32 = 1;
}

/// `IPPROTO_IPV6` options.
pub mod ipv6 {
    pub const IPV6_UNICAST_HOPS: i32 = 4;
    pub const IPV6_MULTICAST_IF: i32 = 9;
    pub const IPV6_MULTICAST_HOPS: i32 = 10;
    pub const IPV6_MULTICAST_LOOP: i32 = 11;
    pub const IPV6_JOIN_GROUP: i32 = 12;
    pub const IPV6_LEAVE_GROUP: i32 = 13;
    pub const IPV6_PORTRANGE: i32 = 14;
    pub const IPV6_CHECKSUM: i32 = 26;
    pub const IPV6_V6ONLY: i32 = 27;
    pub const IPV6_RECVPKTINFO: i32 = 36;
    pub const IPV6_RECVHOPLIMIT: i32 = 37;
    pub const IPV6_RECVRTHDR: i32 = 38;
    pub const IPV6_RECVHOPOPTS: i32 = 39;
    pub const IPV6_RECVDSTOPTS: i32 = 40;
    pub const IPV6_USE_MIN_MTU: i32 = 42;
    pub const IPV6_RECVPATHMTU: i32 = 43;
    pub const IPV6_PKTINFO: i32 = 46;
    pub const IPV6_HOPLIMIT: i32 = 47;
    pub const IPV6_RECVTCLASS: i32 = 57;
    pub const IPV6_AUTOFLOWLABEL: i32 = 59;
    pub const IPV6_TCLASS: i32 = 61;
    pub const IPV6_DONTFRAG: i32 = 62;
    pub const IPV6_PREFER_TEMPADDR: i32 = 63;
    pub const IPV6_BINDANY: i32 = 64;
    pub const IPV6_ORIGDSTADDR: i32 = 72;
    /// `IPV6_DEFHLIM`: the hop limit unless set.
    pub const IPV6_DEFHLIM: i32 = 64;
}

/// `IPPROTO_TCP` options.
pub mod tcp {
    pub const TCP_NODELAY: i32 = 1;
    pub const TCP_MAXSEG: i32 = 2;
    pub const TCP_NOPUSH: i32 = 4;
    pub const TCP_NOOPT: i32 = 8;
    pub const TCP_INFO: i32 = 32;
    pub const TCP_CONGESTION: i32 = 64;
    pub const TCP_KEEPINIT: i32 = 128;
    pub const TCP_KEEPIDLE: i32 = 256;
    pub const TCP_KEEPINTVL: i32 = 512;
    pub const TCP_KEEPCNT: i32 = 1024;
    pub const TCP_FASTOPEN: i32 = 1025;
    pub const TCP_FUNCTION_BLK: i32 = 8192;
    /// `TCP_MSS` (`tcp_mssdflt`), `TCP_MAXWIN`.
    pub const TCP_MSS: i32 = 536;
    pub const TCP_CA_NAME_MAX: usize = 16;
    pub const TCP_LOG_ID_LEN: usize = 64;
    pub const TCP_FUNCTION_NAME_LEN_MAX: usize = 32;
    /// `t_flags` bits.
    pub const TF_NODELAY: u32 = 0x0000_0004;
    pub const TF_NOOPT: u32 = 0x0000_0008;
    pub const TF_SENTFIN: u32 = 0x0000_0010;
    pub const TF_NOPUSH: u32 = 0x0000_1000;
}

/// TCP states (`netinet/tcp_fsm.h`).
pub mod tcps {
    pub const CLOSED: i32 = 0;
    pub const LISTEN: i32 = 1;
    pub const SYN_SENT: i32 = 2;
    pub const SYN_RECEIVED: i32 = 3;
    pub const ESTABLISHED: i32 = 4;
    pub const CLOSE_WAIT: i32 = 5;
    pub const FIN_WAIT_1: i32 = 6;
    pub const CLOSING: i32 = 7;
    pub const LAST_ACK: i32 = 8;
    pub const FIN_WAIT_2: i32 = 9;
    pub const TIME_WAIT: i32 = 10;
}

/// `inp_vflag` bits.
pub const INP_IPV4: u8 = 0x1;
pub const INP_IPV6: u8 = 0x2;
pub const INP_IPV6PROTO: u8 = 0x4;
/// `inp_flags` bits.
pub mod inp {
    pub const RECVOPTS: i32 = 0x0000_0001;
    pub const RECVRETOPTS: i32 = 0x0000_0002;
    pub const RECVDSTADDR: i32 = 0x0000_0004;
    pub const HIGHPORT: i32 = 0x0000_0010;
    pub const LOWPORT: i32 = 0x0000_0020;
    pub const ANONPORT: i32 = 0x0000_0040;
    pub const RECVIF: i32 = 0x0000_0080;
    pub const RECVTTL: i32 = 0x0000_0400;
    pub const DONTFRAG: i32 = 0x0000_0800;
    pub const BINDANY: i32 = 0x0000_1000;
    pub const INHASHLIST: i32 = 0x0000_2000;
    pub const RECVTOS: i32 = 0x0000_4000;
    pub const IPV6_V6ONLY: i32 = 0x0000_8000;
    pub const IN6P_PKTINFO: i32 = 0x0001_0000;
    pub const IN6P_HOPLIMIT: i32 = 0x0002_0000;
    pub const IN6P_TCLASS: i32 = 0x0040_0000;
    pub const IN6P_AUTOFLOWLABEL: i32 = 0x0080_0000;
    pub const ONESBCAST: i32 = 0x0200_0000;
    pub const DROPPED: i32 = 0x0400_0000;
    /// `inp_flags2`.
    pub const ORIGDSTADDR: i32 = 0x0000_0800;
}
/// `inc_flags` bits.
pub const INC_ISIPV6: u8 = 0x01;

/// `so_state` bits.
pub mod ss {
    pub const ISCONNECTED: i16 = 0x0002;
    pub const ISCONNECTING: i16 = 0x0004;
    pub const ISDISCONNECTING: i16 = 0x0008;
    pub const NBIO: i16 = 0x0100;
    pub const ISDISCONNECTED: i16 = 0x2000;
}

/// `union in_dependaddr`: an IPv6 address, or an IPv4 one in its last
/// four bytes.
pub type DependAddr = [u8; 16];

/// `struct in_endpoints`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InEndpoints {
    pub ie_fport: [u8; 2],
    pub ie_lport: [u8; 2],
    pub ie_dependfaddr: DependAddr,
    pub ie_dependladdr: DependAddr,
    pub ie6_zoneid: u32,
}

/// `struct in_conninfo`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InConninfo {
    pub inc_flags: u8,
    pub inc_len: u8,
    pub inc_fibnum: u16,
    pub inc_ie: InEndpoints,
}

/// `struct xsockbuf`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Xsockbuf {
    pub sb_cc: u32,
    pub sb_hiwat: u32,
    pub sb_mbcnt: u32,
    pub sb_spare0: u32,
    pub sb_spare1: u32,
    pub sb_mbmax: u32,
    pub sb_lowat: i32,
    pub sb_timeo: i32,
    pub sb_flags: i16,
    pub _pad: u16,
}

/// `struct xsocket`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Xsocket {
    pub xso_len: u64,
    pub xso_so: u64,
    pub so_pcb: u64,
    pub so_oobmark: u64,
    pub so_splice_so: u64,
    pub so_spare64: [i64; 7],
    pub xso_protocol: i32,
    pub xso_family: i32,
    pub so_qlen: u32,
    pub so_incqlen: u32,
    pub so_qlimit: u32,
    pub so_pgid: i32,
    pub so_uid: u32,
    pub so_fibnum: i32,
    pub so_spare32: [i32; 7],
    pub so_type: i16,
    pub so_options: i16,
    pub so_linger: i16,
    pub so_state: i16,
    pub so_timeo: i16,
    pub so_error: u16,
    pub so_rcv: Xsockbuf,
    pub so_snd: Xsockbuf,
}

/// `struct xinpcb`.
#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Xinpcb {
    pub xi_len: u64,
    pub xi_socket: Xsocket,
    pub inp_inc: InConninfo,
    pub _pad0: u32,
    pub inp_gencnt: u64,
    pub inp_spare64: [i64; 5],
    pub inp_flow: u32,
    pub inp_flowid: u32,
    pub inp_flowtype: u32,
    pub inp_flags: i32,
    pub inp_flags2: i32,
    pub inp_unused: u32,
    pub in6p_cksum: i32,
    pub inp_spare32: [i32; 4],
    pub in6p_hops: u16,
    pub inp_ip_tos: u8,
    pub pad8: i8,
    pub inp_vflag: u8,
    pub inp_ip_ttl: u8,
    pub inp_ip_p: u8,
    pub inp_ip_minttl: u8,
    pub inp_spare8: [i8; 4],
}

/// `struct xinpgen`: the record before and after a pcb list.
#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Xinpgen {
    pub xig_len: u64,
    pub xig_count: u32,
    pub _xig_spare32: u32,
    pub xig_gen: u64,
    pub xig_sogen: u64,
    pub _xig_spare64: [u64; 4],
}

/// `struct xtcpcb`.
#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Xtcpcb {
    pub xt_len: u64,
    pub xt_inp: Xinpcb,
    pub xt_stack: [u8; tcp::TCP_FUNCTION_NAME_LEN_MAX],
    pub xt_logid: [u8; tcp::TCP_LOG_ID_LEN],
    pub xt_cc: [u8; tcp::TCP_CA_NAME_MAX],
    pub spare64: [i64; 6],
    pub t_state: i32,
    pub t_flags: u32,
    pub t_sndzerowin: i32,
    pub t_sndrexmitpack: i32,
    pub t_rcvoopack: i32,
    pub t_rcvtime: i32,
    pub tt_rexmt: i32,
    pub tt_persist: i32,
    pub tt_keep: i32,
    pub tt_2msl: i32,
    pub tt_delack: i32,
    pub t_logstate: i32,
    pub t_snd_cwnd: u32,
    pub t_snd_ssthresh: u32,
    pub t_maxseg: u32,
    pub t_rcv_wnd: u32,
    pub t_snd_wnd: u32,
    pub xt_ecn: u32,
    pub t_dsack_bytes: u32,
    pub t_dsack_tlp_bytes: u32,
    pub t_dsack_pack: u32,
    pub xt_encaps_port: u16,
    pub spare16: i16,
    pub spare32: [i32; 22],
}

impl Default for Xtcpcb {
    fn default() -> Self {
        Self {
            xt_len: 0,
            xt_inp: Xinpcb::default(),
            xt_stack: [0; tcp::TCP_FUNCTION_NAME_LEN_MAX],
            xt_logid: [0; tcp::TCP_LOG_ID_LEN],
            xt_cc: [0; tcp::TCP_CA_NAME_MAX],
            spare64: [0; 6],
            t_state: 0,
            t_flags: 0,
            t_sndzerowin: 0,
            t_sndrexmitpack: 0,
            t_rcvoopack: 0,
            t_rcvtime: 0,
            tt_rexmt: 0,
            tt_persist: 0,
            tt_keep: 0,
            tt_2msl: 0,
            tt_delack: 0,
            t_logstate: 0,
            t_snd_cwnd: 0,
            t_snd_ssthresh: 0,
            t_maxseg: 0,
            t_rcv_wnd: 0,
            t_snd_wnd: 0,
            xt_ecn: 0,
            t_dsack_bytes: 0,
            t_dsack_tlp_bytes: 0,
            t_dsack_pack: 0,
            xt_encaps_port: 0,
            spare16: 0,
            spare32: [0; 22],
        }
    }
}

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("AF_UNSPEC", "sys/socket.h", AF_UNSPEC as i64),
    c("AF_INET", "sys/socket.h", AF_INET as i64),
    c("AF_ROUTE", "sys/socket.h", AF_ROUTE as i64),
    c("AF_LINK", "sys/socket.h", AF_LINK as i64),
    c("AF_INET6", "sys/socket.h", AF_INET6 as i64),
    c("AF_NETLINK", "sys/socket.h", AF_NETLINK as i64),
    c("SOCK_RAW", "sys/socket.h", SOCK_RAW as i64),
    c("IPPROTO_IP", "netinet/in.h", IPPROTO_IP as i64),
    c("IPPROTO_ICMP", "netinet/in.h", IPPROTO_ICMP as i64),
    c("IPPROTO_TCP", "netinet/in.h", IPPROTO_TCP as i64),
    c("IPPROTO_UDP", "netinet/in.h", IPPROTO_UDP as i64),
    c("IPPROTO_IPV6", "netinet/in.h", IPPROTO_IPV6 as i64),
    c("IPPROTO_ICMPV6", "netinet/in.h", IPPROTO_ICMPV6 as i64),
    c("IPPROTO_RAW", "netinet/in.h", IPPROTO_RAW as i64),
    c("INADDR_LOOPBACK", "netinet/in.h", INADDR_LOOPBACK as i64),
    c("IPPORT_RESERVED", "netinet/in.h", IPPORT_RESERVED as i64),
    c(
        "IPPORT_EPHEMERALFIRST",
        "netinet/in.h",
        IPPORT_EPHEMERALFIRST as i64,
    ),
    c(
        "IPPORT_EPHEMERALLAST",
        "netinet/in.h",
        IPPORT_EPHEMERALLAST as i64,
    ),
    c(
        "IPPORT_HIFIRSTAUTO",
        "netinet/in.h",
        IPPORT_HIFIRSTAUTO as i64,
    ),
    c(
        "IPPORT_HILASTAUTO",
        "netinet/in.h",
        IPPORT_HILASTAUTO as i64,
    ),
    c(
        "IPPORT_RESERVEDSTART",
        "netinet/in.h",
        IPPORT_RESERVEDSTART as i64,
    ),
    c("IP_OPTIONS", "netinet/in.h", ip::IP_OPTIONS as i64),
    c("IP_HDRINCL", "netinet/in.h", ip::IP_HDRINCL as i64),
    c("IP_TOS", "netinet/in.h", ip::IP_TOS as i64),
    c("IP_TTL", "netinet/in.h", ip::IP_TTL as i64),
    c("IP_RECVOPTS", "netinet/in.h", ip::IP_RECVOPTS as i64),
    c("IP_RECVRETOPTS", "netinet/in.h", ip::IP_RECVRETOPTS as i64),
    c("IP_RECVDSTADDR", "netinet/in.h", ip::IP_RECVDSTADDR as i64),
    c("IP_RETOPTS", "netinet/in.h", ip::IP_RETOPTS as i64),
    c(
        "IP_MULTICAST_IF",
        "netinet/in.h",
        ip::IP_MULTICAST_IF as i64,
    ),
    c(
        "IP_MULTICAST_TTL",
        "netinet/in.h",
        ip::IP_MULTICAST_TTL as i64,
    ),
    c(
        "IP_MULTICAST_LOOP",
        "netinet/in.h",
        ip::IP_MULTICAST_LOOP as i64,
    ),
    c(
        "IP_ADD_MEMBERSHIP",
        "netinet/in.h",
        ip::IP_ADD_MEMBERSHIP as i64,
    ),
    c(
        "IP_DROP_MEMBERSHIP",
        "netinet/in.h",
        ip::IP_DROP_MEMBERSHIP as i64,
    ),
    c("IP_PORTRANGE", "netinet/in.h", ip::IP_PORTRANGE as i64),
    c("IP_RECVIF", "netinet/in.h", ip::IP_RECVIF as i64),
    c("IP_ONESBCAST", "netinet/in.h", ip::IP_ONESBCAST as i64),
    c("IP_BINDANY", "netinet/in.h", ip::IP_BINDANY as i64),
    c("IP_ORIGDSTADDR", "netinet/in.h", ip::IP_ORIGDSTADDR as i64),
    c("IP_RECVTTL", "netinet/in.h", ip::IP_RECVTTL as i64),
    c("IP_MINTTL", "netinet/in.h", ip::IP_MINTTL as i64),
    c("IP_DONTFRAG", "netinet/in.h", ip::IP_DONTFRAG as i64),
    c("IP_RECVTOS", "netinet/in.h", ip::IP_RECVTOS as i64),
    c("IP_FLOWID", "netinet/in.h", ip::IP_FLOWID as i64),
    c("IP_FLOWTYPE", "netinet/in.h", ip::IP_FLOWTYPE as i64),
    c("IP_RECVFLOWID", "netinet/in.h", ip::IP_RECVFLOWID as i64),
    c(
        "IP_PORTRANGE_DEFAULT",
        "netinet/in.h",
        ip::IP_PORTRANGE_DEFAULT as i64,
    ),
    c(
        "IP_PORTRANGE_HIGH",
        "netinet/in.h",
        ip::IP_PORTRANGE_HIGH as i64,
    ),
    c(
        "IP_PORTRANGE_LOW",
        "netinet/in.h",
        ip::IP_PORTRANGE_LOW as i64,
    ),
    c(
        "IP_DEFAULT_MULTICAST_TTL",
        "netinet/in.h",
        ip::IP_DEFAULT_MULTICAST_TTL as i64,
    ),
    c(
        "IP_DEFAULT_MULTICAST_LOOP",
        "netinet/in.h",
        ip::IP_DEFAULT_MULTICAST_LOOP as i64,
    ),
    c(
        "IPV6_UNICAST_HOPS",
        "netinet/in.h",
        ipv6::IPV6_UNICAST_HOPS as i64,
    ),
    c(
        "IPV6_MULTICAST_IF",
        "netinet/in.h",
        ipv6::IPV6_MULTICAST_IF as i64,
    ),
    c(
        "IPV6_MULTICAST_HOPS",
        "netinet/in.h",
        ipv6::IPV6_MULTICAST_HOPS as i64,
    ),
    c(
        "IPV6_MULTICAST_LOOP",
        "netinet/in.h",
        ipv6::IPV6_MULTICAST_LOOP as i64,
    ),
    c(
        "IPV6_JOIN_GROUP",
        "netinet/in.h",
        ipv6::IPV6_JOIN_GROUP as i64,
    ),
    c(
        "IPV6_LEAVE_GROUP",
        "netinet/in.h",
        ipv6::IPV6_LEAVE_GROUP as i64,
    ),
    c(
        "IPV6_PORTRANGE",
        "netinet/in.h",
        ipv6::IPV6_PORTRANGE as i64,
    ),
    c("IPV6_CHECKSUM", "netinet/in.h", ipv6::IPV6_CHECKSUM as i64),
    c("IPV6_V6ONLY", "netinet/in.h", ipv6::IPV6_V6ONLY as i64),
    c(
        "IPV6_RECVPKTINFO",
        "netinet/in.h",
        ipv6::IPV6_RECVPKTINFO as i64,
    ),
    c(
        "IPV6_RECVHOPLIMIT",
        "netinet/in.h",
        ipv6::IPV6_RECVHOPLIMIT as i64,
    ),
    c(
        "IPV6_RECVRTHDR",
        "netinet/in.h",
        ipv6::IPV6_RECVRTHDR as i64,
    ),
    c(
        "IPV6_RECVHOPOPTS",
        "netinet/in.h",
        ipv6::IPV6_RECVHOPOPTS as i64,
    ),
    c(
        "IPV6_RECVDSTOPTS",
        "netinet/in.h",
        ipv6::IPV6_RECVDSTOPTS as i64,
    ),
    c(
        "IPV6_USE_MIN_MTU",
        "netinet/in.h",
        ipv6::IPV6_USE_MIN_MTU as i64,
    ),
    c(
        "IPV6_RECVPATHMTU",
        "netinet/in.h",
        ipv6::IPV6_RECVPATHMTU as i64,
    ),
    c("IPV6_PKTINFO", "netinet/in.h", ipv6::IPV6_PKTINFO as i64),
    c("IPV6_HOPLIMIT", "netinet/in.h", ipv6::IPV6_HOPLIMIT as i64),
    c(
        "IPV6_RECVTCLASS",
        "netinet/in.h",
        ipv6::IPV6_RECVTCLASS as i64,
    ),
    c(
        "IPV6_AUTOFLOWLABEL",
        "netinet/in.h",
        ipv6::IPV6_AUTOFLOWLABEL as i64,
    ),
    c("IPV6_TCLASS", "netinet/in.h", ipv6::IPV6_TCLASS as i64),
    c("IPV6_DONTFRAG", "netinet/in.h", ipv6::IPV6_DONTFRAG as i64),
    c(
        "IPV6_PREFER_TEMPADDR",
        "netinet/in.h",
        ipv6::IPV6_PREFER_TEMPADDR as i64,
    ),
    c("IPV6_BINDANY", "netinet/in.h", ipv6::IPV6_BINDANY as i64),
    c(
        "IPV6_ORIGDSTADDR",
        "netinet/in.h",
        ipv6::IPV6_ORIGDSTADDR as i64,
    ),
    c("TCP_NODELAY", "netinet/tcp.h", tcp::TCP_NODELAY as i64),
    c("TCP_MAXSEG", "netinet/tcp.h", tcp::TCP_MAXSEG as i64),
    c("TCP_NOPUSH", "netinet/tcp.h", tcp::TCP_NOPUSH as i64),
    c("TCP_NOOPT", "netinet/tcp.h", tcp::TCP_NOOPT as i64),
    c("TCP_INFO", "netinet/tcp.h", tcp::TCP_INFO as i64),
    c(
        "TCP_CONGESTION",
        "netinet/tcp.h",
        tcp::TCP_CONGESTION as i64,
    ),
    c("TCP_KEEPINIT", "netinet/tcp.h", tcp::TCP_KEEPINIT as i64),
    c("TCP_KEEPIDLE", "netinet/tcp.h", tcp::TCP_KEEPIDLE as i64),
    c("TCP_KEEPINTVL", "netinet/tcp.h", tcp::TCP_KEEPINTVL as i64),
    c("TCP_KEEPCNT", "netinet/tcp.h", tcp::TCP_KEEPCNT as i64),
    c("TCP_FASTOPEN", "netinet/tcp.h", tcp::TCP_FASTOPEN as i64),
    c(
        "TCP_FUNCTION_BLK",
        "netinet/tcp.h",
        tcp::TCP_FUNCTION_BLK as i64,
    ),
    c("TCP_MSS", "netinet/tcp.h", tcp::TCP_MSS as i64),
    c(
        "TCP_CA_NAME_MAX",
        "netinet/tcp.h",
        tcp::TCP_CA_NAME_MAX as i64,
    ),
    c(
        "TCP_LOG_ID_LEN",
        "netinet/tcp.h",
        tcp::TCP_LOG_ID_LEN as i64,
    ),
    c(
        "TCP_FUNCTION_NAME_LEN_MAX",
        "netinet/tcp.h",
        tcp::TCP_FUNCTION_NAME_LEN_MAX as i64,
    ),
    c("TCPS_CLOSED", "netinet/tcp_fsm.h", tcps::CLOSED as i64),
    c("TCPS_LISTEN", "netinet/tcp_fsm.h", tcps::LISTEN as i64),
    c("TCPS_SYN_SENT", "netinet/tcp_fsm.h", tcps::SYN_SENT as i64),
    c(
        "TCPS_SYN_RECEIVED",
        "netinet/tcp_fsm.h",
        tcps::SYN_RECEIVED as i64,
    ),
    c(
        "TCPS_ESTABLISHED",
        "netinet/tcp_fsm.h",
        tcps::ESTABLISHED as i64,
    ),
    c(
        "TCPS_CLOSE_WAIT",
        "netinet/tcp_fsm.h",
        tcps::CLOSE_WAIT as i64,
    ),
    c(
        "TCPS_FIN_WAIT_1",
        "netinet/tcp_fsm.h",
        tcps::FIN_WAIT_1 as i64,
    ),
    c("TCPS_CLOSING", "netinet/tcp_fsm.h", tcps::CLOSING as i64),
    c("TCPS_LAST_ACK", "netinet/tcp_fsm.h", tcps::LAST_ACK as i64),
    c(
        "TCPS_FIN_WAIT_2",
        "netinet/tcp_fsm.h",
        tcps::FIN_WAIT_2 as i64,
    ),
    c(
        "TCPS_TIME_WAIT",
        "netinet/tcp_fsm.h",
        tcps::TIME_WAIT as i64,
    ),
    c("INP_IPV4", "netinet/in_pcb.h", INP_IPV4 as i64),
    c("INP_IPV6", "netinet/in_pcb.h", INP_IPV6 as i64),
    c("INP_IPV6PROTO", "netinet/in_pcb.h", INP_IPV6PROTO as i64),
    c("INP_RECVOPTS", "netinet/in_pcb.h", inp::RECVOPTS as i64),
    c(
        "INP_RECVRETOPTS",
        "netinet/in_pcb.h",
        inp::RECVRETOPTS as i64,
    ),
    c(
        "INP_RECVDSTADDR",
        "netinet/in_pcb.h",
        inp::RECVDSTADDR as i64,
    ),
    c("INP_HIGHPORT", "netinet/in_pcb.h", inp::HIGHPORT as i64),
    c("INP_LOWPORT", "netinet/in_pcb.h", inp::LOWPORT as i64),
    c("INP_ANONPORT", "netinet/in_pcb.h", inp::ANONPORT as i64),
    c("INP_RECVIF", "netinet/in_pcb.h", inp::RECVIF as i64),
    c("INP_RECVTTL", "netinet/in_pcb.h", inp::RECVTTL as i64),
    c("INP_DONTFRAG", "netinet/in_pcb.h", inp::DONTFRAG as i64),
    c("INP_BINDANY", "netinet/in_pcb.h", inp::BINDANY as i64),
    c("INP_INHASHLIST", "netinet/in_pcb.h", inp::INHASHLIST as i64),
    c("INP_RECVTOS", "netinet/in_pcb.h", inp::RECVTOS as i64),
    c(
        "IN6P_IPV6_V6ONLY",
        "netinet/in_pcb.h",
        inp::IPV6_V6ONLY as i64,
    ),
    c("IN6P_PKTINFO", "netinet/in_pcb.h", inp::IN6P_PKTINFO as i64),
    c(
        "IN6P_HOPLIMIT",
        "netinet/in_pcb.h",
        inp::IN6P_HOPLIMIT as i64,
    ),
    c("IN6P_TCLASS", "netinet/in_pcb.h", inp::IN6P_TCLASS as i64),
    c(
        "IN6P_AUTOFLOWLABEL",
        "netinet/in_pcb.h",
        inp::IN6P_AUTOFLOWLABEL as i64,
    ),
    c("INP_ONESBCAST", "netinet/in_pcb.h", inp::ONESBCAST as i64),
    c("INP_DROPPED", "netinet/in_pcb.h", inp::DROPPED as i64),
    c(
        "INP_ORIGDSTADDR",
        "netinet/in_pcb.h",
        inp::ORIGDSTADDR as i64,
    ),
    c("INC_ISIPV6", "netinet/in_pcb.h", INC_ISIPV6 as i64),
    c("SS_ISCONNECTED", "sys/socketvar.h", ss::ISCONNECTED as i64),
    c(
        "SS_ISCONNECTING",
        "sys/socketvar.h",
        ss::ISCONNECTING as i64,
    ),
    c(
        "SS_ISDISCONNECTING",
        "sys/socketvar.h",
        ss::ISDISCONNECTING as i64,
    ),
    c("SS_NBIO", "sys/socketvar.h", ss::NBIO as i64),
    c(
        "SS_ISDISCONNECTED",
        "sys/socketvar.h",
        ss::ISDISCONNECTED as i64,
    ),
    c("TF_NODELAY", "netinet/tcp_var.h", tcp::TF_NODELAY as i64),
    c("TF_NOOPT", "netinet/tcp_var.h", tcp::TF_NOOPT as i64),
    c("TF_SENTFIN", "netinet/tcp_var.h", tcp::TF_SENTFIN as i64),
    c("TF_NOPUSH", "netinet/tcp_var.h", tcp::TF_NOPUSH as i64),
];

const fn c(c_name: &'static str, header: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header,
        value,
        arches: Arches::All,
    }
}
