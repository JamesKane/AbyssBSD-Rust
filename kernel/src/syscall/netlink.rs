// SPDX-License-Identifier: BSD-2-Clause
//! Socket system calls on netlink sockets (`PF_NETLINK`), which
//! `socket.rs` hands a netlink socket's file to: the requests a send
//! carries are answered at once, the replies read back with `recv`.
//!
//! Behavior reference: sys/netlink/netlink_domain.c.

use alloc::vec::Vec;

use abi::errno::Errno;
use abi::fcntl::{FREAD, FWRITE, O_NONBLOCK};
use abi::inet::SOCK_RAW;
use abi::socket::{SOCK_CLOEXEC, SOCK_CLOFORK, SOCK_DGRAM, SOCK_NONBLOCK};
use sched::Sleepable;

use super::{SysResult, process};
use crate::fs::fdtable::FdFlags;
use crate::fs::file::{File, FileKind};
use crate::fs::uio::UserIo;
use crate::net::netlink::NetlinkFile;
use crate::uaccess;

/// `socket(PF_NETLINK, type, protocol)`.
pub(super) fn socket(s: &Sleepable, ty: u32, flags: u32, protocol: i32) -> SysResult {
    if ty != SOCK_RAW && ty != SOCK_DGRAM {
        return Err(Errno::EPROTOTYPE);
    }
    let p = process()?;
    let c = p.cred();
    let nl = NetlinkFile::new(protocol, ty, p.pid().cast_unsigned(), c.euid(), c.egid())?;
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
    let file = File::new(FileKind::Netlink(nl), status);
    let fd = p.files.lock(s).alloc_with(file, fdf, None, 0, limit)?;
    Ok(u64::from(fd.cast_unsigned()))
}

/// Whether `f` is a netlink socket.
pub(super) fn is_netlink(f: &File) -> bool {
    matches!(f.kind(), FileKind::Netlink(_))
}

fn nl_of(f: &File) -> Result<&NetlinkFile, Errno> {
    match f.kind() {
        FileKind::Netlink(n) => Ok(n),
        _ => Err(Errno::ENOTSOCK),
    }
}

/// `bind(s, sockaddr_nl, len)`.
pub(super) fn bind(f: &File, sa: &[u8]) -> SysResult {
    nl_of(f)?.nl.bind(sa)?;
    Ok(0)
}

/// `connect(s, sockaddr_nl, len)`: only the kernel (port 0) is a peer.
pub(super) fn connect(f: &File, sa: &[u8]) -> SysResult {
    let nl = nl_of(f)?;
    if sa.len() < 12 || sa[1] != abi::inet::AF_NETLINK {
        return Err(Errno::EINVAL);
    }
    if sa[4..8] != [0; 4] {
        return Err(Errno::ECONNREFUSED);
    }
    let _ = nl;
    Ok(0)
}

/// `getsockname` and `getpeername`.
pub(super) fn name(f: &File, peer: bool) -> Result<Vec<u8>, Errno> {
    Ok(nl_of(f)?.nl.name(peer))
}

pub(super) fn setsockopt(f: &File, level: i32, name: i32, val: &[u8]) -> SysResult {
    nl_of(f)?.nl.setopt(level, name, val)?;
    Ok(0)
}

pub(super) fn getsockopt(f: &File, level: i32, name: i32) -> Result<Vec<u8>, Errno> {
    nl_of(f)?.nl.getopt(level, name)
}

/// `sendto` and `sendmsg`: the requests, answered at once. The
/// destination, if named, must be the kernel.
pub(super) fn sendit(
    s: &Sleepable,
    f: &File,
    sock_fd: u64,
    uio: &mut UserIo,
    flags: i32,
) -> SysResult {
    let _ = flags;
    let ktr = crate::ktrace::genio_start(uio);
    let r = nl_of(f).and_then(|n| n.write(uio));
    crate::ktrace::genio(s, sock_fd, true, ktr, &r);
    r.map(|n| n as u64)
}

/// `recvfrom` and `recvmsg`: the count, name length, control length (no
/// control: `NETLINK_MSG_INFO` is not carried), and flags.
pub(super) fn recvit(
    s: &Sleepable,
    f: &File,
    sock_fd: u64,
    uio: &mut UserIo,
    flags: i32,
    name: Option<(u64, u32)>,
) -> Result<(usize, u32, u32, i32), Errno> {
    let nl = nl_of(f)?;
    let nonblock = f.flags() & O_NONBLOCK != 0;
    let ktr = crate::ktrace::genio_start(uio);
    let r = nl.nl.receive(s, uio, flags, nonblock);
    crate::ktrace::genio(
        s,
        sock_fd,
        false,
        ktr,
        &r.as_ref().map(|r| r.0).map_err(|e| *e),
    );
    let (n, rflags) = r?;
    let mut namelen = 0;
    if let Some((addr, len)) = name
        && len.cast_signed() > 0
    {
        let sa = nl.nl.name(true);
        let k = (len as usize).min(sa.len());
        uaccess::copy_out(addr, &sa[..k]).map_err(|_| Errno::EFAULT)?;
        namelen = u32::try_from(k).unwrap_or(0);
    }
    Ok((n, namelen, 0, flags | rflags))
}
