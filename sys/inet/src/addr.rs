// SPDX-License-Identifier: BSD-2-Clause
//! Addresses: the 16-byte form a PCB keeps (an IPv6 address, or an IPv4
//! one in the last four bytes, as FreeBSD's `in_dependaddr`), and the
//! `sockaddr_in` and `sockaddr_in6` system calls pass.
//!
//! Behavior reference: sys/netinet/in.h, sys/netinet6/in6.h,
//! sys/netinet6/in6.c (`in6_sin6_2_sin`, `in6_sin_2_v4mapsin6`).

use alloc::vec::Vec;

use abi::errno::Errno;
use abi::inet::{AF_INET, AF_INET6, AF_UNSPEC, SockaddrIn, SockaddrIn6};

use crate::Family;

/// An address as a PCB holds it.
pub type In6 = [u8; 16];

/// `INADDR_ANY` and `in6addr_any` (the same 16 zero bytes).
pub const ANY: In6 = [0; 16];

/// IPv6's loopback, `::1`.
pub const LOOPBACK6: In6 = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];

/// An IPv4 address in PCB form.
#[must_use]
pub const fn from_v4(a: [u8; 4]) -> In6 {
    [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, a[0], a[1], a[2], a[3]]
}

/// The IPv4 address of a PCB-form or mapped one.
#[must_use]
pub const fn v4_of(a: &In6) -> [u8; 4] {
    [a[12], a[13], a[14], a[15]]
}

/// `::ffff:a.b.c.d`.
#[must_use]
pub const fn v4mapped(a: [u8; 4]) -> In6 {
    [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, a[0], a[1], a[2], a[3],
    ]
}

/// `IN6_IS_ADDR_V4MAPPED`.
#[must_use]
pub fn is_v4mapped(a: &In6) -> bool {
    a[..10].iter().all(|&b| b == 0) && a[10] == 0xff && a[11] == 0xff
}

/// `IN6_IS_ADDR_UNSPECIFIED` (and `in_nullhost` on the PCB form).
#[must_use]
pub fn is_unspecified(a: &In6) -> bool {
    a.iter().all(|&b| b == 0)
}

/// `IN_MULTICAST`.
#[must_use]
pub const fn is_multicast4(a: &[u8; 4]) -> bool {
    a[0] & 0xf0 == 0xe0
}

/// `INADDR_BROADCAST`.
#[must_use]
pub fn is_broadcast4(a: &[u8; 4]) -> bool {
    *a == [255; 4]
}

/// `IN_LOOPBACK`: `127/8`.
#[must_use]
pub const fn is_loopback4(a: &[u8; 4]) -> bool {
    a[0] == 127
}

/// `IN6_IS_ADDR_MULTICAST`.
#[must_use]
pub const fn is_multicast6(a: &In6) -> bool {
    a[0] == 0xff
}

/// `IN6_IS_ADDR_LINKLOCAL` (and link-local multicast, which has a zone
/// too).
#[must_use]
pub const fn is_linklocal6(a: &In6) -> bool {
    (a[0] == 0xfe && a[1] & 0xc0 == 0x80) || (a[0] == 0xff && a[1] & 0x0f == 0x02)
}

/// Whether the first `plen` bits of `a` and `b`, as addresses of
/// `family`, agree.
#[must_use]
pub fn prefix_match(family: Family, a: &In6, b: &In6, plen: u8) -> bool {
    let (off, bits) = match family {
        Family::Inet => (12usize, usize::from(plen.min(32))),
        Family::Inet6 => (0usize, usize::from(plen.min(128))),
    };
    let full = bits / 8;
    if a[off..off + full] != b[off..off + full] {
        return false;
    }
    let rest = bits % 8;
    if rest == 0 {
        return true;
    }
    let mask = 0xffu8 << (8 - rest);
    a[off + full] & mask == b[off + full] & mask
}

/// An endpoint as a call names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sockaddr {
    pub family: Family,
    /// PCB form: IPv4 in the last four bytes.
    pub addr: In6,
    /// Host order.
    pub port: u16,
    pub scope: u32,
    pub flowinfo: u32,
}

impl Sockaddr {
    /// An IPv4 endpoint.
    #[must_use]
    pub const fn v4(a: [u8; 4], port: u16) -> Self {
        Self {
            family: Family::Inet,
            addr: from_v4(a),
            port,
            scope: 0,
            flowinfo: 0,
        }
    }

    /// An IPv6 endpoint.
    #[must_use]
    pub const fn v6(a: In6, port: u16, scope: u32) -> Self {
        Self {
            family: Family::Inet6,
            addr: a,
            port,
            scope,
            flowinfo: 0,
        }
    }

    /// Its `sockaddr_in` or `sockaddr_in6` bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        match self.family {
            Family::Inet => sockaddr_in(v4_of(&self.addr), self.port),
            Family::Inet6 => sockaddr_in6(&self.addr, self.port, self.scope),
        }
    }

    /// `in6_sin6_2_sin`: a mapped IPv6 endpoint as the IPv4 one.
    #[must_use]
    pub fn unmapped(&self) -> Self {
        Self::v4(v4_of(&self.addr), self.port)
    }

    /// `in6_sin_2_v4mapsin6`: an IPv4 endpoint as the mapped IPv6 one.
    #[must_use]
    pub fn mapped(&self) -> Self {
        Self::v6(v4mapped(v4_of(&self.addr)), self.port, 0)
    }
}

/// A `struct sockaddr_in`.
#[must_use]
pub fn sockaddr_in(a: [u8; 4], port: u16) -> Vec<u8> {
    let s = SockaddrIn {
        sin_len: 16,
        sin_family: AF_INET,
        sin_port: port.to_be_bytes(),
        sin_addr: a,
        sin_zero: [0; 8],
    };
    let mut out = Vec::with_capacity(16);
    out.push(s.sin_len);
    out.push(s.sin_family);
    out.extend_from_slice(&s.sin_port);
    out.extend_from_slice(&s.sin_addr);
    out.extend_from_slice(&s.sin_zero);
    out
}

/// A `struct sockaddr_in6`.
#[must_use]
pub fn sockaddr_in6(a: &In6, port: u16, scope: u32) -> Vec<u8> {
    let s = SockaddrIn6 {
        sin6_len: 28,
        sin6_family: AF_INET6,
        sin6_port: port.to_be_bytes(),
        sin6_flowinfo: 0,
        sin6_addr: *a,
        sin6_scope_id: scope,
    };
    let mut out = Vec::with_capacity(28);
    out.push(s.sin6_len);
    out.push(s.sin6_family);
    out.extend_from_slice(&s.sin6_port);
    out.extend_from_slice(&s.sin6_flowinfo.to_ne_bytes());
    out.extend_from_slice(&s.sin6_addr);
    out.extend_from_slice(&s.sin6_scope_id.to_ne_bytes());
    out
}

/// A `sockaddr_in` call argument, its `sa_len` the length given.
/// `AF_UNSPEC` with `INADDR_ANY` passes as `AF_INET` when `unspec_ok`
/// (`tcp_usr_bind`'s and `udp_bind`'s compatibility for old programs).
///
/// # Errors
///
/// `EAFNOSUPPORT` for another family, `EINVAL` for another length.
pub fn parse_in(raw: &[u8], unspec_ok: bool) -> Result<Sockaddr, Errno> {
    if raw.len() < 2 {
        return Err(Errno::EINVAL);
    }
    let family = raw[1];
    if family != AF_INET {
        // `offsetof(struct sockaddr_in, sin_zero)` is 8.
        let any = raw.len() >= 8 && raw[4..8] == [0; 4];
        if !(unspec_ok && family == AF_UNSPEC && any) {
            return Err(Errno::EAFNOSUPPORT);
        }
    }
    if raw.len() != 16 {
        return Err(Errno::EINVAL);
    }
    Ok(Sockaddr::v4(
        [raw[4], raw[5], raw[6], raw[7]],
        u16::from_be_bytes([raw[2], raw[3]]),
    ))
}

/// A `sockaddr_in6` call argument.
///
/// # Errors
///
/// `EAFNOSUPPORT` for another family, `EINVAL` for another length.
pub fn parse_in6(raw: &[u8]) -> Result<Sockaddr, Errno> {
    if raw.len() < 2 {
        return Err(Errno::EINVAL);
    }
    if raw[1] != AF_INET6 {
        return Err(Errno::EAFNOSUPPORT);
    }
    if raw.len() != 28 {
        return Err(Errno::EINVAL);
    }
    let mut a = [0u8; 16];
    a.copy_from_slice(&raw[8..24]);
    let flowinfo = u32::from_ne_bytes([raw[4], raw[5], raw[6], raw[7]]);
    let scope = u32::from_ne_bytes([raw[24], raw[25], raw[26], raw[27]]);
    Ok(Sockaddr {
        family: Family::Inet6,
        addr: a,
        port: u16::from_be_bytes([raw[2], raw[3]]),
        scope,
        flowinfo,
    })
}

/// Formats `a` as `inet_ntop` would (for messages and tests).
#[must_use]
pub fn fmt4(a: [u8; 4]) -> alloc::string::String {
    alloc::format!("{}.{}.{}.{}", a[0], a[1], a[2], a[3])
}
