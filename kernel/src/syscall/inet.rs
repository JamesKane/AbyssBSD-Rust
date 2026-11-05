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
use abi::inet::{IPPROTO_IP, IPPROTO_IPV6, IPPROTO_TCP, IPPROTO_UDP, ip, ipv6, tcp};
use abi::ktls::{TLS_ENABLE_V0_SIZE, TlsEnable};
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
/// TCP and UDP, by type and (optionally) number, and raw IP sockets of
/// any protocol (`rip_attach`, `rip6_attach`, which need
/// `PRIV_NETINET_RAW`).
fn proto(_family: Family, ty: u32, protocol: i32) -> Result<Proto, Errno> {
    match (ty, protocol) {
        (SOCK_STREAM, 0 | IPPROTO_TCP) => Ok(Proto::Tcp),
        (SOCK_DGRAM, 0 | IPPROTO_UDP) => Ok(Proto::Udp),
        (abi::inet::SOCK_RAW, p) => {
            let p = u8::try_from(p).map_err(|_| Errno::EPROTONOSUPPORT)?;
            // `socreate`: raw IP has no `PR_CAPATTACH`.
            crate::capsicum::refuse(crate::ktrace::CapFail::Proto(i32::from(p)))?;
            if !crate::fs::inet::cred().raw {
                return Err(Errno::EPERM);
            }
            Ok(Proto::Raw(p))
        }
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
    )?;
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
    let p = proto(family, ty, protocol)?;
    let so = Socket::new_in(&crate::net::curvnet(), family, p, &crate::fs::inet::cred());
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
pub(super) fn setsockopt(
    so: &alloc::sync::Arc<Socket>,
    level: i32,
    name: i32,
    val: &[u8],
) -> SysResult {
    if level == IPPROTO_TCP
        && so.proto == Proto::Tcp
        && matches!(name, tcp::TCP_TXTLS_ENABLE | tcp::TCP_RXTLS_ENABLE)
    {
        return tls_enable(so, name, val);
    }
    so.setopt(level, name, val, &crate::fs::inet::cred())?;
    Ok(0)
}

/// `ktls_copyin_tls_enable`: `val` as a `struct tls_enable`, or the older
/// structure without `rec_seq`.
fn tls_header(val: &[u8]) -> Result<TlsEnable, Errno> {
    let mut en = TlsEnable::default();
    let b = uaccess::bytes_of_mut(&mut en);
    let size = b.len();
    if val.len() == TLS_ENABLE_V0_SIZE {
        b[..TLS_ENABLE_V0_SIZE].copy_from_slice(val);
        // Its tail is padding where `rec_seq` is now.
        b[TLS_ENABLE_V0_SIZE - 6..].fill(0);
    } else {
        // `sooptcopyin`: shorter is `EINVAL`, longer is cut.
        b.copy_from_slice(val.get(..size).ok_or(Errno::EINVAL)?);
    }
    ktls::check_lengths(en.cipher_key_len, en.iv_len, en.auth_key_len)?;
    Ok(en)
}

/// A key `en` points at.
fn tls_key(addr: u64, len: i32) -> Result<Vec<u8>, Errno> {
    let mut k = alloc::vec![0; usize::try_from(len).unwrap_or(0)];
    if !k.is_empty() {
        uaccess::copy_in(&mut k, addr).map_err(|_| Errno::EFAULT)?;
    }
    Ok(k)
}

/// `TCP_TXTLS_ENABLE` and `TCP_RXTLS_ENABLE`, with the keys copied in.
fn tls_enable(so: &alloc::sync::Arc<Socket>, name: i32, val: &[u8]) -> SysResult {
    let en = tls_header(val)?;
    let mut cipher_key = tls_key(en.cipher_key, en.cipher_key_len)?;
    let mut iv = tls_key(en.iv, en.iv_len)?;
    let mut auth_key = tls_key(en.auth_key, en.auth_key_len)?;
    let e = ktls::Enable {
        cipher_key: &cipher_key,
        iv: &iv,
        auth_key: &auth_key,
        cipher_algorithm: en.cipher_algorithm,
        auth_algorithm: en.auth_algorithm,
        flags: en.flags,
        vmajor: en.tls_vmajor,
        vminor: en.tls_vminor,
        rec_seq: en.rec_seq,
    };
    let r = if name == tcp::TCP_TXTLS_ENABLE {
        so.tls_enable_tx(&e)
    } else {
        crate::net::ktls_start();
        so.tls_enable_rx(&e)
    };
    cipher_key.fill(0);
    iv.fill(0);
    auth_key.fill(0);
    r.map(|()| 0)
}

/// `getsockopt`'s value.
pub(super) fn getsockopt(so: &Socket, level: i32, name: i32) -> Result<Vec<u8>, Errno> {
    so.getopt(level, name)
}

/// A send's control messages (`udp_send`, `tcp_usr_send`): TCP takes
/// none but, with a sending TLS session, a first `TLS_SET_RECORD_TYPE`
/// (`sosend_generic`); UDP's IP-level `IP_SENDSRCADDR` and `IP_TOS` set the
/// datagram's source and TOS, IPv6's `IPV6_PKTINFO`, `IPV6_HOPLIMIT`,
/// `IPV6_TCLASS`, and `IPV6_NEXTHOP` its source, interface, hop limit,
/// traffic class, and next hop; others at the IP level are `EINVAL`
/// (`ENOPROTOOPT` for IPv6), at other levels ignored.
/// `IPV6_NEXTHOP`.
const IPV6_NEXTHOP: i32 = 48;

fn check_control(so: &Socket, raw: &[u8]) -> Result<inet::SendCtl, Errno> {
    let mut ctl = inet::SendCtl::default();
    if so.proto == Proto::Tcp {
        if raw.is_empty() {
            return Ok(ctl);
        }
        let ty = raw.get(8..12).and_then(|b| b.try_into().ok());
        if ty.map(i32::from_ne_bytes) == Some(tcp::TLS_SET_RECORD_TYPE)
            && so.tls_tx_layout().is_some()
        {
            ctl.tls_type = Some(*raw.get(CMSG_HDR).ok_or(Errno::EINVAL)?);
            return Ok(ctl);
        }
        return Err(Errno::EINVAL);
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
        if level == ip_level && so.family == Family::Inet6 {
            // `ip6_setpktopts` (each `ip6_setpktopt` with `cmsg` set).
            let data = &raw[at + CMSG_HDR..at + len];
            let int = || -> Result<i32, Errno> {
                data.try_into()
                    .map(i32::from_ne_bytes)
                    .map_err(|_| Errno::EINVAL)
            };
            match ty {
                ipv6::IPV6_PKTINFO => {
                    ctl.pktinfo6 = Some(inet::raw6::check_pktinfo(&so.vnet, data)?);
                }
                ipv6::IPV6_HOPLIMIT => {
                    let v = int()?;
                    if !(-1..=255).contains(&v) {
                        return Err(Errno::EINVAL);
                    }
                    ctl.hlim6 = Some(v);
                }
                ipv6::IPV6_TCLASS => {
                    let v = int()?;
                    if !(-1..=255).contains(&v) {
                        return Err(Errno::EINVAL);
                    }
                    ctl.tclass6 = Some(v);
                }
                IPV6_NEXTHOP => ctl.nexthop6 = Some(inet::raw6::check_nexthop(data)?),
                ipv6::IPV6_USE_MIN_MTU | ipv6::IPV6_DONTFRAG | ipv6::IPV6_PREFER_TEMPADDR => {
                    int()?;
                }
                _ => return Err(Errno::ENOPROTOOPT),
            }
        } else if level == ip_level {
            let ok = match so.family {
                // `IP_SENDSRCADDR` is `IP_RECVDSTADDR`'s number.
                Family::Inet => ty == ip::IP_RECVDSTADDR || ty == ip::IP_TOS,
                Family::Inet6 => false,
            };
            if !ok {
                return Err(Errno::EINVAL);
            }
            let data = &raw[at + CMSG_HDR..at + len];
            if so.family == Family::Inet && so.proto == Proto::Udp {
                if ty == ip::IP_RECVDSTADDR {
                    ctl.src = Some(data.try_into().map_err(|_| Errno::EINVAL)?);
                } else {
                    let [t] = data else {
                        return Err(Errno::EINVAL);
                    };
                    ctl.tos = Some(*t);
                }
            }
        }
        at += cmsg_space(len - CMSG_HDR).min(raw.len() - at);
    }
    Ok(ctl)
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
    let ctl = match &control {
        Some(c) => check_control(so, c)?,
        None => inet::SendCtl::default(),
    };
    let ktr = crate::ktrace::genio_start(uio);
    let r = crate::fs::inet::send(s, so, uio, to.as_deref(), ctl, flags, nonblock(f));
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

/// Interface `index`'s link address as `IP_RECVIF` gives it
/// (`ip_savecontrol`'s copy of `if_addr`): a `sockaddr_dl` with its
/// name and hardware address, the size `if_attach` makes it.
fn if_sdl(index: u16) -> Vec<u8> {
    let (name, ty, ll) = crate::net::curvnet()
        .net
        .lock()
        .by_index(index)
        .map(|i| (i.name.clone(), i.ty, i.lladdr))
        .unwrap_or_default();
    let name = name.as_bytes();
    let alen = ll.map_or(0, |l| l.len());
    // `if_attach`: room for the name and twice the address, at least a
    // `sockaddr_dl`'s.
    let len = (8 + name.len() + 2 * alen)
        .max(20)
        .next_multiple_of(4)
        .max(54);
    let mut d = alloc::vec![0u8; len];
    d[0] = u8::try_from(len).unwrap_or(54);
    d[1] = abi::inet::AF_LINK;
    d[2..4].copy_from_slice(&index.to_ne_bytes());
    d[4] = ty;
    d[5] = u8::try_from(name.len()).unwrap_or(0);
    d[6] = u8::try_from(alen).unwrap_or(0);
    d[8..8 + name.len()].copy_from_slice(name);
    if let Some(l) = ll {
        d[8 + name.len()..8 + name.len() + 6].copy_from_slice(&l);
    }
    d
}

/// `ip_savecontrol` and `ip6_savecontrol`: the messages a datagram's
/// receiver asked for, in FreeBSD's order (time stamp first, the
/// original destination last).
fn controls(meta: &inet::Meta) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let w = meta.want;
    if w & (want::TIMESTAMP | want::BINTIME | want::REALTIME | want::MONOTONIC) != 0 {
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
        // `SCM_REALTIME` and `SCM_MONOTONIC`: `struct timespec`s, the
        // latter on the uptime clock the stamp was taken on.
        for (bit, ty, t) in [
            (want::REALTIME, abi::socket::SCM_REALTIME, ns),
            (want::MONOTONIC, abi::socket::SCM_MONOTONIC, meta.stamp),
        ] {
            if w & bit != 0 {
                let mut d = Vec::new();
                d.extend_from_slice(&(t / 1_000_000_000).to_ne_bytes());
                d.extend_from_slice(&(t % 1_000_000_000).to_ne_bytes());
                out.push(cmsg(SOL_SOCKET, ty, &d));
            }
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
        out.push(cmsg(IPPROTO_IP, ip::IP_RECVIF, &if_sdl(meta.ifindex)));
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
        let mut msgs = r.meta.as_ref().map(controls).unwrap_or_default();
        if let Some(t) = r.tls {
            msgs.push(cmsg(IPPROTO_TCP, tcp::TLS_GET_RECORD, &t));
        }
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
