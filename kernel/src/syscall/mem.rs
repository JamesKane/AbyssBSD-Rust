// SPDX-License-Identifier: BSD-2-Clause
//! Memory: `mmap` (anonymous memory and guards), `munmap`, `mprotect`,
//! `minherit`, and `madvise`.
//!
//! Behavior reference: sys/vm/vm_mmap.c.

use abi::errno::Errno;
use abi::mman::*;
use addr::VirtAddr;
use sched::Sleepable;
use vm::PAGE_SIZE;
use vm::map::{Inherit, MapError, Prot};

use super::{SysResult, int, process};
use crate::mm::space::{Anon, SpaceError, USER_END, USER_START};

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
    let p = process()?;
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
    if !guard && flags & MAP_ANON == 0 {
        // File mappings come with file descriptors (P4.5).
        return Err(Errno::EBADF);
    }
    if int(fd) != -1 || pos != 0 {
        return Err(Errno::EINVAL);
    }
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
            VirtAddr::new(p.mmap_base())
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
    let at = p.space().map_anon(s, &a).map_err(map_error)?;
    Ok(at.get())
}

/// `munmap(addr, len)`.
pub(super) fn munmap(s: &Sleepable, addr: u64, len: u64) -> SysResult {
    if !addr.is_multiple_of(PAGE_SIZE) || len == 0 {
        return Err(Errno::EINVAL);
    }
    let (start, end) = pages(addr, len)?;
    process()?.space().unmap(s, start..end).map_err(map_error)?;
    Ok(0)
}

/// `mprotect(addr, len, prot)`.
pub(super) fn mprotect(s: &Sleepable, addr: u64, len: u64, prot: u64) -> SysResult {
    let (prot, _) = prots(prot)?;
    if len == 0 {
        return Ok(0);
    }
    let (start, end) = pages(addr, len)?;
    process()?
        .space()
        .protect(s, start..end, prot)
        .map_err(map_error)?;
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
    process()?
        .space()
        .set_inherit(s, start..end, inherit)
        .map_err(map_error)?;
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
