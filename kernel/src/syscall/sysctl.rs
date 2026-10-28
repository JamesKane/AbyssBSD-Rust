// SPDX-License-Identifier: BSD-2-Clause
//! `__sysctl` and `__sysctlbyname` over the kernel's sysctl tree.
//!
//! An OID below `{0}` asks about the tree itself, as FreeBSD's
//! `sysctl.*` nodes answer: a name, the next OID in listing order, the OID
//! for a name, a node's kind and format, its description. Values come back
//! as FreeBSD returns them: without a buffer, the length needed; with one
//! too small, as much as fits and `ENOMEM`. Nothing is writable yet.
//!
//! A name looked up and not found, an OID not found, and a write are
//! reported as first uses ([`crate::firstuse`]): FreeBSD may have the node.
//! Walking off the end of the tree (`sysctl.next`) is not.
//!
//! Behavior reference: sys/kern/kern_sysctl.c.

use alloc::vec::Vec;

use abi::errno::Errno;
use abi::sysctl::{CTL_MAXNAME, ctl, ctlflag, query};
use sched::Sleepable;
use sysctl::ReadError;

use super::SysResult;
use crate::firstuse::{self, Kind};
use crate::uaccess::{self, UserPtr};

/// Reports a name looked up and not found.
fn missing_name(name: &str) {
    firstuse::report(Kind::Sysctl, format_args!("{name}"));
}

/// An OID as dotted numbers, after the name of the longest part the tree
/// knows: `kern.proc.12.34` style, as `sysctl -N` would print it.
struct Oid<'a>(&'a [i32], Option<(alloc::string::String, usize)>);

impl core::fmt::Display for Oid<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let (mut sep, rest) = match &self.1 {
            Some((name, n)) => {
                f.write_str(name)?;
                (".", &self.0[*n..])
            }
            None => ("", self.0),
        };
        for n in rest {
            write!(f, "{sep}{n}")?;
            sep = ".";
        }
        Ok(())
    }
}

/// `oid`'s known prefix, named.
fn known_prefix(s: &Sleepable, oid: &[i32]) -> Option<(alloc::string::String, usize)> {
    crate::sysctl::read(s, |t| {
        (1..=oid.len())
            .rev()
            .find_map(|n| t.name(&oid[..n]).map(|name| (name, n)))
    })
}

fn read_error(e: ReadError) -> Errno {
    match e {
        ReadError::NotFound => Errno::ENOENT,
        ReadError::IsNode => Errno::EISDIR,
        ReadError::Invalid => Errno::EINVAL,
        ReadError::Failed(e) => e,
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

/// The value of `oid`, given `new` (the bytes to write, if any), and the
/// writer to give them to once the old value is out; `reading` if the
/// caller asked for the value (`oldp`).
fn serve(
    s: &Sleepable,
    oid: &[i32],
    new: Option<&[u8]>,
    reading: bool,
) -> Result<(Vec<u8>, Option<sysctl::WriteFn>), Errno> {
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
                t.name2oid(name).map(|o| ints(&o)).ok_or_else(|| {
                    missing_name(name);
                    Errno::ENOENT
                })
            }
            query::OIDFMT => {
                let (kind, fmt) = t.format(rest).ok_or(Errno::ENOENT)?;
                let mut v = kind.to_ne_bytes().to_vec();
                v.extend_from_slice(&cstr(fmt));
                Ok(v)
            }
            query::OIDDESCR => t.description(rest).map(cstr).ok_or(Errno::ENOENT),
            _ => Err(Errno::ENOENT),
        })
        .map(|v| (v, None));
    }
    // `sysctl_root`'s checks.
    let (kind, writer) = crate::sysctl::read(s, |t| t.access(oid)).unzip();
    let kind = kind.unwrap_or(0);
    let writer = writer.flatten();
    let cred = crate::fs::cred();
    if new.is_some() && (kind & ctlflag::WR == 0 || writer.is_none()) {
        firstuse::report(
            Kind::Sysctl,
            format_args!("{} (write)", Oid(oid, known_prefix(s, oid))),
        );
        return Err(Errno::EPERM);
    }
    // In capability mode only what is marked for it: `EPERM` in 15.1 (16
    // says `ECAPMODE`).
    if cred.in_capability_mode()
        && ((reading && kind & ctlflag::CAPRD == 0)
            || (new.is_some() && kind & ctlflag::CAPWR == 0))
    {
        return Err(Errno::EPERM);
    }
    if new.is_some() {
        if kind & ctlflag::SECURE != 0 {
            let lvl = (kind & ctlflag::MASK_SECURE) >> ctlflag::SHIFT_SECURE;
            cred.securelevel_gt(lvl.cast_signed())?;
        }
        if kind & ctlflag::ANYBODY == 0 {
            cred.priv_check(if kind & ctlflag::PRISON != 0 {
                crate::cred::Priv::SysctlWritejail
            } else {
                crate::cred::Priv::SysctlWrite
            })?;
        }
    }
    let mut out = Vec::new();
    crate::sysctl::read_oid(s, oid, &mut out).map_err(|e| {
        if matches!(e, ReadError::NotFound) {
            firstuse::report(
                Kind::Sysctl,
                format_args!("{}", Oid(oid, known_prefix(s, oid))),
            );
        }
        read_error(e)
    })?;
    Ok((out, new.and(writer)))
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
    let (value, writer) = serve(s, &oid, new.as_deref(), oldp != 0)?;
    reply(&value, oldp, oldlenp)?;
    // The old value is out; now the new one goes in (a handler's
    // `SYSCTL_OUT` before its `SYSCTL_IN`).
    if let (Some(w), Some(new)) = (writer, new) {
        w(&new)?;
    }
    Ok(0)
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
    let oid = crate::sysctl::read(s, |t| t.name2oid(name)).ok_or_else(|| {
        missing_name(name);
        Errno::ENOENT
    })?;
    let new = new_value(newp, newlen)?;
    let (value, writer) = serve(s, &oid, new.as_deref(), oldp != 0)?;
    reply(&value, oldp, oldlenp)?;
    // The old value is out; now the new one goes in (a handler's
    // `SYSCTL_OUT` before its `SYSCTL_IN`).
    if let (Some(w), Some(new)) = (writer, new) {
        w(&new)?;
    }
    Ok(0)
}
