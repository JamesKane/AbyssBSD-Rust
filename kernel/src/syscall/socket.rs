// SPDX-License-Identifier: BSD-2-Clause
//! Sockets: `socket` for UNIX-domain sockets, `bind` and `bindat`, which
//! make the socket's node, and `connect` and `connectat`, which find a
//! bound name but are refused, nothing listening yet (Phase 6 brings the
//! rest; see [`crate::fs::socket`]).
//!
//! Behavior reference: sys/kern/uipc_syscalls.c (`getsockaddr`),
//! sys/kern/uipc_usrreq.c (`unp_bind`, `unp_connect`).

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::errno::Errno;
use abi::fcntl::{AT_FDCWD, FREAD, FWRITE, O_NONBLOCK};
use abi::socket::{
    AF_UNIX, SOCK_CLOEXEC, SOCK_CLOFORK, SOCK_DGRAM, SOCK_MAXADDRLEN, SOCK_NONBLOCK,
    SOCK_SEQPACKET, SOCK_STREAM,
};
use sched::Sleepable;
use vfs::{Kind, NewNode};

use super::{SysResult, int, process};
use crate::fs::fdtable::FdFlags;
use crate::fs::file::{File, FileKind};
use crate::fs::socket::UnixSocket;
use crate::fs::{At, Walk};
use crate::uaccess;

/// Where `sun_path` starts in a `struct sockaddr_un`.
const SUN_PATH: usize = 2;

/// `socket(domain, type, protocol)`: UNIX-domain only.
pub(super) fn socket(s: &Sleepable, domain: u64, kind: u64, protocol: u64) -> SysResult {
    if int(domain) != i32::from(AF_UNIX) {
        return Err(Errno::EAFNOSUPPORT);
    }
    let kind = int(kind).cast_unsigned();
    let flags = kind & (SOCK_CLOEXEC | SOCK_NONBLOCK | SOCK_CLOFORK);
    let kind = kind & !flags;
    if ![SOCK_STREAM, SOCK_DGRAM, SOCK_SEQPACKET].contains(&kind) {
        return Err(Errno::EPROTOTYPE);
    }
    if int(protocol) != 0 {
        return Err(Errno::EPROTONOSUPPORT);
    }
    let p = process()?;
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
    };
    let limit = super::io::max_files(&p);
    let file = File::new(FileKind::Socket(UnixSocket::new()), status);
    let fd = p.files.lock(s).alloc(file, fdf, 0, limit)?;
    Ok(u64::from(fd.cast_unsigned()))
}

/// The UNIX-domain socket open as `fd`.
fn unix_socket(s: &Sleepable, fd: u64) -> Result<Arc<File>, Errno> {
    let f = super::io::file(s, &*process()?, fd)?;
    match f.kind() {
        FileKind::Socket(_) => Ok(f),
        _ => Err(Errno::ENOTSOCK),
    }
}

/// The path in a `struct sockaddr_un` of `len` bytes at `addr`: the
/// kernel takes `len` for its `sun_len` (FreeBSD's `getsockaddr`), and the
/// path ends at the first NUL.
fn unix_path(addr: u64, len: u64) -> Result<Vec<u8>, Errno> {
    let len = usize::try_from(int(len)).map_err(|_| Errno::EINVAL)?;
    if len > usize::try_from(SOCK_MAXADDRLEN).unwrap_or(usize::MAX) {
        return Err(Errno::ENAMETOOLONG);
    }
    if len < SUN_PATH {
        return Err(Errno::EINVAL);
    }
    let mut raw = alloc::vec![0u8; len];
    uaccess::copy_in(&mut raw, addr).map_err(|_| Errno::EFAULT)?;
    if raw[1] != AF_UNIX {
        return Err(Errno::EAFNOSUPPORT);
    }
    let path = &raw[SUN_PATH..];
    if path.is_empty() {
        return Err(Errno::EINVAL);
    }
    let end = path.iter().position(|&b| b == 0).unwrap_or(path.len());
    Ok(path[..end].to_vec())
}

/// `bindat(fd, s, addr, addrlen)`: makes the socket's node at the path,
/// from directory `fd` (`EADDRINUSE` if the name exists).
pub(super) fn bindat(s: &Sleepable, fd: u64, sock: u64, addr: u64, len: u64) -> SysResult {
    let f = unix_socket(s, sock)?;
    let FileKind::Socket(so) = f.kind() else {
        return Err(Errno::ENOTSOCK);
    };
    let path = unix_path(addr, len)?;
    if so.is_bound() {
        return Err(Errno::EINVAL);
    }
    let p = process()?;
    let parent = crate::fs::parent_of(s, &p, At::new(int(fd)), &path, false)?;
    if parent.trailing_slash {
        return Err(Errno::ENOENT);
    }
    let mask = p.umask.load(core::sync::atomic::Ordering::Relaxed);
    let (v, _) = vfs::ops::create(
        s,
        &parent.dir,
        &parent.name,
        &NewNode::Socket,
        &super::fsops::create_as(0o777 & !mask),
        &p.cred(),
        true,
    )
    .map_err(|e| {
        if e == Errno::EEXIST {
            Errno::EADDRINUSE
        } else {
            e
        }
    })?;
    so.set_bound(v.use_ref());
    Ok(0)
}

/// `bind(s, addr, addrlen)`.
pub(super) fn bind(s: &Sleepable, sock: u64, addr: u64, len: u64) -> SysResult {
    bindat(s, u64::from(AT_FDCWD.cast_unsigned()), sock, addr, len)
}

/// `connectat(fd, s, addr, addrlen)`: the name must be a socket the
/// caller may write; nothing listens on one yet, so it is refused.
pub(super) fn connectat(s: &Sleepable, fd: u64, sock: u64, addr: u64, len: u64) -> SysResult {
    unix_socket(s, sock)?;
    let path = unix_path(addr, len)?;
    let p = process()?;
    let v = crate::fs::lookup(s, &p, At::new(int(fd)), &path, Walk::FOLLOW)?;
    if v.kind() != Kind::Socket {
        return Err(Errno::ENOTSOCK);
    }
    vfs::ops::access(&v.attr(s)?, v.kind(), &p.cred(), vfs::ops::VWRITE)?;
    Err(Errno::ECONNREFUSED)
}

/// `connect(s, addr, addrlen)`.
pub(super) fn connect(s: &Sleepable, sock: u64, addr: u64, len: u64) -> SysResult {
    connectat(s, u64::from(AT_FDCWD.cast_unsigned()), sock, addr, len)
}
