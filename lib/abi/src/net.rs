// SPDX-License-Identifier: BSD-2-Clause
//! Interfaces and routes (`net/if.h`, `net/if_dl.h`, `net/if_types.h`,
//! `net/route.h`, `sys/sockio.h`, `netinet/in_var.h`, `netinet6/in6_var.h`,
//! `netinet6/nd6.h`): what `ifconfig`, `netstat -i` and `-r`, `route`, and
//! `getifaddrs(3)` read and set.

use crate::layout::{Arches, ConstValue};

/// `IFNAMSIZ`.
pub const IFNAMSIZ: usize = 16;
/// `IFSTATMAX`.
pub const IFSTATMAX: usize = 800;

/// `if_flags` (`IFF_*`).
pub mod iff {
    pub const UP: u32 = 0x1;
    pub const BROADCAST: u32 = 0x2;
    pub const DEBUG: u32 = 0x4;
    pub const LOOPBACK: u32 = 0x8;
    pub const POINTOPOINT: u32 = 0x10;
    pub const NEEDSEPOCH: u32 = 0x20;
    pub const RUNNING: u32 = 0x40;
    pub const NOARP: u32 = 0x80;
    pub const PROMISC: u32 = 0x100;
    pub const ALLMULTI: u32 = 0x200;
    pub const OACTIVE: u32 = 0x400;
    pub const SIMPLEX: u32 = 0x800;
    pub const LINK0: u32 = 0x1000;
    pub const LINK1: u32 = 0x2000;
    pub const LINK2: u32 = 0x4000;
    pub const MULTICAST: u32 = 0x8000;
    pub const CANTCONFIG: u32 = 0x1_0000;
    pub const PPROMISC: u32 = 0x2_0000;
    pub const MONITOR: u32 = 0x4_0000;
    pub const STATICARP: u32 = 0x8_0000;
    pub const DYING: u32 = 0x20_0000;
    pub const RENAMING: u32 = 0x40_0000;
    /// `IFF_NETLINK_1`, which netlink reports as `IFF_LOWER_UP`.
    pub const LOWER_UP: u32 = 0x100_0000;
    /// `IFF_CANTCHANGE`: what `SIOCSIFFLAGS` leaves alone.
    pub const CANTCHANGE: u32 = BROADCAST
        | POINTOPOINT
        | RUNNING
        | OACTIVE
        | SIMPLEX
        | MULTICAST
        | ALLMULTI
        | PROMISC
        | DYING
        | CANTCONFIG
        | NEEDSEPOCH;
}

/// Capabilities (`IFCAP_*`).
pub mod ifcap {
    pub const RXCSUM: u32 = 1 << 0;
    pub const TXCSUM: u32 = 1 << 1;
    pub const LINKSTATE: u32 = 1 << 19;
    pub const RXCSUM_IPV6: u32 = 1 << 21;
    pub const TXCSUM_IPV6: u32 = 1 << 22;
}

/// `IFT_LOOP`, `LINK_STATE_UP`.
pub const IFT_LOOP: u8 = 0x18;
pub const LINK_STATE_UNKNOWN: u8 = 0;
pub const LINK_STATE_DOWN: u8 = 1;
pub const LINK_STATE_UP: u8 = 2;

/// `struct sockaddr_dl`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SockaddrDl {
    pub sdl_len: u8,
    pub sdl_family: u8,
    pub sdl_index: u16,
    pub sdl_type: u8,
    pub sdl_nlen: u8,
    pub sdl_alen: u8,
    pub sdl_slen: u8,
    pub sdl_data: [u8; 46],
}

/// `struct ifreq`: a name and a 16-byte union.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ifreq {
    pub ifr_name: [u8; IFNAMSIZ],
    pub ifr_ifru: [u64; 2],
}

/// `struct ifaliasreq` (`SIOCAIFADDR`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ifaliasreq {
    pub ifra_name: [u8; IFNAMSIZ],
    pub ifra_addr: [u8; 16],
    pub ifra_broadaddr: [u8; 16],
    pub ifra_mask: [u8; 16],
    pub ifra_vhid: i32,
}

/// `struct in6_addrlifetime`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct In6Addrlifetime {
    pub ia6t_expire: i64,
    pub ia6t_preferred: i64,
    pub ia6t_vltime: u32,
    pub ia6t_pltime: u32,
}

/// `struct in6_aliasreq` (`SIOCAIFADDR_IN6`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct In6Aliasreq {
    pub ifra_name: [u8; IFNAMSIZ],
    pub ifra_addr: crate::inet::SockaddrIn6,
    pub ifra_dstaddr: crate::inet::SockaddrIn6,
    pub ifra_prefixmask: crate::inet::SockaddrIn6,
    pub ifra_flags: i32,
    pub ifra_lifetime: In6Addrlifetime,
    pub ifra_vhid: i32,
}

/// `struct in6_ifreq`: a name and its union (`struct icmp6_ifstat` the
/// largest member, 34 counters).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct In6Ifreq {
    pub ifr_name: [u8; IFNAMSIZ],
    pub ifr_ifru: [u64; 34],
}

/// `struct nd_ifinfo`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NdIfinfo {
    pub linkmtu: u32,
    pub maxmtu: u32,
    pub basereachable: u32,
    pub reachable: u32,
    pub retrans: u32,
    pub flags: u32,
    pub recalctm: i32,
    pub chlim: u8,
    pub initialized: u8,
    pub randomseed0: [u8; 8],
    pub randomseed1: [u8; 8],
    pub randomid: [u8; 8],
    pub _pad: [u8; 2],
}

/// `struct in6_ndireq` (`SIOCGIFINFO_IN6`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct In6Ndireq {
    pub ifname: [u8; IFNAMSIZ],
    pub ndi: NdIfinfo,
}

/// `struct in6_ndifreq` (`SIOCGDEFIFACE_IN6`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct In6Ndifreq {
    pub ifname: [u8; IFNAMSIZ],
    pub ifindex: u64,
}

/// `struct in6_addrpolicy` (`SIOCAADDRCTL_POLICY`,
/// `net.inet6.ip6.addrctlpolicy`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct In6Addrpolicy {
    pub addr: crate::inet::SockaddrIn6,
    pub addrmask: crate::inet::SockaddrIn6,
    pub preced: i32,
    pub label: i32,
    pub r#use: u64,
}

/// `IPV6CTL_ADDRCTLPOLICY`: `net.inet6.ip6.addrctlpolicy`'s number.
pub const IPV6CTL_ADDRCTLPOLICY: i32 = 38;

/// `struct ifgroupreq` (`SIOCGIFGROUP`): `ifgr_groups` a user pointer.
#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ifgroupreq {
    pub ifgr_name: [u8; IFNAMSIZ],
    pub ifgr_len: u32,
    pub _pad: u32,
    pub ifgr_ifgru: [u8; IFNAMSIZ],
}

/// `struct ifconf` (`SIOCGIFCONF`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ifconf {
    pub ifc_len: i32,
    pub _pad: u32,
    pub ifc_buf: u64,
}

/// `struct ifstat` (`SIOCGIFSTATUS`).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ifstat {
    pub ifs_name: [u8; IFNAMSIZ],
    pub ascii: [u8; IFSTATMAX + 1],
}

/// `struct if_data`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IfData {
    pub ifi_type: u8,
    pub ifi_physical: u8,
    pub ifi_addrlen: u8,
    pub ifi_hdrlen: u8,
    pub ifi_link_state: u8,
    pub ifi_vhid: u8,
    pub ifi_datalen: u16,
    pub ifi_mtu: u32,
    pub ifi_metric: u32,
    pub ifi_baudrate: u64,
    pub ifi_ipackets: u64,
    pub ifi_ierrors: u64,
    pub ifi_opackets: u64,
    pub ifi_oerrors: u64,
    pub ifi_collisions: u64,
    pub ifi_ibytes: u64,
    pub ifi_obytes: u64,
    pub ifi_imcasts: u64,
    pub ifi_omcasts: u64,
    pub ifi_iqdrops: u64,
    pub ifi_oqdrops: u64,
    pub ifi_noproto: u64,
    pub ifi_hwassist: u64,
    pub ifi_epoch: i64,
    pub ifi_lastchange: [i64; 2],
}

/// `struct if_msghdrl` (`NET_RT_IFLISTL`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IfMsghdrl {
    pub ifm_msglen: u16,
    pub ifm_version: u8,
    pub ifm_type: u8,
    pub ifm_addrs: i32,
    pub ifm_flags: i32,
    pub ifm_index: u16,
    pub _ifm_spare1: u16,
    pub ifm_len: u16,
    pub ifm_data_off: u16,
    pub _ifm_spare2: i32,
    pub ifm_data: IfData,
}

/// `struct if_msghdr` (`NET_RT_IFLIST`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IfMsghdr {
    pub ifm_msglen: u16,
    pub ifm_version: u8,
    pub ifm_type: u8,
    pub ifm_addrs: i32,
    pub ifm_flags: i32,
    pub ifm_index: u16,
    pub _ifm_spare1: u16,
    pub ifm_data: IfData,
}

/// `struct ifa_msghdrl`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IfaMsghdrl {
    pub ifam_msglen: u16,
    pub ifam_version: u8,
    pub ifam_type: u8,
    pub ifam_addrs: i32,
    pub ifam_flags: i32,
    pub ifam_index: u16,
    pub _ifam_spare1: u16,
    pub ifam_len: u16,
    pub ifam_data_off: u16,
    pub ifam_metric: i32,
    pub ifam_data: IfData,
}

/// `struct ifa_msghdr`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IfaMsghdr {
    pub ifam_msglen: u16,
    pub ifam_version: u8,
    pub ifam_type: u8,
    pub ifam_addrs: i32,
    pub ifam_flags: i32,
    pub ifam_index: u16,
    pub _ifam_spare1: u16,
    pub ifam_metric: i32,
}

/// `struct rt_metrics`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RtMetrics {
    pub rmx_locks: u64,
    pub rmx_mtu: u64,
    pub rmx_hopcount: u64,
    pub rmx_expire: u64,
    pub rmx_recvpipe: u64,
    pub rmx_sendpipe: u64,
    pub rmx_ssthresh: u64,
    pub rmx_rtt: u64,
    pub rmx_rttvar: u64,
    pub rmx_pksent: u64,
    pub rmx_weight: u64,
    pub rmx_nhidx: u64,
    pub rmx_filler: [u64; 2],
}

/// `struct rt_msghdr`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RtMsghdr {
    pub rtm_msglen: u16,
    pub rtm_version: u8,
    pub rtm_type: u8,
    pub rtm_index: u16,
    pub _rtm_spare1: u16,
    pub rtm_flags: i32,
    pub rtm_addrs: i32,
    pub rtm_pid: i32,
    pub rtm_seq: i32,
    pub rtm_errno: i32,
    pub rtm_fmask: i32,
    pub rtm_inits: u64,
    pub rtm_rmx: RtMetrics,
}

/// `RTM_VERSION` and message types.
pub const RTM_VERSION: u8 = 5;
pub mod rtm {
    pub const ADD: u8 = 0x1;
    pub const DELETE: u8 = 0x2;
    pub const CHANGE: u8 = 0x3;
    pub const GET: u8 = 0x4;
    pub const NEWADDR: u8 = 0xc;
    pub const DELADDR: u8 = 0xd;
    pub const IFINFO: u8 = 0xe;
    pub const NEWMADDR: u8 = 0xf;
    pub const IFANNOUNCE: u8 = 0x11;
}
/// `rtm_addrs` bits.
pub mod rta {
    pub const DST: i32 = 0x1;
    pub const GATEWAY: i32 = 0x2;
    pub const NETMASK: i32 = 0x4;
    pub const GENMASK: i32 = 0x8;
    pub const IFP: i32 = 0x10;
    pub const IFA: i32 = 0x20;
    pub const AUTHOR: i32 = 0x40;
    pub const BRD: i32 = 0x80;
}
/// Route flags (`RTF_*`).
pub mod rtf {
    pub const UP: i32 = 0x1;
    pub const GATEWAY: i32 = 0x2;
    pub const HOST: i32 = 0x4;
    pub const REJECT: i32 = 0x8;
    pub const DONE: i32 = 0x40;
    pub const STATIC: i32 = 0x800;
    pub const BLACKHOLE: i32 = 0x1000;
    pub const PINNED: i32 = 0x10_0000;
    pub const LOCAL: i32 = 0x20_0000;
}
/// `NET_RT_*`: the requests under `net.route.<af>`.
pub mod net_rt {
    pub const DUMP: i32 = 1;
    pub const FLAGS: i32 = 2;
    pub const IFLIST: i32 = 3;
    pub const IFMALIST: i32 = 4;
    pub const IFLISTL: i32 = 5;
    pub const NHOP: i32 = 6;
    pub const NHGRP: i32 = 7;
}

/// IPv6 address flags (`IN6_IFF_*`) and interface ND flags
/// (`ND6_IFF_*`).
pub mod in6_iff {
    pub const ANYCAST: i32 = 0x01;
    pub const TENTATIVE: i32 = 0x02;
    pub const DUPLICATED: i32 = 0x04;
    pub const DETACHED: i32 = 0x08;
    pub const DEPRECATED: i32 = 0x10;
    pub const NODAD: i32 = 0x20;
    pub const AUTOCONF: i32 = 0x40;
    pub const TEMPORARY: i32 = 0x80;
}
pub mod nd6_iff {
    pub const PERFORMNUD: u32 = 0x1;
    pub const ACCEPT_RTADV: u32 = 0x2;
    pub const IFDISABLED: u32 = 0x8;
    pub const DONT_SET_IFROUTE: u32 = 0x10;
    pub const AUTO_LINKLOCAL: u32 = 0x20;
    pub const NO_RADR: u32 = 0x40;
    pub const NO_PREFER_IFACE: u32 = 0x80;
    pub const NO_DAD: u32 = 0x100;
}

const fn ioc(dir: u32, group: u8, num: u8, len: usize) -> u32 {
    #[allow(clippy::cast_possible_truncation, reason = "ioctl sizes fit")]
    let len = len as u32;
    dir | ((len & crate::ioctl::IOCPARM_MASK) << 16) | (group as u32) << 8 | num as u32
}
const IN: u32 = crate::ioctl::IOC_IN;
const OUT: u32 = crate::ioctl::IOC_OUT;
const INOUT: u32 = IN | OUT;
const IFREQ: usize = size_of::<Ifreq>();
const IN6IFREQ: usize = size_of::<In6Ifreq>();

/// Socket ioctls (`sys/sockio.h`) and the IPv6 ones (`in6_var.h`).
pub mod sio {
    use super::{
        IFREQ, IN, IN6IFREQ, INOUT, Ifaliasreq, Ifconf, Ifgroupreq, Ifstat, In6Addrpolicy,
        In6Aliasreq, In6Ndifreq, In6Ndireq, OUT, ioc,
    };
    pub const SIOCATMARK: u32 = ioc(OUT, b's', 7, 4);
    pub const SIOCSPGRP: u32 = ioc(IN, b's', 8, 4);
    pub const SIOCGPGRP: u32 = ioc(OUT, b's', 9, 4);
    pub const SIOCSIFADDR: u32 = ioc(IN, b'i', 12, IFREQ);
    pub const SIOCGIFADDR: u32 = ioc(INOUT, b'i', 33, IFREQ);
    pub const SIOCGIFDSTADDR: u32 = ioc(INOUT, b'i', 34, IFREQ);
    pub const SIOCSIFFLAGS: u32 = ioc(IN, b'i', 16, IFREQ);
    pub const SIOCGIFFLAGS: u32 = ioc(INOUT, b'i', 17, IFREQ);
    pub const SIOCGIFBRDADDR: u32 = ioc(INOUT, b'i', 35, IFREQ);
    pub const SIOCGIFCONF: u32 = ioc(INOUT, b'i', 36, size_of::<Ifconf>());
    pub const SIOCGIFNETMASK: u32 = ioc(INOUT, b'i', 37, IFREQ);
    pub const SIOCGIFMETRIC: u32 = ioc(INOUT, b'i', 23, IFREQ);
    pub const SIOCSIFMETRIC: u32 = ioc(IN, b'i', 24, IFREQ);
    pub const SIOCDIFADDR: u32 = ioc(IN, b'i', 25, IFREQ);
    pub const SIOCSIFCAP: u32 = ioc(IN, b'i', 30, IFREQ);
    pub const SIOCGIFCAP: u32 = ioc(INOUT, b'i', 31, IFREQ);
    pub const SIOCGIFINDEX: u32 = ioc(INOUT, b'i', 32, IFREQ);
    pub const SIOCSIFNAME: u32 = ioc(IN, b'i', 40, IFREQ);
    pub const SIOCSIFDESCR: u32 = ioc(IN, b'i', 41, IFREQ);
    pub const SIOCGIFDESCR: u32 = ioc(INOUT, b'i', 42, IFREQ);
    pub const SIOCAIFADDR: u32 = ioc(IN, b'i', 43, size_of::<Ifaliasreq>());
    pub const SIOCGIFDATA: u32 = ioc(IN, b'i', 44, IFREQ);
    pub const SIOCGIFALIAS: u32 = ioc(INOUT, b'i', 45, size_of::<Ifaliasreq>());
    pub const SIOCADDMULTI: u32 = ioc(IN, b'i', 49, IFREQ);
    pub const SIOCDELMULTI: u32 = ioc(IN, b'i', 50, IFREQ);
    pub const SIOCGIFMTU: u32 = ioc(INOUT, b'i', 51, IFREQ);
    pub const SIOCSIFMTU: u32 = ioc(IN, b'i', 52, IFREQ);
    pub const SIOCGIFPHYS: u32 = ioc(INOUT, b'i', 53, IFREQ);
    /// `struct ifmediareq` is 48 bytes on every 64-bit architecture.
    pub const SIOCGIFMEDIA: u32 = ioc(INOUT, b'i', 56, 48);
    pub const SIOCGIFXMEDIA: u32 = ioc(INOUT, b'i', 139, 48);
    pub const SIOCGIFSTATUS: u32 = ioc(INOUT, b'i', 59, size_of::<Ifstat>());
    pub const SIOCGIFPSRCADDR: u32 = ioc(INOUT, b'i', 71, IFREQ);
    pub const SIOCGIFPDSTADDR: u32 = ioc(INOUT, b'i', 72, IFREQ);
    pub const SIOCGIFFIB: u32 = ioc(INOUT, b'i', 92, IFREQ);
    pub const SIOCSIFFIB: u32 = ioc(IN, b'i', 93, IFREQ);
    pub const SIOCGTUNFIB: u32 = ioc(INOUT, b'i', 94, IFREQ);
    pub const SIOCIFCREATE2: u32 = ioc(INOUT, b'i', 124, IFREQ);
    pub const SIOCIFDESTROY: u32 = ioc(IN, b'i', 121, IFREQ);
    pub const SIOCAIFGROUP: u32 = ioc(IN, b'i', 135, size_of::<Ifgroupreq>());
    pub const SIOCGIFGROUP: u32 = ioc(INOUT, b'i', 136, size_of::<Ifgroupreq>());
    pub const SIOCDIFGROUP: u32 = ioc(IN, b'i', 137, size_of::<Ifgroupreq>());
    pub const SIOCGIFGMEMB: u32 = ioc(INOUT, b'i', 138, size_of::<Ifgroupreq>());
    pub const SIOCGLANPCP: u32 = ioc(INOUT, b'i', 152, IFREQ);
    pub const SIOCGIFCAPNV: u32 = ioc(INOUT, b'i', 156, IFREQ);
    pub const SIOCGIFADDR_IN6: u32 = ioc(INOUT, b'i', 33, IN6IFREQ);
    pub const SIOCGIFDSTADDR_IN6: u32 = ioc(INOUT, b'i', 34, IN6IFREQ);
    pub const SIOCGIFNETMASK_IN6: u32 = ioc(INOUT, b'i', 37, IN6IFREQ);
    pub const SIOCDIFADDR_IN6: u32 = ioc(IN, b'i', 25, IN6IFREQ);
    pub const SIOCAIFADDR_IN6: u32 = ioc(IN, b'i', 27, size_of::<In6Aliasreq>());
    pub const SIOCGIFPSRCADDR_IN6: u32 = ioc(INOUT, b'i', 71, IN6IFREQ);
    pub const SIOCGIFPDSTADDR_IN6: u32 = ioc(INOUT, b'i', 72, IN6IFREQ);
    pub const SIOCGIFAFLAG_IN6: u32 = ioc(INOUT, b'i', 73, IN6IFREQ);
    pub const SIOCGIFALIFETIME_IN6: u32 = ioc(INOUT, b'i', 81, IN6IFREQ);
    pub const SIOCGIFSTAT_IN6: u32 = ioc(INOUT, b'i', 83, IN6IFREQ);
    pub const SIOCGIFSTAT_ICMP6: u32 = ioc(INOUT, b'i', 84, IN6IFREQ);
    pub const SIOCGIFINFO_IN6: u32 = ioc(INOUT, b'i', 108, size_of::<In6Ndireq>());
    pub const SIOCSIFINFO_IN6: u32 = ioc(INOUT, b'i', 109, size_of::<In6Ndireq>());
    pub const SIOCSDEFIFACE_IN6: u32 = ioc(INOUT, b'i', 85, size_of::<In6Ndifreq>());
    pub const SIOCGDEFIFACE_IN6: u32 = ioc(INOUT, b'i', 86, size_of::<In6Ndifreq>());
    pub const SIOCAADDRCTL_POLICY: u32 = ioc(IN, b'u', 108, size_of::<In6Addrpolicy>());
    pub const SIOCDADDRCTL_POLICY: u32 = ioc(IN, b'u', 109, size_of::<In6Addrpolicy>());
}

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("IFNAMSIZ", "net/if.h", IFNAMSIZ as i64),
    c("IFSTATMAX", "net/if.h", IFSTATMAX as i64),
    c("IFF_UP", "net/if.h", iff::UP as i64),
    c("IFF_BROADCAST", "net/if.h", iff::BROADCAST as i64),
    c("IFF_DEBUG", "net/if.h", iff::DEBUG as i64),
    c("IFF_LOOPBACK", "net/if.h", iff::LOOPBACK as i64),
    c("IFF_POINTOPOINT", "net/if.h", iff::POINTOPOINT as i64),
    c("IFF_RUNNING", "net/if.h", iff::RUNNING as i64),
    c("IFF_NOARP", "net/if.h", iff::NOARP as i64),
    c("IFF_PROMISC", "net/if.h", iff::PROMISC as i64),
    c("IFF_ALLMULTI", "net/if.h", iff::ALLMULTI as i64),
    c("IFF_OACTIVE", "net/if.h", iff::OACTIVE as i64),
    c("IFF_SIMPLEX", "net/if.h", iff::SIMPLEX as i64),
    c("IFF_LINK0", "net/if.h", iff::LINK0 as i64),
    c("IFF_LINK1", "net/if.h", iff::LINK1 as i64),
    c("IFF_LINK2", "net/if.h", iff::LINK2 as i64),
    c("IFF_MULTICAST", "net/if.h", iff::MULTICAST as i64),
    c("IFF_CANTCONFIG", "net/if.h", iff::CANTCONFIG as i64),
    c("IFF_PPROMISC", "net/if.h", iff::PPROMISC as i64),
    c("IFF_MONITOR", "net/if.h", iff::MONITOR as i64),
    c("IFF_STATICARP", "net/if.h", iff::STATICARP as i64),
    c("IFF_DYING", "net/if.h", iff::DYING as i64),
    c("IFF_RENAMING", "net/if.h", iff::RENAMING as i64),
    c("IFF_NETLINK_1", "net/if.h", iff::LOWER_UP as i64),
    c("IFF_CANTCHANGE", "net/if.h", iff::CANTCHANGE as i64),
    c("IFCAP_RXCSUM", "net/if.h", ifcap::RXCSUM as i64),
    c("IFCAP_TXCSUM", "net/if.h", ifcap::TXCSUM as i64),
    c("IFCAP_LINKSTATE", "net/if.h", ifcap::LINKSTATE as i64),
    c("IFCAP_RXCSUM_IPV6", "net/if.h", ifcap::RXCSUM_IPV6 as i64),
    c("IFCAP_TXCSUM_IPV6", "net/if.h", ifcap::TXCSUM_IPV6 as i64),
    c("IFT_LOOP", "net/if_types.h", IFT_LOOP as i64),
    c("LINK_STATE_UNKNOWN", "net/if.h", LINK_STATE_UNKNOWN as i64),
    c("LINK_STATE_DOWN", "net/if.h", LINK_STATE_DOWN as i64),
    c("LINK_STATE_UP", "net/if.h", LINK_STATE_UP as i64),
    c("RTM_VERSION", "net/route.h", RTM_VERSION as i64),
    c("RTM_ADD", "net/route.h", rtm::ADD as i64),
    c("RTM_DELETE", "net/route.h", rtm::DELETE as i64),
    c("RTM_CHANGE", "net/route.h", rtm::CHANGE as i64),
    c("RTM_GET", "net/route.h", rtm::GET as i64),
    c("RTM_IFINFO", "net/route.h", rtm::IFINFO as i64),
    c("RTM_NEWMADDR", "net/route.h", rtm::NEWMADDR as i64),
    c("RTM_IFANNOUNCE", "net/route.h", rtm::IFANNOUNCE as i64),
    c("RTA_DST", "net/route.h", rta::DST as i64),
    c("RTA_NETMASK", "net/route.h", rta::NETMASK as i64),
    c("RTA_GENMASK", "net/route.h", rta::GENMASK as i64),
    c("RTA_IFP", "net/route.h", rta::IFP as i64),
    c("RTA_IFA", "net/route.h", rta::IFA as i64),
    c("RTA_AUTHOR", "net/route.h", rta::AUTHOR as i64),
    c("RTA_BRD", "net/route.h", rta::BRD as i64),
    c("RTF_UP", "net/route.h", rtf::UP as i64),
    c("RTF_GATEWAY", "net/route.h", rtf::GATEWAY as i64),
    c("RTF_HOST", "net/route.h", rtf::HOST as i64),
    c("RTF_REJECT", "net/route.h", rtf::REJECT as i64),
    c("RTF_DONE", "net/route.h", rtf::DONE as i64),
    c("RTF_STATIC", "net/route.h", rtf::STATIC as i64),
    c("RTF_BLACKHOLE", "net/route.h", rtf::BLACKHOLE as i64),
    c("RTF_PINNED", "net/route.h", rtf::PINNED as i64),
    c("RTF_LOCAL", "net/route.h", rtf::LOCAL as i64),
    c("NET_RT_DUMP", "sys/socket.h", net_rt::DUMP as i64),
    c("NET_RT_FLAGS", "sys/socket.h", net_rt::FLAGS as i64),
    c("NET_RT_IFLIST", "sys/socket.h", net_rt::IFLIST as i64),
    c("NET_RT_IFMALIST", "sys/socket.h", net_rt::IFMALIST as i64),
    c("NET_RT_IFLISTL", "sys/socket.h", net_rt::IFLISTL as i64),
    c("NET_RT_NHOP", "sys/socket.h", net_rt::NHOP as i64),
    c("NET_RT_NHGRP", "sys/socket.h", net_rt::NHGRP as i64),
    c(
        "IN6_IFF_ANYCAST",
        "netinet6/in6_var.h",
        in6_iff::ANYCAST as i64,
    ),
    c(
        "IN6_IFF_TENTATIVE",
        "netinet6/in6_var.h",
        in6_iff::TENTATIVE as i64,
    ),
    c(
        "IN6_IFF_DUPLICATED",
        "netinet6/in6_var.h",
        in6_iff::DUPLICATED as i64,
    ),
    c(
        "IN6_IFF_DETACHED",
        "netinet6/in6_var.h",
        in6_iff::DETACHED as i64,
    ),
    c(
        "IN6_IFF_DEPRECATED",
        "netinet6/in6_var.h",
        in6_iff::DEPRECATED as i64,
    ),
    c("IN6_IFF_NODAD", "netinet6/in6_var.h", in6_iff::NODAD as i64),
    c(
        "IN6_IFF_AUTOCONF",
        "netinet6/in6_var.h",
        in6_iff::AUTOCONF as i64,
    ),
    c(
        "IN6_IFF_TEMPORARY",
        "netinet6/in6_var.h",
        in6_iff::TEMPORARY as i64,
    ),
    c(
        "ND6_IFF_PERFORMNUD",
        "netinet6/nd6.h",
        nd6_iff::PERFORMNUD as i64,
    ),
    c(
        "ND6_IFF_ACCEPT_RTADV",
        "netinet6/nd6.h",
        nd6_iff::ACCEPT_RTADV as i64,
    ),
    c(
        "ND6_IFF_IFDISABLED",
        "netinet6/nd6.h",
        nd6_iff::IFDISABLED as i64,
    ),
    c(
        "ND6_IFF_DONT_SET_IFROUTE",
        "netinet6/nd6.h",
        nd6_iff::DONT_SET_IFROUTE as i64,
    ),
    c(
        "ND6_IFF_AUTO_LINKLOCAL",
        "netinet6/nd6.h",
        nd6_iff::AUTO_LINKLOCAL as i64,
    ),
    c("ND6_IFF_NO_RADR", "netinet6/nd6.h", nd6_iff::NO_RADR as i64),
    c(
        "ND6_IFF_NO_PREFER_IFACE",
        "netinet6/nd6.h",
        nd6_iff::NO_PREFER_IFACE as i64,
    ),
    c("ND6_IFF_NO_DAD", "netinet6/nd6.h", nd6_iff::NO_DAD as i64),
    c("SIOCATMARK", "sys/sockio.h", sio::SIOCATMARK as i64),
    c("SIOCSPGRP", "sys/sockio.h", sio::SIOCSPGRP as i64),
    c("SIOCGPGRP", "sys/sockio.h", sio::SIOCGPGRP as i64),
    c("SIOCSIFADDR", "sys/sockio.h", sio::SIOCSIFADDR as i64),
    c("SIOCGIFADDR", "sys/sockio.h", sio::SIOCGIFADDR as i64),
    c("SIOCGIFDSTADDR", "sys/sockio.h", sio::SIOCGIFDSTADDR as i64),
    c("SIOCSIFFLAGS", "sys/sockio.h", sio::SIOCSIFFLAGS as i64),
    c("SIOCGIFFLAGS", "sys/sockio.h", sio::SIOCGIFFLAGS as i64),
    c("SIOCGIFBRDADDR", "sys/sockio.h", sio::SIOCGIFBRDADDR as i64),
    c("SIOCGIFCONF", "sys/sockio.h", sio::SIOCGIFCONF as i64),
    c("SIOCGIFNETMASK", "sys/sockio.h", sio::SIOCGIFNETMASK as i64),
    c("SIOCGIFMETRIC", "sys/sockio.h", sio::SIOCGIFMETRIC as i64),
    c("SIOCSIFMETRIC", "sys/sockio.h", sio::SIOCSIFMETRIC as i64),
    c("SIOCDIFADDR", "sys/sockio.h", sio::SIOCDIFADDR as i64),
    c("SIOCSIFCAP", "sys/sockio.h", sio::SIOCSIFCAP as i64),
    c("SIOCGIFCAP", "sys/sockio.h", sio::SIOCGIFCAP as i64),
    c("SIOCGIFINDEX", "sys/sockio.h", sio::SIOCGIFINDEX as i64),
    c("SIOCSIFNAME", "sys/sockio.h", sio::SIOCSIFNAME as i64),
    c("SIOCSIFDESCR", "sys/sockio.h", sio::SIOCSIFDESCR as i64),
    c("SIOCGIFDESCR", "sys/sockio.h", sio::SIOCGIFDESCR as i64),
    c("SIOCAIFADDR", "sys/sockio.h", sio::SIOCAIFADDR as i64),
    c("SIOCGIFDATA", "sys/sockio.h", sio::SIOCGIFDATA as i64),
    c("SIOCGIFALIAS", "sys/sockio.h", sio::SIOCGIFALIAS as i64),
    c("SIOCADDMULTI", "sys/sockio.h", sio::SIOCADDMULTI as i64),
    c("SIOCDELMULTI", "sys/sockio.h", sio::SIOCDELMULTI as i64),
    c("SIOCGIFMTU", "sys/sockio.h", sio::SIOCGIFMTU as i64),
    c("SIOCSIFMTU", "sys/sockio.h", sio::SIOCSIFMTU as i64),
    c("SIOCGIFPHYS", "sys/sockio.h", sio::SIOCGIFPHYS as i64),
    c("SIOCGIFMEDIA", "sys/sockio.h", sio::SIOCGIFMEDIA as i64),
    c("SIOCGIFXMEDIA", "sys/sockio.h", sio::SIOCGIFXMEDIA as i64),
    c("SIOCGIFSTATUS", "sys/sockio.h", sio::SIOCGIFSTATUS as i64),
    c(
        "SIOCGIFPSRCADDR",
        "sys/sockio.h",
        sio::SIOCGIFPSRCADDR as i64,
    ),
    c(
        "SIOCGIFPDSTADDR",
        "sys/sockio.h",
        sio::SIOCGIFPDSTADDR as i64,
    ),
    c("SIOCGIFFIB", "sys/sockio.h", sio::SIOCGIFFIB as i64),
    c("SIOCSIFFIB", "sys/sockio.h", sio::SIOCSIFFIB as i64),
    c("SIOCGTUNFIB", "sys/sockio.h", sio::SIOCGTUNFIB as i64),
    c("SIOCIFCREATE2", "sys/sockio.h", sio::SIOCIFCREATE2 as i64),
    c("SIOCIFDESTROY", "sys/sockio.h", sio::SIOCIFDESTROY as i64),
    c("SIOCAIFGROUP", "sys/sockio.h", sio::SIOCAIFGROUP as i64),
    c("SIOCGIFGROUP", "sys/sockio.h", sio::SIOCGIFGROUP as i64),
    c("SIOCDIFGROUP", "sys/sockio.h", sio::SIOCDIFGROUP as i64),
    c("SIOCGIFGMEMB", "sys/sockio.h", sio::SIOCGIFGMEMB as i64),
    c("SIOCGLANPCP", "sys/sockio.h", sio::SIOCGLANPCP as i64),
    c("SIOCGIFCAPNV", "sys/sockio.h", sio::SIOCGIFCAPNV as i64),
    c(
        "SIOCGIFADDR_IN6",
        "netinet6/in6_var.h",
        sio::SIOCGIFADDR_IN6 as i64,
    ),
    c(
        "SIOCGIFDSTADDR_IN6",
        "netinet6/in6_var.h",
        sio::SIOCGIFDSTADDR_IN6 as i64,
    ),
    c(
        "SIOCGIFNETMASK_IN6",
        "netinet6/in6_var.h",
        sio::SIOCGIFNETMASK_IN6 as i64,
    ),
    c(
        "SIOCDIFADDR_IN6",
        "netinet6/in6_var.h",
        sio::SIOCDIFADDR_IN6 as i64,
    ),
    c(
        "SIOCAIFADDR_IN6",
        "netinet6/in6_var.h",
        sio::SIOCAIFADDR_IN6 as i64,
    ),
    c(
        "SIOCGIFPSRCADDR_IN6",
        "netinet6/in6_var.h",
        sio::SIOCGIFPSRCADDR_IN6 as i64,
    ),
    c(
        "SIOCGIFPDSTADDR_IN6",
        "netinet6/in6_var.h",
        sio::SIOCGIFPDSTADDR_IN6 as i64,
    ),
    c(
        "SIOCGIFAFLAG_IN6",
        "netinet6/in6_var.h",
        sio::SIOCGIFAFLAG_IN6 as i64,
    ),
    c(
        "SIOCGIFALIFETIME_IN6",
        "netinet6/in6_var.h",
        sio::SIOCGIFALIFETIME_IN6 as i64,
    ),
    c(
        "SIOCGIFSTAT_IN6",
        "netinet6/in6_var.h",
        sio::SIOCGIFSTAT_IN6 as i64,
    ),
    c(
        "SIOCGIFSTAT_ICMP6",
        "netinet6/in6_var.h",
        sio::SIOCGIFSTAT_ICMP6 as i64,
    ),
    c(
        "SIOCGIFINFO_IN6",
        "netinet6/in6_var.h",
        sio::SIOCGIFINFO_IN6 as i64,
    ),
    c(
        "SIOCSIFINFO_IN6",
        "netinet6/in6_var.h",
        sio::SIOCSIFINFO_IN6 as i64,
    ),
    c(
        "SIOCSDEFIFACE_IN6",
        "netinet6/in6_var.h",
        sio::SIOCSDEFIFACE_IN6 as i64,
    ),
    c(
        "SIOCGDEFIFACE_IN6",
        "netinet6/in6_var.h",
        sio::SIOCGDEFIFACE_IN6 as i64,
    ),
    c(
        "SIOCAADDRCTL_POLICY",
        "netinet6/in6_var.h",
        sio::SIOCAADDRCTL_POLICY as i64,
    ),
    c(
        "SIOCDADDRCTL_POLICY",
        "netinet6/in6_var.h",
        sio::SIOCDADDRCTL_POLICY as i64,
    ),
    c(
        "IPV6CTL_ADDRCTLPOLICY",
        "netinet/in.h",
        IPV6CTL_ADDRCTLPOLICY as i64,
    ),
];

const fn c(c_name: &'static str, header: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header,
        value,
        arches: Arches::All,
    }
}
