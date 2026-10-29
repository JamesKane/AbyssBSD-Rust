// SPDX-License-Identifier: BSD-2-Clause
//! Netlink (`netlink/netlink.h`, `netlink/route/*.h`,
//! `netlink/netlink_bitset.h`): the socket address, the message and
//! attribute headers, and the `NETLINK_ROUTE` numbers `ifconfig(8)` and
//! `route(8)` use.

use crate::layout::{Arches, ConstValue};

/// `struct sockaddr_nl`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SockaddrNl {
    pub nl_len: u8,
    pub nl_family: u8,
    pub nl_pad: u16,
    pub nl_pid: u32,
    pub nl_groups: u32,
}

/// `struct nlmsghdr`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Nlmsghdr {
    pub nlmsg_len: u32,
    pub nlmsg_type: u16,
    pub nlmsg_flags: u16,
    pub nlmsg_seq: u32,
    pub nlmsg_pid: u32,
}

/// `struct nlattr`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Nlattr {
    pub nla_len: u16,
    pub nla_type: u16,
}

/// `struct ifinfomsg`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ifinfomsg {
    pub ifi_family: u8,
    pub __ifi_pad: u8,
    pub ifi_type: u16,
    pub ifi_index: i32,
    pub ifi_flags: u32,
    pub ifi_change: u32,
}

/// `struct ifaddrmsg`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ifaddrmsg {
    pub ifa_family: u8,
    pub ifa_prefixlen: u8,
    pub ifa_flags: u8,
    pub ifa_scope: u8,
    pub ifa_index: u32,
}

/// `struct rtmsg`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Rtmsg {
    pub rtm_family: u8,
    pub rtm_dst_len: u8,
    pub rtm_src_len: u8,
    pub rtm_tos: u8,
    pub rtm_table: u8,
    pub rtm_protocol: u8,
    pub rtm_scope: u8,
    pub rtm_type: u8,
    pub rtm_flags: u32,
}

/// `struct ifa_cacheinfo`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IfaCacheinfo {
    pub ifa_prefered: u32,
    pub ifa_valid: u32,
    pub cstamp: u32,
    pub tstamp: u32,
}

pub const NETLINK_ROUTE: i32 = 0;
pub const NETLINK_GENERIC: i32 = 16;
pub const SOL_NETLINK: i32 = 270;
pub const NETLINK_ADD_MEMBERSHIP: i32 = 1;
pub const NETLINK_DROP_MEMBERSHIP: i32 = 2;
pub const NETLINK_LIST_MEMBERSHIPS: i32 = 9;
pub const NETLINK_CAP_ACK: i32 = 10;
pub const NETLINK_EXT_ACK: i32 = 11;
pub const NETLINK_GET_STRICT_CHK: i32 = 12;
pub const NETLINK_MSG_INFO: i32 = 257;

pub const NLMSG_NOOP: u16 = 1;
pub const NLMSG_ERROR: u16 = 2;
pub const NLMSG_DONE: u16 = 3;
pub const NLMSG_MIN_TYPE: u16 = 0x10;
pub const NLM_F_REQUEST: u16 = 0x01;
pub const NLM_F_MULTI: u16 = 0x02;
pub const NLM_F_ACK: u16 = 0x04;
pub const NLM_F_DUMP: u16 = 0x300;
pub const NLM_F_REPLACE: u16 = 0x100;
pub const NLM_F_EXCL: u16 = 0x200;
pub const NLM_F_CREATE: u16 = 0x400;
pub const NLM_F_CAPPED: u16 = 0x100;
pub const NLM_F_ACK_TLVS: u16 = 0x200;
pub const NLMSGERR_ATTR_MSG: u16 = 1;

pub const RTM_NEWLINK: u16 = 16;
pub const RTM_DELLINK: u16 = 17;
pub const RTM_GETLINK: u16 = 18;
pub const RTM_NEWADDR: u16 = 20;
pub const RTM_DELADDR: u16 = 21;
pub const RTM_GETADDR: u16 = 22;
pub const RTM_NEWROUTE: u16 = 24;
pub const RTM_DELROUTE: u16 = 25;
pub const RTM_GETROUTE: u16 = 26;
pub const RTM_GETNEIGH: u16 = 30;

pub const IFLA_ADDRESS: u16 = 1;
pub const IFLA_IFNAME: u16 = 3;
pub const IFLA_MTU: u16 = 4;
pub const IFLA_OPERSTATE: u16 = 16;
pub const IFLA_IFALIAS: u16 = 20;
pub const IFLA_STATS64: u16 = 23;
pub const IFLA_PROMISCUITY: u16 = 30;
pub const IFLA_CARRIER: u16 = 33;
pub const IFLA_FREEBSD: u16 = 64;
pub const IFLAF_ORIG_HWADDR: u16 = 2;
pub const IFLAF_CAPS: u16 = 3;
pub const IF_OPER_DOWN: u8 = 2;
pub const IF_OPER_UP: u8 = 6;
pub const NLA_BITSET_SIZE: u16 = 2;
pub const NLA_BITSET_VALUE: u16 = 4;
pub const NLA_BITSET_MASK: u16 = 5;

pub const IFA_ADDRESS: u16 = 1;
pub const IFA_LOCAL: u16 = 2;
pub const IFA_LABEL: u16 = 3;
pub const IFA_BROADCAST: u16 = 4;
pub const IFA_CACHEINFO: u16 = 6;
pub const IFA_FLAGS: u16 = 8;
pub const IFA_FREEBSD: u16 = 11;
pub const IFAF_VHID: u16 = 1;
pub const IFAF_FLAGS: u16 = 2;

pub const NL_RTA_DST: u16 = 1;
pub const NL_RTA_OIF: u16 = 4;
pub const NL_RTA_GATEWAY: u16 = 5;
pub const NL_RTA_METRICS: u16 = 8;
pub const NL_RTA_KNH_ID: u16 = 10;
pub const NL_RTA_WEIGHT: u16 = 13;
pub const NL_RTA_RTFLAGS: u16 = 14;
pub const NL_RTA_TABLE: u16 = 15;
pub const NL_RTAX_MTU: u16 = 2;
pub const RTN_UNICAST: u8 = 1;
pub const RTN_BLACKHOLE: u8 = 6;
pub const RTN_UNREACHABLE: u8 = 7;
pub const RTN_PROHIBIT: u8 = 8;
pub const RTPROT_KERNEL: u8 = 2;
pub const RTPROT_STATIC: u8 = 4;
pub const RT_SCOPE_UNIVERSE: u8 = 0;
pub const RT_SCOPE_LINK: u8 = 253;
pub const RT_SCOPE_HOST: u8 = 254;

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("NETLINK_ROUTE", NETLINK_ROUTE as i64),
    c("NETLINK_GENERIC", NETLINK_GENERIC as i64),
    c("SOL_NETLINK", SOL_NETLINK as i64),
    c("NETLINK_ADD_MEMBERSHIP", NETLINK_ADD_MEMBERSHIP as i64),
    c("NETLINK_DROP_MEMBERSHIP", NETLINK_DROP_MEMBERSHIP as i64),
    c("NETLINK_LIST_MEMBERSHIPS", NETLINK_LIST_MEMBERSHIPS as i64),
    c("NETLINK_CAP_ACK", NETLINK_CAP_ACK as i64),
    c("NETLINK_EXT_ACK", NETLINK_EXT_ACK as i64),
    c("NETLINK_GET_STRICT_CHK", NETLINK_GET_STRICT_CHK as i64),
    c("NETLINK_MSG_INFO", NETLINK_MSG_INFO as i64),
    c("NLMSG_NOOP", NLMSG_NOOP as i64),
    c("NLMSG_ERROR", NLMSG_ERROR as i64),
    c("NLMSG_DONE", NLMSG_DONE as i64),
    c("NLMSG_MIN_TYPE", NLMSG_MIN_TYPE as i64),
    c("NLM_F_REQUEST", NLM_F_REQUEST as i64),
    c("NLM_F_MULTI", NLM_F_MULTI as i64),
    c("NLM_F_ACK", NLM_F_ACK as i64),
    c("NLM_F_DUMP", NLM_F_DUMP as i64),
    c("NLM_F_REPLACE", NLM_F_REPLACE as i64),
    c("NLM_F_EXCL", NLM_F_EXCL as i64),
    c("NLM_F_CREATE", NLM_F_CREATE as i64),
    c("NLM_F_CAPPED", NLM_F_CAPPED as i64),
    c("NLM_F_ACK_TLVS", NLM_F_ACK_TLVS as i64),
    c("NLMSGERR_ATTR_MSG", NLMSGERR_ATTR_MSG as i64),
    c("NL_RTM_NEWLINK", RTM_NEWLINK as i64),
    c("NL_RTM_DELLINK", RTM_DELLINK as i64),
    c("NL_RTM_GETLINK", RTM_GETLINK as i64),
    c("NL_RTM_NEWADDR", RTM_NEWADDR as i64),
    c("NL_RTM_DELADDR", RTM_DELADDR as i64),
    c("NL_RTM_GETADDR", RTM_GETADDR as i64),
    c("NL_RTM_NEWROUTE", RTM_NEWROUTE as i64),
    c("NL_RTM_DELROUTE", RTM_DELROUTE as i64),
    c("NL_RTM_GETROUTE", RTM_GETROUTE as i64),
    c("NL_RTM_GETNEIGH", RTM_GETNEIGH as i64),
    c("IFLA_ADDRESS", IFLA_ADDRESS as i64),
    c("IFLA_IFNAME", IFLA_IFNAME as i64),
    c("IFLA_MTU", IFLA_MTU as i64),
    c("IFLA_OPERSTATE", IFLA_OPERSTATE as i64),
    c("IFLA_IFALIAS", IFLA_IFALIAS as i64),
    c("IFLA_STATS64", IFLA_STATS64 as i64),
    c("IFLA_PROMISCUITY", IFLA_PROMISCUITY as i64),
    c("IFLA_CARRIER", IFLA_CARRIER as i64),
    c("IFLA_FREEBSD", IFLA_FREEBSD as i64),
    c("IFLAF_ORIG_HWADDR", IFLAF_ORIG_HWADDR as i64),
    c("IFLAF_CAPS", IFLAF_CAPS as i64),
    c("IF_OPER_DOWN", IF_OPER_DOWN as i64),
    c("IF_OPER_UP", IF_OPER_UP as i64),
    c("NLA_BITSET_SIZE", NLA_BITSET_SIZE as i64),
    c("NLA_BITSET_VALUE", NLA_BITSET_VALUE as i64),
    c("NLA_BITSET_MASK", NLA_BITSET_MASK as i64),
    c("IFA_ADDRESS", IFA_ADDRESS as i64),
    c("IFA_LOCAL", IFA_LOCAL as i64),
    c("IFA_LABEL", IFA_LABEL as i64),
    c("IFA_BROADCAST", IFA_BROADCAST as i64),
    c("IFA_CACHEINFO", IFA_CACHEINFO as i64),
    c("IFA_FLAGS", IFA_FLAGS as i64),
    c("IFA_FREEBSD", IFA_FREEBSD as i64),
    c("IFAF_VHID", IFAF_VHID as i64),
    c("IFAF_FLAGS", IFAF_FLAGS as i64),
    c("NL_RTA_DST", NL_RTA_DST as i64),
    c("NL_RTA_OIF", NL_RTA_OIF as i64),
    c("NL_RTA_GATEWAY", NL_RTA_GATEWAY as i64),
    c("NL_RTA_METRICS", NL_RTA_METRICS as i64),
    c("NL_RTA_KNH_ID", NL_RTA_KNH_ID as i64),
    c("NL_RTA_WEIGHT", NL_RTA_WEIGHT as i64),
    c("NL_RTA_RTFLAGS", NL_RTA_RTFLAGS as i64),
    c("NL_RTA_TABLE", NL_RTA_TABLE as i64),
    c("NL_RTAX_MTU", NL_RTAX_MTU as i64),
    c("RTN_UNICAST", RTN_UNICAST as i64),
    c("RTN_BLACKHOLE", RTN_BLACKHOLE as i64),
    c("RTN_UNREACHABLE", RTN_UNREACHABLE as i64),
    c("RTN_PROHIBIT", RTN_PROHIBIT as i64),
    c("RTPROT_KERNEL", RTPROT_KERNEL as i64),
    c("RTPROT_STATIC", RTPROT_STATIC as i64),
    c("RT_SCOPE_UNIVERSE", RT_SCOPE_UNIVERSE as i64),
    c("RT_SCOPE_LINK", RT_SCOPE_LINK as i64),
    c("RT_SCOPE_HOST", RT_SCOPE_HOST as i64),
];

/// Every netlink header the constants come from, one including the rest.
const fn c(c_name: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header: "netlink/netlink_route.h",
        value,
        arches: Arches::All,
    }
}
