// SPDX-License-Identifier: BSD-2-Clause
//! Socket system calls on internet sockets (docs/MULTIUSER.md §8.4): the
//! `AF_INET` and `AF_INET6` halves of `socket`, `bind`, `connect`,
//! `listen`, `accept`, `send*`, `recv*`, `shutdown`, `getsockname`,
//! `getpeername`, and the option calls, which `socket.rs` hands an
//! internet socket's file to; and the control messages a datagram's
//! receiver asked for (`ip_savecontrol`, `ip6_savecontrol`).
//!
//! Behavior reference: sys/kern/uipc_syscalls.c, sys/kern/uipc_socket.c
//! (`socreate`'s protocol lookup), sys/netinet/ip_input.c
//! (`ip_savecontrol`), sys/netinet6/ip6_input.c (`ip6_savecontrol`),
//! sys/netinet/udp_usrreq.c (`udp_append`'s `IP_ORIGDSTADDR`,
//! `udp_send`'s control).

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::errno::Errno;
use abi::fcntl::{FREAD, FWRITE, O_NONBLOCK};
use abi::inet::{IPPROTO_IP, IPPROTO_IPV6, IPPROTO_TCP, IPPROTO_UDP, ip, ipv6};
use abi::socket::{
    CMSG_HDR, MSG_TRUNC, SCM_BINTIME, SCM_TIMESTAMP, SHUT_RDWR, SOCK_CLOEXEC, SOCK_CLOFORK,
    SOCK_DGRAM, SOCK_NONBLOCK, SOCK_SEQPACKET, SOCK_STREAM, SOL_SOCKET, cmsg_len, cmsg_space,
};
use inet::{Family, Proto, Socket, want};
use sched::Sleepable;

use super::{SysResult, int, process};
use crate::fs::fdtable::FdFlags;
use crate::fs::file::{File, FileKind};
use crate::fs::inet::InetFile;
use crate::fs::uio::UserIo;
use crate::uaccess;

/// `socreate`'s `pffindproto` and `pffindtype` for the internet domains:
/// TCP and UDP, by type and (optionally) number.
fn proto(ty: u32, protocol: i32) -> Result<Proto, Errno> {
    match (ty, protocol) {
        (SOCK_STREAM, 0 | IPPROTO_TCP) => Ok(Proto::Tcp),
        (SOCK_DGRAM, 0 | IPPROTO_UDP) => Ok(Proto::Udp),
        // No raw IP, SCTP, or UDP-Lite on loopback yet (P7).
        (abi::inet::SOCK_RAW, _) => Err(Errno::EPROTONOSUPPORT),
        (SOCK_SEQPACKET, 0) => Err(Errno::EPROTOTYPE),
        (SOCK_STREAM | SOCK_DGRAM | SOCK_SEQPACKET, _) => Err(Errno::EPROTONOSUPPORT),
        _ => Err(Errno::EPROTOTYPE),
    }
}

/// A new descriptor for internet socket `so`, with Capsicum limits (an
/// accepted socket's are its listener's).
fn install(
    s: &Sleepable,
    so: Arc<Socket>,
    flags: u32,
    caps: Option<crate::fs::fdtable::FileCaps>,
) -> Result<i32, Errno> {
    let p = process()?;
    let c = p.cred();
    let status = FREAD
        | FWRITE
        | if flags & SOCK_NONBLOCK != 0 {
            O_NONBLOCK
        } else {
            0
        };
    let fdf = FdFlags {
        cloexec: flags & SOCK_CLOEXEC != 0,
        clofork: flags & SOCK_CLOFORK != 0,
        resolve_beneath: false,
    };
    let limit = super::io::max_files(&p);
    let file = File::new(
        FileKind::Inet(InetFile::new(so, c.euid(), c.egid())),
        status,
    );
    p.files.lock(s).alloc_with(file, fdf, caps, 0, limit)
}

/// `socket(AF_INET | AF_INET6, type, protocol)`.
pub(super) fn socket(
    s: &Sleepable,
    family: Family,
    ty: u32,
    flags: u32,
    protocol: i32,
) -> SysResult {
    let p = proto(ty, protocol)?;
    let so = Socket::new(family, p, &crate::fs::inet::cred());
    let fd = install(s, so, flags, None)?;
    Ok(u64::from(fd.cast_unsigned()))
}

/// The internet socket of a socket file.
pub(super) fn so_of(f: &File) -> Option<&Arc<Socket>> {
    match f.kind() {
        FileKind::Inet(i) => Some(&i.so),
        _ => None,
    }
}

fn nonblock(f: &File) -> bool {
    f.flags() & O_NONBLOCK != 0
}

/// `bind(s, addr, addrlen)`.
pub(super) fn bind(so: &Socket, sa: &[u8]) -> SysResult {
    so.bind(sa, &crate::fs::inet::cred())?;
    Ok(0)
}

/// `connect(s, addr, addrlen)`.
pub(super) fn connect(s: &Sleepable, f: &File, so: &Arc<Socket>, sa: &[u8]) -> SysResult {
    crate::fs::inet::connect(s, so, sa, nonblock(f))?;
    Ok(0)
}

/// `listen(s, backlog)`.
pub(super) fn listen(so: &Socket, backlog: i32) -> SysResult {
    so.listen(backlog, &crate::fs::inet::cred())?;
    Ok(0)
}

/// `accept4`'s internet half: the next connection, its peer's name out.
#[allow(clippy::too_many_arguments, reason = "kern_accept4's, with the file")]
pub(super) fn accept(
    s: &Sleepable,
    f: &File,
    so: &Arc<Socket>,
    sock_fd: u64,
    name: u64,
    lenp: u64,
    flags: u32,
    inherit: bool,
) -> SysResult {
    if !so.is_listening() {
        return Err(Errno::EINVAL);
    }
    let new = if nonblock(f) {
        so.accept()?
    } else {
        let _w = sched::wait_message("accept");
        crate::fs::socket::wait_until(s, &so.rd, 0, || so.accept())?
    };
    so.rd.knote(0);
    let mut nflags = flags & (SOCK_CLOEXEC | SOCK_CLOFORK);
    if (inherit && nonblock(f)) || flags & SOCK_NONBLOCK != 0 {
        nflags |= SOCK_NONBLOCK;
    }
    let peer = new.peername().map_or_else(|_| Vec::new(), |sa| sa.encode());
    let caps = process()?
        .files
        .lock(s)
        .slot(int(sock_fd))
        .ok()
        .and_then(|slot| slot.caps.as_deref().cloned());
    let fd = install(s, new, nflags, caps)?;
    if let Err(e) = super::socket::put_name(&peer, name, lenp, true) {
        if let Ok(p) = process() {
            let closed = p.files.lock(s).close(fd);
            drop(closed);
        }
        return Err(e);
    }
    if name != 0 {
        crate::ktrace::sockaddr(s, &peer);
    }
    Ok(u64::from(fd.cast_unsigned()))
}

/// `shutdown(s, how)`.
pub(super) fn shutdown(so: &Arc<Socket>, how: i32) -> SysResult {
    if !(0..=SHUT_RDWR).contains(&how) {
        return Err(Errno::EINVAL);
    }
    so.shutdown(how)?;
    Ok(0)
}

/// `getsockname` and `getpeername`: the name, encoded.
pub(super) fn name(so: &Socket, peer: bool) -> Result<Vec<u8>, Errno> {
    Ok(if peer {
        so.peername()?.encode()
    } else {
        so.sockname().encode()
    })
}

/// `setsockopt`.
pub(super) fn setsockopt(so: &Socket, level: i32, name: i32, val: &[u8]) -> SysResult {
    so.setopt(level, name, val, &crate::fs::inet::cred())?;
    Ok(0)
}

/// `getsockopt`'s value.
pub(super) fn getsockopt(so: &Socket, level: i32, name: i32) -> Result<Vec<u8>, Errno> {
    so.getopt(level, name)
}

/// A send's control messages (`udp_send`, `tcp_usr_send`): TCP takes
/// none; UDP's IP-level `IP_SENDSRCADDR` and `IP_TOS` are the only ones
/// with meaning, and are taken without effect here; others at the IP
/// level are `EINVAL`, at other levels ignored.
fn check_control(so: &Socket, raw: &[u8]) -> Result<(), Errno> {
    if so.proto == Proto::Tcp {
        return if raw.is_empty() {
            Ok(())
        } else {
            Err(Errno::EINVAL)
        };
    }
    let rd = |i: usize| -> u32 {
        u32::from_ne_bytes(
            raw.get(i..i + 4)
                .and_then(|b| b.try_into().ok())
                .unwrap_or([0; 4]),
        )
    };
    let mut at = 0;
    while raw.len() - at >= CMSG_HDR {
        let len = rd(at) as usize;
        let level = rd(at + 4).cast_signed();
        let ty = rd(at + 8).cast_signed();
        if len < CMSG_HDR || len > raw.len() - at {
            return Err(Errno::EINVAL);
        }
        let ip_level = match so.family {
            Family::Inet => IPPROTO_IP,
            Family::Inet6 => IPPROTO_IPV6,
        };
        if level == ip_level {
            let ok = match so.family {
                // `IP_SENDSRCADDR` is `IP_RECVDSTADDR`'s number.
                Family::Inet => ty == ip::IP_RECVDSTADDR || ty == ip::IP_TOS,
                Family::Inet6 => {
                    ty == ipv6::IPV6_PKTINFO || ty == ipv6::IPV6_HOPLIMIT || ty == ipv6::IPV6_TCLASS
                }
            };
            if !ok {
                return Err(Errno::EINVAL);
            }
        }
        at += cmsg_space(len - CMSG_HDR).min(raw.len() - at);
    }
    Ok(())
}

/// The common part of `sendto` and `sendmsg` (`kern_sendit`).
#[allow(clippy::too_many_arguments, reason = "kern_sendit's, with the file")]
pub(super) fn sendit(
    s: &Sleepable,
    f: &File,
    so: &Arc<Socket>,
    sock_fd: u64,
    uio: &mut UserIo,
    to: Option<Vec<u8>>,
    control: Option<Vec<u8>>,
    flags: i32,
) -> SysResult {
    if let Some(c) = &control {
        check_control(so, c)?;
    }
    // A stream's destination is its peer, whatever is named
    // (`tcp_usr_send` with an address on a connected socket).
    let to = match (so.proto, to) {
        (Proto::Udp, t) => t,
        (Proto::Tcp, _) => None,
    };
    let ktr = crate::ktrace::genio_start(uio);
    let r = crate::fs::inet::send(s, so, uio, to.as_deref(), flags, nonblock(f));
    crate::ktrace::genio(s, sock_fd, true, ktr, &r);
    r.map(|n| n as u64)
}

/// One control message, `CMSG_SPACE`-sized.
fn cmsg(level: i32, ty: i32, data: &[u8]) -> Vec<u8> {
    let mut m = alloc::vec![0u8; cmsg_space(data.len())];
    m[0..4].copy_from_slice(
        &u32::try_from(cmsg_len(data.len()))
            .unwrap_or(0)
            .to_ne_bytes(),
    );
    m[4..8].copy_from_slice(&level.to_ne_bytes());
    m[8..12].copy_from_slice(&ty.to_ne_bytes());
    m[CMSG_HDR..CMSG_HDR + data.len()].copy_from_slice(data);
    m
}

/// `lo0`'s link address as `IP_RECVIF` gives it: a `sockaddr_dl` the
/// size `if_attach` makes it.
fn lo0_sdl() -> Vec<u8> {
    let mut d = alloc::vec![0u8; 54];
    d[0] = 54;
    d[1] = abi::inet::AF_LINK;
    d[2..4].copy_from_slice(&1u16.to_ne_bytes());
    d[4] = abi::net::IFT_LOOP;
    d[5] = 3;
    d[8..11].copy_from_slice(b"lo0");
    d
}

/// `ip_savecontrol` and `ip6_savecontrol`: the messages a datagram's
/// receiver asked for, in FreeBSD's order (time stamp first, the
/// original destination last).
fn controls(meta: &inet::Meta) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let w = meta.want;
    if w & (want::TIMESTAMP | want::BINTIME) != 0 {
        let ns = crate::time::realtime_base_ns().saturating_add(meta.stamp);
        if w & want::BINTIME != 0 {
            let frac = u64::try_from((u128::from(ns % 1_000_000_000) << 64) / 1_000_000_000)
                .unwrap_or(u64::MAX);
            let mut d = Vec::new();
            d.extend_from_slice(&(ns / 1_000_000_000).to_ne_bytes());
            d.extend_from_slice(&frac.to_ne_bytes());
            out.push(cmsg(SOL_SOCKET, SCM_BINTIME, &d));
        }
        if w & want::TIMESTAMP != 0 {
            let mut d = Vec::new();
            d.extend_from_slice(&(ns / 1_000_000_000).to_ne_bytes());
            d.extend_from_slice(&(ns % 1_000_000_000 / 1000).to_ne_bytes());
            out.push(cmsg(SOL_SOCKET, SCM_TIMESTAMP, &d));
        }
    }
    let dst = &meta.dst;
    if w & want::DSTADDR != 0 {
        out.push(cmsg(IPPROTO_IP, ip::IP_RECVDSTADDR, &dst.addr[12..16]));
    }
    if w & want::TTL != 0 {
        out.push(cmsg(IPPROTO_IP, ip::IP_RECVTTL, &[meta.ttl]));
    }
    if w & want::TOS != 0 {
        out.push(cmsg(IPPROTO_IP, ip::IP_RECVTOS, &[meta.tos]));
    }
    if w & want::IF != 0 {
        out.push(cmsg(IPPROTO_IP, ip::IP_RECVIF, &lo0_sdl()));
    }
    if w & want::PKTINFO != 0 {
        let mut d = dst.addr.to_vec();
        d.extend_from_slice(&u32::from(meta.ifindex).to_ne_bytes());
        out.push(cmsg(IPPROTO_IPV6, ipv6::IPV6_PKTINFO, &d));
    }
    if w & want::HOPLIMIT != 0 {
        out.push(cmsg(
            IPPROTO_IPV6,
            ipv6::IPV6_HOPLIMIT,
            &i32::from(meta.ttl).to_ne_bytes(),
        ));
    }
    if w & want::TCLASS != 0 {
        out.push(cmsg(
            IPPROTO_IPV6,
            ipv6::IPV6_TCLASS,
            &i32::from(meta.tos).to_ne_bytes(),
        ));
    }
    if w & want::ORIGDSTADDR != 0 {
        let (level, ty) = match dst.family {
            Family::Inet => (IPPROTO_IP, ip::IP_ORIGDSTADDR),
            Family::Inet6 => (IPPROTO_IPV6, ipv6::IPV6_ORIGDSTADDR),
        };
        out.push(cmsg(level, ty, &dst.encode()));
    }
    out
}

/// The common part of `recvfrom` and `recvmsg` (`kern_recvit`) on an
/// internet socket: the count, the name's and control's lengths, and the
/// flags out.
#[allow(clippy::too_many_arguments, reason = "kern_recvit's, with the file")]
pub(super) fn recvit(
    s: &Sleepable,
    f: &File,
    so: &Arc<Socket>,
    sock_fd: u64,
    uio: &mut UserIo,
    flags: i32,
    name: Option<(u64, u32)>,
    control: Option<(u64, u32)>,
) -> Result<(usize, u32, u32, i32), Errno> {
    let ktr = crate::ktrace::genio_start(uio);
    let r = crate::fs::inet::receive(s, so, uio, flags, nonblock(f));
    crate::ktrace::genio(
        s,
        sock_fd,
        false,
        ktr,
        &r.as_ref().map(|r| r.count).map_err(|e| *e),
    );
    let r = r?;
    let mut outflags = flags | r.flags;
    let count = if so.proto == Proto::Udp && flags & MSG_TRUNC != 0 {
        r.full
    } else {
        r.count
    };
    let mut namelen = 0;
    if let Some((addr, len)) = name
        && let Some(from) = &r.from
        && len.cast_signed() > 0
    {
        let from = from.encode();
        let k = (len as usize).min(from.len());
        uaccess::copy_out(addr, &from[..k]).map_err(|_| Errno::EFAULT)?;
        namelen = u32::try_from(k).unwrap_or(0);
        crate::ktrace::sockaddr(s, &from);
    }
    let mut ctllen = 0;
    if let Some((addr, len)) = control {
        let msgs = r.meta.as_ref().map(controls).unwrap_or_default();
        let mut left = len as usize;
        let mut at = 0;
        for m in &msgs {
            if left < m.len() {
                outflags |= abi::socket::MSG_CTRUNC;
                break;
            }
            uaccess::copy_out(addr + at as u64, m).map_err(|_| Errno::EFAULT)?;
            at += m.len();
            left -= m.len();
        }
        ctllen = u32::try_from(at).unwrap_or(0);
    }
    Ok((count, namelen, ctllen, outflags))
}
