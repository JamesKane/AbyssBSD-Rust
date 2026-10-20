// SPDX-License-Identifier: BSD-2-Clause
//! Memory: `mmap` (anonymous memory, guards, files, and `/dev/zero`),
//! `munmap`, `mprotect`, `minherit`, and `madvise`.
//!
//! A file is mapped privately: its pages are shared until written, then
//! copied. That is also what `MAP_SHARED` gives, since no file can be
//! open for writing yet, so no shared mapping can be writable.
//!
//! Behavior reference: sys/vm/vm_mmap.c.

use alloc::sync::Arc;

use abi::errno::Errno;
use abi::mman::*;
use addr::VirtAddr;
use sched::Sleepable;
use vm::PAGE_SIZE;
use vm::map::{Inherit, MapError, Prot};

use super::{SysResult, int};
use crate::mm::space::{AddressSpace, Anon, SpaceError, USER_END, USER_START};
use crate::proc::Process;

fn map_error(e: SpaceError) -> Errno {
    match e {
        SpaceError::OutOfMemory | SpaceError::Map(MapError::NoSpace) => Errno::ENOMEM,
        SpaceError::Map(MapError::NotMapped) => Errno::ENOMEM,
        SpaceError::Map(MapError::Protection) => Errno::EACCES,
        SpaceError::Map(_) => Errno::EINVAL,
    }
}

/// `PROT_*` bits (and a `PROT_MAX`) as rights; anything else is invalid.
fn prots(bits: u64) -> Result<(Prot, Prot), Errno> {
    let bits = u32::try_from(bits).map_err(|_| Errno::EINVAL)?;
    let prot = Prot::from_bits(bits & 0xffff).ok_or(Errno::EINVAL)?;
    let max = Prot::from_bits(bits >> PROT_MAX_SHIFT).ok_or(Errno::EINVAL)?;
    let max = if max == Prot::NONE { Prot::ALL } else { max };
    if !max.contains(prot) {
        return Err(Errno::ENOTSUP);
    }
    Ok((prot, max))
}

/// `[addr, addr + len)` widened to pages, if it is a user range.
fn pages(addr: u64, len: u64) -> Result<(VirtAddr, VirtAddr), Errno> {
    let start = addr & !(PAGE_SIZE - 1);
    let end = addr
        .checked_add(len)
        .and_then(|e| e.checked_next_multiple_of(PAGE_SIZE))
        .ok_or(Errno::EINVAL)?;
    if start < USER_START || end > USER_END {
        return Err(Errno::EINVAL);
    }
    Ok((VirtAddr::new(start), VirtAddr::new(end)))
}

/// What a descriptor maps as.
enum Backing {
    /// Zero-filled memory (`/dev/zero`), as `MAP_ANON` gives.
    Zeros,
    /// A file's object, from a page on.
    Object(Arc<vm::object::Object>, u64),
}

/// What a descriptor maps as, for `mmap` at page-aligned `pos`, and the
/// most rights the mapping may have (FreeBSD's `vn_mmap`).
fn file_object(
    s: &Sleepable,
    fd: u64,
    pos: u64,
    len: u64,
    prot: Prot,
    flags: u32,
) -> Result<(Backing, Prot), Errno> {
    use crate::fs::file::FileKind;
    use abi::fcntl::{FREAD, FWRITE};
    let f = super::io::current_file(s, fd)?;
    let FileKind::Vnode(vf) = f.kind() else {
        return Err(Errno::ENODEV);
    };
    if let Some(d) = &vf.device {
        return if d.ops.maps_as_zeros() {
            Ok((Backing::Zeros, Prot::ALL))
        } else {
            Err(Errno::ENODEV)
        };
    }
    let v = vf.vnode.vnode();
    match v.kind() {
        vfs::Kind::Regular => {
            let mut max = Prot::EXEC;
            if f.flags() & FREAD != 0 {
                max = max.union(Prot::READ);
            } else if prot.contains(Prot::READ) {
                return Err(Errno::EACCES);
            }
            if flags & MAP_SHARED == 0 || f.flags() & FWRITE != 0 {
                max = max.union(Prot::WRITE);
            } else if prot.contains(Prot::WRITE) {
                return Err(Errno::EACCES);
            }
            // Offsets are `off_t`s: no negative ones, no wrapping past the
            // largest.
            let size = len.next_multiple_of(PAGE_SIZE);
            if pos > i64::MAX.cast_unsigned() - size {
                return Err(Errno::EINVAL);
            }
            let object = crate::fs::object(s, v).ok_or(Errno::ENODEV)?;
            Ok((Backing::Object(object, pos / PAGE_SIZE), max))
        }
        vfs::Kind::CharDevice => Err(Errno::ENODEV),
        _ => Err(Errno::EINVAL),
    }
}

/// Runs `f` on the current address space, borrowed.
fn in_space<R>(f: impl FnOnce(&AddressSpace) -> Result<R, SpaceError>) -> Result<R, Errno> {
    AddressSpace::with_current(f)
        .ok_or(Errno::EINVAL)?
        .map_err(map_error)
}

/// `mmap(addr, len, prot, flags, fd, pos)`.
pub(super) fn mmap(
    s: &Sleepable,
    addr: u64,
    len: u64,
    prot: u64,
    flags: u64,
    fd: u64,
    pos: u64,
) -> SysResult {
    let flags = u32::try_from(flags).map_err(|_| Errno::EINVAL)?;
    let (prot, max_prot) = prots(prot)?;
    let known = MAP_SHARED
        | MAP_PRIVATE
        | MAP_FIXED
        | MAP_HASSEMAPHORE
        | MAP_STACK
        | MAP_NOSYNC
        | MAP_ANON
        | MAP_GUARD
        | MAP_EXCL
        | MAP_NOCORE
        | MAP_PREFAULT_READ
        | MAP_ALIGNMENT_MASK;
    if flags & !known != 0 || len == 0 {
        return Err(Errno::EINVAL);
    }
    if flags & (MAP_SHARED | MAP_PRIVATE) == MAP_SHARED | MAP_PRIVATE {
        return Err(Errno::EINVAL);
    }
    let guard = flags & MAP_GUARD != 0;
    if guard
        && (prot != Prot::NONE
            || flags & !(MAP_GUARD | MAP_FIXED | MAP_EXCL | MAP_ALIGNMENT_MASK) != 0
            || int(fd) != -1)
    {
        return Err(Errno::EINVAL);
    }
    let mut max_prot = max_prot;
    // A file's mapping starts at a page; the result keeps `pos`'s offset
    // into it.
    let mut pageoff = 0;
    let object = if guard || flags & MAP_ANON != 0 {
        if int(fd) != -1 || pos != 0 {
            return Err(Errno::EINVAL);
        }
        Backing::Zeros
    } else {
        if flags & (MAP_SHARED | MAP_PRIVATE) == 0 {
            return Err(Errno::EINVAL);
        }
        pageoff = pos & (PAGE_SIZE - 1);
        let (object, file_max) = file_object(s, fd, pos - pageoff, len, prot, flags)?;
        max_prot = Prot::from_bits(max_prot.bits() & file_max.bits()).unwrap_or(Prot::NONE);
        if !max_prot.contains(prot) {
            return Err(Errno::EACCES);
        }
        object
    };
    let len = len.checked_add(pageoff).ok_or(Errno::ENOMEM)?;
    let addr = if flags & MAP_FIXED != 0 {
        addr.wrapping_sub(pageoff)
    } else {
        addr
    };
    let align = match (flags & MAP_ALIGNMENT_MASK) >> MAP_ALIGNMENT_SHIFT {
        0 => PAGE_SIZE,
        // A superpage: 2 MiB on both architectures.
        1 => 2 << 20,
        n if (12..64).contains(&n) => 1 << n,
        _ => return Err(Errno::EINVAL),
    };
    let fixed = flags & MAP_FIXED != 0;
    if fixed && !addr.is_multiple_of(align.max(PAGE_SIZE)) {
        return Err(Errno::EINVAL);
    }
    let (start, end) = pages(
        if addr == 0 && !fixed {
            USER_START
        } else {
            addr
        },
        len,
    )?;
    let len = end.get() - start.get();
    let a = Anon {
        fixed: fixed.then_some(start),
        exclusive: flags & MAP_EXCL != 0,
        // Like FreeBSD, an address without MAP_FIXED is only a hint.
        hint: if fixed || addr == 0 {
            VirtAddr::new(Process::with_current(Process::mmap_base).ok_or(Errno::EINVAL)?)
        } else {
            start
        },
        len,
        align,
        prot,
        max_prot: if guard { Prot::NONE } else { max_prot },
        inherit: if flags & MAP_SHARED != 0 {
            Inherit::Share
        } else {
            Inherit::Copy
        },
    };
    if a.exclusive && !fixed {
        return Err(Errno::EINVAL);
    }
    let at = in_space(|space| match object {
        Backing::Object(object, page) => space.map_object(s, &a, object, page),
        Backing::Zeros => space.map_anon(s, &a),
    })?;
    Ok(at.get() + pageoff)
}

/// `munmap(addr, len)`.
pub(super) fn munmap(s: &Sleepable, addr: u64, len: u64) -> SysResult {
    if !addr.is_multiple_of(PAGE_SIZE) || len == 0 {
        return Err(Errno::EINVAL);
    }
    let (start, end) = pages(addr, len)?;
    in_space(|space| space.unmap(s, start..end))?;
    Ok(0)
}

/// `mprotect(addr, len, prot)`.
pub(super) fn mprotect(s: &Sleepable, addr: u64, len: u64, prot: u64) -> SysResult {
    let (prot, _) = prots(prot)?;
    if len == 0 {
        return Ok(0);
    }
    let (start, end) = pages(addr, len)?;
    in_space(|space| space.protect(s, start..end, prot))?;
    Ok(0)
}

/// `minherit(addr, len, inherit)`.
pub(super) fn minherit(s: &Sleepable, addr: u64, len: u64, inherit: u64) -> SysResult {
    let inherit = match u32::try_from(inherit).map_err(|_| Errno::EINVAL)? {
        INHERIT_SHARE => Inherit::Share,
        INHERIT_COPY => Inherit::Copy,
        INHERIT_NONE => Inherit::None,
        INHERIT_ZERO => Inherit::Zero,
        _ => return Err(Errno::EINVAL),
    };
    if len == 0 {
        return Ok(0);
    }
    let (start, end) = pages(addr, len)?;
    in_space(|space| space.set_inherit(s, start..end, inherit))?;
    Ok(0)
}

/// `madvise(addr, len, behav)`: advice is taken (and so far ignored).
pub(super) fn madvise(addr: u64, len: u64, behav: u64) -> SysResult {
    if behav > u64::from(MADV_PROTECT) {
        return Err(Errno::EINVAL);
    }
    if len > 0 {
        pages(addr, len)?;
    }
    Ok(0)
}
