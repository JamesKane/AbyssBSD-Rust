// SPDX-License-Identifier: BSD-2-Clause
//! `__sysctl` and `__sysctlbyname` over the kernel's sysctl tree.
//!
//! An OID below `{0}` asks about the tree itself, as FreeBSD's
//! `sysctl.*` nodes answer: a name, the next OID in listing order, the OID
//! for a name, a node's kind and format, its description. Values come back
//! as FreeBSD returns them: without a buffer, the length needed; with one
//! too small, as much as fits and `ENOMEM`. Nothing is writable yet.
//!
//! Behavior reference: sys/kern/kern_sysctl.c.

use alloc::vec::Vec;

use abi::errno::Errno;
use abi::sysctl::{CTL_MAXNAME, ctl, query};
use sched::Sleepable;
use sysctl::ReadError;

use super::SysResult;
use crate::uaccess::{self, UserPtr};

fn read_error(e: ReadError) -> Errno {
    match e {
        ReadError::NotFound => Errno::ENOENT,
        ReadError::IsNode => Errno::EISDIR,
        ReadError::Invalid => Errno::EINVAL,
    }
}

fn ints(v: &[i32]) -> Vec<u8> {
    v.iter().flat_map(|n| n.to_ne_bytes()).collect()
}

fn cstr(s: &str) -> Vec<u8> {
    let mut v = s.as_bytes().to_vec();
    v.push(0);
    v
}

/// The value of `oid`, given `new` (the bytes to write, if any).
fn serve(s: &Sleepable, oid: &[i32], new: Option<&[u8]>) -> Result<Vec<u8>, Errno> {
    if oid.first() == Some(&ctl::SYSCTL) {
        let (what, rest) = (oid.get(1).copied().ok_or(Errno::ENOENT)?, &oid[2..]);
        return crate::sysctl::read(s, |t| match what {
            query::NAME => t.name(rest).map(|n| cstr(&n)).ok_or(Errno::ENOENT),
            query::NEXT => t.next(rest, true).map(|o| ints(&o)).ok_or(Errno::ENOENT),
            query::NEXTNOSKIP => t.next(rest, false).map(|o| ints(&o)).ok_or(Errno::ENOENT),
            query::NAME2OID => {
                let name = new.ok_or(Errno::EINVAL)?;
                let name = name.strip_suffix(&[0]).unwrap_or(name);
                let name = core::str::from_utf8(name).map_err(|_| Errno::ENOENT)?;
                t.name2oid(name).map(|o| ints(&o)).ok_or(Errno::ENOENT)
            }
            query::OIDFMT => {
                let (kind, fmt) = t.format(rest).ok_or(Errno::ENOENT)?;
                let mut v = kind.to_ne_bytes().to_vec();
                v.extend_from_slice(&cstr(fmt));
                Ok(v)
            }
            query::OIDDESCR => t.description(rest).map(cstr).ok_or(Errno::ENOENT),
            _ => Err(Errno::ENOENT),
        });
    }
    if new.is_some() {
        return Err(Errno::EPERM);
    }
    let mut out = Vec::new();
    crate::sysctl::read_oid(s, oid, &mut out).map_err(read_error)?;
    Ok(out)
}

/// Hands `value` back through `oldp`/`oldlenp`, FreeBSD's way.
fn reply(value: &[u8], oldp: u64, oldlenp: u64) -> SysResult {
    if oldlenp == 0 {
        return Ok(0);
    }
    let lenp = UserPtr::<u64>::new(oldlenp);
    if oldp == 0 {
        lenp.write(&(value.len() as u64))
            .map_err(|_| Errno::EFAULT)?;
        return Ok(0);
    }
    let room = usize::try_from(lenp.read().map_err(|_| Errno::EFAULT)?).unwrap_or(usize::MAX);
    let n = value.len().min(room);
    uaccess::copy_out(oldp, &value[..n]).map_err(|_| Errno::EFAULT)?;
    lenp.write(&(n as u64)).map_err(|_| Errno::EFAULT)?;
    if n < value.len() {
        return Err(Errno::ENOMEM);
    }
    Ok(0)
}

fn new_value(newp: u64, newlen: u64) -> Result<Option<Vec<u8>>, Errno> {
    if newp == 0 {
        return Ok(None);
    }
    let len = usize::try_from(newlen)
        .ok()
        .filter(|&l| l <= 64 * 1024)
        .ok_or(Errno::EINVAL)?;
    let mut v = alloc::vec![0u8; len];
    uaccess::copy_in(&mut v, newp).map_err(|_| Errno::EFAULT)?;
    Ok(Some(v))
}

/// `__sysctl(name, namelen, old, oldlenp, new, newlen)`.
pub(super) fn sysctl(
    s: &Sleepable,
    name: u64,
    namelen: u64,
    oldp: u64,
    oldlenp: u64,
    newp: u64,
    newlen: u64,
) -> SysResult {
    let n = usize::try_from(namelen)
        .ok()
        .filter(|n| (2..=CTL_MAXNAME).contains(n))
        .ok_or(Errno::EINVAL)?;
    let oid = (0..n)
        .map(|i| UserPtr::<i32>::new(name + 4 * i as u64).read())
        .collect::<Result<Vec<i32>, _>>()
        .map_err(|_| Errno::EFAULT)?;
    let new = new_value(newp, newlen)?;
    let value = serve(s, &oid, new.as_deref())?;
    reply(&value, oldp, oldlenp)
}

/// `__sysctlbyname(name, namelen, old, oldlenp, new, newlen)`.
pub(super) fn sysctlbyname(
    s: &Sleepable,
    name: u64,
    namelen: u64,
    oldp: u64,
    oldlenp: u64,
    newp: u64,
    newlen: u64,
) -> SysResult {
    let len = usize::try_from(namelen)
        .ok()
        .filter(|&l| l <= 1024)
        .ok_or(Errno::ENAMETOOLONG)?;
    let mut buf = alloc::vec![0u8; len];
    uaccess::copy_in(&mut buf, name).map_err(|_| Errno::EFAULT)?;
    let name = buf.strip_suffix(&[0]).unwrap_or(&buf);
    let name = core::str::from_utf8(name).map_err(|_| Errno::ENOENT)?;
    let oid = crate::sysctl::read(s, |t| t.name2oid(name)).ok_or(Errno::ENOENT)?;
    let new = new_value(newp, newlen)?;
    let value = serve(s, &oid, new.as_deref())?;
    reply(&value, oldp, oldlenp)
}
