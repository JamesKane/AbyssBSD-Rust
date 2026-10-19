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
//!
//! Behavior reference: sys/vm/vm_fault.c, `vm_map_lookup` in
//! sys/vm/vm_map.c.

use alloc::sync::Arc;

use crate::map::{Prot, VmMap};
use crate::object::{Found, LookupError, Object};
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
}

/// Resolves a fault at `va` for `access`.
pub fn resolve(map: &mut VmMap, va: VirtAddr, access: Access) -> Result<Resolved, FaultError> {
    let pages = map.pages;
    let page = va.align_down(PAGE_SIZE);
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
    let object = Arc::clone(object);
    if !entry.needs_copy {
        // Our object is ours alone; fold away what only it still reaches.
        object.collapse();
    }
    let index = entry.index_of(page);
    let read_only = entry.prot.without(Prot::WRITE);
    let found = object.lookup(index).map_err(|e| match e {
        LookupError::OutOfMemory => FaultError::OutOfMemory,
        LookupError::PastEnd => FaultError::PastEnd,
    })?;
    let (pfn, prot) = match found {
        Found::Top(pfn) if entry.needs_copy => (pfn, read_only),
        Found::Top(pfn) => (pfn, entry.prot),
        Found::Backing(pfn) if write => (
            object
                .get_or_insert(index, Some(pfn))
                .ok_or(FaultError::OutOfMemory)?,
            entry.prot,
        ),
        Found::Backing(pfn) => (pfn, read_only),
        Found::Absent => {
            let pfn = object
                .get_or_insert(index, None)
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
    Ok(Resolved { page, pfn, prot })
}
