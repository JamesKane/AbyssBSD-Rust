// SPDX-License-Identifier: BSD-2-Clause
//! Socket system calls, for UNIX-domain sockets (docs/MULTIUSER.md §8;
//! an internet socket's are `inet.rs`'s, a netlink one's `netlink.rs`'s):
//! `socket`, `socketpair`, `bind(at)`, `listen`, `accept(4)`,
//! `connect(at)`, `sendto`, `sendmsg`, `recvfrom`, `recvmsg`, `shutdown`,
//! `setsockopt`, `getsockopt`, `getsockname`, and `getpeername`, with
//! control messages (`SCM_RIGHTS`, `SCM_CREDS`, the timestamps).
//!
//! Behavior reference: sys/kern/uipc_syscalls.c, sys/kern/uipc_usrreq.c
//! (`uipc_bindat`, `unp_connectat`, `unp_internalize`,
//! `unp_externalize`).

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::errno::Errno;
use abi::fcntl::{AT_FDCWD, FREAD, FWRITE, O_NONBLOCK};
use abi::socket::{
    AF_UNIX, CMSG_HDR, MSG_CMSG_CLOEXEC, MSG_CMSG_CLOFORK, MSG_CTRUNC, MSG_TRUNC, Msghdr,
    SCM_BINTIME, SCM_CREDS, SCM_MONOTONIC, SCM_REALTIME, SCM_RIGHTS, SCM_TIMESTAMP, SHUT_RDWR,
    SO_SPLICE, SOCK_CLOEXEC, SOCK_CLOFORK, SOCK_DGRAM, SOCK_MAXADDRLEN, SOCK_NONBLOCK,
    SOCK_SEQPACKET, SOCK_STREAM, SOL_SOCKET, cmsg_len, cmsg_space,
};
use sched::Sleepable;
use socket::{Control, InFlight, Socket, Type};
use vfs::{Kind, NewNode};

use super::{SysResult, int, process};
use crate::fs::fdtable::FdFlags;
use crate::fs::file::{File, FileKind};
use crate::fs::socket::SocketFile;
use crate::fs::uio::UserIo;
use crate::fs::{At, Walk};
use crate::uaccess::{self, Plain, UserPtr};

// SAFETY: integers, the padding explicit fields.
unsafe impl Plain for Msghdr {}

/// Where `sun_path` starts in a `struct sockaddr_un`.
const SUN_PATH: usize = 2;
/// `MCLBYTES`: the most control a send takes.
const MCLBYTES: usize = 2048;

/// A socket's type from `socket`'s `type`, and its flags.
fn kind(ty: u64) -> Result<(Type, u32), Errno> {
    let ty = int(ty).cast_unsigned();
    let flags = ty & (SOCK_CLOEXEC | SOCK_NONBLOCK | SOCK_CLOFORK);
    let t = match ty & !flags {
        SOCK_STREAM => Type::Stream,
        SOCK_DGRAM => Type::Dgram,
        SOCK_SEQPACKET => Type::Seqpacket,
        _ => return Err(Errno::EPROTOTYPE),
    };
    Ok((t, flags))
}

/// A new descriptor for socket `so`.
fn install(s: &Sleepable, so: Arc<Socket>, flags: u32) -> Result<i32, Errno> {
    install_caps(s, so, flags, None)
}

/// [`install`] with Capsicum limits (an accepted socket's, its
/// listener's).
fn install_caps(
    s: &Sleepable,
    so: Arc<Socket>,
    flags: u32,
    caps: Option<crate::fs::fdtable::FileCaps>,
) -> Result<i32, Errno> {
    let p = process()?;
    let c = crate::fs::cred();
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
        FileKind::Socket(SocketFile::new(so, c.euid(), c.egid())),
        status,
    )?;
    p.files.lock(s).alloc_with(file, fdf, caps, 0, limit)
}

/// `socket(domain, type, protocol)`: UNIX-domain here, the internet and
/// netlink domains in their modules.
pub(super) fn socket(s: &Sleepable, domain: u64, ty: u64, protocol: u64) -> SysResult {
    let d = int(domain);
    let raw = int(ty).cast_unsigned();
    let tflags = raw & (SOCK_CLOEXEC | SOCK_NONBLOCK | SOCK_CLOFORK);
    if let Some(family) = u8::try_from(d).ok().and_then(inet::Family::from_af) {
        return super::inet::socket(s, family, raw & !tflags, tflags, int(protocol));
    }
    if d == i32::from(abi::inet::AF_NETLINK) {
        return super::netlink::socket(s, raw & !tflags, tflags, int(protocol));
    }
    if d == i32::from(abi::inet::AF_ROUTE) {
        return super::netlink::socket_route(s, raw & !tflags, tflags, int(protocol));
    }
    if d != i32::from(AF_UNIX) {
        return Err(Errno::EAFNOSUPPORT);
    }
    let (t, flags) = kind(ty)?;
    if int(protocol) != 0 {
        return Err(Errno::EPROTONOSUPPORT);
    }
    let fd = install(s, Socket::new(t), flags)?;
    Ok(u64::from(fd.cast_unsigned()))
}

/// `socketpair(domain, type, protocol, rsv)`.
pub(super) fn socketpair(
    s: &Sleepable,
    domain: u64,
    ty: u64,
    protocol: u64,
    rsv: u64,
) -> SysResult {
    // An internet protocol has no `pr_connect2`.
    if u8::try_from(int(domain))
        .ok()
        .and_then(inet::Family::from_af)
        .is_some()
    {
        return Err(Errno::EOPNOTSUPP);
    }
    if int(domain) != i32::from(AF_UNIX) {
        return Err(Errno::EAFNOSUPPORT);
    }
    let (t, flags) = kind(ty)?;
    if int(protocol) != 0 {
        return Err(Errno::EPROTONOSUPPORT);
    }
    let (a, b) = (Socket::new(t), Socket::new(t));
    Socket::pair(&a, &b, &crate::fs::socket::cred())?;
    let fa = install(s, a, flags)?;
    let fb = match install(s, b, flags) {
        Ok(fb) => fb,
        Err(e) => {
            close_fd(s, fa);
            return Err(e);
        }
    };
    let mut out = [0u8; 8];
    out[..4].copy_from_slice(&fa.to_ne_bytes());
    out[4..].copy_from_slice(&fb.to_ne_bytes());
    if uaccess::copy_out(rsv, &out).is_err() {
        close_fd(s, fa);
        close_fd(s, fb);
        return Err(Errno::EFAULT);
    }
    Ok(0)
}

fn close_fd(s: &Sleepable, fd: i32) {
    if let Ok(p) = process() {
        let closed = p.files.lock(s).close(fd);
        drop(closed);
    }
}

/// The socket open as `fd`, with the rights in `need` (`getsock`).
fn sock(s: &Sleepable, fd: u64, need: &abi::capsicum::CapRights) -> Result<Arc<File>, Errno> {
    let f = super::io::file(s, &*process()?, fd, need)?;
    match f.kind() {
        FileKind::Socket(_) | FileKind::Inet(_) => Ok(f),
        FileKind::Netlink(_) => Ok(f),
        _ => Err(Errno::ENOTSOCK),
    }
}

fn so_of(f: &File) -> &SocketFile {
    match f.kind() {
        FileKind::Socket(sf) => sf,
        _ => unreachable!("a socket file"),
    }
}

fn nonblock(f: &File) -> bool {
    f.flags() & O_NONBLOCK != 0
}

/// `getsockaddr`: the address of `len` bytes at `addr`, its `sa_len` set
/// to `len`.
fn sockaddr(addr: u64, len: u64) -> Result<Vec<u8>, Errno> {
    let len = usize::try_from(int(len)).map_err(|_| Errno::EINVAL)?;
    if len > SOCK_MAXADDRLEN as usize {
        return Err(Errno::ENAMETOOLONG);
    }
    if len < SUN_PATH {
        return Err(Errno::EINVAL);
    }
    let mut raw = alloc::vec![0u8; len];
    uaccess::copy_in(&mut raw, addr).map_err(|_| Errno::EFAULT)?;
    raw[0] = u8::try_from(len).unwrap_or(u8::MAX);
    Ok(raw)
}

/// `unp_sun_path`: the path in a UNIX-domain address, to its first NUL.
fn sun_path(sa: &[u8]) -> Result<&[u8], Errno> {
    if sa[1] != AF_UNIX {
        return Err(Errno::EAFNOSUPPORT);
    }
    let path = &sa[SUN_PATH..];
    let end = path.iter().position(|&b| b == 0).unwrap_or(path.len());
    Ok(&path[..end])
}

/// `bindat(fd, s, addr, addrlen)`: makes the socket's node at the path,
/// from directory `fd` (`EADDRINUSE` if the name exists).
pub(super) fn bindat(s: &Sleepable, fd: u64, sock_fd: u64, addr: u64, len: u64) -> SysResult {
    // Not from the working directory in capability mode (`kern_bindat`).
    if int(fd) == AT_FDCWD {
        crate::capsicum::refuse(crate::ktrace::CapFail::Namei(b"AT_FDCWD"))?;
    }
    let f = sock(s, sock_fd, &crate::capsicum::rights::BIND)?;
    if let Some(so) = super::inet::so_of(&f) {
        // `pr_bindat` is the local domain's alone.
        if int(fd) != AT_FDCWD {
            return Err(Errno::EOPNOTSUPP);
        }
        let sa = sockaddr(addr, len)?;
        crate::ktrace::sockaddr(s, &sa);
        return super::inet::bind(so, &sa);
    }
    if super::netlink::is_netlink(&f) {
        let sa = sockaddr(addr, len)?;
        return super::netlink::bind(&f, &sa);
    }
    let sf = so_of(&f);
    let sa = sockaddr(addr, len)?;
    crate::ktrace::sockaddr(s, &sa);
    let path = sun_path(&sa)?;
    if path.is_empty() {
        return Err(Errno::EINVAL);
    }
    let mode = sf.so.bind_begin()?;
    let r = (|| {
        let p = process()?;
        let at = At::new(int(fd), crate::capsicum::rights::BINDAT);
        let parent = crate::fs::parent_of(s, &p, at, path, false)?;
        if parent.trailing_slash {
            return Err(Errno::ENOENT);
        }
        let mask = p.umask.load(core::sync::atomic::Ordering::Relaxed);
        let (v, _) = vfs::ops::create(
            s,
            &parent.dir,
            &parent.name,
            &NewNode::Socket,
            &super::fsops::create_as(mode & !mask),
            &crate::fs::cred(),
            true,
        )
        .map_err(|e| {
            if e == Errno::EEXIST {
                Errno::EADDRINUSE
            } else {
                e
            }
        })?;
        Ok(v)
    })();
    match r {
        Ok(v) => {
            sf.set_bound(&v);
            sf.so.bind_end(Some(sa));
            Ok(0)
        }
        Err(e) => {
            sf.so.bind_end(None);
            Err(e)
        }
    }
}

/// `bind(s, addr, addrlen)`.
pub(super) fn bind(s: &Sleepable, sock_fd: u64, addr: u64, len: u64) -> SysResult {
    bindat(s, u64::from(AT_FDCWD.cast_unsigned()), sock_fd, addr, len)
}

/// `unp_connectat_peer`: the socket bound to the name, which the caller
/// may write.
fn peer_by_name(s: &Sleepable, fd: i32, path: &[u8]) -> Result<Arc<Socket>, Errno> {
    let p = process()?;
    let v = crate::fs::lookup(
        s,
        &p,
        At::new(fd, crate::capsicum::rights::CONNECTAT),
        path,
        Walk::FOLLOW,
    )?;
    if v.kind() != Kind::Socket {
        return Err(Errno::ENOTSOCK);
    }
    vfs::acl::access(s, &v, &crate::fs::cred(), vfs::ops::VWRITE)?;
    crate::fs::socket::bound_to(&v).ok_or(Errno::ECONNREFUSED)
}

/// `connectat(fd, s, addr, addrlen)`.
pub(super) fn connectat(s: &Sleepable, fd: u64, sock_fd: u64, addr: u64, len: u64) -> SysResult {
    // Not from the working directory in capability mode
    // (`kern_connectat`).
    if int(fd) == AT_FDCWD {
        crate::capsicum::refuse(crate::ktrace::CapFail::Namei(b"AT_FDCWD"))?;
    }
    let f = sock(s, sock_fd, &crate::capsicum::rights::CONNECT)?;
    if let Some(so) = super::inet::so_of(&f) {
        if int(fd) != AT_FDCWD {
            return Err(Errno::EOPNOTSUPP);
        }
        let sa = sockaddr(addr, len)?;
        crate::ktrace::sockaddr(s, &sa);
        return super::inet::connect(s, &f, so, &sa);
    }
    if super::netlink::is_netlink(&f) {
        let sa = sockaddr(addr, len)?;
        return super::netlink::connect(&f, &sa);
    }
    let so = &so_of(&f).so;
    let sa = sockaddr(addr, len)?;
    crate::ktrace::sockaddr(s, &sa);
    let path = sun_path(&sa)?.to_vec();
    if path.is_empty() && int(fd) == AT_FDCWD {
        return Err(Errno::EINVAL);
    }
    so.connect_begin()?;
    let to = match peer_by_name(s, int(fd), &path) {
        Ok(t) => t,
        Err(e) => {
            so.connect_abort();
            return Err(e);
        }
    };
    so.connect_to(&to, &crate::fs::socket::cred())?;
    Ok(0)
}

/// `connect(s, addr, addrlen)`.
pub(super) fn connect(s: &Sleepable, sock_fd: u64, addr: u64, len: u64) -> SysResult {
    connectat(s, u64::from(AT_FDCWD.cast_unsigned()), sock_fd, addr, len)
}

/// `listen(s, backlog)`.
pub(super) fn listen(s: &Sleepable, sock_fd: u64, backlog: u64) -> SysResult {
    let f = sock(s, sock_fd, &crate::capsicum::rights::LISTEN)?;
    if let Some(so) = super::inet::so_of(&f) {
        return super::inet::listen(so, int(backlog));
    }
    if super::netlink::is_netlink(&f) {
        return Err(Errno::EOPNOTSUPP);
    }
    so_of(&f)
        .so
        .listen(int(backlog), &crate::fs::socket::cred())?;
    Ok(0)
}

/// Copies out a name to `name` of the size at `lenp` (if given): as much
/// as fits, and at `lenp`, `real` (the name's whole size, for `accept`)
/// or what was copied.
pub(super) fn put_name(sa: &[u8], name: u64, lenp: u64, real: bool) -> Result<(), Errno> {
    if name == 0 || lenp == 0 {
        return Ok(());
    }
    let len = UserPtr::<u32>::new(lenp)
        .read()
        .map_err(|_| Errno::EFAULT)?;
    if len.cast_signed() < 0 {
        return Err(Errno::EINVAL);
    }
    let n = (len as usize).min(sa.len());
    uaccess::copy_out(name, &sa[..n]).map_err(|_| Errno::EFAULT)?;
    let out = if real { sa.len() } else { n };
    UserPtr::<u32>::new(lenp)
        .write(&u32::try_from(out).unwrap_or(u32::MAX))
        .map_err(|_| Errno::EFAULT)
}

/// `accept(s, name, anamelen)` and `accept4(..., flags)` (`None`:
/// `accept`, which the listener's non-blocking flag passes to).
pub(super) fn accept4(
    s: &Sleepable,
    sock_fd: u64,
    name: u64,
    lenp: u64,
    flags: Option<u64>,
) -> SysResult {
    let inherit = flags.is_none();
    let flags = int(flags.unwrap_or(0)).cast_unsigned();
    if flags & !(SOCK_CLOEXEC | SOCK_CLOFORK | SOCK_NONBLOCK) != 0 {
        return Err(Errno::EINVAL);
    }
    // `accept1` reads the length whenever there is a name, before waiting
    // (a NULL `anamelen` is EFAULT), and `kern_accept4` refuses a negative one.
    if name != 0 {
        let len = UserPtr::<u32>::new(lenp)
            .read()
            .map_err(|_| Errno::EFAULT)?;
        if len.cast_signed() < 0 {
            return Err(Errno::EINVAL);
        }
    }
    let f = sock(s, sock_fd, &crate::capsicum::rights::ACCEPT)?;
    if let Some(so) = super::inet::so_of(&f) {
        return super::inet::accept(s, &f, so, sock_fd, name, lenp, flags, inherit);
    }
    if super::netlink::is_netlink(&f) {
        return Err(Errno::EOPNOTSUPP);
    }
    let so = &so_of(&f).so;
    if !so.is_listening() {
        return Err(Errno::EINVAL);
    }
    let new = if nonblock(&f) {
        so.accept()?
    } else {
        let _w = sched::wait_message("accept");
        crate::fs::socket::wait_until(s, &so.rd, 0, || so.accept())?
    };
    so.rd.knote(0);
    let mut nflags = flags & (SOCK_CLOEXEC | SOCK_CLOFORK);
    if (inherit && nonblock(&f)) || flags & SOCK_NONBLOCK != 0 {
        nflags |= SOCK_NONBLOCK;
    }
    let peer = new.peername().unwrap_or_else(|_| socket::noname());
    // The new descriptor has the listener's limits (`kern_accept4`).
    let caps = process()?
        .files
        .lock(s)
        .slot(int(sock_fd))
        .ok()
        .and_then(|slot| slot.caps.as_deref().cloned());
    let fd = install_caps(s, new, nflags, caps)?;
    if let Err(e) = put_name(&peer, name, lenp, true) {
        close_fd(s, fd);
        return Err(e);
    }
    if name != 0 {
        crate::ktrace::sockaddr(s, &peer);
    }
    Ok(u64::from(fd.cast_unsigned()))
}

/// `shutdown(s, how)`.
pub(super) fn shutdown(s: &Sleepable, sock_fd: u64, how: u64) -> SysResult {
    let f = sock(s, sock_fd, &crate::capsicum::rights::SHUTDOWN)?;
    if let Some(so) = super::inet::so_of(&f) {
        return super::inet::shutdown(so, int(how));
    }
    if super::netlink::is_netlink(&f) {
        return Err(Errno::EOPNOTSUPP);
    }
    let how = int(how);
    if !(0..=SHUT_RDWR).contains(&how) {
        return Err(Errno::EINVAL);
    }
    so_of(&f).so.shutdown(how)?;
    Ok(0)
}

/// `getsockname(s, name, namelen)` and `getpeername(...)`.
pub(super) fn getname(s: &Sleepable, sock_fd: u64, name: u64, lenp: u64, peer: bool) -> SysResult {
    let len = UserPtr::<u32>::new(lenp)
        .read()
        .map_err(|_| Errno::EFAULT)?;
    if len.cast_signed() < 0 {
        return Err(Errno::EINVAL);
    }
    let f = sock(
        s,
        sock_fd,
        if peer {
            &crate::capsicum::rights::GETPEERNAME
        } else {
            &crate::capsicum::rights::GETSOCKNAME
        },
    )?;
    let sa = if let Some(so) = super::inet::so_of(&f) {
        super::inet::name(so, peer)?
    } else if super::netlink::is_netlink(&f) {
        super::netlink::name(&f, peer)?
    } else {
        let so = &so_of(&f).so;
        if peer { so.peername()? } else { so.sockname() }
    };
    put_name(&sa, name, lenp, false)?;
    crate::ktrace::sockaddr(s, &sa);
    Ok(0)
}

/// `setsockopt(s, level, name, val, valsize)`.
pub(super) fn setsockopt(
    s: &Sleepable,
    sock_fd: u64,
    level: u64,
    name: u64,
    val: u64,
    valsize: u64,
) -> SysResult {
    let f = sock(s, sock_fd, &crate::capsicum::rights::SETSOCKOPT)?;
    let n = usize::try_from(int(valsize)).map_err(|_| Errno::EINVAL)?;
    if val == 0 && n != 0 {
        return Err(Errno::EFAULT);
    }
    let mut buf = alloc::vec![0u8; n.min(256)];
    if !buf.is_empty() {
        uaccess::copy_in(&mut buf, val).map_err(|_| Errno::EFAULT)?;
    }
    if int(level) == SOL_SOCKET && int(name) == SO_SPLICE {
        return splice(s, sock_fd, &f, &buf);
    }
    if let Some(so) = super::inet::so_of(&f) {
        return super::inet::setsockopt(so, int(level), int(name), &buf);
    }
    if super::netlink::is_netlink(&f) {
        return super::netlink::setsockopt(&f, int(level), int(name), &buf);
    }
    so_of(&f).so.setopt(int(level), int(name), &buf)?;
    Ok(0)
}

/// `sosetopt`'s `SO_SPLICE`: splices the socket open as `sock_fd` (`f`)
/// to the one `struct splice` names, or unsplices it (`sp_fd` -1). The
/// source needs `CAP_RECV`, the sink `CAP_SEND`; unsplicing needs only
/// the `CAP_SETSOCKOPT` already checked.
fn splice(s: &Sleepable, sock_fd: u64, f: &File, raw: &[u8]) -> SysResult {
    let mut sp = abi::socket::Splice::default();
    let b = uaccess::bytes_of_mut(&mut sp);
    // `sooptcopyin`: shorter than the structure is `EINVAL`.
    let Some(raw) = raw.get(..b.len()) else {
        return Err(Errno::EINVAL);
    };
    b.copy_from_slice(raw);
    let Ok(fd) = u32::try_from(sp.sp_fd) else {
        return match super::inet::so_of(f) {
            Some(so) => so.unsplice().map(|()| 0),
            None => Err(Errno::ENOTCONN),
        };
    };
    sock(s, sock_fd, &crate::capsicum::rights::RECV)?;
    let sink = sock(s, u64::from(fd), &crate::capsicum::rights::SEND)?;
    let (Some(src), Some(dst)) = (super::inet::so_of(f), super::inet::so_of(&sink)) else {
        // `so_splice`'s first checks.
        return Err(if sp.sp_max < 0 {
            Errno::EINVAL
        } else {
            Errno::EPROTONOSUPPORT
        });
    };
    let spliced = src.splice(dst, sp.sp_max)?;
    let idle = sp.sp_idle;
    if idle.tv_sec != 0 || idle.tv_usec != 0 {
        let sec = u64::try_from(idle.tv_sec).unwrap_or(0);
        let usec = u64::try_from(idle.tv_usec).unwrap_or(0);
        let ns = sec
            .saturating_mul(1_000_000_000)
            .saturating_add(usec.saturating_mul(1000));
        crate::net::splice::arm(&spliced, ns);
    }
    Ok(0)
}

/// `getsockopt(s, level, name, val, avalsize)`.
pub(super) fn getsockopt(
    s: &Sleepable,
    sock_fd: u64,
    level: u64,
    name: u64,
    val: u64,
    avalsize: u64,
) -> SysResult {
    let size = if avalsize == 0 {
        0
    } else {
        UserPtr::<u32>::new(avalsize)
            .read()
            .map_err(|_| Errno::EFAULT)?
    };
    if size.cast_signed() < 0 {
        return Err(Errno::EINVAL);
    }
    let f = sock(s, sock_fd, &crate::capsicum::rights::GETSOCKOPT)?;
    let v = if int(level) == SOL_SOCKET && int(name) == SO_SPLICE {
        // Only internet sockets splice; the others have moved nothing.
        let n = super::inet::so_of(&f).map_or(0, |so| so.splice_sent());
        n.to_ne_bytes().to_vec()
    } else if let Some(so) = super::inet::so_of(&f) {
        super::inet::getsockopt(so, int(level), int(name))?
    } else if super::netlink::is_netlink(&f) {
        super::netlink::getsockopt(&f, int(level), int(name))?
    } else {
        so_of(&f).so.getopt(int(level), int(name))?
    };
    let n = (size as usize).min(v.len());
    if val != 0 {
        uaccess::copy_out(val, &v[..n]).map_err(|_| Errno::EFAULT)?;
    }
    if avalsize != 0 {
        UserPtr::<u32>::new(avalsize)
            .write(&u32::try_from(n).unwrap_or(0))
            .map_err(|_| Errno::EFAULT)?;
    }
    Ok(0)
}

/// `unp_internalize`: a send's control messages, in the kernel's form.
fn internalize(s: &Sleepable, raw: &[u8]) -> Result<Vec<Control>, Errno> {
    let p = process()?;
    let mut out = Vec::new();
    let rd = |b: &[u8], i: usize| -> u32 {
        u32::from_ne_bytes(b[i..i + 4].try_into().unwrap_or([0; 4]))
    };
    // FreeBSD's walk: each message whole, at the socket level, its
    // length past its header; what is left over is `EINVAL`.
    let mut at = 0;
    let mut clen = raw.len();
    while clen >= 12 {
        let len = rd(raw, at) as usize;
        let level = rd(raw, at + 4).cast_signed();
        let ty = rd(raw, at + 8).cast_signed();
        if level != SOL_SOCKET || clen < len || len < CMSG_HDR {
            break;
        }
        let data = &raw[at + CMSG_HDR..at + len];
        match ty {
            SCM_CREDS => out.push(socket::cmsgcred(&crate::fs::socket::cred())),
            SCM_RIGHTS => {
                let n = data.len() / 4;
                if n > 0 {
                    if cmsg_space(n * 8) > MCLBYTES {
                        return Err(Errno::EMSGSIZE);
                    }
                    let t = p.files.lock(s);
                    let mut files = Vec::with_capacity(n);
                    for i in 0..n {
                        let fd = rd(data, i * 4).cast_signed();
                        let slot = t.slot(fd)?;
                        files.push((Arc::clone(&slot.file), slot.caps.as_deref().cloned()));
                    }
                    drop(t);
                    // Each goes with its Capsicum limits (`unp_internalize`'s
                    // `filecaps_copy`).
                    let rights = files
                        .into_iter()
                        .map(|(f, caps)| {
                            let mut r = InFlight::new(f as Arc<dyn socket::Passable>, 0);
                            r.caps = caps.map(|c| {
                                alloc::boxed::Box::new(c)
                                    as alloc::boxed::Box<dyn core::any::Any + Send + Sync>
                            });
                            r
                        })
                        .collect();
                    out.push(Control::Rights(rights));
                }
            }
            SCM_TIMESTAMP => {
                let us = crate::shared_page::to_us(crate::shared_page::realtime());
                let mut d = Vec::new();
                d.extend_from_slice(&(us / 1_000_000).to_ne_bytes());
                d.extend_from_slice(&(us % 1_000_000).to_ne_bytes());
                out.push(Control::Raw { level, ty, data: d });
            }
            SCM_BINTIME | SCM_REALTIME | SCM_MONOTONIC => {
                let clock = if ty == SCM_MONOTONIC {
                    abi::time::clock::MONOTONIC
                } else {
                    abi::time::clock::REALTIME
                };
                let ns = super::clock_ns(clock)?;
                let mut d = Vec::new();
                d.extend_from_slice(&(ns / 1_000_000_000).to_ne_bytes());
                if ty == SCM_BINTIME {
                    let frac =
                        u64::try_from((u128::from(ns % 1_000_000_000) << 64) / 1_000_000_000)
                            .unwrap_or(u64::MAX);
                    d.extend_from_slice(&frac.to_ne_bytes());
                } else {
                    d.extend_from_slice(&(ns % 1_000_000_000).to_ne_bytes());
                }
                out.push(Control::Raw { level, ty, data: d });
            }
            _ => return Err(Errno::EINVAL),
        }
        let step = cmsg_space(data.len()).min(clen);
        clen -= step;
        at += step;
    }
    if clen > 0 {
        return Err(Errno::EINVAL);
    }
    Ok(out)
}

/// The common part of `sendto` and `sendmsg` (`kern_sendit`).
fn sendit(
    s: &Sleepable,
    sock_fd: u64,
    uio: &mut UserIo,
    to: Option<Vec<u8>>,
    control: Option<Vec<u8>>,
    flags: i32,
) -> SysResult {
    // An address names something outside the sandbox (`kern_sendit`).
    if let Some(to) = &to {
        crate::capsicum::refuse(crate::ktrace::CapFail::Sockaddr(to))?;
        crate::ktrace::sockaddr(s, to);
    }
    let f = sock(
        s,
        sock_fd,
        if to.is_some() {
            &crate::capsicum::rights::SEND_CONNECT
        } else {
            &crate::capsicum::rights::SEND
        },
    )?;
    if let Some(so) = super::inet::so_of(&f) {
        return super::inet::sendit(s, &f, so, sock_fd, uio, to, control, flags);
    }
    if super::netlink::is_netlink(&f) {
        return super::netlink::sendit(s, &f, sock_fd, uio, flags);
    }
    let so = &so_of(&f).so;
    let control = match control {
        Some(raw) => internalize(s, &raw)?,
        None => Vec::new(),
    };
    let to = match to {
        Some(sa) if so.ty == Type::Dgram => {
            let path = sun_path(&sa)?.to_vec();
            if so.is_connected() {
                return Err(Errno::EISCONN);
            }
            Some(peer_by_name(s, AT_FDCWD, &path)?)
        }
        // A stream's destination is its peer, whatever is named.
        _ => None,
    };
    let ktr = crate::ktrace::genio_start(uio);
    let r = crate::fs::socket::send(s, so, uio, control, to, flags, nonblock(&f));
    crate::ktrace::genio(s, sock_fd, true, ktr, &r);
    r.map(|n| n as u64)
}

/// `sendto(s, buf, len, flags, to, tolen)`.
pub(super) fn sendto(
    s: &Sleepable,
    sock_fd: u64,
    buf: u64,
    len: u64,
    flags: u64,
    to: u64,
    tolen: u64,
) -> SysResult {
    let mut uio = UserIo::one(buf, len)?;
    let to = if to == 0 {
        None
    } else {
        Some(sockaddr(to, tolen)?)
    };
    sendit(s, sock_fd, &mut uio, to, None, int(flags))
}

/// `sendmsg(s, msg, flags)`.
pub(super) fn sendmsg(s: &Sleepable, sock_fd: u64, msg: u64, flags: u64) -> SysResult {
    let m = UserPtr::<Msghdr>::new(msg)
        .read()
        .map_err(|_| Errno::EFAULT)?;
    let mut uio =
        UserIo::vector(m.msg_iov, u64::from(m.msg_iovlen.cast_unsigned())).map_err(|e| {
            if e == Errno::EINVAL {
                Errno::EMSGSIZE
            } else {
                e
            }
        })?;
    let to = if m.msg_name == 0 {
        None
    } else {
        Some(sockaddr(m.msg_name, u64::from(m.msg_namelen))?)
    };
    let control = if m.msg_control == 0 {
        None
    } else {
        let n = m.msg_controllen as usize;
        // `sockargs`.
        if !(12..=MCLBYTES).contains(&n) {
            return Err(Errno::EINVAL);
        }
        let mut raw = alloc::vec![0u8; n];
        uaccess::copy_in(&mut raw, m.msg_control).map_err(|_| Errno::EFAULT)?;
        Some(raw)
    };
    sendit(s, sock_fd, &mut uio, to, control, int(flags))
}

/// `unp_externalize`: received control messages in the user's form, with
/// their descriptors installed; each a whole `CMSG_SPACE`.
fn externalize(s: &Sleepable, control: Vec<Control>, flags: i32) -> Result<Vec<Vec<u8>>, Errno> {
    let p = process()?;
    let cred = crate::fs::cred();
    let fdf = FdFlags {
        cloexec: flags & MSG_CMSG_CLOEXEC != 0,
        clofork: flags & MSG_CMSG_CLOFORK != 0,
        resolve_beneath: false,
    };
    let limit = super::io::max_files(&p);
    let mut out = Vec::new();
    let mut err = Ok(());
    for c in control {
        match c {
            Control::Rights(rights) => {
                if err.is_err() {
                    continue;
                }
                // Each descriptor is written into the message as it is
                // installed; all of a message's or none (`fdallocn`).
                let mut m = cmsg(SOL_SOCKET, SCM_RIGHTS, rights.len() * 4);
                let mut got = 0;
                let mut t = p.files.lock(s);
                for mut r in rights {
                    if err.is_err() {
                        continue;
                    }
                    let caps = r
                        .caps
                        .take()
                        .and_then(|c| c.downcast::<crate::fs::fdtable::FileCaps>().ok())
                        .map(|c| *c);
                    let file = r.take();
                    match crate::fs::file::File::from_passable(file) {
                        // A directory from another jail's tree stays
                        // beneath itself here (`restrict_rights`).
                        Some(file) => match t.alloc_with(
                            Arc::clone(&file),
                            FdFlags {
                                resolve_beneath: file.restricted_for(&cred),
                                ..fdf
                            },
                            caps,
                            0,
                            limit,
                        ) {
                            Ok(fd) => {
                                let at = CMSG_HDR + got * 4;
                                m[at..at + 4].copy_from_slice(&fd.to_ne_bytes());
                                got += 1;
                            }
                            Err(e) => err = Err(e),
                        },
                        None => err = Err(Errno::EBADF),
                    }
                }
                if err.is_err() {
                    let closed: Vec<_> = m[CMSG_HDR..CMSG_HDR + got * 4]
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .filter_map(|fd| t.close(i32::from_ne_bytes(*fd)).ok())
                        .collect();
                    drop(t);
                    drop(closed);
                    continue;
                }
                drop(t);
                out.push(m);
            }
            Control::Raw { level, ty, data } => {
                let mut m = cmsg(level, ty, data.len());
                m[CMSG_HDR..CMSG_HDR + data.len()].copy_from_slice(&data);
                out.push(m);
            }
        }
    }
    err.map(|()| out)
}

/// A control message of `len` data bytes, its header filled in and its
/// data zero, a whole `CMSG_SPACE`.
fn cmsg(level: i32, ty: i32, len: usize) -> Vec<u8> {
    let mut m = alloc::vec![0u8; cmsg_space(len)];
    m[0..4].copy_from_slice(&u32::try_from(cmsg_len(len)).unwrap_or(0).to_ne_bytes());
    m[4..8].copy_from_slice(&level.to_ne_bytes());
    m[8..12].copy_from_slice(&ty.to_ne_bytes());
    m
}

/// Closes the descriptors of messages that did not fit
/// (`m_dispose_extcontrolm`).
fn dispose(s: &Sleepable, msgs: &[Vec<u8>]) {
    for m in msgs {
        let ty = i32::from_ne_bytes(m[8..12].try_into().unwrap_or([0; 4]));
        let len = u32::from_ne_bytes(m[0..4].try_into().unwrap_or([0; 4])) as usize;
        if ty != SCM_RIGHTS {
            continue;
        }
        for c in m[CMSG_HDR..len.min(m.len())].as_chunks::<4>().0 {
            close_fd(s, i32::from_ne_bytes(*c));
        }
    }
}

/// The common part of `recvfrom` and `recvmsg` (`kern_recvit`): the
/// count, and the name's and control's lengths and the flags out.
fn recvit(
    s: &Sleepable,
    sock_fd: u64,
    uio: &mut UserIo,
    flags: i32,
    name: Option<(u64, u32)>,
    control: Option<(u64, u32)>,
) -> Result<(usize, u32, u32, i32), Errno> {
    let f = sock(s, sock_fd, &crate::capsicum::rights::RECV)?;
    if let Some(so) = super::inet::so_of(&f) {
        return super::inet::recvit(s, &f, so, sock_fd, uio, flags, name, control);
    }
    if super::netlink::is_netlink(&f) {
        return super::netlink::recvit(s, &f, sock_fd, uio, flags, name);
    }
    let so = &so_of(&f).so;
    let mut msgs: Vec<Vec<u8>> = Vec::new();
    let mut ext = |c: Vec<Control>| -> Result<(), Errno> {
        msgs.extend(externalize(s, c, flags)?);
        Ok(())
    };
    let ext: Option<&mut dyn FnMut(Vec<Control>) -> Result<(), Errno>> = if control.is_some() {
        Some(&mut ext)
    } else {
        None
    };
    let ktr = crate::ktrace::genio_start(uio);
    let r = crate::fs::socket::receive(s, so, uio, flags, nonblock(&f), ext);
    crate::ktrace::genio(
        s,
        sock_fd,
        false,
        ktr,
        &r.as_ref().map(|r| r.count).map_err(|e| *e),
    );
    let r = r?;
    let n = r.count;
    // `msg_flags` comes back with the flags asked with (`kern_recvit`).
    let mut outflags = flags | r.flags;
    // `MSG_TRUNC` asks for a datagram's whole length.
    let count = if so.ty == Type::Dgram && flags & MSG_TRUNC != 0 {
        r.full
    } else {
        n
    };
    let mut namelen = 0;
    if let Some((addr, len)) = name
        && let Some(from) = &r.from
        && len.cast_signed() > 0
    {
        let k = (len as usize).min(from.len());
        uaccess::copy_out(addr, &from[..k]).map_err(|_| Errno::EFAULT)?;
        namelen = u32::try_from(k).unwrap_or(0);
        crate::ktrace::sockaddr(s, from);
    }
    let mut ctllen = 0;
    if let Some((addr, len)) = control {
        let mut left = len as usize;
        let mut at = 0;
        for (i, m) in msgs.iter().enumerate() {
            if left < m.len() {
                outflags |= MSG_CTRUNC;
                dispose(s, &msgs[i..]);
                break;
            }
            if uaccess::copy_out(addr + at as u64, m).is_err() {
                // Every received descriptor goes (`m_dispose_extcontrolm`).
                dispose(s, &msgs);
                return Err(Errno::EFAULT);
            }
            at += m.len();
            left -= m.len();
        }
        ctllen = u32::try_from(at).unwrap_or(0);
    }
    Ok((count, namelen, ctllen, outflags))
}

/// `recvfrom(s, buf, len, flags, from, fromlenaddr)`.
pub(super) fn recvfrom(
    s: &Sleepable,
    sock_fd: u64,
    buf: u64,
    len: u64,
    flags: u64,
    from: u64,
    lenp: u64,
) -> SysResult {
    let mut uio = UserIo::one(buf, len)?;
    let name = if from != 0 && lenp != 0 {
        Some((
            from,
            UserPtr::<u32>::new(lenp)
                .read()
                .map_err(|_| Errno::EFAULT)?,
        ))
    } else {
        None
    };
    let (n, namelen, _, _) = recvit(s, sock_fd, &mut uio, int(flags), name, None)?;
    if name.is_some() {
        UserPtr::<u32>::new(lenp)
            .write(&namelen)
            .map_err(|_| Errno::EFAULT)?;
    }
    Ok(n as u64)
}

/// `recvmsg(s, msg, flags)`.
pub(super) fn recvmsg(s: &Sleepable, sock_fd: u64, msg: u64, flags: u64) -> SysResult {
    let mut m = UserPtr::<Msghdr>::new(msg)
        .read()
        .map_err(|_| Errno::EFAULT)?;
    let mut uio =
        UserIo::vector(m.msg_iov, u64::from(m.msg_iovlen.cast_unsigned())).map_err(|e| {
            if e == Errno::EINVAL {
                Errno::EMSGSIZE
            } else {
                e
            }
        })?;
    let name = (m.msg_name != 0).then_some((m.msg_name, m.msg_namelen));
    let control = (m.msg_control != 0).then_some((m.msg_control, m.msg_controllen));
    let (n, namelen, ctllen, outflags) = recvit(s, sock_fd, &mut uio, int(flags), name, control)?;
    if name.is_some() {
        m.msg_namelen = namelen;
    }
    m.msg_controllen = ctllen;
    m.msg_flags = outflags;
    UserPtr::<Msghdr>::new(msg)
        .write(&m)
        .map_err(|_| Errno::EFAULT)?;
    Ok(n as u64)
}
