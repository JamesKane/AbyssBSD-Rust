// SPDX-License-Identifier: BSD-2-Clause
//! Soft updates (stage 3 of docs/VFS.md §10.3): the dependencies between
//! metadata changes, so delayed writes may reach the disk in any order and
//! a crash at any point leaves only what `fsck_ffs -p` repairs (leaked
//! blocks and inodes, link counts too high).
//!
//! FreeBSD's rules, from McKusick and Ganger:
//! - A pointer reaches the disk only after what it points at: a block
//!   pointer after the block's group map and contents, a directory entry
//!   after the inode it names (with its new link count), a new
//!   directory's entry after its first block and its parent's link.
//! - A freed resource is reused only after every pointer to it is gone
//!   from the disk: blocks and inodes are freed in their groups only once
//!   the inode or indirect block that pointed at them is written.
//! - A link count drops only once the entry that held the link is gone
//!   from the disk.
//!
//! The engine records each change as an item with the waits it needs.
//! Pointers and entries that may not go out yet are rolled back in the
//! copy of a buffer being written ([`Deps::write_start`]), so the buffer
//! stays dirty for them; frees and link drops wait as deferred [`Work`]
//! the file system then performs ([`Deps::take_work`]).
//!
//! Waits are ordered by stamps: each change, and each wait on a buffer,
//! is stamped with the buffer held, and a write that starts later carries
//! it. An inode is written by copying it into its block (the engine's
//! `write_inode`); what waits on "the inode written" waits for the next
//! copy, then for the write of that copy.
//!
//! The tracker is plain data: whoever holds it locks it, and calls it
//! with buffers held but never the other way round (it does no I/O).
//!
//! Behavior reference: sys/ufs/ffs/ffs_softdep.c and softdep.h (the rules
//! and the rollbacks, not their structures), McKusick and Ganger, "Soft
//! Updates: A Technique for Eliminating Most Synchronous Writes in the
//! Fast Filesystem" (1999).

use alloc::collections::{BTreeMap, BTreeSet, VecDeque};
use alloc::vec::Vec;

use intmap::{IntKey, IntMap};

use crate::inode::{self, raw};
use crate::sb::{CK_INODE, Superblock};
use crate::{NDIRECT, NIADDR, idx, len64};

/// What a removal waits for to be on the disk (from
/// [`Deps::remove_entry`]), for something else to wait on too.
#[derive(Clone, Debug, Default)]
pub struct Removal(Vec<Wait>);

/// An item's number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Id(u64);

impl IntKey for Id {
    fn bits(self) -> u64 {
        self.0
    }
}

/// Items waiting for writes after stamps.
type Watchers = Vec<(Stamp, Id)>;

/// A point in the sequence of changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Stamp(u64);

/// Where a block pointer lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Slot {
    /// In inode `ino`: direct block `index` (0 to 11), or the indirect
    /// root of level `index - 12`.
    Inode { ino: u32, index: usize },
    /// Entry `index` of the indirect block whose buffer is at byte `at`.
    Indirect { at: u64, index: usize },
}

/// What an entry gone from a directory held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gone {
    /// One link (of a file, or of a directory moved elsewhere).
    Link,
    /// A directory removed: its two links (the entry and its `.`), then
    /// its parent's, for its `..`, once it is emptied on the disk.
    Dir(u32),
}

impl Gone {
    /// The links the inode itself loses.
    #[must_use]
    pub fn links(self) -> u16 {
        match self {
            Self::Link => 1,
            Self::Dir(_) => 2,
        }
    }
}

/// What the file system does once its waits are over.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Work {
    /// Free these fragments (address, bytes) in their groups.
    Free(Vec<(u64, u64)>),
    /// Free inode `ino`, which was of `mode`, in its group.
    FreeFile { ino: u32, mode: u16 },
    /// An entry naming `ino` is gone from the disk: its links drop.
    Unlink { ino: u32, gone: Gone },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Wait {
    /// Until its registration is complete.
    Arming,
    /// The buffer at byte `at` written after `stamp`.
    Buffer { at: u64, stamp: Stamp },
    /// The next copy of inode `ino` into its block; then [`Wait::Inode`].
    Copy(u32),
    /// Inode `ino` written after `stamp` (not zeroed for its allocation).
    Inode { ino: u32, stamp: Stamp },
    /// Item done.
    Item(Id),
    /// A new block's contents written as file data.
    Data(u64),
}

/// A block pointer set (FreeBSD's `allocdirect` and `allocindir`, with
/// the `newblk` and `freefrag` they carry).
#[derive(Debug)]
struct Alloc {
    bno: u64,
    /// `None` until the pointer is set.
    slot: Option<Slot>,
    old: u64,
    old_size: u64,
    owner: u32,
    /// Fragments moved away from, freed once this pointer is on the disk.
    free_old: Option<(u64, u64)>,
    /// In a copy of its container (inode slots attach at the next copy).
    attached: bool,
    /// The directory block it holds, whose entries wait for it.
    dir_block: Option<(u32, u64)>,
}

/// A directory entry made or changed (FreeBSD's `diradd`).
#[derive(Debug)]
struct DirAdd {
    /// Its block's buffer, and the entry's byte in it; `None` until made.
    place: Option<(u64, u32)>,
    dir: (u32, u64),
    ino: u32,
    /// What the entry named before (0: nothing), and its type.
    old: u32,
    old_type: u8,
    /// Its entry went out unrolled; it is done once its block's pointer
    /// is on the disk too.
    written: bool,
}

#[derive(Debug)]
enum Kind {
    /// An inode allocated: zero on the disk until its group is written.
    Born(u32),
    Alloc(Alloc),
    DirAdd(DirAdd),
    Deferred(Work),
}

#[derive(Debug)]
struct Item {
    waits: Vec<Wait>,
    kind: Kind,
}

/// An inode with dependencies.
#[derive(Debug, Default)]
struct InodeDep {
    born: Option<Id>,
    /// Items waiting for the next copy.
    copy_wait: Vec<Id>,
    /// Items waiting for a copy to be written.
    watch: Vec<(Stamp, Id)>,
    /// Links dropped but not yet on the disk (FreeBSD's `id_nlinkdelta`).
    delta: u32,
}

/// A directory block whose pointer is not on the disk yet.
#[derive(Debug)]
struct DirBlock {
    at: u64,
    alloc: Option<Id>,
}

/// File data a new block waits for.
#[derive(Debug)]
struct Cover {
    id: Id,
    bno: u64,
    len: u64,
    /// The 512-byte sectors written.
    have: u128,
}

/// A buffer write under way.
#[derive(Debug)]
struct Inflight {
    seq: u64,
    /// Pointers and entries that went out unrolled.
    out: Vec<Id>,
    /// Inodes written zeroed.
    zeroed: BTreeSet<u32>,
    /// For an indirect block, whether it went out with no pointers.
    empty: Option<bool>,
}

const SECTOR: u64 = 512;

/// The dependencies of one mounted file system.
#[derive(Debug)]
pub struct Deps {
    sb: Superblock,
    seq: u64,
    next: u64,
    items: IntMap<Id, Item>,
    buf_watch: IntMap<u64, Vec<(Stamp, Id)>>,
    item_watch: IntMap<Id, Vec<Id>>,
    covers: BTreeMap<(u64, Id), Cover>,
    /// The latest allocation of each block.
    by_bno: IntMap<u64, Id>,
    unplaced: IntMap<u64, Id>,
    slots: BTreeMap<Slot, Vec<Id>>,
    owned: IntMap<u32, BTreeSet<Id>>,
    inodes: IntMap<u32, InodeDep>,
    inode_blocks: IntMap<u64, BTreeSet<u32>>,
    dir_blocks: BTreeMap<(u32, u64), DirBlock>,
    dir_at: IntMap<u64, (u32, u64)>,
    entries: BTreeMap<(u64, u32), Vec<Id>>,
    named: IntMap<u32, BTreeSet<Id>>,
    inflight: IntMap<u64, Inflight>,
    /// Indirect blocks written since mounting (their buffers), and those
    /// whose last write held no pointers: a pointer to one of those never
    /// goes out (a file's size past its last block on the disk is cut
    /// short by fsck, which leaves the indirect block, a later fsck
    /// refuses).
    indirects: BTreeSet<u64>,
    empty: BTreeSet<u64>,
    work: VecDeque<Work>,
    /// Bytes waiting to be freed.
    pending_free: u64,
}

impl Deps {
    #[must_use]
    pub fn new(sb: &Superblock) -> Self {
        Self {
            sb: sb.clone(),
            seq: 0,
            next: 0,
            items: IntMap::new(),
            buf_watch: IntMap::new(),
            item_watch: IntMap::new(),
            covers: BTreeMap::new(),
            by_bno: IntMap::new(),
            unplaced: IntMap::new(),
            slots: BTreeMap::new(),
            owned: IntMap::new(),
            inodes: IntMap::new(),
            inode_blocks: IntMap::new(),
            dir_blocks: BTreeMap::new(),
            dir_at: IntMap::new(),
            entries: BTreeMap::new(),
            named: IntMap::new(),
            inflight: IntMap::new(),
            indirects: BTreeSet::new(),
            empty: BTreeSet::new(),
            work: VecDeque::new(),
            pending_free: 0,
        }
    }

    fn stamp(&mut self) -> Stamp {
        self.seq += 1;
        Stamp(self.seq)
    }

    fn add(&mut self, kind: Kind, waits: &[Wait]) -> Id {
        self.next += 1;
        let id = Id(self.next);
        self.items.insert(
            id,
            Item {
                waits: Vec::new(),
                kind,
            },
        );
        for &w in waits {
            self.add_wait(id, w);
        }
        id
    }

    fn inode_dep(&mut self, ino: u32) -> &mut InodeDep {
        if !self.inodes.contains_key(&ino) {
            let (at, _) = self.sb.inode_location(ino);
            self.inode_blocks.entry(at).or_default().insert(ino);
        }
        self.inodes.entry(ino).or_default()
    }

    /// Forgets inode `ino`'s record if nothing is left in it.
    fn maybe_drop_inode(&mut self, ino: u32) {
        let Some(d) = self.inodes.get(&ino) else {
            return;
        };
        let slots = self
            .slots
            .range(Slot::Inode { ino, index: 0 }..=Slot::Inode { ino, index: 14 })
            .next()
            .is_some();
        if d.born.is_none()
            && d.copy_wait.is_empty()
            && d.watch.is_empty()
            && d.delta == 0
            && !slots
        {
            self.inodes.remove(&ino);
            let (at, _) = self.sb.inode_location(ino);
            if let Some(s) = self.inode_blocks.get_mut(&at) {
                s.remove(&ino);
                if s.is_empty() {
                    self.inode_blocks.remove(&at);
                }
            }
        }
    }

    fn add_wait(&mut self, id: Id, w: Wait) {
        match w {
            Wait::Item(other) if !self.items.contains_key(&other) => return,
            Wait::Item(other) => self.item_watch.entry(other).or_default().push(id),
            Wait::Buffer { at, stamp } => self.buf_watch.entry(at).or_default().push((stamp, id)),
            Wait::Copy(ino) => self.inode_dep(ino).copy_wait.push(id),
            Wait::Inode { ino, stamp } => self.inode_dep(ino).watch.push((stamp, id)),
            Wait::Arming | Wait::Data(_) => {}
        }
        if let Some(item) = self.items.get_mut(&id) {
            item.waits.push(w);
        }
    }

    /// Wait `w` of item `id` is over.
    fn satisfy(&mut self, id: Id, w: Wait) {
        let Some(item) = self.items.get_mut(&id) else {
            return;
        };
        let Some(i) = item.waits.iter().position(|x| *x == w) else {
            return;
        };
        item.waits.swap_remove(i);
        if item.waits.is_empty() {
            self.ready(id);
        }
    }

    fn is_ready(&self, id: Id) -> bool {
        self.items.get(&id).is_some_and(|i| i.waits.is_empty())
    }

    fn ready(&mut self, id: Id) {
        match self.items.get(&id).map(|i| &i.kind) {
            Some(Kind::Born(ino)) => {
                let ino = *ino;
                self.items.remove(&id);
                if let Some(d) = self.inodes.get_mut(&ino) {
                    d.born = None;
                }
                self.maybe_drop_inode(ino);
            }
            Some(Kind::Deferred(_)) => {
                if let Some(Item {
                    kind: Kind::Deferred(w),
                    ..
                }) = self.items.remove(&id)
                {
                    self.work.push_back(w);
                }
                self.followers_done(id);
            }
            // Pointers and entries go out with their next write.
            _ => {}
        }
    }

    /// Item `id` is gone: what waited on it is free of that.
    fn followers_done(&mut self, id: Id) {
        for f in self.item_watch.remove(&id).unwrap_or_default() {
            self.satisfy(f, Wait::Item(id));
        }
    }

    /// Item `id` was dropped: what waited on it waits for `instead`.
    fn retarget_followers(&mut self, id: Id, instead: &[Wait]) {
        for f in self.item_watch.remove(&id).unwrap_or_default() {
            for &w in instead {
                self.add_wait(f, w);
            }
            self.satisfy(f, Wait::Item(id));
        }
    }

    // ---- Groups: called with the group's buffer held, as it changes ----

    /// Bytes `[from, size)` of block `bno` were allocated in the group
    /// whose buffer is at `cg_at` (`from` is 0, or the old size of
    /// fragments extended in place). Pointers to it wait for the group
    /// and for its contents (as file data, unless [`Self::contents`]).
    pub fn new_block(&mut self, bno: u64, from: u64, size: u64, cg_at: u64, owner: u32) {
        let stamp = self.stamp();
        let id = self.add(
            Kind::Alloc(Alloc {
                bno,
                slot: None,
                old: 0,
                old_size: 0,
                owner,
                free_old: None,
                attached: false,
                dir_block: None,
            }),
            &[Wait::Buffer { at: cg_at, stamp }, Wait::Data(bno)],
        );
        let start = bno * u64::from(self.sb.fsize) + from;
        self.covers.insert(
            (start, id),
            Cover {
                id,
                bno,
                len: size - from,
                have: 0,
            },
        );
        self.unplaced.insert(bno, id);
        self.by_bno.insert(bno, id);
    }

    /// Block `bno` keeps only its first `size` bytes (a file's last
    /// fragments, cut short): what pointers to it wait for shrinks too.
    pub fn shrink_block(&mut self, bno: u64, size: u64) {
        let base = bno * u64::from(self.sb.fsize);
        let end = base + size;
        let keys: Vec<(u64, Id)> = self
            .covers
            .range(Self::covers_of(base))
            .filter(|(_, c)| c.bno == bno)
            .map(|(k, _)| *k)
            .collect();
        for key in keys {
            let Some(c) = self.covers.get_mut(&key) else {
                continue;
            };
            let (start, _) = key;
            c.len = end.saturating_sub(start).min(c.len);
            let need = c.len.div_ceil(SECTOR);
            let full = if need >= 128 {
                u128::MAX
            } else {
                (1u128 << need) - 1
            };
            if c.have & full == full {
                let (id, bno) = (c.id, c.bno);
                self.covers.remove(&key);
                self.satisfy(id, Wait::Data(bno));
            }
        }
    }

    /// Inode `ino` was allocated in the group whose buffer is at `cg_at`:
    /// it goes out zeroed until the group is written.
    pub fn new_inode(&mut self, ino: u32, cg_at: u64) {
        let stamp = self.stamp();
        let id = self.add(Kind::Born(ino), &[Wait::Buffer { at: cg_at, stamp }]);
        let d = self.inode_dep(ino);
        if let Some(old) = d.born.replace(id) {
            self.items.remove(&old);
        }
    }

    // ---- Pointers ----

    /// Block `bno`'s new pointer is set at `slot` (in place of `old`, of
    /// `old_size` bytes, for an inode's direct block), in file `owner`.
    /// An inode slot attaches at its next copy; an indirect one is set
    /// with the indirect block's buffer held, and attaches now.
    pub fn place(
        &mut self,
        bno: u64,
        slot: Slot,
        old: u64,
        old_size: u64,
        free_old: Option<(u64, u64)>,
    ) {
        let Some(id) = self.unplaced.remove(&bno) else {
            return;
        };
        let mut owner = 0;
        if let Some(Item {
            kind: Kind::Alloc(a),
            ..
        }) = self.items.get_mut(&id)
        {
            a.slot = Some(slot);
            a.old = old;
            a.old_size = old_size;
            a.free_old = free_old;
            a.attached = matches!(slot, Slot::Indirect { .. });
            owner = a.owner;
        }
        if let Slot::Inode { ino, .. } = slot {
            self.inode_dep(ino);
            self.merge(id, slot, old);
        }
        self.slots.entry(slot).or_default().push(id);
        self.owned.entry(owner).or_default().insert(id);
    }

    /// A direct block's pointer set again (a fragment grown in place or
    /// moved) before the last one reached the disk: the new one takes its
    /// place, and goes out in place of what the disk still holds
    /// (FreeBSD's `allocdirect_merge`). Otherwise the last one would wait
    /// for data written where the fragments no longer are, holding every
    /// later pointer back.
    fn merge(&mut self, id: Id, slot: Slot, old: u64) {
        let Some(&prev) = self.slots.get(&slot).and_then(|v| v.last()) else {
            return;
        };
        // Out with a write under way: it completes when that write does.
        if self.inflight.values().any(|f| f.out.contains(&prev)) {
            return;
        }
        let Some(p) = self.alloc_of(prev) else {
            return;
        };
        if p.bno != old {
            return;
        }
        let (p_old, p_old_size, p_free, p_bno) = (p.old, p.old_size, p.free_old, p.bno);
        let Some(Item {
            kind: Kind::Alloc(a),
            ..
        }) = self.items.get_mut(&id)
        else {
            return;
        };
        let moved = a.bno != p_bno;
        a.old = p_old;
        a.old_size = p_old_size;
        // The fragments the new pointer moved away from were never on the
        // disk: they are free now, and the new one frees what the last
        // one would have. Not so for fragments grown in place with nothing
        // to free: what the disk holds is still the start of the new one.
        let mut freed = None;
        if p_free.is_some() || p_old == 0 {
            freed = core::mem::replace(&mut a.free_old, p_free);
        }
        if let Some((bno, size)) = freed {
            self.pending_free += size;
            self.work.push_back(Work::Free(alloc::vec![(bno, size)]));
        }
        let waits = self
            .items
            .get(&prev)
            .map(|i| i.waits.clone())
            .unwrap_or_default();
        // Moved, the data goes where the new pointer points and nothing
        // of the last one is left to wait for. Grown in place, the old
        // part's data, and the group, are still to be written for the new
        // pointer too.
        if !moved {
            let base = p_bno * u64::from(self.sb.fsize);
            let keys: Vec<(u64, Id)> = self
                .covers
                .range(Self::covers_of(base))
                .filter(|(_, c)| c.id == prev)
                .map(|(k, _)| *k)
                .collect();
            for key in keys {
                if let Some(mut c) = self.covers.remove(&key) {
                    c.id = id;
                    self.covers.insert((key.0, id), c);
                }
            }
            for w in waits {
                self.add_wait(id, w);
            }
        }
        // What waited on the last one waits on the new one.
        self.retarget_followers(prev, &[Wait::Item(id)]);
        self.drop_alloc(prev);
    }

    /// Block `bno` is metadata whose contents are in the buffer at `at`,
    /// held as it is filled: pointers to it wait for that buffer's write.
    pub fn contents(&mut self, bno: u64, at: u64) {
        let Some(&id) = self.by_bno.get(&bno) else {
            return;
        };
        self.forget_cover(bno, id);
        let stamp = self.stamp();
        self.add_wait(id, Wait::Buffer { at, stamp });
        // One for each cover (two, for fragments grown in place).
        let n = self.items.get(&id).map_or(0, |i| {
            i.waits.iter().filter(|w| **w == Wait::Data(bno)).count()
        });
        for _ in 0..n {
            self.satisfy(id, Wait::Data(bno));
        }
    }

    /// The buffer at `at` (held) is an indirect block.
    pub fn indirect(&mut self, at: u64) {
        self.indirects.insert(at);
    }

    /// Block `bno` (just placed) holds block `lbn` of directory `dir`:
    /// entries in it are on the disk only once its pointer is. A block
    /// moved here from elsewhere brings its entries and waits along (the
    /// old buffer is discarded, and never written again).
    pub fn dir_block(&mut self, dir: u32, lbn: u64, bno: u64) {
        let Some(&id) = self.by_bno.get(&bno) else {
            return;
        };
        let at = bno * u64::from(self.sb.fsize);
        let old_at = self.dir_blocks.get(&(dir, lbn)).map(|b| b.at);
        self.dir_blocks.insert(
            (dir, lbn),
            DirBlock {
                at,
                alloc: Some(id),
            },
        );
        if let Some(Item {
            kind: Kind::Alloc(a),
            ..
        }) = self.items.get_mut(&id)
        {
            a.dir_block = Some((dir, lbn));
        }
        if let Some(old) = old_at
            && old != at
        {
            self.dir_at.remove(&old);
            let moved: Vec<((u64, u32), Vec<Id>)> = self
                .entries
                .range((old, 0)..=(old, u32::MAX))
                .map(|(k, v)| (*k, v.clone()))
                .collect();
            for ((_, off), ids) in moved {
                self.entries.remove(&(old, off));
                for &a in &ids {
                    if let Some(Item {
                        kind: Kind::DirAdd(d),
                        ..
                    }) = self.items.get_mut(&a)
                    {
                        d.place = Some((at, off));
                    }
                }
                self.entries.insert((at, off), ids);
            }
            for (stamp, w) in self.buf_watch.remove(&old).unwrap_or_default() {
                self.add_wait(w, Wait::Item(id));
                self.satisfy(w, Wait::Buffer { at: old, stamp });
            }
        }
        self.dir_at.insert(at, (dir, lbn));
    }

    /// Inode `ino` was copied into its block (with that buffer held):
    /// its new pointers attach, and what waited for a copy waits for this
    /// one's write.
    pub fn inode_copied(&mut self, ino: u32) {
        if !self.inodes.contains_key(&ino) {
            return;
        }
        let stamp = self.stamp();
        let ids: Vec<Id> = self
            .slots
            .range(Slot::Inode { ino, index: 0 }..=Slot::Inode { ino, index: 14 })
            .flat_map(|(_, v)| v.iter().copied())
            .collect();
        for id in ids {
            if let Some(Item {
                kind: Kind::Alloc(a),
                ..
            }) = self.items.get_mut(&id)
            {
                a.attached = true;
            }
        }
        let waiting = core::mem::take(&mut self.inode_dep(ino).copy_wait);
        for id in waiting {
            if self.items.contains_key(&id) {
                self.add_wait(id, Wait::Inode { ino, stamp });
                self.satisfy(id, Wait::Copy(ino));
            }
        }
    }

    /// An allocation of block `bno` that was never pointed to went back.
    pub fn cancel_block(&mut self, bno: u64) {
        if let Some(id) = self.unplaced.remove(&bno) {
            self.drop_alloc(id);
        }
    }

    /// The pointers set at `slot` are cleared (truncation): their
    /// allocations are forgotten. Returns fragments moved away from that
    /// may still be pointed to on the disk, to free with the rest.
    pub fn cancel_slot(&mut self, slot: Slot) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        for id in self.slots.remove(&slot).unwrap_or_default() {
            if let Some(f) = self.drop_alloc(id) {
                out.push(f);
            }
        }
        if let Slot::Inode { ino, .. } = slot {
            self.maybe_drop_inode(ino);
        }
        out
    }

    /// The keys of covers that may lie in the block at byte `base`: each
    /// starts within the block.
    fn covers_of(base: u64) -> core::ops::Range<(u64, Id)> {
        (base, Id(0))..(base + u64::from(crate::sb::MAXBSIZE), Id(0))
    }

    /// Forgets allocation `id`'s covers of block `bno` (two, for fragments
    /// grown in place and merged), if it has any.
    fn forget_cover(&mut self, bno: u64, id: Id) {
        let base = bno * u64::from(self.sb.fsize);
        let keys: Vec<(u64, Id)> = self
            .covers
            .range(Self::covers_of(base))
            .filter(|(_, c)| c.id == id)
            .map(|(k, _)| *k)
            .collect();
        for key in keys {
            self.covers.remove(&key);
        }
    }

    /// Forgets allocation `id`; what waited on it waits for its file's
    /// next inode write instead.
    fn drop_alloc(&mut self, id: Id) -> Option<(u64, u64)> {
        let Some(Item {
            kind: Kind::Alloc(a),
            ..
        }) = self.items.remove(&id)
        else {
            return None;
        };
        self.forget_cover(a.bno, id);
        if self.by_bno.get(&a.bno) == Some(&id) {
            self.by_bno.remove(&a.bno);
        }
        if self.unplaced.get(&a.bno) == Some(&id) {
            self.unplaced.remove(&a.bno);
        }
        if let Some(slot) = a.slot
            && let Some(v) = self.slots.get_mut(&slot)
        {
            v.retain(|x| *x != id);
            if v.is_empty() {
                self.slots.remove(&slot);
            }
        }
        if let Some(o) = self.owned.get_mut(&a.owner) {
            o.remove(&id);
            if o.is_empty() {
                self.owned.remove(&a.owner);
            }
        }
        if let Some(key) = a.dir_block
            && let Some(b) = self.dir_blocks.get_mut(&key)
            && b.alloc == Some(id)
        {
            b.alloc = None;
        }
        self.retarget_followers(id, &[Wait::Copy(a.owner)]);
        a.free_old
    }

    /// The pointer of allocation `id` is on the disk.
    fn alloc_done(&mut self, id: Id) {
        let Some(Item {
            kind: Kind::Alloc(a),
            ..
        }) = self.items.remove(&id)
        else {
            return;
        };
        if self.by_bno.get(&a.bno) == Some(&id) {
            self.by_bno.remove(&a.bno);
        }
        if let Some(slot) = a.slot
            && let Some(v) = self.slots.get_mut(&slot)
        {
            v.retain(|x| *x != id);
            if v.is_empty() {
                self.slots.remove(&slot);
            }
        }
        if let Some(o) = self.owned.get_mut(&a.owner) {
            o.remove(&id);
            if o.is_empty() {
                self.owned.remove(&a.owner);
            }
        }
        if let Some((bno, size)) = a.free_old {
            self.pending_free += size;
            self.work.push_back(Work::Free(alloc::vec![(bno, size)]));
        }
        // A directory block now on the disk: its entries written are.
        let blocks = a.dir_block.filter(|key| {
            self.dir_blocks
                .get(key)
                .is_some_and(|b| b.alloc == Some(id))
        });
        if let Some(key) = blocks {
            let at = self.dir_blocks.get(&key).map_or(0, |b| b.at);
            self.dir_blocks.remove(&key);
            self.dir_at.remove(&at);
            let written: Vec<Id> = self
                .entries
                .range((at, 0)..=(at, u32::MAX))
                .flat_map(|(_, v)| v.iter().copied())
                .filter(|a| {
                    matches!(
                        self.items.get(a),
                        Some(Item {
                            kind: Kind::DirAdd(DirAdd { written: true, .. }),
                            ..
                        })
                    )
                })
                .collect();
            for a in written {
                self.diradd_done(a);
            }
        }
        if let Slot::Inode { ino, .. } = a.slot.unwrap_or(Slot::Indirect { at: 0, index: 0 }) {
            self.maybe_drop_inode(ino);
        }
        self.followers_done(id);
    }

    // ---- Directories ----

    /// A new entry (or a changed one) will name inode `ino`: its waits
    /// are added ([`Self::wait_copy`], [`Self::wait_block`]), then it is
    /// made with the directory block held ([`Self::place_entry`]).
    pub fn new_entry(&mut self, ino: u32) -> Id {
        self.add(
            Kind::DirAdd(DirAdd {
                place: None,
                dir: (0, 0),
                ino,
                old: 0,
                old_type: 0,
                written: false,
            }),
            &[Wait::Arming],
        )
    }

    /// Item `id` waits for inode `ino`'s next copy to be written.
    pub fn wait_copy(&mut self, id: Id, ino: u32) {
        self.add_wait(id, Wait::Copy(ino));
    }

    /// Item `id` waits for the pointer to block `bno` to be on the disk.
    pub fn wait_block(&mut self, id: Id, bno: u64) {
        if let Some(&a) = self.by_bno.get(&bno) {
            self.add_wait(id, Wait::Item(a));
        }
    }

    /// Item `id` waits for the buffer at `at` (held, as it changes) to be
    /// written.
    pub fn wait_buffer(&mut self, id: Id, at: u64) {
        let stamp = self.stamp();
        self.add_wait(id, Wait::Buffer { at, stamp });
    }

    /// Entry `id` is made: in block `lbn` of directory `dir`, whose buffer
    /// at `at` is held, at byte `off`, naming what it did before `old` (0
    /// for a new entry; of type `old_type`). A changed entry's old inode
    /// loses what it held (`gone`) once the change is on the disk.
    /// The old inode's drop waits for `also` too (the removal of another
    /// entry naming it).
    #[allow(clippy::too_many_arguments)]
    pub fn place_entry(
        &mut self,
        id: Id,
        dir: u32,
        lbn: u64,
        at: u64,
        off: u32,
        (old, old_type): (u32, u8),
        gone: Gone,
        also: &Removal,
    ) {
        let ino = match self.items.get_mut(&id) {
            Some(Item {
                kind: Kind::DirAdd(d),
                ..
            }) => {
                d.place = Some((at, off));
                d.dir = (dir, lbn);
                d.old = old;
                d.old_type = old_type;
                d.ino
            }
            _ => return,
        };
        self.entries.entry((at, off)).or_default().push(id);
        self.named.entry(ino).or_default().insert(id);
        if old != 0 {
            self.count_drop(old, gone);
            let rem = self.add(
                Kind::Deferred(Work::Unlink { ino: old, gone }),
                &[Wait::Arming, Wait::Item(id)],
            );
            for &w in &also.0 {
                self.add_wait(rem, w);
            }
            self.satisfy(rem, Wait::Arming);
        }
        self.satisfy(id, Wait::Arming);
    }

    /// Links that will drop once on the disk.
    fn count_drop(&mut self, ino: u32, gone: Gone) {
        self.inode_dep(ino).delta += u32::from(gone.links());
        if let Gone::Dir(p) = gone {
            self.inode_dep(p).delta += 1;
        }
    }

    /// Forgets entry `id` (never made, or its entry gone); returns what
    /// waited for it.
    fn drop_entry(&mut self, id: Id) -> Vec<Id> {
        let Some(Item {
            kind: Kind::DirAdd(d),
            ..
        }) = self.items.remove(&id)
        else {
            return Vec::new();
        };
        if let Some(key) = d.place
            && let Some(v) = self.entries.get_mut(&key)
        {
            v.retain(|x| *x != id);
            if v.is_empty() {
                self.entries.remove(&key);
            }
        }
        if let Some(n) = self.named.get_mut(&d.ino) {
            n.remove(&id);
            if n.is_empty() {
                self.named.remove(&d.ino);
            }
        }
        self.item_watch.remove(&id).unwrap_or_default()
    }

    /// An entry that was never made goes.
    pub fn cancel(&mut self, id: Id) {
        if matches!(
            self.items.get(&id),
            Some(Item {
                kind: Kind::DirAdd(_),
                ..
            })
        ) {
            for f in self.drop_entry(id) {
                self.satisfy(f, Wait::Item(id));
            }
        } else if self.items.remove(&id).is_some() {
            self.followers_done(id);
        }
    }

    fn diradd_done(&mut self, id: Id) {
        for f in self.drop_entry(id) {
            self.satisfy(f, Wait::Item(id));
        }
    }

    /// The entry at byte `off` of block `lbn` of directory `dir` (buffer
    /// at `at`, held), naming `ino`, was removed: what it held (`gone`)
    /// drops once the removal is on the disk, or now if the entry never
    /// got there.
    /// Returns what the removal waits for, for others to wait on.
    #[allow(clippy::too_many_arguments)]
    pub fn remove_entry(
        &mut self,
        dir: u32,
        lbn: u64,
        at: u64,
        off: u32,
        ino: u32,
        gone: Gone,
    ) -> Removal {
        let stamp = self.stamp();
        let adds = self.entries.get(&(at, off)).cloned().unwrap_or_default();
        let on_disk = adds.last().is_none_or(|a| {
            matches!(
                self.items.get(a),
                Some(Item {
                    kind: Kind::DirAdd(DirAdd { written: true, .. }),
                    ..
                })
            )
        });
        let block = self.dir_blocks.get(&(dir, lbn)).and_then(|b| b.alloc);
        let mut removal = alloc::vec![Wait::Buffer { at, stamp }];
        if let Some(a) = block {
            removal.push(Wait::Item(a));
        }
        // Older names the entry held on the disk go once the removal is
        // there.
        for a in adds {
            for f in self.drop_entry(a) {
                for &w in &removal {
                    self.add_wait(f, w);
                }
                self.satisfy(f, Wait::Item(a));
            }
        }
        self.count_drop(ino, gone);
        let rem = self.add(Kind::Deferred(Work::Unlink { ino, gone }), &[Wait::Arming]);
        if on_disk {
            for &w in &removal {
                self.add_wait(rem, w);
            }
        }
        self.satisfy(rem, Wait::Arming);
        Removal(removal)
    }

    /// Forgets the buffer at `at`, of a block of file `owner` that is
    /// freed: its pointers and entries go, and what waited on its write
    /// waits for `owner`'s next inode write. Returns fragments moved away
    /// from by its pointers, to free with the rest.
    pub fn forget_buffer(&mut self, at: u64, owner: u32) -> Vec<(u64, u64)> {
        for (stamp, w) in self.buf_watch.remove(&at).unwrap_or_default() {
            self.add_wait(w, Wait::Copy(owner));
            self.satisfy(w, Wait::Buffer { at, stamp });
        }
        let slots: Vec<Slot> = self
            .slots
            .range(
                Slot::Indirect { at, index: 0 }..=Slot::Indirect {
                    at,
                    index: usize::MAX,
                },
            )
            .map(|(s, _)| *s)
            .collect();
        let mut out = Vec::new();
        for s in slots {
            out.extend(self.cancel_slot(s));
        }
        let adds: Vec<Id> = self
            .entries
            .range((at, 0)..=(at, u32::MAX))
            .flat_map(|(_, v)| v.iter().copied())
            .collect();
        for a in adds {
            for f in self.drop_entry(a) {
                self.add_wait(f, Wait::Copy(owner));
                self.satisfy(f, Wait::Item(a));
            }
        }
        if let Some(key) = self.dir_at.remove(&at) {
            self.dir_blocks.remove(&key);
        }
        self.indirects.remove(&at);
        self.empty.remove(&at);
        out
    }

    // ---- Frees and links ----

    /// Work to do once its waits ([`Self::wait_copy`],
    /// [`Self::wait_buffer`]) are over, which begin with [`Self::arm`].
    pub fn defer(&mut self, work: Work) -> Id {
        if let Work::Free(list) = &work {
            self.pending_free += list.iter().map(|(_, s)| s).sum::<u64>();
        }
        self.add(Kind::Deferred(work), &[Wait::Arming])
    }

    /// Item `id`'s waits are all given.
    pub fn arm(&mut self, id: Id) {
        self.satisfy(id, Wait::Arming);
    }

    /// Links of `ino` that dropped but are not on the disk yet: an inode
    /// read again shows that many fewer.
    #[must_use]
    pub fn nlink_delta(&self, ino: u32) -> u32 {
        self.inodes.get(&ino).map_or(0, |d| d.delta)
    }

    /// `n` of those reached the inode.
    pub fn nlink_applied(&mut self, ino: u32, n: u32) {
        if let Some(d) = self.inodes.get_mut(&ino) {
            d.delta = d.delta.saturating_sub(n);
        }
        self.maybe_drop_inode(ino);
    }

    /// The work whose waits are over.
    pub fn take_work(&mut self) -> Vec<Work> {
        let out: Vec<Work> = self.work.drain(..).collect();
        for w in &out {
            if let Work::Free(list) = w {
                let n: u64 = list.iter().map(|(_, s)| s).sum();
                self.pending_free = self.pending_free.saturating_sub(n);
            }
        }
        out
    }

    /// The frees whose waits are over (blocks and inodes), leaving the
    /// rest: what an allocation short of space can do itself.
    pub fn take_frees(&mut self) -> Vec<Work> {
        let (frees, rest): (Vec<Work>, Vec<Work>) = self
            .work
            .drain(..)
            .partition(|w| matches!(w, Work::Free(_) | Work::FreeFile { .. }));
        self.work = rest.into();
        for w in &frees {
            if let Work::Free(list) = w {
                let n: u64 = list.iter().map(|(_, s)| s).sum();
                self.pending_free = self.pending_free.saturating_sub(n);
            }
        }
        frees
    }

    /// Nothing is waiting, and no work is left.
    #[must_use]
    pub fn is_idle(&self) -> bool {
        self.items.is_empty() && self.work.is_empty()
    }

    /// Items held, for limits and the log.
    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// What is held, for a log when something never settles.
    #[must_use]
    pub fn describe(&self) -> alloc::string::String {
        use core::fmt::Write;
        let mut out = alloc::string::String::new();
        for (id, item) in &self.items {
            let _ = writeln!(out, "{id:?}: {:?} waits {:?}", item.kind, item.waits);
        }
        let _ = writeln!(out, "slots {:?}", self.slots);
        let _ = writeln!(out, "entries {:?}", self.entries);
        let _ = writeln!(out, "dir blocks {:?}", self.dir_blocks);
        let _ = writeln!(out, "buffer waits {:?}", self.buf_watch);
        let _ = writeln!(out, "item waits {:?}", self.item_watch);
        let _ = writeln!(out, "covers {:?}", self.covers);
        let _ = writeln!(out, "inodes {:?}", self.inodes);
        let _ = writeln!(out, "indirects {:?} empty {:?}", self.indirects, self.empty);
        out
    }

    /// Bytes freed but not yet back in their groups.
    #[must_use]
    pub fn pending_free(&self) -> u64 {
        self.pending_free
    }

    // ---- Writes ----

    /// A write of the buffer at `at`, holding `bytes`, begins: returns a
    /// copy with what may not go out yet rolled back, if anything.
    pub fn write_start(&mut self, at: u64, bytes: &[u8]) -> Option<Vec<u8>> {
        let mut copy: Option<Vec<u8>> = None;
        let mut out = Vec::new();
        let mut zeroed = BTreeSet::new();
        let inos: Vec<u32> = self
            .inode_blocks
            .get(&at)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default();
        for ino in inos {
            let (_, within) = self.sb.inode_location(ino);
            if within + inode::SIZE > bytes.len() {
                continue;
            }
            if self.inodes.get(&ino).is_some_and(|d| d.born.is_some()) {
                let c = copy.get_or_insert_with(|| bytes.to_vec());
                raw::zero_but_generation(&mut c[within..within + inode::SIZE]);
                zeroed.insert(ino);
                continue;
            }
            self.rollback_inode(ino, bytes, &mut copy, within, &mut out);
        }
        // Any inode's root over an indirect block empty on the disk stays
        // back, dependencies or not.
        if !self.empty.is_empty() && self.is_inode_block(at) {
            let fsize = u64::from(self.sb.fsize);
            let keep = self.sb.metackhash & CK_INODE != 0;
            for within in (0..bytes.len()).step_by(inode::SIZE) {
                let b = &copy.as_deref().unwrap_or(bytes)[within..within + inode::SIZE];
                // A short symlink keeps its target where pointers would be.
                if !raw::has_blocks(b, self.sb.maxsymlinklen) {
                    continue;
                }
                let hit: Vec<usize> = (NDIRECT..NDIRECT + NIADDR)
                    .filter(|&i| {
                        let v = raw::pointer(b, i);
                        v != 0
                            && v.checked_mul(fsize)
                                .is_some_and(|a| self.empty.contains(&a))
                    })
                    .collect();
                if hit.is_empty() {
                    continue;
                }
                let c = copy.get_or_insert_with(|| bytes.to_vec());
                let b = &mut c[within..within + inode::SIZE];
                for i in hit {
                    raw::set_pointer(b, i, 0);
                }
                raw::rehash(b, keep);
            }
        }
        // Indirect block pointers.
        let slots: Vec<(Slot, Vec<Id>)> = self
            .slots
            .range(
                Slot::Indirect { at, index: 0 }..=Slot::Indirect {
                    at,
                    index: usize::MAX,
                },
            )
            .map(|(s, v)| (*s, v.clone()))
            .collect();
        let mut went: BTreeMap<usize, Vec<Id>> = BTreeMap::new();
        for (slot, ids) in slots {
            let Slot::Indirect { index, .. } = slot else {
                continue;
            };
            // Only what the buffer holds goes out.
            let held = bytes
                .get(index * 8..index * 8 + 8)
                .map_or(0, |p| u64::from_le_bytes(p.try_into().unwrap_or([0; 8])));
            let ids: Vec<Id> = match ids
                .iter()
                .rposition(|&id| self.alloc_of(id).is_some_and(|a| a.bno == held))
            {
                Some(last) => ids[..=last].to_vec(),
                None => Vec::new(),
            };
            match ids.iter().position(|&id| !self.is_ready(id)) {
                Some(k) => {
                    went.insert(index, ids[..k].to_vec());
                    let old = self.alloc_of(ids[k]).map_or(0, |a| a.old);
                    let c = copy.get_or_insert_with(|| bytes.to_vec());
                    if let Some(p) = c.get_mut(index * 8..index * 8 + 8) {
                        p.copy_from_slice(&old.to_le_bytes());
                    }
                }
                None => {
                    went.insert(index, ids);
                }
            }
        }
        let indirect = self.indirects.contains(&at);
        if indirect && !self.empty.is_empty() {
            // Pointers to indirect blocks empty on the disk stay back.
            let fsize = u64::from(self.sb.fsize);
            let src = copy.as_deref().unwrap_or(bytes);
            let hits: Vec<usize> = src
                .as_chunks::<8>()
                .0
                .iter()
                .enumerate()
                .filter(|(_, c)| {
                    let v = u64::from_le_bytes(**c);
                    v != 0
                        && v.checked_mul(fsize)
                            .is_some_and(|a| self.empty.contains(&a))
                })
                .map(|(i, _)| i)
                .collect();
            for i in hits {
                let c = copy.get_or_insert_with(|| bytes.to_vec());
                c[i * 8..i * 8 + 8].fill(0);
                went.remove(&i);
            }
        }
        out.extend(went.into_values().flatten());
        let empty = indirect.then(|| {
            copy.as_deref()
                .unwrap_or(bytes)
                .as_chunks::<8>()
                .0
                .iter()
                .all(|c| *c == [0; 8])
        });
        // Directory entries.
        let entries: Vec<(u32, Vec<Id>)> = self
            .entries
            .range((at, 0)..=(at, u32::MAX))
            .map(|((_, off), v)| (*off, v.clone()))
            .collect();
        for (off, ids) in entries {
            let unwritten = |s: &Self, id: &Id| {
                matches!(
                    s.items.get(id),
                    Some(Item {
                        kind: Kind::DirAdd(DirAdd { written: false, .. }),
                        ..
                    })
                )
            };
            match ids.iter().position(|&id| !self.is_ready(id)) {
                Some(k) => {
                    out.extend(ids[..k].iter().filter(|id| unwritten(self, id)));
                    let (old, old_type) = match self.items.get(&ids[k]) {
                        Some(Item {
                            kind: Kind::DirAdd(d),
                            ..
                        }) => (d.old, d.old_type),
                        _ => (0, 0),
                    };
                    let c = copy.get_or_insert_with(|| bytes.to_vec());
                    let o = idx(off);
                    if let Some(p) = c.get_mut(o..o + 4) {
                        p.copy_from_slice(&old.to_le_bytes());
                    }
                    // A changed entry keeps the old inode's type too.
                    if old != 0
                        && let Some(t) = c.get_mut(o + 6)
                    {
                        *t = old_type;
                    }
                }
                None => out.extend(ids.iter().filter(|id| unwritten(self, id))),
            }
        }
        self.inflight.insert(
            at,
            Inflight {
                seq: self.seq,
                out,
                zeroed,
                empty,
            },
        );
        copy
    }

    /// Whether the buffer at `at` is a block of inodes.
    fn is_inode_block(&self, at: u64) -> bool {
        let sb = &self.sb;
        let fsize = u64::from(sb.fsize);
        if !at.is_multiple_of(fsize) {
            return false;
        }
        let frag = at / fsize;
        let fpg = u64::from(sb.fpg);
        let within = frag % fpg;
        let start = u64::from(sb.iblkno);
        let len = u64::from(sb.ipg / sb.inopb) * u64::from(sb.frag);
        frag / fpg < u64::from(sb.ncg) && within >= start && within < start + len
    }

    fn alloc_of(&self, id: Id) -> Option<&Alloc> {
        match self.items.get(&id) {
            Some(Item {
                kind: Kind::Alloc(a),
                ..
            }) => Some(a),
            _ => None,
        }
    }

    /// Rolls back inode `ino`'s pointers not ready, at byte `within` of
    /// the block, as FreeBSD's `initiate_write_inodeblock_ufs2` does: a
    /// pointer goes back to what it was, and the size with it so the
    /// inode never claims fragments not yet its own (or ends in a hole of
    /// its direct blocks). Pointers that went out are added to `out`.
    fn rollback_inode(
        &self,
        ino: u32,
        bytes: &[u8],
        copy: &mut Option<Vec<u8>>,
        within: usize,
        out: &mut Vec<Id>,
    ) {
        const SLOTS: usize = NDIRECT + NIADDR;
        let bsize = u64::from(self.sb.bsize);
        let mut rolled: [Option<(u64, u64)>; SLOTS] = [None; SLOTS];
        let mut went: [Vec<Id>; SLOTS] = Default::default();
        for (slot, ids) in self.slots.range(
            Slot::Inode { ino, index: 0 }..=Slot::Inode {
                ino,
                index: SLOTS - 1,
            },
        ) {
            let Slot::Inode { index, .. } = *slot else {
                continue;
            };
            let mut attached: Vec<Id> = ids
                .iter()
                .copied()
                .filter(|&id| self.alloc_of(id).is_some_and(|a| a.attached))
                .collect();
            // Only what the copy holds goes out.
            let held = raw::pointer(&bytes[within..within + inode::SIZE], index);
            match attached
                .iter()
                .rposition(|&id| self.alloc_of(id).is_some_and(|a| a.bno == held))
            {
                Some(last) => attached.truncate(last + 1),
                None => attached.clear(),
            }
            match attached.iter().position(|&id| !self.is_ready(id)) {
                Some(k) => {
                    went[index] = attached[..k].to_vec();
                    rolled[index] = self.alloc_of(attached[k]).map(|a| (a.old, a.old_size));
                }
                None => went[index] = attached,
            }
        }
        // Roots pointing at indirect blocks empty on the disk stay back.
        let fsize = u64::from(self.sb.fsize);
        let mut emptied = [false; NIADDR];
        // A short symlink keeps its target where pointers would be.
        let pointers = raw::has_blocks(&bytes[within..within + inode::SIZE], self.sb.maxsymlinklen);
        for (level, e) in emptied.iter_mut().enumerate().filter(|_| pointers) {
            let index = NDIRECT + level;
            let v = match rolled[index] {
                Some((old, _)) => old,
                None => raw::pointer(&bytes[within..within + inode::SIZE], index),
            };
            if v != 0
                && v.checked_mul(fsize)
                    .is_some_and(|a| self.empty.contains(&a))
            {
                *e = true;
                went[index].clear();
            }
        }
        if rolled.iter().any(Option::is_some) || emptied.iter().any(|e| *e) {
            let c = copy.get_or_insert_with(|| bytes.to_vec());
            let b = &mut c[within..within + inode::SIZE];
            for (level, e) in emptied.iter().enumerate() {
                if *e {
                    raw::set_pointer(b, NDIRECT + level, 0);
                }
            }
            for (index, r) in rolled.iter().enumerate() {
                if let Some((old, _)) = r {
                    raw::set_pointer(b, index, *old);
                }
            }
            let cut = (0..NDIRECT).find_map(|i| match rolled[i] {
                Some((_, osz)) if osz != 0 && osz < bsize => Some((i, osz)),
                _ => None,
            });
            if let Some((i, osz)) = cut {
                // A fragment rolled back ends the file there.
                raw::set_size(b, len64(i) * bsize + osz);
                for (j, w) in went.iter_mut().enumerate().skip(i + 1) {
                    raw::set_pointer(b, j, 0);
                    w.clear();
                }
            } else if let Some(last) = (0..NDIRECT).rev().find(|&i| rolled[i].is_some())
                && raw::size(b) <= (len64(last) + 1) * bsize
            {
                // The last direct block rolled back to a hole: the file
                // ends with the last block left.
                let end = (0..=last).rev().find(|&i| raw::pointer(b, i) != 0);
                raw::set_size(b, end.map_or(0, |i| (len64(i) + 1) * bsize));
            }
            raw::rehash(b, self.sb.metackhash & CK_INODE != 0);
        }
        for w in went {
            out.extend(w);
        }
    }

    /// The write of the buffer at `at` ended.
    pub fn write_done(&mut self, at: u64, ok: bool) {
        let Some(f) = self.inflight.remove(&at) else {
            return;
        };
        if !ok {
            return;
        }
        match f.empty {
            Some(true) => {
                self.empty.insert(at);
            }
            Some(false) => {
                self.empty.remove(&at);
            }
            None => {}
        }
        if let Some(list) = self.buf_watch.remove(&at) {
            let (hit, keep): (Watchers, Watchers) =
                list.into_iter().partition(|(s, _)| s.0 <= f.seq);
            if !keep.is_empty() {
                self.buf_watch.insert(at, keep);
            }
            for (stamp, id) in hit {
                self.satisfy(id, Wait::Buffer { at, stamp });
            }
        }
        let inos: Vec<u32> = self
            .inode_blocks
            .get(&at)
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default();
        for ino in inos {
            if f.zeroed.contains(&ino) {
                continue;
            }
            let Some(d) = self.inodes.get_mut(&ino) else {
                continue;
            };
            let (hit, keep): (Watchers, Watchers) =
                d.watch.drain(..).partition(|(s, _)| s.0 <= f.seq);
            d.watch = keep;
            for (stamp, id) in hit {
                self.satisfy(id, Wait::Inode { ino, stamp });
            }
            self.maybe_drop_inode(ino);
        }
        for id in f.out {
            match self.items.get_mut(&id) {
                Some(Item {
                    kind: Kind::Alloc(_),
                    ..
                }) => self.alloc_done(id),
                Some(Item {
                    kind: Kind::DirAdd(d),
                    ..
                }) => {
                    d.written = true;
                    let key = d.dir;
                    let pending = self.dir_blocks.get(&key).is_some_and(|b| b.alloc.is_some());
                    if !pending {
                        self.diradd_done(id);
                    }
                }
                _ => {}
            }
        }
    }

    /// File data was written at byte `offset`: new blocks it fills may be
    /// pointed to.
    pub fn data_written(&mut self, offset: u64, len: u64) {
        let end = offset + len;
        let from = offset.saturating_sub(u64::from(crate::sb::MAXBSIZE));
        let hits: Vec<(u64, Id)> = self
            .covers
            .range((from, Id(0))..(end, Id(0)))
            .filter(|((start, _), c)| start + c.len > offset)
            .map(|(k, _)| *k)
            .collect();
        for key in hits {
            let Some(c) = self.covers.get_mut(&key) else {
                continue;
            };
            let (start, _) = key;
            let lo = offset.max(start) - start;
            let hi = end.min(start + c.len) - start;
            for s in lo / SECTOR..hi.div_ceil(SECTOR) {
                if s < 128 {
                    c.have |= 1 << s;
                }
            }
            let need = c.len.div_ceil(SECTOR);
            let full = if need >= 128 {
                u128::MAX
            } else {
                (1u128 << need) - 1
            };
            if c.have & full == full {
                let (id, bno) = (c.id, c.bno);
                self.covers.remove(&key);
                self.satisfy(id, Wait::Data(bno));
            }
        }
    }

    // ---- What fsync needs ----

    /// The buffers to write, in order, for inode `ino`'s pointers and its
    /// own block to go out unrolled and its names to reach the disk; empty
    /// once nothing it needs is left.
    #[must_use]
    pub fn needs(&self, ino: u32) -> Vec<u64> {
        let mut first = Vec::new();
        let mut last = Vec::new();
        self.needs_of(ino, &mut first, &mut last);
        // Entries naming it, once it is out.
        for id in self.named.get(&ino).into_iter().flatten() {
            if let Some(Item {
                kind: Kind::DirAdd(d),
                ..
            }) = self.items.get(id)
            {
                if let Some((at, _)) = d.place {
                    last.push(at);
                }
                if self
                    .dir_blocks
                    .get(&d.dir)
                    .is_some_and(|b| b.alloc.is_some())
                {
                    self.needs_of(d.dir.0, &mut first, &mut last);
                }
            }
        }
        first.extend(last);
        let mut seen = BTreeSet::new();
        first.retain(|at| seen.insert(*at));
        first
    }

    fn needs_of(&self, ino: u32, first: &mut Vec<u64>, last: &mut Vec<u64>) {
        let mut any = false;
        if let Some(d) = self.inodes.get(&ino) {
            if let Some(b) = d.born {
                self.waits_buffers(b, first);
            }
            any = d.born.is_some() || !d.watch.is_empty() || !d.copy_wait.is_empty();
        }
        for id in self.owned.get(&ino).into_iter().flatten() {
            any = true;
            self.waits_buffers(*id, first);
            if let Some(Alloc {
                slot: Some(Slot::Indirect { at, .. }),
                ..
            }) = self.alloc_of(*id)
            {
                last.push(*at);
            }
        }
        if any {
            last.push(self.sb.inode_location(ino).0);
        }
    }

    fn waits_buffers(&self, id: Id, out: &mut Vec<u64>) {
        for w in self
            .items
            .get(&id)
            .map(|i| i.waits.as_slice())
            .unwrap_or(&[])
        {
            if let Wait::Buffer { at, .. } = w {
                out.push(*at);
            }
        }
    }
}

// A block's sectors fit a cover's map.
const _: () = assert!(crate::sb::MAXBSIZE <= 128 * 512);
