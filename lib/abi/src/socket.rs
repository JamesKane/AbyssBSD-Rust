// SPDX-License-Identifier: BSD-2-Clause
//! Sockets (`sys/socket.h`, `sys/un.h`): the parts UNIX-domain sockets
//! use.

use crate::layout::{Arches, ConstValue};

/// `AF_UNIX` (`AF_LOCAL`).
pub const AF_UNIX: u8 = 1;

pub const SOCK_STREAM: u32 = 1;
pub const SOCK_DGRAM: u32 = 2;
pub const SOCK_SEQPACKET: u32 = 5;
/// Flags `socket` takes with the type.
pub const SOCK_CLOEXEC: u32 = 0x1000_0000;
pub const SOCK_NONBLOCK: u32 = 0x2000_0000;
pub const SOCK_CLOFORK: u32 = 0x4000_0000;
/// The longest address a call takes.
pub const SOCK_MAXADDRLEN: u32 = 255;

/// `struct sockaddr_un`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SockaddrUn {
    pub sun_len: u8,
    pub sun_family: u8,
    pub sun_path: [u8; 104],
}

/// `SOL_SOCKET` and `SOL_LOCAL`.
pub const SOL_SOCKET: i32 = 0xffff;
pub const SOL_LOCAL: i32 = 0;

/// `SOL_SOCKET` boolean options (the `so_options` bits).
pub const SO_DEBUG: i32 = 0x0001;
pub const SO_ACCEPTCONN: i32 = 0x0002;
pub const SO_REUSEADDR: i32 = 0x0004;
pub const SO_KEEPALIVE: i32 = 0x0008;
pub const SO_DONTROUTE: i32 = 0x0010;
pub const SO_BROADCAST: i32 = 0x0020;
pub const SO_USELOOPBACK: i32 = 0x0040;
pub const SO_LINGER: i32 = 0x0080;
pub const SO_OOBINLINE: i32 = 0x0100;
pub const SO_REUSEPORT: i32 = 0x0200;
pub const SO_TIMESTAMP: i32 = 0x0400;
pub const SO_NOSIGPIPE: i32 = 0x0800;
pub const SO_ACCEPTFILTER: i32 = 0x1000;
pub const SO_BINTIME: i32 = 0x2000;
pub const SO_NO_OFFLOAD: i32 = 0x4000;
pub const SO_NO_DDP: i32 = 0x8000;
pub const SO_REUSEPORT_LB: i32 = 0x0001_0000;
pub const SO_RERROR: i32 = 0x0002_0000;
/// Valued options.
pub const SO_SNDBUF: i32 = 0x1001;
pub const SO_RCVBUF: i32 = 0x1002;
pub const SO_SNDLOWAT: i32 = 0x1003;
pub const SO_RCVLOWAT: i32 = 0x1004;
pub const SO_SNDTIMEO: i32 = 0x1005;
pub const SO_RCVTIMEO: i32 = 0x1006;
pub const SO_ERROR: i32 = 0x1007;
pub const SO_TYPE: i32 = 0x1008;
pub const SO_LABEL: i32 = 0x1009;
pub const SO_PEERLABEL: i32 = 0x1010;
pub const SO_LISTENQLIMIT: i32 = 0x1011;
pub const SO_LISTENQLEN: i32 = 0x1012;
pub const SO_LISTENINCQLEN: i32 = 0x1013;
pub const SO_FIB: i32 = 0x1014;
pub const SO_USER_COOKIE: i32 = 0x1015;
pub const SO_PROTOCOL: i32 = 0x1016;
pub const SO_TS_CLOCK: i32 = 0x1017;
pub const SO_MAX_PACING_RATE: i32 = 0x1018;
pub const SO_DOMAIN: i32 = 0x1019;
pub const SO_SPLICE: i32 = 0x1023;
pub const SO_TS_CLOCK_MAX: i32 = 3;

/// `SOL_LOCAL` options.
pub const LOCAL_PEERCRED: i32 = 1;
pub const LOCAL_CREDS: i32 = 2;
pub const LOCAL_CREDS_PERSISTENT: i32 = 3;

pub const MSG_OOB: i32 = 0x0001;
pub const MSG_PEEK: i32 = 0x0002;
pub const MSG_DONTROUTE: i32 = 0x0004;
pub const MSG_EOR: i32 = 0x0008;
pub const MSG_TRUNC: i32 = 0x0010;
pub const MSG_CTRUNC: i32 = 0x0020;
pub const MSG_WAITALL: i32 = 0x0040;
pub const MSG_DONTWAIT: i32 = 0x0080;
pub const MSG_EOF: i32 = 0x0100;
pub const MSG_NBIO: i32 = 0x4000;
pub const MSG_NOSIGNAL: i32 = 0x0002_0000;
pub const MSG_CMSG_CLOEXEC: i32 = 0x0004_0000;
pub const MSG_WAITFORONE: i32 = 0x0008_0000;
pub const MSG_CMSG_CLOFORK: i32 = 0x0040_0000;

pub const SCM_RIGHTS: i32 = 0x01;
pub const SCM_TIMESTAMP: i32 = 0x02;
pub const SCM_CREDS: i32 = 0x03;
pub const SCM_BINTIME: i32 = 0x04;
pub const SCM_REALTIME: i32 = 0x05;
pub const SCM_MONOTONIC: i32 = 0x06;
pub const SCM_CREDS2: i32 = 0x08;

pub const SHUT_RD: i32 = 0;
pub const SHUT_WR: i32 = 1;
pub const SHUT_RDWR: i32 = 2;

/// `somaxconn`, the most a backlog is.
pub const SOMAXCONN: i32 = 128;
/// `CMGROUP_MAX`, `XU_NGROUPS`, `XUCRED_VERSION`.
pub const CMGROUP_MAX: usize = 16;
pub const XU_NGROUPS: usize = 16;
pub const XUCRED_VERSION: u32 = 0;

/// `struct msghdr`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Msghdr {
    pub msg_name: u64,
    pub msg_namelen: u32,
    pub _pad0: u32,
    pub msg_iov: u64,
    pub msg_iovlen: i32,
    pub _pad1: u32,
    pub msg_control: u64,
    pub msg_controllen: u32,
    pub msg_flags: i32,
}

/// `struct cmsghdr`, followed by its data at [`CMSG_HDR`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cmsghdr {
    pub cmsg_len: u32,
    pub cmsg_level: i32,
    pub cmsg_type: i32,
}

/// `_ALIGNBYTES` on the 64-bit architectures.
pub const ALIGNBYTES: usize = 7;

/// `_ALIGN(n)`.
#[must_use]
pub const fn cmsg_align(n: usize) -> usize {
    (n + ALIGNBYTES) & !ALIGNBYTES
}

/// Where a control message's data starts (`CMSG_DATA` less the header).
pub const CMSG_HDR: usize = cmsg_align(size_of::<Cmsghdr>());

/// `CMSG_LEN(l)` and `CMSG_SPACE(l)`.
#[must_use]
pub const fn cmsg_len(l: usize) -> usize {
    CMSG_HDR + l
}
#[must_use]
pub const fn cmsg_space(l: usize) -> usize {
    CMSG_HDR + cmsg_align(l)
}

/// `struct xucred` (`LOCAL_PEERCRED`): pointer-aligned, for its union.
#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Xucred {
    pub cr_version: u32,
    pub cr_uid: u32,
    pub cr_ngroups: i16,
    pub _pad0: u16,
    pub cr_groups: [u32; XU_NGROUPS],
    pub _pad1: u32,
    /// The union of `cr_pid` and a pointer.
    pub cr_pid: i32,
    pub _pad2: u32,
}

/// `struct cmsgcred` (`SCM_CREDS` a sender attaches).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Cmsgcred {
    pub cmcred_pid: i32,
    pub cmcred_uid: u32,
    pub cmcred_euid: u32,
    pub cmcred_gid: u32,
    pub cmcred_ngroups: i16,
    pub _pad0: u16,
    pub cmcred_groups: [u32; CMGROUP_MAX],
}

/// `struct sockcred` without its groups (`SCM_CREDS` for `LOCAL_CREDS`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sockcred {
    pub sc_uid: u32,
    pub sc_euid: u32,
    pub sc_gid: u32,
    pub sc_egid: u32,
    pub sc_ngroups: i32,
    pub sc_groups: [u32; 1],
}

/// `struct sockcred2` without its groups (`SCM_CREDS2`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sockcred2 {
    pub sc_version: i32,
    pub sc_pid: i32,
    pub sc_uid: u32,
    pub sc_euid: u32,
    pub sc_gid: u32,
    pub sc_egid: u32,
    pub sc_ngroups: i32,
    pub sc_groups: [u32; 1],
}

/// `struct linger`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Linger {
    pub l_onoff: i32,
    pub l_linger: i32,
}

/// `struct splice`: `SO_SPLICE`'s argument, the sink's descriptor (-1
/// to unsplice), the most bytes to move (0: no limit), and the timeout.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Splice {
    pub sp_fd: i32,
    /// Alignment for `sp_max`.
    pub _pad0: [u8; 4],
    pub sp_max: i64,
    pub sp_idle: crate::time::Timeval,
}

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("AF_UNIX", "sys/socket.h", AF_UNIX as i64),
    c("SOCK_STREAM", "sys/socket.h", SOCK_STREAM as i64),
    c("SOCK_DGRAM", "sys/socket.h", SOCK_DGRAM as i64),
    c("SOCK_SEQPACKET", "sys/socket.h", SOCK_SEQPACKET as i64),
    c("SOCK_CLOEXEC", "sys/socket.h", SOCK_CLOEXEC as i64),
    c("SOCK_NONBLOCK", "sys/socket.h", SOCK_NONBLOCK as i64),
    c("SOCK_CLOFORK", "sys/socket.h", SOCK_CLOFORK as i64),
    c("SOCK_MAXADDRLEN", "sys/socket.h", SOCK_MAXADDRLEN as i64),
    c("SOL_SOCKET", "sys/socket.h", SOL_SOCKET as i64),
    c("SO_DEBUG", "sys/socket.h", SO_DEBUG as i64),
    c("SO_ACCEPTCONN", "sys/socket.h", SO_ACCEPTCONN as i64),
    c("SO_REUSEADDR", "sys/socket.h", SO_REUSEADDR as i64),
    c("SO_KEEPALIVE", "sys/socket.h", SO_KEEPALIVE as i64),
    c("SO_DONTROUTE", "sys/socket.h", SO_DONTROUTE as i64),
    c("SO_BROADCAST", "sys/socket.h", SO_BROADCAST as i64),
    c("SO_USELOOPBACK", "sys/socket.h", SO_USELOOPBACK as i64),
    c("SO_LINGER", "sys/socket.h", SO_LINGER as i64),
    c("SO_OOBINLINE", "sys/socket.h", SO_OOBINLINE as i64),
    c("SO_REUSEPORT", "sys/socket.h", SO_REUSEPORT as i64),
    c("SO_TIMESTAMP", "sys/socket.h", SO_TIMESTAMP as i64),
    c("SO_NOSIGPIPE", "sys/socket.h", SO_NOSIGPIPE as i64),
    c("SO_ACCEPTFILTER", "sys/socket.h", SO_ACCEPTFILTER as i64),
    c("SO_BINTIME", "sys/socket.h", SO_BINTIME as i64),
    c("SO_NO_OFFLOAD", "sys/socket.h", SO_NO_OFFLOAD as i64),
    c("SO_NO_DDP", "sys/socket.h", SO_NO_DDP as i64),
    c("SO_REUSEPORT_LB", "sys/socket.h", SO_REUSEPORT_LB as i64),
    c("SO_RERROR", "sys/socket.h", SO_RERROR as i64),
    c("SO_SNDBUF", "sys/socket.h", SO_SNDBUF as i64),
    c("SO_RCVBUF", "sys/socket.h", SO_RCVBUF as i64),
    c("SO_SNDLOWAT", "sys/socket.h", SO_SNDLOWAT as i64),
    c("SO_RCVLOWAT", "sys/socket.h", SO_RCVLOWAT as i64),
    c("SO_SNDTIMEO", "sys/socket.h", SO_SNDTIMEO as i64),
    c("SO_RCVTIMEO", "sys/socket.h", SO_RCVTIMEO as i64),
    c("SO_ERROR", "sys/socket.h", SO_ERROR as i64),
    c("SO_TYPE", "sys/socket.h", SO_TYPE as i64),
    c("SO_LABEL", "sys/socket.h", SO_LABEL as i64),
    c("SO_PEERLABEL", "sys/socket.h", SO_PEERLABEL as i64),
    c("SO_LISTENQLIMIT", "sys/socket.h", SO_LISTENQLIMIT as i64),
    c("SO_LISTENQLEN", "sys/socket.h", SO_LISTENQLEN as i64),
    c("SO_LISTENINCQLEN", "sys/socket.h", SO_LISTENINCQLEN as i64),
    c("SO_FIB", "sys/socket.h", SO_FIB as i64),
    c("SO_USER_COOKIE", "sys/socket.h", SO_USER_COOKIE as i64),
    c("SO_PROTOCOL", "sys/socket.h", SO_PROTOCOL as i64),
    c("SO_TS_CLOCK", "sys/socket.h", SO_TS_CLOCK as i64),
    c(
        "SO_MAX_PACING_RATE",
        "sys/socket.h",
        SO_MAX_PACING_RATE as i64,
    ),
    c("SO_DOMAIN", "sys/socket.h", SO_DOMAIN as i64),
    c("SO_SPLICE", "sys/socket.h", SO_SPLICE as i64),
    c("SO_TS_CLOCK_MAX", "sys/socket.h", SO_TS_CLOCK_MAX as i64),
    c("MSG_OOB", "sys/socket.h", MSG_OOB as i64),
    c("MSG_PEEK", "sys/socket.h", MSG_PEEK as i64),
    c("MSG_DONTROUTE", "sys/socket.h", MSG_DONTROUTE as i64),
    c("MSG_EOR", "sys/socket.h", MSG_EOR as i64),
    c("MSG_TRUNC", "sys/socket.h", MSG_TRUNC as i64),
    c("MSG_CTRUNC", "sys/socket.h", MSG_CTRUNC as i64),
    c("MSG_WAITALL", "sys/socket.h", MSG_WAITALL as i64),
    c("MSG_DONTWAIT", "sys/socket.h", MSG_DONTWAIT as i64),
    c("MSG_EOF", "sys/socket.h", MSG_EOF as i64),
    c("MSG_NBIO", "sys/socket.h", MSG_NBIO as i64),
    c("MSG_NOSIGNAL", "sys/socket.h", MSG_NOSIGNAL as i64),
    c("MSG_CMSG_CLOEXEC", "sys/socket.h", MSG_CMSG_CLOEXEC as i64),
    c("MSG_WAITFORONE", "sys/socket.h", MSG_WAITFORONE as i64),
    c("MSG_CMSG_CLOFORK", "sys/socket.h", MSG_CMSG_CLOFORK as i64),
    c("SCM_RIGHTS", "sys/socket.h", SCM_RIGHTS as i64),
    c("SCM_TIMESTAMP", "sys/socket.h", SCM_TIMESTAMP as i64),
    c("SCM_CREDS", "sys/socket.h", SCM_CREDS as i64),
    c("SCM_BINTIME", "sys/socket.h", SCM_BINTIME as i64),
    c("SCM_REALTIME", "sys/socket.h", SCM_REALTIME as i64),
    c("SCM_MONOTONIC", "sys/socket.h", SCM_MONOTONIC as i64),
    c("SCM_CREDS2", "sys/socket.h", SCM_CREDS2 as i64),
    c("SHUT_RD", "sys/socket.h", SHUT_RD as i64),
    c("SHUT_WR", "sys/socket.h", SHUT_WR as i64),
    c("SHUT_RDWR", "sys/socket.h", SHUT_RDWR as i64),
    c("SOMAXCONN", "sys/socket.h", SOMAXCONN as i64),
    c("CMGROUP_MAX", "sys/socket.h", CMGROUP_MAX as i64),
    c("SOL_LOCAL", "sys/un.h", SOL_LOCAL as i64),
    c("LOCAL_PEERCRED", "sys/un.h", LOCAL_PEERCRED as i64),
    c("LOCAL_CREDS", "sys/un.h", LOCAL_CREDS as i64),
    c(
        "LOCAL_CREDS_PERSISTENT",
        "sys/un.h",
        LOCAL_CREDS_PERSISTENT as i64,
    ),
    c("XU_NGROUPS", "sys/ucred.h", XU_NGROUPS as i64),
    c("XUCRED_VERSION", "sys/ucred.h", XUCRED_VERSION as i64),
];

const fn c(c_name: &'static str, header: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header,
        value,
        arches: Arches::All,
    }
}
