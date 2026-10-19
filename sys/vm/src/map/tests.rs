// SPDX-License-Identifier: BSD-2-Clause
//! Maps, objects, and faults together, against a simulated MMU: each test
//! process keeps its own page table and resolves its faults through
//! [`crate::fault::resolve`], as the kernel's trap handler will.

extern crate std;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Mutex;

use super::*;
use crate::Pfn;
use crate::fault::{Access, FaultError, resolve};

/// Fake physical memory: each page holds one number.
#[derive(Default)]
struct Pages {
    state: Mutex<(u64, HashMap<u64, u64>)>,
}

impl Pages {
    fn new() -> &'static Self {
        Box::leak(Box::default())
    }

    fn live(&self) -> usize {
        self.state.lock().unwrap().1.len()
    }

    fn get(&self, pfn: Pfn) -> u64 {
        *self
            .state
            .lock()
            .unwrap()
            .1
            .get(&pfn.0)
            .expect("read of a freed page")
    }

    fn set(&self, pfn: Pfn, value: u64) {
        *self
            .state
            .lock()
            .unwrap()
            .1
            .get_mut(&pfn.0)
            .expect("write to a freed page") = value;
    }

    fn alloc(&self, value: u64) -> Pfn {
        let mut s = self.state.lock().unwrap();
        s.0 += 1;
        let pfn = s.0;
        s.1.insert(pfn, value);
        Pfn(pfn)
    }
}

impl PageOps for Pages {
    fn alloc_zeroed(&self) -> Option<Pfn> {
        Some(self.alloc(0))
    }

    fn alloc_copy(&self, from: Pfn) -> Option<Pfn> {
        let v = self.get(from);
        Some(self.alloc(v))
    }

    fn free(&self, pfn: Pfn) {
        assert!(
            self.state.lock().unwrap().1.remove(&pfn.0).is_some(),
            "double free of {pfn:?}"
        );
    }
}

const BASE: u64 = 0x1000_0000;
const P: u64 = PAGE_SIZE;

fn va(page: u64) -> VirtAddr {
    VirtAddr::new(BASE + page * P)
}

fn range(first: u64, pages: u64) -> Range<VirtAddr> {
    va(first)..va(first + pages)
}

/// A process: its map and its page table.
struct Proc {
    pages: &'static Pages,
    map: VmMap,
    ptes: BTreeMap<u64, (Pfn, Prot)>,
    faults: usize,
}

impl Proc {
    fn new(pages: &'static Pages) -> Self {
        Self {
            pages,
            map: VmMap::new(VirtAddr::new(BASE)..VirtAddr::new(BASE + (1 << 30)), pages),
            ptes: BTreeMap::new(),
            faults: 0,
        }
    }

    fn mmap(&mut self, first: u64, n: u64) {
        self.map
            .insert(Entry::anonymous(range(first, n), Prot::RW, Prot::ALL))
            .unwrap();
    }

    /// The page an access reaches, faulting as the MMU would.
    fn access(&mut self, page: u64, access: Access) -> Result<Pfn, FaultError> {
        let need = match access {
            Access::Read => Prot::READ,
            Access::Write => Prot::WRITE,
            Access::Execute => Prot::EXEC,
        };
        if let Some(&(pfn, prot)) = self.ptes.get(&page)
            && prot.contains(need)
        {
            return Ok(pfn);
        }
        self.faults += 1;
        let r = resolve(&mut self.map, va(page) + 123, access)?;
        assert_eq!(r.page, va(page));
        self.ptes.insert(page, (r.pfn, r.prot));
        Ok(r.pfn)
    }

    fn read(&mut self, page: u64) -> Result<u64, FaultError> {
        let pfn = self.access(page, Access::Read)?;
        Ok(self.pages.get(pfn))
    }

    fn write(&mut self, page: u64, value: u64) -> Result<(), FaultError> {
        let pfn = self.access(page, Access::Write)?;
        self.pages.set(pfn, value);
        Ok(())
    }

    fn fork(&mut self) -> Self {
        let forked = self.map.fork();
        for r in forked.write_protect {
            for (_, (_, prot)) in self.ptes.range_mut(page_of(r.start)..page_of(r.end)) {
                *prot = prot.without(Prot::WRITE);
            }
        }
        Self {
            pages: self.pages,
            map: forked.child,
            ptes: BTreeMap::new(),
            faults: 0,
        }
    }

    fn munmap(&mut self, first: u64, n: u64) {
        self.ptes.retain(|&p, _| p < first || p >= first + n);
        self.map.remove(range(first, n)).unwrap();
    }

    fn mprotect(&mut self, first: u64, n: u64, prot: Prot) -> Result<(), MapError> {
        self.map.protect(range(first, n), prot)?;
        for (_, (_, p)) in self.ptes.range_mut(first..first + n) {
            *p = Prot(p.0 & prot.0);
        }
        Ok(())
    }

    /// The depth of the object chain under `page`.
    fn depth(&self, page: u64) -> usize {
        self.map.entry(va(page)).unwrap().object().unwrap().depth()
    }
}

fn page_of(v: VirtAddr) -> u64 {
    (v.get() - BASE) / P
}

#[test]
fn anonymous_memory_reads_zero_and_keeps_writes() {
    let pages = Pages::new();
    let mut p = Proc::new(pages);
    p.mmap(0, 4);
    assert_eq!(p.read(0), Ok(0));
    p.write(1, 7).unwrap();
    assert_eq!(p.read(1), Ok(7));
    // A read fault on private, unshared memory maps it writable at once.
    p.write(0, 1).unwrap();
    assert_eq!(p.faults, 2);
    assert_eq!(pages.live(), 2);
    drop(p);
    assert_eq!(pages.live(), 0);
}

#[test]
fn unmapped_and_protected_accesses_fail() {
    let pages = Pages::new();
    let mut p = Proc::new(pages);
    p.mmap(0, 2);
    assert_eq!(p.read(5), Err(FaultError::NotMapped));
    assert_eq!(p.access(0, Access::Execute), Err(FaultError::Protection));
    p.mprotect(0, 1, Prot::READ).unwrap();
    assert_eq!(p.write(0, 1), Err(FaultError::Protection));
    assert_eq!(p.read(0), Ok(0));
    p.write(1, 1).unwrap();
}

#[test]
fn fork_copies_on_write() {
    let pages = Pages::new();
    let mut parent = Proc::new(pages);
    parent.mmap(0, 4);
    parent.write(0, 1).unwrap();
    let mut child = parent.fork();
    assert_eq!(child.read(0), Ok(1));
    child.write(0, 2).unwrap();
    assert_eq!(parent.read(0), Ok(1));
    parent.write(0, 3).unwrap();
    assert_eq!(child.read(0), Ok(2));
    assert_eq!(parent.read(0), Ok(3));
    drop(child);
    drop(parent);
    assert_eq!(pages.live(), 0);
}

#[test]
fn a_zero_page_faulted_after_fork_stays_shared_until_written() {
    let pages = Pages::new();
    let mut parent = Proc::new(pages);
    parent.mmap(0, 4);
    parent.write(0, 1).unwrap();
    let mut child = parent.fork();
    // Page 1 was never touched: the child's read puts a zero page in the
    // shared object, mapped read-only.
    assert_eq!(child.read(1), Ok(0));
    assert_eq!(child.ptes[&1].1, Prot::READ);
    parent.write(1, 5).unwrap();
    assert_eq!(child.read(1), Ok(0));
    child.write(1, 6).unwrap();
    assert_eq!(parent.read(1), Ok(5));
    drop((parent, child));
    assert_eq!(pages.live(), 0);
}

#[test]
fn repeated_forks_do_not_grow_the_chain() {
    let pages = Pages::new();
    let mut parent = Proc::new(pages);
    parent.mmap(0, 4);
    parent.write(0, 1).unwrap();
    for i in 0..20 {
        let mut child = parent.fork();
        child.write(0, 100 + i).unwrap();
        drop(child);
        parent.write(0, i).unwrap();
        assert!(parent.depth(0) <= 1, "depth {}", parent.depth(0));
    }
    assert_eq!(parent.read(0), Ok(19));
    // Only the parent's page (and nothing collapsed away) is left.
    assert_eq!(pages.live(), 1);
}

#[test]
fn inheritance_share_zero_and_none() {
    let pages = Pages::new();
    let mut parent = Proc::new(pages);
    parent.mmap(0, 1);
    parent.mmap(1, 1);
    parent.mmap(2, 1);
    for page in 0..3 {
        parent.write(page, 9).unwrap();
    }
    parent.map.set_inherit(range(0, 1), Inherit::Share).unwrap();
    parent.map.set_inherit(range(1, 1), Inherit::Zero).unwrap();
    parent.map.set_inherit(range(2, 1), Inherit::None).unwrap();
    let mut child = parent.fork();
    child.write(0, 4).unwrap();
    assert_eq!(parent.read(0), Ok(4));
    assert_eq!(child.read(1), Ok(0));
    assert_eq!(child.read(2), Err(FaultError::NotMapped));
    assert_eq!(parent.read(1), Ok(9));
}

#[test]
fn sharing_a_copy_on_write_entry_shares_a_private_copy() {
    let pages = Pages::new();
    let mut a = Proc::new(pages);
    a.mmap(0, 1);
    a.write(0, 1).unwrap();
    let mut b = a.fork();
    // b shares its (still copy-on-write) page with its own child c.
    b.map.set_inherit(range(0, 1), Inherit::Share).unwrap();
    let mut c = b.fork();
    c.write(0, 2).unwrap();
    assert_eq!(b.read(0), Ok(2));
    assert_eq!(a.read(0), Ok(1));
}

#[test]
fn munmap_frees_what_only_it_mapped() {
    let pages = Pages::new();
    let mut p = Proc::new(pages);
    p.mmap(0, 8);
    for page in 0..8 {
        p.write(page, page).unwrap();
    }
    p.munmap(2, 3);
    assert_eq!(pages.live(), 5);
    assert_eq!(p.read(2), Err(FaultError::NotMapped));
    assert_eq!(p.read(5), Ok(5));
    assert_eq!(p.map.entries().count(), 2);
    // Pages of an object shared by fork stay until the object goes, even
    // once the sharer is gone (the object is no longer one mapping's).
    let child = p.fork();
    p.munmap(0, 2);
    assert_eq!(pages.live(), 5);
    drop(child);
    assert_eq!(pages.live(), 5);
    p.munmap(5, 3);
    assert_eq!(pages.live(), 0);
}

#[test]
fn protect_checks_coverage_and_maximum() {
    let pages = Pages::new();
    let mut p = Proc::new(pages);
    p.map
        .insert(Entry::anonymous(range(0, 2), Prot::READ, Prot::READ))
        .unwrap();
    p.mmap(3, 1);
    assert_eq!(p.mprotect(0, 1, Prot::RW), Err(MapError::Protection));
    assert_eq!(p.mprotect(0, 4, Prot::READ), Err(MapError::NotMapped));
    p.mprotect(1, 1, Prot::NONE).unwrap();
    assert_eq!(p.map.entries().count(), 3);
    assert_eq!(p.read(1), Err(FaultError::Protection));
    assert_eq!(p.read(0), Ok(0));
}

#[test]
fn insert_rejects_bad_ranges() {
    let pages = Pages::new();
    let mut p = Proc::new(pages);
    p.mmap(4, 4);
    let anon = |r| Entry::anonymous(r, Prot::RW, Prot::ALL);
    assert_eq!(p.map.insert(anon(range(6, 4))), Err(MapError::Overlap));
    assert_eq!(p.map.insert(anon(range(0, 5))), Err(MapError::Overlap));
    assert_eq!(
        p.map.insert(anon(va(0)..va(0) + 1)),
        Err(MapError::Misaligned)
    );
    assert_eq!(p.map.insert(anon(range(1, 0))), Err(MapError::Misaligned));
    let low = VirtAddr::new(BASE - P)..VirtAddr::new(BASE);
    assert_eq!(p.map.insert(anon(low)), Err(MapError::OutOfRange));
    p.map.insert(anon(range(0, 4))).unwrap();
    p.map.insert(anon(range(8, 1))).unwrap();
}

#[test]
fn find_space_is_first_fit_above_the_hint() {
    let pages = Pages::new();
    let mut p = Proc::new(pages);
    p.mmap(0, 2);
    p.mmap(3, 1);
    p.mmap(8, 1);
    assert_eq!(p.map.find_space(P, P, va(0)), Some(va(2)));
    assert_eq!(p.map.find_space(2 * P, P, va(0)), Some(va(4)));
    assert_eq!(p.map.find_space(4 * P, P, va(0)), Some(va(4)));
    assert_eq!(p.map.find_space(5 * P, P, va(0)), Some(va(9)));
    // Inside an entry: the gap after it.
    assert_eq!(p.map.find_space(P, P, va(3)), Some(va(4)));
    // Aligned: 16 KiB alignment from page 4 (BASE is 16 KiB aligned).
    assert_eq!(p.map.find_space(P, 4 * P, va(1)), Some(va(4)));
    assert_eq!(p.map.find_space(P, 8 * P, va(1)), Some(va(16)));
    // Wraps to the bottom when nothing fits above the hint.
    let top = p.map.bounds().end;
    p.map
        .insert(Entry::anonymous(
            VirtAddr::new(top.get() - 4 * P)..top,
            Prot::RW,
            Prot::ALL,
        ))
        .unwrap();
    assert_eq!(
        p.map.find_space(P, P, VirtAddr::new(top.get() - 2 * P)),
        Some(va(2))
    );
    assert_eq!(p.map.find_space(1 << 31, P, va(0)), None);
    assert_eq!(p.map.find_space(0, P, va(0)), None);
}

#[test]
fn clipped_entries_keep_their_object_offsets() {
    let pages = Pages::new();
    let mut p = Proc::new(pages);
    p.mmap(0, 6);
    for page in 0..6 {
        p.write(page, 10 + page).unwrap();
    }
    p.ptes.clear();
    p.mprotect(2, 2, Prot::READ).unwrap();
    for page in 0..6 {
        assert_eq!(p.read(page), Ok(10 + page));
    }
    let mut c = p.fork();
    for page in 0..6 {
        assert_eq!(c.read(page), Ok(10 + page));
    }
    c.write(5, 1).unwrap();
    assert_eq!(p.read(5), Ok(15));
}

#[test]
fn a_long_chain_drops_without_recursing() {
    let pages = Pages::new();
    let mut p = Proc::new(pages);
    p.mmap(0, 1);
    p.write(0, 1).unwrap();
    // Keep every generation alive, so nothing collapses.
    let mut keep = alloc::vec::Vec::new();
    for i in 0..10_000 {
        keep.push(p.fork());
        p.write(0, i).unwrap();
    }
    assert!(p.depth(0) >= 10_000);
    drop(keep);
    drop(p);
    assert_eq!(pages.live(), 0);
}
