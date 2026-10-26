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

    fn alloc_filled(&self, fill: &mut dyn FnMut(&mut [u8])) -> Option<Pfn> {
        let mut page = std::vec![0u8; 4096];
        fill(&mut page);
        let v = u64::from_le_bytes(page[..8].try_into().unwrap());
        Some(self.alloc(v))
    }

    fn free(&self, pfn: Pfn) {
        assert!(
            self.state.lock().unwrap().1.remove(&pfn.0).is_some(),
            "double free of {pfn:?}"
        );
    }

    fn access(&self, pfn: Pfn, f: &mut dyn FnMut(&mut [u8])) {
        let mut page = std::vec![0u8; 4096];
        page[..8].copy_from_slice(&self.get(pfn).to_le_bytes());
        f(&mut page);
        self.set(pfn, u64::from_le_bytes(page[..8].try_into().unwrap()));
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
        let r = loop {
            match resolve(&mut self.map, va(page) + 123, access)? {
                crate::fault::Outcome::Map(r) => break r,
                // The map lock would be let go here.
                crate::fault::Outcome::Fill(o, i) => {
                    o.fill(i).map_err(|_| FaultError::PastEnd)?;
                }
            }
        };
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
fn forks_whose_parent_writes_first_do_not_grow_the_chain() {
    // The parent writes while the child still lives (the shared object
    // then has two referrers and cannot fold), and the child goes after:
    // the next fork must fold what only the parent reaches by then.
    let pages = Pages::new();
    let mut parent = Proc::new(pages);
    parent.mmap(0, 4);
    parent.write(0, 1).unwrap();
    for i in 0..50 {
        let child = parent.fork();
        parent.write(0, i).unwrap();
        drop(child);
        assert!(
            parent.depth(0) <= 2,
            "depth {} after {i} forks",
            parent.depth(0)
        );
    }
    assert_eq!(parent.read(0), Ok(49));
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
fn only_touched_memory_is_populated() {
    let pages = Pages::new();
    let mut p = Proc::new(pages);
    p.mmap(0, 4);
    p.mmap(10, 4);
    assert!(!p.map.populated(&range(0, 14)));
    p.read(12).unwrap();
    assert!(p.map.populated(&range(0, 14)));
    assert!(p.map.populated(&range(13, 1)));
    assert!(!p.map.populated(&range(0, 10)));
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

/// A file of `pages` pages whose page `i` holds `1000 + i`, counting fills.
struct File {
    pages: u64,
    fills: std::sync::atomic::AtomicU64,
}

impl crate::object::Pager for File {
    fn size(&self) -> u64 {
        self.pages
    }

    fn fill(&self, index: u64, page: &mut [u8]) -> Result<(), crate::object::PagerError> {
        self.fills
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        page.fill(0);
        page[..8].copy_from_slice(&(1000 + index).to_le_bytes());
        Ok(())
    }
}

/// A file that takes writes back, recording them.
struct WrittenFile {
    pages: u64,
    written: std::sync::Mutex<Vec<(u64, u64)>>,
}

impl crate::object::Pager for WrittenFile {
    fn size(&self) -> u64 {
        self.pages
    }

    fn fill(&self, index: u64, page: &mut [u8]) -> Result<(), crate::object::PagerError> {
        page.fill(0);
        page[..8].copy_from_slice(&(1000 + index).to_le_bytes());
        Ok(())
    }

    fn writes_back(&self) -> bool {
        true
    }

    fn write_back(&self, index: u64, page: &[u8]) -> Result<(), crate::object::PagerError> {
        let v = u64::from_le_bytes(page[..8].try_into().unwrap());
        self.written.lock().unwrap().push((index, v));
        Ok(())
    }
}

#[test]
fn shared_file_mappings_share_writes_and_track_dirt() {
    let pages = Pages::new();
    let file = Arc::new(WrittenFile {
        pages: 4,
        written: std::sync::Mutex::new(Vec::new()),
    });
    let object = crate::object::Object::with_pager(
        pages,
        Arc::clone(&file) as Arc<dyn crate::object::Pager>,
    );
    let mut a = Proc::new(pages);
    let mut b = Proc::new(pages);
    a.map
        .insert(Entry::shared(
            range(0, 4),
            Prot::RW,
            Prot::ALL,
            Arc::clone(&object),
            0,
        ))
        .unwrap();
    b.map
        .insert(Entry::shared(
            range(8, 4),
            Prot::RW,
            Prot::ALL,
            Arc::clone(&object),
            0,
        ))
        .unwrap();
    // A read maps a clean page without write; the write then faults again.
    assert_eq!(a.read(1), Ok(1001));
    assert!(!a.ptes[&1].1.contains(Prot::WRITE));
    assert!(!object.is_dirty(1));
    a.write(1, 42).unwrap();
    assert!(object.is_dirty(1));
    // One page for both mappings.
    assert_eq!(b.read(9), Ok(42));
    object.sync(0, 4).unwrap();
    assert_eq!(*file.written.lock().unwrap(), [(1, 42)]);
    // Mapped writable: it stays dirty, so later writes are not lost.
    assert!(object.is_dirty(1));
    // A fork shares the mapping.
    let mut c = a.fork();
    c.write(2, 7).unwrap();
    assert_eq!(a.read(2), Ok(7));
}

impl Proc {
    /// Maps `file` pages `[offset, offset + n)` at page `first`, private.
    fn map_file(
        &mut self,
        first: u64,
        n: u64,
        file: &Arc<crate::object::Object>,
        offset: u64,
        prot: Prot,
    ) {
        self.map
            .insert(Entry::private(
                range(first, n),
                prot,
                Prot::ALL,
                Arc::clone(file),
                offset,
            ))
            .unwrap();
    }
}

#[test]
fn file_pages_are_filled_once_and_shared() {
    let pages = Pages::new();
    let file = Arc::new(File {
        pages: 4,
        fills: std::sync::atomic::AtomicU64::new(0),
    });
    let object = crate::object::Object::with_pager(
        pages,
        Arc::clone(&file) as Arc<dyn crate::object::Pager>,
    );
    let mut a = Proc::new(pages);
    let mut b = Proc::new(pages);
    // Text: read-only, at file page 1.
    a.map_file(0, 2, &object, 1, Prot::READ.union(Prot::EXEC));
    b.map_file(8, 2, &object, 1, Prot::READ.union(Prot::EXEC));
    assert_eq!(a.read(0), Ok(1001));
    assert_eq!(b.read(8), Ok(1001));
    assert_eq!(a.ptes[&0].0, b.ptes[&8].0, "one cached page");
    assert_eq!(file.fills.load(std::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(a.write(0, 5), Err(FaultError::Protection));

    // Data: private and writable; a write copies, the file page stays.
    a.map_file(4, 3, &object, 2, Prot::RW);
    assert_eq!(a.read(4), Ok(1002));
    a.write(4, 7).unwrap();
    assert_eq!(a.read(4), Ok(7));
    assert_eq!(b.read(9), Ok(1002));
    // Page 6 maps file page 4: past the end.
    assert_eq!(a.read(6), Err(FaultError::PastEnd));

    // Unmapping never frees cached file pages; the object's end does.
    a.munmap(0, 8);
    b.munmap(8, 2);
    assert_eq!(pages.live(), 2);
    drop(object);
    drop((a, b));
    assert_eq!(pages.live(), 0);
}

#[test]
fn fork_of_a_file_mapping_copies_on_write() {
    let pages = Pages::new();
    let file = Arc::new(File {
        pages: 2,
        fills: std::sync::atomic::AtomicU64::new(0),
    });
    let object = crate::object::Object::with_pager(pages, file as Arc<dyn crate::object::Pager>);
    let mut p = Proc::new(pages);
    p.map_file(0, 2, &object, 0, Prot::RW);
    p.write(0, 1).unwrap();
    for i in 1..6 {
        let mut c = p.fork();
        assert_eq!(c.read(0), Ok(i));
        assert_eq!(c.read(1), Ok(1001));
        c.write(1, 9).unwrap();
        drop(c);
        p.write(0, i + 1).unwrap();
        // The file object never folds into the private copy.
        assert!(p.depth(0) <= 2, "depth {}", p.depth(0));
    }
    assert_eq!(p.read(1), Ok(1001));
}

/// A file read in clusters of up to 8 pages, recording each run's length.
struct ClusterFile {
    pages: u64,
    runs: std::sync::Mutex<Vec<(u64, usize)>>,
}

impl crate::object::Pager for ClusterFile {
    fn size(&self) -> u64 {
        self.pages
    }

    fn max_run(&self) -> u64 {
        8
    }

    fn fill(&self, index: u64, page: &mut [u8]) -> Result<(), crate::object::PagerError> {
        self.fill_run(index, page)
    }

    fn fill_run(&self, index: u64, buf: &mut [u8]) -> Result<(), crate::object::PagerError> {
        self.runs.lock().unwrap().push((index, buf.len() / 4096));
        for (i, page) in (index..).zip(buf.as_chunks_mut::<4096>().0) {
            page.fill(0);
            page[..8].copy_from_slice(&(1000 + i).to_le_bytes());
        }
        Ok(())
    }

    fn writes_back(&self) -> bool {
        true
    }
}

#[test]
fn sequential_fills_read_ahead_and_random_ones_do_not() {
    let pages = Pages::new();
    let file = Arc::new(ClusterFile {
        pages: 20,
        runs: std::sync::Mutex::new(Vec::new()),
    });
    let object = crate::object::Object::with_pager(
        pages,
        Arc::clone(&file) as Arc<dyn crate::object::Pager>,
    );
    let mut a = Proc::new(pages);
    a.map_file(0, 20, &object, 0, Prot::READ);
    for i in 0..20 {
        assert_eq!(a.read(i), Ok(1000 + i));
    }
    // A read from the start counts as in order; the window doubles to the
    // pager's run, and stops at the end.
    assert_eq!(
        *file.runs.lock().unwrap(),
        [(0, 2), (2, 4), (6, 8), (14, 6)]
    );
    // Out of order: a page at a time.
    let other = crate::object::Object::with_pager(
        pages,
        Arc::clone(&file) as Arc<dyn crate::object::Pager>,
    );
    file.runs.lock().unwrap().clear();
    let mut b = Proc::new(pages);
    b.map_file(0, 20, &other, 0, Prot::READ);
    for i in [10, 3, 17] {
        assert_eq!(b.read(i), Ok(1000 + i));
    }
    assert_eq!(*file.runs.lock().unwrap(), [(10, 1), (3, 1), (17, 1)]);
}

#[test]
fn clean_pages_nothing_maps_are_reclaimed() {
    let pages = Pages::new();
    let file = Arc::new(ClusterFile {
        pages: 4,
        runs: std::sync::Mutex::new(Vec::new()),
    });
    let object = crate::object::Object::with_pager(
        pages,
        Arc::clone(&file) as Arc<dyn crate::object::Pager>,
    );
    for i in 0..4 {
        object.fill(i).unwrap();
    }
    object.mark_dirty(2);
    // Mapped: kept.
    let mut a = Proc::new(pages);
    a.map_file(0, 4, &object, 0, Prot::READ);
    assert_eq!(Arc::clone(&object).drop_clean(usize::MAX), 0);
    drop(a);
    // A transfer under way: kept.
    let io = object.io();
    assert_eq!(Arc::clone(&object).drop_clean(usize::MAX), 0);
    drop(io);
    // Otherwise the clean ones go; the dirty one stays.
    assert_eq!(Arc::clone(&object).drop_clean(usize::MAX), 3);
    assert_eq!(object.resident(), 1);
    assert!(object.is_dirty(2));
    // And come back from the pager.
    file.runs.lock().unwrap().clear();
    object.fill(0).unwrap();
    assert_eq!(file.runs.lock().unwrap().len(), 1);
    // Mapped, after the file system let go: the mapping's reference alone
    // keeps the pages.
    let mut b = Proc::new(pages);
    b.map_file(0, 4, &object, 0, Prot::READ);
    object.disown();
    // `held` stands for the reclaimer's own reference.
    let held = Arc::clone(&object);
    drop(object);
    assert_eq!(held.drop_clean(usize::MAX), 0);
    drop(b);
    assert_eq!(held.drop_clean(usize::MAX), 1);
}

#[test]
fn a_device_mapping_reaches_the_device_pages_write_combining() {
    use crate::object::MemAttr;
    let pages = Pages::new();
    let mut p = Proc::new(pages);
    // Pages 1000 to 1003, mapped from the second.
    let dev = Object::device(pages, Pfn(1000), 4, MemAttr::WriteCombining);
    assert!(dev.is_device());
    p.map
        .insert(Entry::shared(
            range(0, 4),
            Prot::RW,
            Prot::ALL,
            Arc::clone(&dev),
            1,
        ))
        .unwrap();
    assert_eq!(p.access(0, Access::Write), Ok(Pfn(1001)));
    assert_eq!(p.access(2, Access::Read), Ok(Pfn(1003)));
    // Past the device's pages.
    assert_eq!(p.access(3, Access::Read), Err(FaultError::PastEnd));
    match resolve(&mut p.map, va(1), Access::Write) {
        Ok(crate::fault::Outcome::Map(r)) => {
            assert_eq!(
                (r.pfn, r.prot, r.attr),
                (Pfn(1002), Prot::RW, MemAttr::WriteCombining)
            );
        }
        _ => panic!("no mapping"),
    }
    // A child shares it, even inheriting a copy: there is none.
    let mut child = p.fork();
    assert_eq!(child.access(0, Access::Write), Ok(Pfn(1001)));
    p.map.set_inherit(range(0, 4), Inherit::Copy).unwrap();
    let mut child = p.fork();
    assert_eq!(child.access(0, Access::Write), Ok(Pfn(1001)));
    // Nothing was allocated, and unmapping frees nothing.
    p.munmap(0, 4);
    drop(child);
    drop(p);
    assert_eq!(pages.live(), 0);
}

/// Pages in [`SGROWSIZ`].
const GROW: u64 = SGROWSIZ / P;

/// A 1024-page stack at pages 0-1023 with a one-page guard.
fn stack(p: &mut Proc) {
    p.map
        .insert_stack(range(0, 1024), P, Prot::RW, Prot::ALL)
        .unwrap();
}

#[test]
fn a_stack_starts_with_its_top_mapped() {
    let pages = Pages::new();
    let mut p = Proc::new(pages);
    stack(&mut p);
    let top = p.map.entry(va(1023)).unwrap();
    assert_eq!(top.role(), Role::Stack);
    assert_eq!(top.range(), range(1024 - GROW, GROW));
    let gap = p.map.entry(va(0)).unwrap();
    assert!(matches!(gap.role(), Role::StackGap { guard: P, .. }));
    assert_eq!(gap.range(), range(0, 1024 - GROW));
}

#[test]
fn an_access_below_grows_the_stack_to_it() {
    let pages = Pages::new();
    let mut p = Proc::new(pages);
    stack(&mut p);
    // A few pages below the mapped top: one growth of SGROWSIZ.
    p.write(1024 - GROW - 3, 7).unwrap();
    assert_eq!(p.read(1024 - GROW - 3), Ok(7));
    assert_eq!(
        p.map.entry(va(0)).unwrap().range(),
        range(0, 1024 - 2 * GROW)
    );
    // Far below: straight there, in whole SGROWSIZ steps.
    p.write(100, 1).unwrap();
    let gap_end = p.map.entry(va(0)).unwrap().end();
    assert!(gap_end <= va(100) && (va(1024).get() - gap_end.get()).is_multiple_of(SGROWSIZ));
}

#[test]
fn the_guard_is_never_given() {
    let pages = Pages::new();
    let mut p = Proc::new(pages);
    stack(&mut p);
    p.write(1, 1).unwrap();
    // Grown down to the guard, which stays.
    assert_eq!(p.map.entry(va(0)).unwrap().range(), range(0, 1));
    assert_eq!(p.read(0), Err(FaultError::Protection));
}

#[test]
fn the_main_stack_stops_at_its_limit() {
    let pages = Pages::new();
    let mut p = Proc::new(pages);
    stack(&mut p);
    p.map.set_proc_stack(ProcStack {
        area: range(0, 1024),
        size: SGROWSIZ,
    });
    // Two growths' worth.
    p.map.set_stack_limit(3 * SGROWSIZ);
    p.write(1024 - 3 * GROW, 1).unwrap();
    assert_eq!(p.map.proc_stack().unwrap().size, 3 * SGROWSIZ);
    assert_eq!(p.write(1024 - 3 * GROW - 1, 1), Err(FaultError::Protection));
    // Raising the limit lets it grow on.
    p.map.set_stack_limit(4 * SGROWSIZ);
    p.write(1024 - 3 * GROW - 1, 1).unwrap();
}

#[test]
fn a_thread_stack_is_not_bound_by_the_limit() {
    let pages = Pages::new();
    let mut p = Proc::new(pages);
    stack(&mut p);
    p.map.set_stack_limit(SGROWSIZ);
    p.write(10, 1).unwrap();
}

#[test]
fn mprotect_passes_guards_and_tells_the_gap() {
    let pages = Pages::new();
    let mut p = Proc::new(pages);
    p.map.insert(Entry::guard(range(2000, 4))).unwrap();
    p.mmap(2004, 4);
    // Over a guard and memory: the guard is left alone.
    p.mprotect(2000, 8, Prot::READ).unwrap();
    assert_eq!(p.map.entry(va(2000)).unwrap().prot(), Prot::NONE);
    assert_eq!(p.map.entry(va(2004)).unwrap().prot(), Prot::READ);
    // Over a whole stack: what grows later is executable too.
    stack(&mut p);
    p.mprotect(0, 1024, Prot::ALL).unwrap();
    p.access(500, Access::Execute).unwrap();
    assert_eq!(p.map.entry(va(500)).unwrap().prot(), Prot::ALL);
}

#[test]
fn a_forked_stack_grows_on_its_own() {
    let pages = Pages::new();
    let mut p = Proc::new(pages);
    stack(&mut p);
    p.write(1023, 5).unwrap();
    let mut c = p.fork();
    c.write(900, 6).unwrap();
    assert_eq!(c.read(1023), Ok(5));
    // The parent's gap is untouched.
    assert_eq!(p.map.entry(va(900)).unwrap().prot(), Prot::NONE);
}
