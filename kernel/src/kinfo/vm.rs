// SPDX-License-Identifier: BSD-2-Clause
//! A process's map as `struct kinfo_vmentry`s: `kern.proc.vmmap`
//! (`procstat -v`, libutil's `kinfo_getvmmap`), and one entry by index
//! for `ptrace(PT_VM_ENTRY)` and core dumps.
//!
//! Each entry gives its range, rights and maximum rights, flags (copy on
//! write, still to copy, grows down, user-wired), resident pages, the
//! type of the object at the bottom of its chain (a file's vnode, swap
//! for anonymous memory, the shared page's physical memory, a device,
//! a guard), and for a file its attributes and path. Which file or POSIX
//! shared memory object a memory object belongs to is noted when the
//! object is first handed out for mapping ([`note_file`],
//! [`note_shm`]), as FreeBSD's object keeps its `handle`.
//!
//! Behavior reference: sys/kern/kern_proc.c (`kern_proc_vmmap_out`,
//! `vm_object_kvme_type`).

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use abi::kinfo::*;
use sched::Sleepable;
use sync::PreemptLock;
use sysctl::{Number, Tree};
use vm::map::{Entry, Prot, Role};
use vm::object::Object;

use super::{Access, arg, process, sleepable, text};
use crate::proc::Process;
use crate::uaccess::{Plain, bytes_of};

// SAFETY: integers and a byte array, no padding (size asserted in abi;
// `abi-check` the layout).
unsafe impl Plain for KinfoVmentry {}

/// What a memory object holds the pages of.
enum Owner {
    /// A file: its file system and inode (looked up again when asked).
    File { mount: Weak<vfs::Mount>, ino: u64 },
    /// A POSIX shared memory object.
    Shm(Weak<crate::fs::shm::Shm>),
}

/// Memory objects' owners, by object.
static OWNERS: PreemptLock<BTreeMap<usize, (Weak<Object>, Owner)>> =
    PreemptLock::new(BTreeMap::new());
/// The table's size when it was last pruned.
static PRUNED: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(64);

fn note(o: &Arc<Object>, owner: Owner) {
    let key = Arc::as_ptr(o).addr();
    let mut t = OWNERS.lock();
    if t.get(&key).is_some_and(|(w, _)| w.strong_count() > 0) {
        return;
    }
    t.insert(key, (Arc::downgrade(o), owner));
    // Objects gone since leave their entries: drop them now and then.
    let at = PRUNED.load(core::sync::atomic::Ordering::Relaxed);
    if t.len() > 2 * at {
        t.retain(|_, (w, _)| w.strong_count() > 0);
        PRUNED.store(t.len().max(64), core::sync::atomic::Ordering::Relaxed);
    }
}

/// `o` holds file `v`'s pages (its `v_object`).
pub(crate) fn note_file(o: &Arc<Object>, v: &vfs::Vnode) {
    note(
        o,
        Owner::File {
            mount: Arc::downgrade(v.mount()),
            ino: v.id().ino,
        },
    );
}

/// `o` is POSIX shared memory object `shm`'s (`OBJ_POSIXSHM`).
pub(crate) fn note_shm(o: &Arc<Object>, shm: &Arc<crate::fs::shm::Shm>) {
    note(o, Owner::Shm(Arc::downgrade(shm)));
}

/// What `o` is, if it was noted.
enum Found {
    File(Arc<vfs::Vnode>),
    Shm(Arc<crate::fs::shm::Shm>),
}

fn owner_of(s: &Sleepable, o: &Arc<Object>) -> Option<Found> {
    let (mount, ino, shm) = {
        let t = OWNERS.lock();
        let (w, owner) = t.get(&Arc::as_ptr(o).addr())?;
        if !w.upgrade().is_some_and(|x| Arc::ptr_eq(&x, o)) {
            return None;
        }
        match owner {
            Owner::File { mount, ino } => (mount.upgrade(), *ino, None),
            Owner::Shm(w) => (None, 0, w.upgrade()),
        }
    };
    if let Some(shm) = shm {
        return Some(Found::Shm(shm));
    }
    mount?.vget(s, ino).ok().map(Found::File)
}

/// The bottom of `e`'s object chain, and the offset into it in bytes.
fn bottom(e: &Entry) -> Option<(Arc<Object>, u64)> {
    let mut o = Arc::clone(e.object()?);
    let mut offset = e.offset();
    while let Some((b, off)) = o.backing() {
        offset += off;
        o = b;
    }
    Some((o, offset * ::vm::PAGE_SIZE))
}

/// The program's base: where the first page of its file is mapped.
pub(crate) fn text_base(s: &Sleepable, p: &Process) -> u64 {
    let Some(v) = p.text_vnode() else {
        return 0;
    };
    let space = p.space();
    space
        .entries(s)
        .iter()
        .find(|e| {
            bottom(e).is_some_and(|(o, off)| {
                off == 0 && matches!(owner_of(s, &o), Some(Found::File(f)) if Arc::ptr_eq(&f, &v))
            })
        })
        .map_or(0, |e| e.start().get())
}

/// Entry `e` of `p`'s map as `kinfo_vmentry`; `entries` is the whole
/// snapshot it came from (whose copies of each object do not count).
fn entry_kinfo(s: &Sleepable, p: &Process, e: &Entry, entries: &[Entry]) -> Box<KinfoVmentry> {
    let space = p.space();
    let mut k: Box<KinfoVmentry> = super::zeroed();
    k.kve_structsize = i32::try_from(KINFO_VMENTRY_SIZE).unwrap_or(0);
    k.kve_start = e.start().get();
    k.kve_end = e.end().get();
    let rights = |p: Prot, r, w, x| {
        let mut v = 0;
        if p.contains(Prot::READ) {
            v |= r;
        }
        if p.contains(Prot::WRITE) {
            v |= w;
        }
        if p.contains(Prot::EXEC) {
            v |= x;
        }
        v
    };
    k.kve_protection = rights(e.prot(), KVME_PROT_READ, KVME_PROT_WRITE, KVME_PROT_EXEC)
        | rights(
            e.max_prot(),
            KVME_MAX_PROT_READ,
            KVME_MAX_PROT_WRITE,
            KVME_MAX_PROT_EXEC,
        );
    let shared_page = crate::shared_page::object_ptr();
    let is_shared_page = e
        .object()
        .is_some_and(|o| Some(Arc::as_ptr(o).addr()) == shared_page);
    if e.is_cow() && !is_shared_page {
        k.kve_flags |= KVME_FLAG_COW;
    }
    if e.needs_copy() && !is_shared_page {
        k.kve_flags |= KVME_FLAG_NEEDS_COPY;
    }
    if e.role() == Role::Stack {
        k.kve_flags |= KVME_FLAG_GROWS_DOWN;
    }
    let pages = |a: u64, b: u64| (a / ::vm::PAGE_SIZE, b.div_ceil(::vm::PAGE_SIZE));
    let (first, last) = pages(e.start().get(), e.end().get());
    let wired = space.wired();
    if wired.covers(first, last) {
        k.kve_flags |= KVME_FLAG_USER_WIRED;
    }
    drop(wired);
    let Some((low, offset)) = bottom(e) else {
        // No object: a guard, or memory nothing touched yet.
        k.kve_type = if matches!(e.role(), Role::Guard | Role::StackGap { .. }) {
            KVME_TYPE_GUARD
        } else {
            KVME_TYPE_NONE
        };
        return k;
    };
    let top = Arc::clone(e.object().unwrap_or(&low));
    k.kve_offset = offset;
    k.kve_resident = i32::try_from(space.resident(e.range())).unwrap_or(i32::MAX);
    if top.backing().is_none() {
        k.kve_private_resident = i32::try_from(top.resident()).unwrap_or(i32::MAX);
    }
    let copies = entries
        .iter()
        .filter(|x| x.object().is_some_and(|o| Arc::ptr_eq(o, &top)))
        .count();
    // The table's reference, this function's, and the snapshot's go.
    k.kve_ref_count =
        i32::try_from(Arc::strong_count(&top).saturating_sub(copies + 1)).unwrap_or(i32::MAX);
    if is_shared_page {
        k.kve_type = KVME_TYPE_PHYS;
        return k;
    }
    if low.is_device() {
        k.kve_type = KVME_TYPE_DEVICE;
        return k;
    }
    if top.backing().is_none() && Arc::ptr_eq(&top, &low) {
        k.kve_type_spec = Arc::as_ptr(&top).addr() as u64;
    }
    match owner_of(s, &low) {
        Some(Found::File(v)) => {
            k.kve_type = KVME_TYPE_VNODE;
            let kf = super::file::vnode_kinfo(s, p, &v);
            k.kve_vn_type = kf.kf_un.kf_file_type;
            if kf.kf_status & KF_ATTR_VALID != 0 {
                k.kve_vn_fileid = kf.kf_un.kf_file_fileid;
                k.kve_type_spec = kf.kf_un.kf_file_fsid;
                k.kve_vn_fsid_freebsd11 = kf.kf_un.kf_file_fsid_freebsd11;
                k.kve_vn_mode = kf.kf_un.kf_file_mode;
                k.kve_vn_size = kf.kf_un.kf_file_size;
                k.kve_vn_rdev = kf.kf_un.kf_file_rdev;
                k.kve_vn_rdev_freebsd11 = kf.kf_un.kf_file_rdev_freebsd11;
                k.kve_status = KF_ATTR_VALID;
            }
            k.kve_path = kf.kf_path;
        }
        Some(Found::Shm(shm)) => {
            k.kve_type = KVME_TYPE_SWAP;
            k.kve_flags |= KVME_FLAG_POSIXSHM;
            if let (_, _, Some(name)) = shm.kinfo(s) {
                text(&mut k.kve_path, &name);
            }
        }
        // Anonymous memory is swap-backed (`OBJT_SWAP`).
        None => k.kve_type = KVME_TYPE_SWAP,
    }
    k
}

/// The bytes of a record, packed to its path's end (`KERN_VMMAP_PACK_KINFO`).
#[allow(
    clippy::boxed_local,
    reason = "a kilobyte and more: kept off the stack"
)]
fn packed(mut k: Box<KinfoVmentry>) -> Vec<u8> {
    let at = core::mem::offset_of!(KinfoVmentry, kve_path);
    let n = k.kve_path.iter().position(|&b| b == 0).unwrap_or(PATH_MAX);
    let len = (at + n + 1).next_multiple_of(8);
    k.kve_structsize = i32::try_from(len).unwrap_or(0);
    bytes_of(&*k)[..len].to_vec()
}

/// `p`'s map, packed, as `kern.proc.vmmap` returns it. For core dumps'
/// `NT_PROCSTAT_VMMAP` too.
pub(crate) fn vmmap(s: &Sleepable, p: &Process) -> Vec<u8> {
    let entries = p.space().entries(s);
    let mut out = Vec::new();
    for e in &entries {
        out.extend(packed(entry_kinfo(s, p, e, &entries)));
    }
    out
}

/// Entry `index` of `p`'s map, whole (for `PT_VM_ENTRY`); `None` past
/// the last.
pub(crate) fn entry(s: &Sleepable, p: &Process, index: usize) -> Option<Box<KinfoVmentry>> {
    let entries = p.space().entries(s);
    let e = entries.get(index)?;
    Some(entry_kinfo(s, p, e, &entries))
}

/// Adds `kern.proc.vmmap`.
pub(crate) fn add_nodes(t: &mut Tree, procs: sysctl::NodeId) {
    t.add_handler(
        procs,
        Number::Fixed(node::VMMAP),
        "vmmap",
        "Process vm map entries",
        Arc::new(|r: &[i32], out: &mut Vec<u8>| {
            let p = process(arg(r)?, Access::Debug)?;
            if p.is_zombie() {
                return Err(sysctl::ReadError::Failed(abi::errno::Errno::ESRCH));
            }
            out.extend(vmmap(&sleepable(), &p));
            Ok(())
        }),
    )
    .expect("kern.proc.vmmap");
}
