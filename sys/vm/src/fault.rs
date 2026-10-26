// SPDX-License-Identifier: BSD-2-Clause
//! Page-fault resolution (`vm_fault`): what to map at a faulting address,
//! or why nothing may be.
//!
//! [`resolve`] walks from the map entry down the object chain and returns
//! the page and the rights to map it with. Copy-on-write happens here: a
//! write to a shared entry first gives it a shadow, and a write to a page
//! the chain supplies from below copies it up into that shadow. A read of
//! such a page maps it without write rights, so the later write faults
//! again and copies.
//!
//! The caller holds the map's lock across the call and the page-table
//! update that follows, so a concurrent `munmap` cannot slip in between.
//! A page a pager has not read yet is not read under that lock (reading
//! may sleep on the file system): [`resolve`] returns [`Outcome::Fill`],
//! and the caller fills the page with the lock released and resolves
//! again.
//!
//! A shared mapping of an object whose pages go back to a pager maps a
//! clean page without write rights, so the first write faults, marks it
//! dirty, and maps it writable (dirty tracking without the hardware's
//! dirty bits).
//!
//! An access to the gap below a stack that grows down first grows it
//! ([`VmMap::grow_stack`]), as `vm_map_lookup` does.
//!
//! Behavior reference: sys/vm/vm_fault.c, `vm_map_lookup` in
//! sys/vm/vm_map.c.

use alloc::sync::Arc;

use crate::map::{Prot, Role, VmMap};
use crate::object::{Found, LookupError, MemAttr, Object, PageIndex};
use crate::{PAGE_SIZE, Pfn, VirtAddr};

/// The kind of access that faulted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
    Execute,
}

impl Access {
    const fn needs(self) -> Prot {
        match self {
            Self::Read => Prot::READ,
            Self::Write => Prot::WRITE,
            Self::Execute => Prot::EXEC,
        }
    }
}

/// Why a fault cannot be resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultError {
    /// Nothing is mapped there (`SIGSEGV`, `SEGV_MAPERR`).
    NotMapped,
    /// The mapping does not allow the access (`SIGSEGV`, `SEGV_ACCERR`).
    Protection,
    /// No memory for the page; the caller may wait and retry.
    OutOfMemory,
    /// Past the end of the object a file mapping reaches (`SIGBUS`).
    PastEnd,
}

/// What to map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// The faulting page.
    pub page: VirtAddr,
    /// The physical page to map there.
    pub pfn: Pfn,
    /// The rights to map it with: the entry's, without write if the page
    /// still belongs to an object below the entry's own.
    pub prot: Prot,
    /// How to map it: a device's memory as the device says.
    pub attr: MemAttr,
}

/// What a fault needs.
#[derive(Debug)]
pub enum Outcome {
    /// Map this.
    Map(Resolved),
    /// Read page `PageIndex` of this pager object first (no map lock
    /// held), then resolve again.
    Fill(Arc<Object>, PageIndex),
}

/// Resolves a fault at `va` for `access`.
pub fn resolve(map: &mut VmMap, va: VirtAddr, access: Access) -> Result<Outcome, FaultError> {
    let pages = map.pages;
    let page = va.align_down(PAGE_SIZE);
    // An access to a stack's gap grows the stack down to it, if it may.
    if map
        .entry(page)
        .is_some_and(|e| matches!(e.role, Role::StackGap { .. }))
    {
        map.grow_stack(page).map_err(|_| FaultError::Protection)?;
    }
    let entry = map.entry_mut(page).ok_or(FaultError::NotMapped)?;
    if !entry.prot.contains(access.needs()) {
        return Err(FaultError::Protection);
    }
    let write = access == Access::Write;
    let object = entry.object.get_or_insert_with(|| Object::anonymous(pages));
    if write && entry.needs_copy {
        // The entry's own copy: a shadow over the shared object, from
        // this entry's offset on.
        *object = Object::shadow(Arc::clone(object), entry.offset);
        entry.offset = 0;
        entry.needs_copy = false;
    }
    let entry = &*entry;
    // Made above.
    let Some(object) = &entry.object else {
        return Err(FaultError::NotMapped);
    };
    let index = entry.index_of(page);
    let read_only = entry.prot.without(Prot::WRITE);
    // If our object is ours alone, the lookup folds away what only it
    // still reaches.
    let found = object
        .lookup(index, !entry.needs_copy, false)
        .map_err(|e| match e {
            LookupError::OutOfMemory => FaultError::OutOfMemory,
            LookupError::PastEnd | LookupError::Io => FaultError::PastEnd,
        })?;
    let (pfn, prot) = match found {
        Found::Fill(o, i) => return Ok(Outcome::Fill(o, i)),
        Found::Top(pfn) if entry.needs_copy => (pfn, read_only),
        // A shared mapping of a written-back object: writable once dirty.
        Found::Top(pfn) if object.tracks_dirty() => {
            if write {
                object.mark_mapped_writable(index);
                (pfn, entry.prot)
            } else {
                (pfn, read_only)
            }
        }
        Found::Top(pfn) => (pfn, entry.prot),
        Found::Backing(pfn) if write => (
            object
                .insert_new(index, Some(pfn))
                .ok_or(FaultError::OutOfMemory)?,
            entry.prot,
        ),
        Found::Backing(pfn) => (pfn, read_only),
        Found::Absent => {
            let pfn = object
                .insert_new(index, None)
                .ok_or(FaultError::OutOfMemory)?;
            // A zero page in a still-shared object is shared too.
            (
                pfn,
                if entry.needs_copy {
                    read_only
                } else {
                    entry.prot
                },
            )
        }
    };
    let attr = object.memattr();
    Ok(Outcome::Map(Resolved {
        page,
        pfn,
        prot,
        attr,
    }))
}

/// Pages around a fault worth mapping with it (FreeBSD's
/// `vm_fault_prefault`): up to `around` pages each side of `va` within its
/// entry, in an outward spiral, that some object of the entry's chain
/// already holds. The first one missing ends the search. Each is to be
/// mapped read-only (the first write still faults, for copy-on-write and
/// dirtiness) and only where nothing is mapped yet.
#[must_use]
pub fn prefault(map: &VmMap, va: VirtAddr, around: u64) -> alloc::vec::Vec<Resolved> {
    let mut out = alloc::vec::Vec::new();
    let page = va.align_down(PAGE_SIZE);
    let Some(entry) = map.entry(page) else {
        return out;
    };
    let Some(object) = &entry.object else {
        return out;
    };
    let prot = entry.prot.without(Prot::WRITE);
    if prot == Prot::NONE {
        return out;
    }
    for i in 0..2 * around {
        let step = (i / 2 + 1) * PAGE_SIZE;
        let addr = if i % 2 == 0 {
            page.get().checked_sub(step)
        } else {
            page.get().checked_add(step)
        };
        let Some(addr) = addr.map(VirtAddr::new) else {
            continue;
        };
        if addr < entry.start() || addr >= entry.end() {
            continue;
        }
        match object.resident_page(entry.index_of(addr)) {
            Some(pfn) => out.push(Resolved {
                page: addr,
                pfn,
                prot,
                attr: object.memattr(),
            }),
            None => break,
        }
    }
    out
}
