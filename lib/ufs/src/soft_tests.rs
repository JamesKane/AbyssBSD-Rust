// SPDX-License-Identifier: BSD-2-Clause
//! Soft updates on the host: random workloads over a cache that writes
//! buffers back in random order (leaving some writes under way, as the
//! kernel's cache does) and file data late, crashes at random points, and
//! a check of each crash image. On FreeBSD (the build server) `fsck_ffs
//! -p` must also repair each one with nothing but what soft updates
//! allows, and the settled image must be clean.

#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

extern crate std;

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;
use std::process::Command;

use super::softdep::Deps;
use super::tests::{
    MNode, Mem, Rng, check_model, fsck_clean, have_makefs, maker, scratch, workload,
};
use super::*;

/// Debugging: logs what touches byte `UFS_TRACE` of the disk.
fn trace(what: &str, offset: u64, len: u64) {
    if let Ok(v) = std::env::var("UFS_TRACE")
        && let Ok(at) = v.parse::<u64>()
        && offset < at + 4096
        && at < offset + len
    {
        std::eprintln!(
            "trace: {what} {offset} +{len}\n{}",
            std::backtrace::Backtrace::force_capture()
        );
    }
}

/// A disk behind a cache of buffers that go out only when told, through
/// the tracker's hooks, with file data queued behind it. A write may be
/// begun and left under way ([`Sim::begin`]): it goes out from a copy, as
/// the kernel cache's background writes do, the buffer changing meanwhile,
/// and lands later ([`Sim::land`]).
pub(crate) struct Sim {
    pub(crate) disk: RefCell<Vec<u8>>,
    bufs: RefCell<BTreeMap<u64, Vec<u8>>>,
    dirty: RefCell<BTreeSet<u64>>,
    /// Writes under way: the bytes going out, how often the buffer had
    /// changed when the write began, and whether the tracker held some
    /// changes back.
    flying: RefCell<BTreeMap<u64, Flying>>,
    /// How often each buffer has changed (the kernel cache's generation).
    changes: RefCell<BTreeMap<u64, u64>>,
    /// File data not on the disk yet, in order.
    data: RefCell<Vec<(u64, Vec<u8>)>>,
    deps: RefCell<Deps>,
}

/// A buffer write under way.
struct Flying {
    out: Vec<u8>,
    changes: u64,
    rolled: bool,
}

impl Sim {
    pub(crate) fn new(image: Vec<u8>) -> Self {
        let sb = Superblock::find(&(), &image[..], 512).unwrap();
        Self {
            disk: RefCell::new(image),
            bufs: RefCell::new(BTreeMap::new()),
            dirty: RefCell::new(BTreeSet::new()),
            flying: RefCell::new(BTreeMap::new()),
            changes: RefCell::new(BTreeMap::new()),
            data: RefCell::new(Vec::new()),
            deps: RefCell::new(Deps::new(&sb)),
        }
    }

    /// The buffer at `offset` holds `len` bytes: cached, resized keeping
    /// its bytes (as the kernel's cache does), or read.
    fn load(&self, offset: u64, len: usize) -> Result<(), Error> {
        let o = offset as usize;
        let mut bufs = self.bufs.borrow_mut();
        let disk = self.disk.borrow();
        match bufs.get_mut(&offset) {
            Some(b) if b.len() == len => {}
            Some(b) => {
                let old = b.len();
                if len > old {
                    b.extend_from_slice(disk.get(o + old..o + len).ok_or(Error::Io)?);
                } else {
                    b.truncate(len);
                }
            }
            None => {
                bufs.insert(offset, disk.get(o..o + len).ok_or(Error::Io)?.to_vec());
            }
        }
        Ok(())
    }

    /// Begins a write of the buffer at `offset` through the tracker, and
    /// leaves it under way; a buffer has one write at a time.
    pub(crate) fn begin(&self, offset: u64) {
        if self.flying.borrow().contains_key(&offset) {
            return;
        }
        let Some(held) = self.bufs.borrow().get(&offset).cloned() else {
            self.dirty.borrow_mut().remove(&offset);
            return;
        };
        let copy = self.deps.borrow_mut().write_start(offset, &held);
        let rolled = copy.is_some();
        let out = copy.unwrap_or(held);
        let changes = self.changed(offset);
        self.flying.borrow_mut().insert(
            offset,
            Flying {
                out,
                changes,
                rolled,
            },
        );
    }

    /// How often the buffer at `offset` has changed.
    fn changed(&self, offset: u64) -> u64 {
        self.changes.borrow().get(&offset).copied().unwrap_or(0)
    }

    /// The write under way of the buffer at `offset`, if any, lands: the
    /// buffer is clean if all of it went out and it has not changed since.
    pub(crate) fn land(&self, offset: u64) {
        let Some(f) = self.flying.borrow_mut().remove(&offset) else {
            return;
        };
        let o = offset as usize;
        self.disk.borrow_mut()[o..o + f.out.len()].copy_from_slice(&f.out);
        self.deps.borrow_mut().write_done(offset, true);
        let same = self.changed(offset) == f.changes;
        if same && !f.rolled {
            self.dirty.borrow_mut().remove(&offset);
        }
    }

    /// Writes the buffer at `offset` through the tracker, after any write
    /// of it under way.
    pub(crate) fn write_buf(&self, offset: u64) {
        self.land(offset);
        self.begin(offset);
        self.land(offset);
    }

    /// Checks the disk as a crash now may leave it ([`check_crash`]): with
    /// none of the writes under way landed, and with all of them. Returns
    /// how many images that was.
    pub(crate) fn check_crashes(&self) -> Result<usize, String> {
        check_crash(&self.disk.borrow()).map_err(|e| std::format!("nothing landed:\n{e}"))?;
        let flying = self.flying.borrow();
        if flying.is_empty() {
            return Ok(1);
        }
        let mut landed = self.disk.borrow().clone();
        for (at, f) in flying.iter() {
            let o = *at as usize;
            landed[o..o + f.out.len()].copy_from_slice(&f.out);
        }
        check_crash(&landed).map_err(|e| std::format!("all landed:\n{e}"))?;
        Ok(2)
    }

    /// Writes the first `n` file data writes queued.
    fn write_data_out(&self, n: usize) {
        let out: Vec<(u64, Vec<u8>)> = {
            let mut q = self.data.borrow_mut();
            let n = n.min(q.len());
            q.drain(..n).collect()
        };
        for (offset, data) in out {
            trace("data out", offset, data.len() as u64);
            let o = offset as usize;
            self.disk.borrow_mut()[o..o + data.len()].copy_from_slice(&data);
            self.deps
                .borrow_mut()
                .data_written(offset, data.len() as u64);
        }
    }

    /// Writes a random part of what is dirty, in a random order; some of
    /// the writes are left under way, and some of those under way land.
    pub(crate) fn write_some(&self, rng: &mut Rng) {
        let n = self.data.borrow().len();
        self.write_data_out(rng.below(n as u64 + 1) as usize);
        let flying: Vec<u64> = self.flying.borrow().keys().copied().collect();
        for at in flying {
            if rng.below(2) == 0 {
                self.land(at);
            }
        }
        let mut dirty: Vec<u64> = self.dirty.borrow().iter().copied().collect();
        for i in (1..dirty.len()).rev() {
            dirty.swap(i, rng.below(i as u64 + 1) as usize);
        }
        let keep = rng.below(dirty.len() as u64 + 1) as usize;
        for at in &dirty[keep..] {
            if rng.below(4) == 0 {
                self.begin(*at);
            } else {
                self.write_buf(*at);
            }
        }
    }

    /// Every write under way lands.
    pub(crate) fn land_all(&self) {
        let flying: Vec<u64> = self.flying.borrow().keys().copied().collect();
        for at in flying {
            self.land(at);
        }
    }

    fn idle(&self) -> bool {
        self.dirty.borrow().is_empty()
            && self.flying.borrow().is_empty()
            && self.data.borrow().is_empty()
            && self.deps.borrow().is_idle()
    }
}

impl BlockIo for Sim {
    type Cx = ();

    fn read(&self, _: &(), offset: u64, len: usize, f: &mut dyn FnMut(&[u8])) -> Result<(), Error> {
        self.load(offset, len)?;
        f(&self.bufs.borrow()[&offset]);
        Ok(())
    }

    fn modify(
        &self,
        _: &(),
        offset: u64,
        len: usize,
        fill: Fill,
        mode: Mode,
        f: &mut dyn FnMut(&mut [u8]),
    ) -> Result<(), Error> {
        match fill {
            Fill::Zero => {
                self.bufs.borrow_mut().insert(offset, alloc::vec![0; len]);
            }
            Fill::Read => self.load(offset, len)?,
        }
        let mut b = self.bufs.borrow_mut().remove(&offset).ok_or(Error::Io)?;
        f(&mut b);
        self.bufs.borrow_mut().insert(offset, b);
        *self.changes.borrow_mut().entry(offset).or_default() += 1;
        self.dirty.borrow_mut().insert(offset);
        if mode == Mode::Sync {
            self.write_buf(offset);
        }
        Ok(())
    }

    fn read_data(&self, _: &(), offset: u64, buf: &mut [u8]) -> Result<(), Error> {
        let o = offset as usize;
        buf.copy_from_slice(self.disk.borrow().get(o..o + buf.len()).ok_or(Error::Io)?);
        // What is queued is newer.
        for (at, d) in self.data.borrow().iter() {
            let (s, e) = (*at as usize, *at as usize + d.len());
            let (lo, hi) = (s.max(o), e.min(o + buf.len()));
            if lo < hi {
                buf[lo - o..hi - o].copy_from_slice(&d[lo - s..hi - s]);
            }
        }
        Ok(())
    }

    fn write_data(&self, _: &(), offset: u64, data: &[u8]) -> Result<(), Error> {
        trace("write_data", offset, data.len() as u64);
        self.data.borrow_mut().push((offset, data.to_vec()));
        Ok(())
    }

    fn forget(&self, _: &(), offset: u64, len: usize) {
        trace("forget", offset, len as u64);
        let end = offset + len as u64;
        let gone: Vec<u64> = self
            .bufs
            .borrow()
            .iter()
            .filter(|(o, b)| **o < end && **o + b.len() as u64 > offset)
            .map(|(o, _)| *o)
            .collect();
        for o in gone {
            // A write of it under way lands before the blocks are reused
            // (the kernel cache's `discard` waits for it).
            self.land(o);
            self.bufs.borrow_mut().remove(&o);
            self.dirty.borrow_mut().remove(&o);
        }
        // Data for freed fragments never goes out (the kernel drops a
        // truncated file's pages first); what lies around them does.
        let mut q = self.data.borrow_mut();
        let mut kept = Vec::new();
        for (at, d) in q.drain(..) {
            let e = at + d.len() as u64;
            if at >= end || e <= offset {
                kept.push((at, d));
                continue;
            }
            if at < offset {
                kept.push((at, d[..(offset - at) as usize].to_vec()));
            }
            if e > end {
                kept.push((end, d[(end - at) as usize..].to_vec()));
            }
        }
        *q = kept;
    }

    fn deps(&self, _: &(), f: &mut dyn FnMut(&mut Deps)) -> bool {
        f(&mut self.deps.borrow_mut());
        true
    }
}

/// Deferred work over inodes read from the cache (nothing stays in core
/// between operations here, and nothing is open).
struct Env<'a> {
    fs: &'a Ufs<Sim>,
    sum: &'a mut Summary,
}

impl ops::WorkEnv<Sim> for Env<'_> {
    fn with_inode(
        &mut self,
        ino: u32,
        f: &mut dyn FnMut(&mut Inode, bool, &mut Summary) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let mut ip = self.fs.inode(&(), ino)?;
        f(&mut ip, false, self.sum)
    }

    fn with_summary(
        &mut self,
        f: &mut dyn FnMut(&mut Summary) -> Result<(), Error>,
    ) -> Result<(), Error> {
        f(self.sum)
    }
}

/// Does the work that is due.
pub(crate) fn do_work(fs: &Ufs<Sim>, sum: &mut Summary) {
    loop {
        let work = fs.take_work(&());
        if work.is_empty() {
            return;
        }
        for w in work {
            let what = std::format!("{w:?}");
            fs.apply(&(), w, &mut Env { fs, sum })
                .unwrap_or_else(|e| panic!("{what}: {e:?}"));
        }
    }
}

/// Writes everything and does all the work, until nothing is left.
pub(crate) fn settle(fs: &Ufs<Sim>, sum: &mut Summary) {
    let io = fs.io();
    for _ in 0..200 {
        io.write_data_out(usize::MAX);
        io.land_all();
        let dirty: Vec<u64> = io.dirty.borrow().iter().copied().collect();
        for at in dirty {
            io.write_buf(at);
        }
        do_work(fs, sum);
        if io.idle() {
            return;
        }
    }
    let mut state = io.deps.borrow().describe();
    state.truncate(6000);
    panic!(
        "soft updates never settled: {} items, buffers dirty {:?}\n{state}",
        io.deps.borrow().len(),
        io.dirty.borrow()
    );
}

/// What `fsck_ffs -p` must accept in a crash image of a file system with
/// soft updates, checked here without it: no entry names a free inode; no
/// fragment is claimed twice or is free in its group; no directory has a
/// hole or claims fragments it has not; no link count is below the names.
pub(crate) fn check_crash(image: &[u8]) -> Result<(), String> {
    let fs = Ufs::open(&(), image, 512).map_err(|e| std::format!("{e:?}"))?;
    let sb = fs.superblock().clone();
    let fsize = u64::from(sb.fsize);
    let bsize = u64::from(sb.bsize);
    let mut cgs = Vec::new();
    for c in 0..sb.ncg {
        let at = (sb.cgtod(c) * fsize) as usize;
        cgs.push(image[at..at + sb.cgsize as usize].to_vec());
    }
    let frag_used = |f: u64, cgs: &mut Vec<Vec<u8>>| {
        let c = sb.dtog(f);
        let g = cg::Cg::new(&sb, &mut cgs[c as usize]);
        !g.frag_free(sb.dtogd(f))
    };
    let mut claimed: BTreeMap<u64, u32> = BTreeMap::new();
    let mut refs: BTreeMap<u32, u32> = BTreeMap::new();
    let mut problems = Vec::new();
    let mut inodes = BTreeMap::new();
    let mut empty = BTreeSet::new();
    let mut entries: Vec<(u32, Vec<u8>, u32)> = Vec::new();
    for ino in ROOT_INO..sb.ncg * sb.ipg {
        let ip = match fs.inode(&(), ino) {
            Ok(ip) => ip,
            Err(e) => {
                problems.push(std::format!("inode {ino}: {e:?}"));
                continue;
            }
        };
        if ip.mode == 0 {
            continue;
        }
        let local = ino % sb.ipg;
        let used = cg::Cg::new(&sb, &mut cgs[(ino / sb.ipg) as usize]).inode_used(local);
        if !used {
            problems.push(std::format!("inode {ino} allocated but free in its group"));
        }
        inodes.insert(ino, ip);
    }
    // Extended attribute blocks, of any kind of file, as fsck counts them:
    // those within `di_extsize`, the last one its fragments.
    for (&ino, ip) in &inodes {
        let size = u64::from(ip.extsize);
        for (i, &p) in ip.extb.iter().enumerate() {
            let i = i as u64;
            if p == 0 {
                continue;
            }
            if i * bsize >= size {
                problems.push(std::format!(
                    "inode {ino}: ext block {i} past its size {size}"
                ));
                continue;
            }
            let n = u64::from(sb.block_size(size, i)) / fsize;
            for x in p..p + n {
                if x >= sb.size {
                    problems.push(std::format!("inode {ino}: ext block {x} past the end"));
                    break;
                }
                if let Some(other) = claimed.insert(x, ino) {
                    problems.push(std::format!("fragment {x}: inodes {other} and {ino} (ext)"));
                }
                if !frag_used(x, &mut cgs) {
                    problems.push(std::format!("inode {ino}: ext block {x} free in its group"));
                }
            }
        }
    }
    for (&ino, ip) in &inodes {
        let has_blocks = match ip.kind() {
            Kind::Regular | Kind::Directory => true,
            Kind::Symlink => ip.size >= u64::from(sb.maxsymlinklen),
            _ => false,
        };
        if !has_blocks {
            continue;
        }
        let mut claim = |f: u64, n: u64, what: &str, problems: &mut Vec<String>| {
            for x in f..f + n {
                if x >= sb.size {
                    problems.push(std::format!("inode {ino}: {what} {x} past the end"));
                    return;
                }
                if let Some(other) = claimed.insert(x, ino) {
                    problems.push(std::format!("fragment {x}: inodes {other} and {ino}"));
                }
                if !frag_used(x, &mut cgs) {
                    problems.push(std::format!("inode {ino}: {what} {x} free in its group"));
                }
            }
        };
        let blocks = ip.size.div_ceil(bsize);
        let mut last_alloc: Option<u64> = None;
        // The last data block on the disk, as fsck finds it.
        let mut lballoc: Option<u64> = None;
        for (i, &p) in ip.db.iter().enumerate() {
            let i = i as u64;
            if p == 0 {
                continue;
            }
            if i >= blocks {
                problems.push(std::format!("inode {ino}: block {i} past its size"));
                continue;
            }
            let n = u64::from(sb.block_size(ip.size, i)) / fsize;
            claim(p, n, "block", &mut problems);
            last_alloc = Some(i);
            lballoc = Some(i);
        }
        // Indirect trees: every pointer in them.
        // Each indirect tree, with the file block its pointers start at.
        let nindir = u64::from(sb.nindir);
        let mut first = NDADDR;
        let mut stack: Vec<(u64, usize, u64)> = Vec::new();
        for (l, &p) in ip.ib.iter().enumerate() {
            if p != 0 {
                stack.push((p, l, first));
            }
            first += nindir.pow(l as u32 + 1);
        }
        let any_indirect = !stack.is_empty();
        while let Some((p, level, base)) = stack.pop() {
            claim(p, u64::from(sb.frag), "indirect block", &mut problems);
            let at = (p * fsize) as usize;
            let span = nindir.pow(level as u32);
            for (i, c) in image[at..at + bsize as usize]
                .as_chunks::<8>()
                .0
                .iter()
                .enumerate()
            {
                let q = u64::from_le_bytes(*c);
                if q == 0 {
                    continue;
                }
                let lbn = base + i as u64 * span;
                if level == 0 {
                    claim(q, u64::from(sb.frag), "block", &mut problems);
                    lballoc = lballoc.max(Some(lbn));
                } else {
                    stack.push((q, level - 1, lbn));
                }
            }
        }
        // fsck shortens a file to its last block on the disk; no root may
        // then lie past it (a second fsck refuses that).
        if ip.kind() == Kind::Regular && ip.size > 0 {
            let end = lballoc.map_or(0, |l| l + 1);
            if end < blocks {
                let mut start = NDADDR;
                for (l, &p) in ip.ib.iter().enumerate() {
                    if p != 0 && end <= start {
                        problems.push(std::format!(
                            "inode {ino}: fsck shortens it to {end} blocks, past indirect root {l} \
                             (size {}, roots {:?}, direct {:?})",
                            ip.size,
                            ip.ib,
                            ip.db
                        ));
                    }
                    start += nindir.pow(l as u32 + 1);
                }
            }
        }
        if ip.kind() == Kind::Directory {
            // A directory's blocks all hold entries.
            for i in 0..blocks.min(NDADDR) {
                if ip.db[i as usize] == 0 {
                    problems.push(std::format!("directory {ino}: hole at block {i}"));
                }
            }
            if ip.size == 0 {
                // Its first block rolled back: fine while nothing names it.
                empty.insert(ino);
                continue;
            }
            if problems.iter().any(|p| p.contains(&std::format!("{ino}:"))) {
                continue;
            }
            let r = fs.read_dir(&(), ip, 0, &mut |e| {
                // fsck sets `..` right itself.
                if e.ino != 0 && e.name != b".." {
                    entries.push((ino, e.name.to_vec(), e.ino));
                    if !inodes.contains_key(&e.ino) {
                        problems.push(std::format!(
                            "directory {ino}: {:?} names free inode {}",
                            String::from_utf8_lossy(e.name),
                            e.ino
                        ));
                    }
                }
                true
            });
            if let Err(e) = r {
                problems.push(std::format!("directory {ino}: {e:?}"));
            }
        } else if blocks <= NDADDR && !any_indirect && blocks > 0 && last_alloc != Some(blocks - 1)
        {
            problems.push(std::format!(
                "inode {ino}: ends in a hole of its direct blocks"
            ));
        }
    }
    // Links as fsck counts them: every entry, but only one name of a
    // directory (it removes the others), whose `..` then counts for the
    // directory naming it (it sets `..` so); a directory nothing names
    // goes to lost+found instead.
    let mut parent: BTreeMap<u32, u32> = BTreeMap::new();
    for (d, name, i) in &entries {
        let is_dir = inodes.get(i).is_some_and(|ip| ip.kind() == Kind::Directory);
        if name == b"." || !is_dir {
            *refs.entry(*i).or_default() += 1;
        } else if !parent.contains_key(i) {
            parent.insert(*i, *d);
            *refs.entry(*i).or_default() += 1;
        }
    }
    parent.insert(ROOT_INO, ROOT_INO);
    for p in parent.values() {
        *refs.entry(*p).or_default() += 1;
    }
    for (ino, n) in &refs {
        if empty.contains(ino) {
            problems.push(std::format!("directory {ino}: empty, but named"));
        }
        if let Some(ip) = inodes.get(ino)
            && u32::from(ip.nlink) < *n
        {
            let names: Vec<(u32, String)> = entries
                .iter()
                .filter(|(_, _, i)| i == ino)
                .map(|(d, n, _)| (*d, String::from_utf8_lossy(n).into_owned()))
                .collect();
            let children: Vec<u32> = parent
                .iter()
                .filter(|(_, p)| *p == ino)
                .map(|(c, _)| *c)
                .collect();
            problems.push(std::format!(
                "inode {ino} ({:?}): {} links but {n} names: {names:?}, children {children:?}",
                ip.kind(),
                ip.nlink
            ));
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        problems.truncate(20);
        Err(problems.join("\n"))
    }
}

/// `fsck_ffs -p` repairs crash image `path` (preening a file system with
/// soft updates fails on anything soft updates should not leave), again
/// until `-f -n` finds nothing (a directory cleared leaves what was in it
/// for the next run); returns what preening repaired, each kind once.
pub(crate) fn fsck_preens(path: &std::path::Path) -> Result<BTreeSet<String>, String> {
    let mut kinds = BTreeSet::new();
    let mut log = String::new();
    for _ in 0..4 {
        let text = preen(path)?;
        log += &text;
        // A repair's message may span lines: "/dev/mdN: WHAT I=..", then
        // details, then "(DONE)".
        let mut what = String::new();
        for l in text.lines() {
            let l = l.split_once(": ").map_or(l, |(_, r)| r);
            let mut words: Vec<&str> = l.split_whitespace().collect();
            if words.first() == Some(&"INODE") {
                words.drain(..2.min(words.len()));
            }
            let lead: Vec<&str> = words
                .iter()
                .copied()
                .take_while(|t| {
                    !t.contains('=')
                        && !t.starts_with(['(', '/'])
                        && !t.starts_with(|c: char| c.is_ascii_digit())
                })
                .collect();
            if !lead.is_empty() {
                what = lead.join(" ");
            }
            if let Some((_, done)) = l.rsplit_once('(') {
                let done = done.trim_end_matches(')');
                if !done.is_empty() && done.chars().all(|c| c.is_ascii_uppercase()) {
                    kinds.insert(std::format!("{what} ({done})"));
                }
            }
        }
        let after = Command::new("fsck_ffs")
            .args(["-f", "-n"])
            .arg(path)
            .output()
            .unwrap();
        let listing = String::from_utf8_lossy(&after.stdout).into_owned();
        if !listing
            .lines()
            .any(|l| l.ends_with("? no") && !l.starts_with("CONTINUE"))
        {
            return Ok(kinds);
        }
        log += &listing;
    }
    Err(std::format!("preening never finished:\n{log}"))
}

/// One `fsck_ffs -p` of image `path`, as a memory disk (preening wants a
/// disk); returns what it printed.
fn preen(path: &std::path::Path) -> Result<String, String> {
    let md = Command::new("doas")
        .args(["-n", "mdconfig", "-a", "-t", "vnode", "-f"])
        .arg(path)
        .output()
        .unwrap();
    let unit = String::from(String::from_utf8_lossy(&md.stdout).trim());
    assert!(
        md.status.success() && unit.starts_with("md"),
        "mdconfig: {md:?}"
    );
    let out = Command::new("doas")
        .args(["-n", "fsck_ffs", "-p", "-f", &std::format!("/dev/{unit}")])
        .output()
        .unwrap();
    let _ = Command::new("doas")
        .args(["-n", "mdconfig", "-d", "-u", &unit])
        .status();
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    if out.status.success() {
        Ok(text)
    } else {
        Err(std::format!("fsck_ffs -p:\n{text}"))
    }
}

/// A file system made with soft updates on (`newfs -U`), opened as a
/// mount does: the summary loaded, the superblock marked dirty.
fn soft_fs(img: &std::path::Path) -> (Ufs<Sim>, Summary) {
    let mut fs = Ufs::open(&(), Sim::new(std::fs::read(img).unwrap()), 512).unwrap();
    assert!(fs.use_soft_updates(&()));
    let mut sum = fs.load_summary(&()).unwrap();
    fs.write_summary(&(), &mut sum, false, 1_760_000_000)
        .unwrap();
    (fs, sum)
}

#[test]
fn crashes_at_random_points_leave_what_fsck_preens() {
    if !have_makefs() {
        std::eprintln!("no newfs (not FreeBSD): skipped");
        return;
    }
    let work = scratch("soft");
    let cases = [
        (32768u32, 4096u32, "64m", 21u64, 2500usize),
        (4096, 512, "24m", 23, 2500),
        (8192, 1024, "16m", 29, 3000),
    ];
    // `UFS_SOFT_SEEDS` runs more of each, `UFS_SOFT_CRASHES` crashes one
    // step in that many (100).
    let seeds: u64 = std::env::var("UFS_SOFT_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1);
    let every: u64 = std::env::var("UFS_SOFT_CRASHES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);
    let mut crashes = 0;
    let mut found: BTreeMap<String, usize> = BTreeMap::new();
    let runs = cases
        .iter()
        .flat_map(|&(b, f, sz, seed, n)| (0..seeds).map(move |i| (b, f, sz, seed + 100 * i, n)));
    for (bsize, fsize, size, seed, n) in runs {
        let img = work.join(std::format!("s-{bsize}-{seed}.img"));
        let ok = Command::new("truncate")
            .args(["-s", size])
            .arg(&img)
            .status()
            .unwrap()
            .success()
            && Command::new("newfs")
                .args([
                    "-U",
                    "-b",
                    &std::format!("{bsize}"),
                    "-f",
                    &std::format!("{fsize}"),
                ])
                .arg(&img)
                .stdout(std::process::Stdio::null())
                .status()
                .unwrap()
                .success();
        assert!(ok, "newfs");
        let (fs, mut sum) = soft_fs(&img);
        let mut rng = Rng(seed * 7919);
        let crash_img = work.join("crash.img");
        let mut step = 0;
        let model = workload(&fs, &mut sum, seed, n, &mut |fs, sum, _| {
            step += 1;
            let io = fs.io();
            if rng.below(2) == 0 {
                io.write_some(&mut rng);
            }
            if rng.below(3) == 0 {
                do_work(fs, sum);
            }
            if rng.below(every) == 0 {
                let image = io.disk.borrow().clone();
                if let Err(e) = check_crash(&image) {
                    panic!("seed {seed}: crash after step {step} ({bsize}/{fsize}):\n{e}");
                }
                std::fs::write(&crash_img, &image).unwrap();
                match fsck_preens(&crash_img) {
                    Ok(kinds) => {
                        for k in kinds {
                            *found.entry(k).or_default() += 1;
                        }
                    }
                    Err(e) => {
                        // Kept for a look.
                        let keep = std::env::temp_dir().join("abyss-soft-fail.img");
                        std::fs::write(&keep, &image).unwrap();
                        panic!(
                            "seed {seed}: crash after step {step} ({bsize}/{fsize}), image in {}:\n{e}",
                            keep.display()
                        )
                    }
                }
                crashes += 1;
            }
        });
        settle(&fs, &mut sum);
        fs.write_summary(&(), &mut sum, true, 1_760_000_100)
            .unwrap();
        let bytes = fs.io().disk.borrow().clone();
        let back = Ufs::open(&(), &bytes[..], 512).unwrap();
        check_model(&back, &model, ROOT_INO);
        std::fs::write(&img, &bytes).unwrap();
        fsck_clean(&img);
        std::eprintln!(
            "soft updates {bsize}/{fsize} seed {seed}: {} nodes, clean after settling",
            model.nodes.len()
        );
    }
    std::eprintln!("{crashes} crash images preened; fsck found:");
    for (k, n) in &found {
        std::eprintln!("  {n:6} {k}");
    }
    let _ = std::fs::remove_dir_all(&work);
}

/// Grows a file into the double-indirect range of 4096-byte blocks (512
/// pointers a block: the range starts at block 524), then cuts it down
/// step by step, some ends under the double-indirect block, which keep
/// the single indirect block whole; after each step `flush` puts the file
/// data queued on the disk and the contents are checked. Returns the
/// file's inode and what it must hold.
fn grow_and_cut<B: BlockIo<Cx = ()>>(
    fs: &Ufs<B>,
    sum: &mut Summary,
    flush: &dyn Fn(),
) -> (u32, Vec<u8>) {
    let r = Reserve::Allowed;
    let mut root = fs.inode(&(), ROOT_INO).unwrap();
    let (ino, mut ip) = fs
        .create(
            &(),
            sum,
            ROOT_INO,
            &mut root,
            b"f",
            ops::NewKind::Regular,
            &maker(&mut Rng(1), r),
        )
        .unwrap();
    let mut want: Vec<u8> = (0..700 * 4096 + 100).map(|i| (i * 7 % 251) as u8).collect();
    fs.write_data_range(&(), sum, ino, &mut ip, 0, &want, r)
        .unwrap();
    fs.write_inode(&(), ino, &ip, Mode::Sync).unwrap();
    let check = |ip: &Inode, want: &[u8], what: &str| {
        flush();
        let mut got = alloc::vec![0u8; want.len()];
        assert_eq!(fs.read(&(), ip, 0, &mut got).unwrap(), want.len());
        let bad = got.iter().zip(want).position(|(a, b)| a != b);
        assert!(
            bad.is_none(),
            "contents {what}: first wrong byte at {bad:?}"
        );
    };
    check(&ip, &want, "as written");
    for blocks in [600u64, 530, 524, 300, 13, 5] {
        let len = blocks * 4096 + 100;
        fs.truncate(&(), sum, ino, &mut ip, len, r, false)
            .unwrap_or_else(|e| panic!("truncate to {len}: {e:?}"));
        fs.write_inode(&(), ino, &ip, Mode::Sync).unwrap();
        want.truncate(len as usize);
        check(&ip, &want, &std::format!("after truncating to {len}"));
    }
    (ino, want)
}

/// FreeBSD's `ffs_truncate` stops at the level the new end lies under
/// (`goto done`): the levels below are kept whole. fsx hit this on the
/// Orange Pi (an index past the single indirect block's end).
#[test]
fn truncating_under_the_double_indirect_block_keeps_the_single_whole() {
    if !have_makefs() {
        std::eprintln!("no newfs (not FreeBSD): skipped");
        return;
    }
    let work = scratch("trunc2");
    for soft in [false, true] {
        let img = work.join(std::format!("t-{soft}.img"));
        let mut newfs = Command::new("newfs");
        if soft {
            newfs.arg("-U");
        }
        let ok = Command::new("truncate")
            .args(["-s", "24m"])
            .arg(&img)
            .status()
            .unwrap()
            .success()
            && newfs
                .args(["-b", "4096", "-f", "512"])
                .arg(&img)
                .stdout(std::process::Stdio::null())
                .status()
                .unwrap()
                .success();
        assert!(ok, "newfs");
        let (ino, want, bytes) = if soft {
            let (fs, mut sum) = soft_fs(&img);
            let (ino, want) = grow_and_cut(&fs, &mut sum, &|| fs.io().write_data_out(usize::MAX));
            do_work(&fs, &mut sum);
            settle(&fs, &mut sum);
            fs.write_summary(&(), &mut sum, true, 1_760_000_100)
                .unwrap();
            let bytes = fs.io().disk.borrow().clone();
            (ino, want, bytes)
        } else {
            let fs = Ufs::open(&(), Mem(RefCell::new(std::fs::read(&img).unwrap())), 512).unwrap();
            let mut sum = fs.load_summary(&()).unwrap();
            let (ino, want) = grow_and_cut(&fs, &mut sum, &|| {});
            fs.write_summary(&(), &mut sum, true, 1_760_000_100)
                .unwrap();
            let bytes = fs.io().0.borrow().clone();
            (ino, want, bytes)
        };
        let back = Ufs::open(&(), &bytes[..], 512).unwrap();
        let ip = back.inode(&(), ino).unwrap();
        let mut got = alloc::vec![0u8; want.len()];
        assert_eq!(back.read(&(), &ip, 0, &mut got).unwrap(), want.len());
        assert!(got == want, "contents on the disk (soft updates: {soft})");
        std::fs::write(&img, &bytes).unwrap();
        fsck_clean(&img);
    }
    let _ = std::fs::remove_dir_all(&work);
}

/// Images made with `newfs -U` to start from without FreeBSD's tools:
/// `UFS_SOFT_IMG` (paths separated by commas), or what `fetch-base` left
/// in `build/dist`.
fn local_images() -> Vec<std::path::PathBuf> {
    if let Ok(v) = std::env::var("UFS_SOFT_IMG") {
        return v.split(',').map(std::path::PathBuf::from).collect();
    }
    let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build/dist");
    let mut out = Vec::new();
    for rel in std::fs::read_dir(&dist).into_iter().flatten().flatten() {
        let p = rel.path().join("amd64/soft-test.img");
        if p.exists() {
            out.push(p);
        }
    }
    if out.is_empty() {
        out.extend(emptied_image());
    }
    out
}

/// Where no `soft-test.img` is: the UFS2 test disk `fetch-base` left
/// (`ufs-test.img`, which `makefs` filled), with everything in it removed
/// through the engine, without soft updates. Kept in the temporary
/// directory, and made again when the test disk is newer.
fn emptied_image() -> Option<std::path::PathBuf> {
    static MADE: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    MADE.get_or_init(|| {
        let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build/dist");
        let src = std::fs::read_dir(&dist)
            .into_iter()
            .flatten()
            .flatten()
            .map(|r| r.path().join("amd64/ufs-test.img"))
            .find(|p| p.exists())?;
        let img = std::env::temp_dir().join("abyss-ufs-emptied.img");
        let age = |p: &std::path::Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        if age(&img).zip(age(&src)).is_some_and(|(made, of)| made > of) {
            return Some(img);
        }
        let fs = Ufs::open(&(), Mem(RefCell::new(std::fs::read(src).ok()?)), 512).ok()?;
        let mut sum = fs.load_summary(&()).ok()?;
        let mut root = fs.inode(&(), ROOT_INO).ok()?;
        empty_dir(&fs, &mut sum, ROOT_INO, &mut root);
        fs.write_inode(&(), ROOT_INO, &root, Mode::Sync).ok()?;
        fs.write_summary(&(), &mut sum, true, 1_760_000_000).ok()?;
        let bytes = fs.io().0.borrow().clone();
        check_crash(&bytes).unwrap();
        // Whole or not at all: other test runs may be reading it.
        let tmp = img.with_extension(std::format!("{}.tmp", std::process::id()));
        std::fs::write(&tmp, bytes).ok()?;
        std::fs::rename(&tmp, &img).ok()?;
        Some(img)
    })
    .clone()
}

/// Removes everything in directory `dir` (without soft updates).
fn empty_dir(fs: &Ufs<Mem>, sum: &mut Summary, dir: u32, dip: &mut Inode) {
    let now = Timestamp {
        sec: 1_760_000_000,
        nsec: 0,
    };
    let r = Reserve::Allowed;
    for (name, ino) in fs.names(&(), dip).unwrap() {
        let mut ip = fs.inode(&(), ino).unwrap();
        if ip.kind() == Kind::Directory {
            empty_dir(fs, sum, ino, &mut ip);
            fs.rmdir(&(), sum, dir, dip, &name, ino, &mut ip, now)
                .unwrap();
            fs.free_inode(&(), sum, ino, &mut ip, r).unwrap();
        } else {
            fs.unlink(&(), dir, dip, &name, ino, &mut ip, now).unwrap();
            if ip.nlink == 0 {
                fs.free_inode(&(), sum, ino, &mut ip, r).unwrap();
            }
        }
    }
}

/// The crash checks here, on images FreeBSD made, where its tools are not.
#[test]
fn crashes_pass_the_local_checks() {
    let images = local_images();
    if images.is_empty() {
        std::eprintln!("no soft-test.img (run fetch-base): skipped");
        return;
    }
    let work = scratch("soft-local");
    for (k, src) in images.iter().enumerate() {
        // `UFS_SOFT_SEEDS` runs more.
        let seeds: u64 = std::env::var("UFS_SOFT_SEEDS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3);
        // `UFS_SOFT_SEED` runs one.
        let only: Option<u64> = std::env::var("UFS_SOFT_SEED")
            .ok()
            .and_then(|v| v.parse().ok());
        for seed in (0..seeds)
            .map(|i| 3 + 2 * i)
            .filter(|s| only.is_none_or(|o| o == *s))
        {
            let img = work.join(std::format!("l-{k}-{seed}.img"));
            std::fs::copy(src, &img).unwrap();
            let (fs, mut sum) = soft_fs(&img);
            let mut rng = Rng(seed * 104_729);
            let mut step = 0;
            let mut crashes = 0;
            let model = workload(&fs, &mut sum, seed, 2000, &mut |fs, sum, _| {
                step += 1;
                let io = fs.io();
                if rng.below(2) == 0 {
                    io.write_some(&mut rng);
                }
                if rng.below(3) == 0 {
                    do_work(fs, sum);
                }
                if rng.below(20) == 0 {
                    match io.check_crashes() {
                        Ok(n) => crashes += n,
                        Err(e) => {
                            if std::env::var("UFS_LOG").is_ok() {
                                std::eprintln!("{}", io.deps.borrow().describe());
                            }
                            panic!(
                                "{} seed {seed}: crash after step {step}, {e}",
                                src.display()
                            );
                        }
                    }
                }
            });
            settle(&fs, &mut sum);
            fs.write_summary(&(), &mut sum, true, 1_760_000_100)
                .unwrap();
            let bytes = fs.io().disk.borrow().clone();
            check_crash(&bytes).unwrap();
            let back = Ufs::open(&(), &bytes[..], 512).unwrap();
            check_model(&back, &model, ROOT_INO);
            let _ = std::fs::remove_file(&img);
            std::eprintln!(
                "{} seed {seed}: {} nodes, {crashes} crash images checked",
                src.display(),
                model.nodes.len()
            );
        }
    }
    let _ = std::fs::remove_dir_all(&work);
}

/// `fsync` as the kernel does it: file data out (here all of it), then the
/// buffers the tracker says the file needs, until none.
fn fsync(fs: &Ufs<Sim>, ino: u32) {
    let io = fs.io();
    io.write_data_out(usize::MAX);
    for _ in 0..32 {
        let need = io.deps.borrow().needs(ino);
        if need.is_empty() {
            return;
        }
        for at in need {
            io.write_buf(at);
        }
    }
    panic!(
        "fsync of {ino} never finished:\n{}",
        io.deps.borrow().describe()
    );
}

/// After `fsync`, a crash keeps the file: its name in its directory and
/// its contents, whatever else is lost.
#[test]
fn fsync_makes_a_file_durable() {
    let images = local_images();
    if images.is_empty() {
        std::eprintln!("no soft-test.img (run fetch-base): skipped");
        return;
    }
    let work = scratch("soft-fsync");
    for (k, src) in images.iter().enumerate() {
        for seed in [11u64, 13, 17, 19] {
            let img = work.join(std::format!("f-{k}-{seed}.img"));
            std::fs::copy(src, &img).unwrap();
            let (fs, mut sum) = soft_fs(&img);
            let mut rng = Rng(seed * 31_337);
            let mut checked = 0;
            let mut step = 0;
            workload(&fs, &mut sum, seed, 1500, &mut |fs, sum, model| {
                step += 1;
                if rng.below(2) == 0 {
                    fs.io().write_some(&mut rng);
                }
                if rng.below(3) == 0 {
                    do_work(fs, sum);
                }
                if rng.below(25) != 0 {
                    return;
                }
                let files: Vec<(u32, Vec<u8>, u32)> = model
                    .nodes
                    .iter()
                    .filter_map(|(d, n)| match n {
                        MNode::Dir(m) => Some(m.iter().map(|(name, i)| (*d, name.clone(), *i))),
                        _ => None,
                    })
                    .flatten()
                    .filter(|(_, _, i)| matches!(model.nodes.get(i), Some(MNode::File(_))))
                    .collect();
                if files.is_empty() {
                    return;
                }
                let (dir, name, ino) = files[rng.below(files.len() as u64) as usize].clone();
                fsync(fs, ino);
                let image = fs.io().disk.borrow().clone();
                let back = Ufs::open(&(), &image[..], 512).unwrap();
                let dip = back.inode(&(), dir).unwrap();
                assert_eq!(
                    back.lookup(&(), &dip, &name).unwrap(),
                    Some(ino),
                    "seed {seed} step {step}: {:?} in {dir} after fsync",
                    String::from_utf8_lossy(&name)
                );
                let ip = back.inode(&(), ino).unwrap();
                let Some(MNode::File(want)) = model.nodes.get(&ino) else {
                    unreachable!();
                };
                assert_eq!(
                    ip.size,
                    want.len() as u64,
                    "seed {seed} step {step}: size of {ino}"
                );
                let mut got = alloc::vec![0u8; want.len()];
                back.read(&(), &ip, 0, &mut got).unwrap();
                assert!(
                    got == *want,
                    "seed {seed} step {step}: contents of {ino} after fsync"
                );
                checked += 1;
            });
            settle(&fs, &mut sum);
            let _ = std::fs::remove_file(&img);
            std::eprintln!(
                "{} seed {seed}: {checked} files durable after fsync",
                src.display()
            );
        }
    }
    let _ = std::fs::remove_dir_all(&work);
}

/// A superblock to track dependencies against: of a new image (on
/// FreeBSD), or of one `fetch-base` left.
fn a_superblock() -> Option<Superblock> {
    let img = if have_makefs() {
        let work = scratch("soft-sb");
        let img = work.join("sb.img");
        let ok = Command::new("truncate")
            .args(["-s", "16m"])
            .arg(&img)
            .status()
            .is_ok_and(|s| s.success())
            && Command::new("newfs")
                .args(["-U"])
                .arg(&img)
                .stdout(std::process::Stdio::null())
                .status()
                .is_ok_and(|s| s.success());
        ok.then_some(img)
    } else {
        let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build/dist");
        std::fs::read_dir(&dist)
            .into_iter()
            .flatten()
            .flatten()
            .map(|r| r.path().join("amd64/ufs-test.img"))
            .find(|p| p.exists())
    }?;
    let bytes = std::fs::read(img).ok()?;
    Superblock::find(&(), &bytes[..], 512).ok()
}

/// A file's last fragments grown and moved again and again before any of
/// it reaches the disk, as fsx does through the page cache: the data is
/// written only where it ends up, and the pointers still settle, freeing
/// every fragment moved away from (FreeBSD's `allocdirect_merge`).
#[test]
fn fragments_moved_before_their_data_is_written_settle() {
    let Some(sb) = a_superblock() else {
        std::eprintln!("no image (not FreeBSD, no fetch-base): skipped");
        return;
    };
    let f = u64::from(sb.fsize);
    let ino = 4;
    let slot = softdep::Slot::Inode { ino, index: 0 };
    let cg_at = sb.cgtod(0) * f;
    let (b1, b2, b3) = (sb.cgdmin(0) + 64, sb.cgdmin(0) + 128, sb.cgdmin(0) + 192);
    let mut d = Deps::new(&sb);
    // Two fragments; moved to three; grown in place to four; moved to five.
    d.new_block(b1, 0, 2 * f, cg_at, ino);
    d.place(b1, slot, 0, 0, None);
    d.inode_copied(ino);
    d.new_block(b2, 0, 3 * f, cg_at, ino);
    d.place(b2, slot, b1, 2 * f, Some((b1, 2 * f)));
    d.inode_copied(ino);
    d.new_block(b2, 3 * f, 4 * f, cg_at, ino);
    d.place(b2, slot, b2, 3 * f, None);
    d.inode_copied(ino);
    d.new_block(b3, 0, 5 * f, cg_at, ino);
    d.place(b3, slot, b2, 4 * f, Some((b2, 4 * f)));
    d.inode_copied(ino);
    d.data_written(b3 * f, 5 * f);
    let group = alloc::vec![0u8; idx64(u64::from(sb.bsize))];
    assert!(d.write_start(cg_at, &group).is_none());
    d.write_done(cg_at, true);
    let (at, within) = sb.inode_location(ino);
    let mut block = alloc::vec![0u8; idx64(u64::from(sb.bsize))];
    inode::raw::set_pointer(&mut block[within..within + inode::SIZE], 0, b3);
    // Nothing is rolled back: the pointer to the last fragments goes out.
    assert!(d.write_start(at, &block).is_none(), "{}", d.describe());
    d.write_done(at, true);
    assert_eq!(d.pending_free(), 6 * f);
    let mut freed: Vec<(u64, u64)> = d
        .take_work()
        .into_iter()
        .flat_map(|w| match w {
            softdep::Work::Free(v) => v,
            w => panic!("{w:?}"),
        })
        .collect();
    freed.sort_unstable();
    assert_eq!(freed, [(b1, 2 * f), (b2, 4 * f)]);
    assert!(d.is_idle(), "{}", d.describe());
}

// ---- What never reached the disk goes at once ----

const NOW: Timestamp = Timestamp {
    sec: 1_760_000_000,
    nsec: 0,
};

/// An image to test on (the first of [`local_images`]), read once.
fn an_image() -> Option<Vec<u8>> {
    static IMAGE: std::sync::OnceLock<Option<Vec<u8>>> = std::sync::OnceLock::new();
    IMAGE
        .get_or_init(|| std::fs::read(local_images().first()?).ok())
        .clone()
}

/// A file system with soft updates on image `bytes`, as [`soft_fs`].
fn soft_on(bytes: Vec<u8>) -> (Ufs<Sim>, Summary) {
    let mut fs = Ufs::open(&(), Sim::new(bytes), 512).unwrap();
    assert!(fs.use_soft_updates(&()));
    let mut sum = fs.load_summary(&()).unwrap();
    fs.write_summary(&(), &mut sum, false, 1_760_000_000)
        .unwrap();
    (fs, sum)
}

/// Every image a crash now may leave passes [`check_crash`].
fn crash_ok(fs: &Ufs<Sim>, what: &str) {
    if let Err(e) = fs.io().check_crashes() {
        panic!("a crash {what}, {e}\n{}", fs.io().deps.borrow().describe());
    }
}

/// The buffers of inode `ino`'s group, of its block, and of the root
/// directory's first block.
fn buffers_of(fs: &Ufs<Sim>, ino: u32) -> (u64, u64, u64) {
    let sb = fs.superblock();
    let f = u64::from(sb.fsize);
    let root = fs.inode(&(), ROOT_INO).unwrap();
    (
        sb.cgtod(ino / sb.ipg) * f,
        sb.inode_location(ino).0,
        fs.bmap(&(), &root, 0).unwrap() * f,
    )
}

/// Makes regular file `name` in the root.
fn make(fs: &Ufs<Sim>, sum: &mut Summary, name: &[u8]) -> (u32, Inode) {
    let mut root = fs.inode(&(), ROOT_INO).unwrap();
    let who = maker(&mut Rng(1), Reserve::Allowed);
    fs.create(
        &(),
        sum,
        ROOT_INO,
        &mut root,
        name,
        ops::NewKind::Regular,
        &who,
    )
    .unwrap()
}

/// Writes `len` bytes into file `ino` from its start (file data, queued).
fn fill(fs: &Ufs<Sim>, sum: &mut Summary, ino: u32, ip: &mut Inode, len: usize) {
    let data: Vec<u8> = (0..len).map(|i| (i % 249) as u8).collect();
    fs.write_data_range(&(), sum, ino, ip, 0, &data, Reserve::Allowed)
        .unwrap();
    fs.write_inode(&(), ino, ip, Mode::Delayed).unwrap();
}

/// Removes `name` (inode `ino`) from the root, as the kernel does with
/// nothing else using the file: at no links left it is freed now.
/// Returns whether it was.
fn remove(fs: &Ufs<Sim>, sum: &mut Summary, name: &[u8], ino: u32, ip: &mut Inode) -> bool {
    let mut root = fs.inode(&(), ROOT_INO).unwrap();
    fs.unlink(&(), ROOT_INO, &mut root, name, ino, ip, NOW)
        .unwrap();
    if ip.nlink != 0 {
        return false;
    }
    fs.free_inode(&(), sum, ino, ip, Reserve::Allowed).unwrap();
    true
}

/// The tracker holds nothing, no work is due, and nothing waits to be
/// freed.
fn nothing_held(fs: &Ufs<Sim>, what: &str) {
    let d = fs.io().deps.borrow();
    assert!(
        d.is_idle() && d.is_empty() && d.most() == 0 && d.pending_free() == 0,
        "{what}: the tracker still holds\n{}",
        d.describe()
    );
}

/// The groups' own counts add up to the summary's.
fn groups_agree(fs: &Ufs<Sim>, sum: &Summary, what: &str) {
    let counted = fs.summary_from_groups(&()).unwrap();
    assert_eq!(counted.totals, sum.totals, "{what}: the groups' counts");
}

/// Settles, and checks that the disk then holds what `sum` says and
/// passes [`check_crash`].
fn settled(fs: &Ufs<Sim>, sum: &mut Summary, what: &str) {
    settle(fs, sum);
    groups_agree(fs, sum, what);
    crash_ok(fs, what);
}

/// FreeBSD's `softdep_setup_remove`, `softdep_setup_freeblocks`, and
/// `softdep_freefile` on a file nothing of which reached the disk: the
/// link drops with the entry, and the blocks and the inode are free again
/// at once, with nothing left for the work; the next file made takes the
/// same inode (`ffs_freefile` keeps the group's rotor at it).
#[test]
fn a_file_removed_before_anything_of_it_is_written_goes_at_once() {
    let Some(image) = an_image() else {
        std::eprintln!("no soft-test.img or ufs-test.img (run fetch-base): skipped");
        return;
    };
    let bsize = Superblock::find(&(), &image[..], 512).unwrap().bsize as usize;
    // No data; fragments; whole blocks; an indirect block.
    for len in [0, 100, bsize + 100, 14 * bsize + 100] {
        let (fs, mut sum) = soft_on(image.clone());
        let start = sum.totals;
        let (ino, mut ip) = make(&fs, &mut sum, b"a");
        if len != 0 {
            fill(&fs, &mut sum, ino, &mut ip, len);
            assert_ne!(
                sum.totals.nbfree + sum.totals.nffree,
                start.nbfree + start.nffree
            );
        }
        assert!(fs.io().deps.borrow().unwritten(ino));
        assert!(remove(&fs, &mut sum, b"a", ino, &mut ip), "{len}: freed");
        assert_eq!((ip.nlink, ip.effnlink, ip.mode, ip.blocks), (0, 0, 0, 0));
        assert_eq!(sum.totals, start, "{len}: all is free again");
        nothing_held(&fs, "after the removal");
        assert!(
            fs.io().data.borrow().is_empty(),
            "{len}: its data is dropped"
        );
        groups_agree(&fs, &sum, "after the removal");
        crash_ok(&fs, "after the removal");
        // The same inode again, and its blocks.
        let (again, mut ip) = make(&fs, &mut sum, b"b");
        assert_eq!(again, ino, "{len}: the inode is taken again");
        fill(&fs, &mut sum, again, &mut ip, 100);
        crash_ok(&fs, "after making the next file");
        settled(&fs, &mut sum, "at the end");
        let disk = fs.io().disk.borrow().clone();
        let back = Ufs::open(&(), &disk[..], 512).unwrap();
        let root = back.inode(&(), ROOT_INO).unwrap();
        assert_eq!(back.lookup(&(), &root, b"a").unwrap(), None);
        assert_eq!(back.lookup(&(), &root, b"b").unwrap(), Some(ino));
        let on_disk = back.inode(&(), ino).unwrap();
        assert_eq!((on_disk.nlink, on_disk.size), (1, 100));
    }
}

/// Once the group claiming the inode is on the disk the inode is no
/// longer known to be zero there (FreeBSD's `DEPCOMPLETE`), and once the
/// entry is there its removal must be too: the frees then wait, in order.
#[test]
fn a_file_that_reached_the_disk_is_freed_in_order() {
    let Some(image) = an_image() else {
        std::eprintln!("no soft-test.img or ufs-test.img (run fetch-base): skipped");
        return;
    };
    // The group written; the group and the inode; everything. With data,
    // and with none (nothing but the inode itself to wait for).
    for (wrote, len) in (0..3).flat_map(|w| [(w, 5000), (w, 0)]) {
        let (fs, mut sum) = soft_on(image.clone());
        let start = sum.totals;
        let (ino, mut ip) = make(&fs, &mut sum, b"a");
        if len != 0 {
            fill(&fs, &mut sum, ino, &mut ip, len);
        }
        let (cg, block, _) = buffers_of(&fs, ino);
        let io = fs.io();
        match wrote {
            0 => io.write_buf(cg),
            1 => {
                io.write_buf(cg);
                io.write_data_out(usize::MAX);
                io.write_buf(block);
            }
            _ => settle(&fs, &mut sum),
        }
        crash_ok(&fs, "before the removal");
        assert!(!io.deps.borrow().unwritten(ino), "{wrote}: written");
        let freed = remove(&fs, &mut sum, b"a", ino, &mut ip);
        // The entry on the disk holds its link until its removal is.
        assert_eq!(freed, wrote < 2, "{wrote}: the link dropped at once");
        assert_eq!(ip.effnlink, 0);
        // Nothing is free yet: the inode's free waits for its block, and
        // so do its blocks'.
        assert_eq!(sum.totals.nifree, start.nifree - 1, "{wrote}");
        assert!(!io.deps.borrow().is_idle(), "{wrote}: work is left");
        if len != 0 {
            assert_ne!(
                sum.totals.nbfree + sum.totals.nffree,
                start.nbfree + start.nffree
            );
        }
        if freed && len != 0 {
            assert_ne!(io.deps.borrow().pending_free(), 0, "{wrote}");
        }
        crash_ok(&fs, "after the removal");
        // Each write on the way is a crash point.
        for _ in 0..200 {
            if io.idle() {
                break;
            }
            io.write_data_out(usize::MAX);
            let dirty: Vec<u64> = io.dirty.borrow().iter().copied().collect();
            for at in dirty {
                io.write_buf(at);
                crash_ok(&fs, "while settling");
            }
            do_work(&fs, &mut sum);
            crash_ok(&fs, "after the work");
        }
        assert_eq!(sum.totals, start, "{wrote}: all is free in the end");
        nothing_held(&fs, "in the end");
        settled(&fs, &mut sum, "in the end");
    }
}

/// A renamed file: with nothing on the disk, both entries go with their
/// links and the file at once; with the old name on the disk, the new
/// one's link drops at once but the old one's waits for its removal, and
/// the inode with it (FreeBSD's `softdep_setup_remove`: only the entry
/// that never reached the disk is `COMPLETE`).
#[test]
fn a_renamed_file_goes_at_once_only_if_no_name_of_it_is_on_the_disk() {
    let Some(image) = an_image() else {
        std::eprintln!("no soft-test.img or ufs-test.img (run fetch-base): skipped");
        return;
    };
    for old_on_disk in [false, true] {
        let (fs, mut sum) = soft_on(image.clone());
        let start = sum.totals;
        let (ino, mut ip) = make(&fs, &mut sum, b"a");
        fill(&fs, &mut sum, ino, &mut ip, 5000);
        if old_on_disk {
            settle(&fs, &mut sum);
        }
        let mut root = fs.inode(&(), ROOT_INO).unwrap();
        fs.rename(
            &(),
            &mut sum,
            ROOT_INO,
            &mut root,
            b"a",
            ino,
            &mut ip,
            None,
            b"b",
            None,
            NOW,
            Reserve::Allowed,
        )
        .unwrap();
        // The old entry's link is gone already if it never was on the
        // disk.
        assert_eq!(
            (ip.effnlink, ip.nlink),
            (1, if old_on_disk { 2 } else { 1 })
        );
        crash_ok(&fs, "after the rename");
        let freed = remove(&fs, &mut sum, b"b", ino, &mut ip);
        assert_eq!(freed, !old_on_disk);
        assert_eq!((ip.effnlink, ip.nlink), (0, u16::from(old_on_disk)));
        crash_ok(&fs, "after the removal");
        if old_on_disk {
            // The disk still names it `a`: nothing of it is free.
            assert_eq!(sum.totals.nifree, start.nifree - 1);
            let disk = fs.io().disk.borrow().clone();
            let back = Ufs::open(&(), &disk[..], 512).unwrap();
            let root = back.inode(&(), ROOT_INO).unwrap();
            assert_eq!(back.lookup(&(), &root, b"a").unwrap(), Some(ino));
            assert_ne!(back.inode(&(), ino).unwrap().mode, 0);
            // The removal written, the work drops the link and frees it.
            let (_, _, dir) = buffers_of(&fs, ino);
            fs.io().write_buf(dir);
            crash_ok(&fs, "with the removal written");
            do_work(&fs, &mut sum);
            crash_ok(&fs, "after the work");
            assert_eq!(fs.inode(&(), ino).unwrap().mode, 0);
        } else {
            assert_eq!(sum.totals, start);
            nothing_held(&fs, "after the removal");
        }
        settled(&fs, &mut sum, "in the end");
        assert_eq!(sum.totals, start, "all is free in the end");
    }
}

/// A file with two names: removing one that never reached the disk drops
/// its link at once and no more; the file stays, whatever the other name
/// is (written or not), and goes with it.
#[test]
fn one_name_of_two_removed_leaves_the_file() {
    let Some(image) = an_image() else {
        std::eprintln!("no soft-test.img or ufs-test.img (run fetch-base): skipped");
        return;
    };
    for first_on_disk in [false, true] {
        let (fs, mut sum) = soft_on(image.clone());
        let start = sum.totals;
        let (ino, mut ip) = make(&fs, &mut sum, b"a");
        fill(&fs, &mut sum, ino, &mut ip, 5000);
        if first_on_disk {
            settle(&fs, &mut sum);
        }
        let mut root = fs.inode(&(), ROOT_INO).unwrap();
        fs.link(
            &(),
            &mut sum,
            ROOT_INO,
            &mut root,
            b"b",
            ino,
            &mut ip,
            NOW,
            Reserve::Allowed,
        )
        .unwrap();
        assert_eq!((ip.effnlink, ip.nlink), (2, 2));
        crash_ok(&fs, "after the link");
        // The new name goes: it never reached the disk.
        assert!(!remove(&fs, &mut sum, b"b", ino, &mut ip));
        assert_eq!((ip.effnlink, ip.nlink), (1, 1));
        assert_ne!(ip.mode, 0);
        crash_ok(&fs, "after removing the new name");
        settled(&fs, &mut sum, "with one name left");
        let disk = fs.io().disk.borrow().clone();
        let back = Ufs::open(&(), &disk[..], 512).unwrap();
        let root = back.inode(&(), ROOT_INO).unwrap();
        assert_eq!(back.lookup(&(), &root, b"a").unwrap(), Some(ino));
        assert_eq!(back.lookup(&(), &root, b"b").unwrap(), None);
        let on_disk = back.inode(&(), ino).unwrap();
        assert_eq!((on_disk.nlink, on_disk.size), (1, 5000));
        let mut got = alloc::vec![0u8; 5000];
        back.read(&(), &on_disk, 0, &mut got).unwrap();
        assert!(got.iter().enumerate().all(|(i, b)| *b == (i % 249) as u8));
        // The last name is on the disk now: its removal waits.
        let mut ip = fs.inode(&(), ino).unwrap();
        assert!(!remove(&fs, &mut sum, b"a", ino, &mut ip));
        assert_eq!((ip.effnlink, ip.nlink), (0, 1));
        crash_ok(&fs, "after removing the last name");
        settled(&fs, &mut sum, "in the end");
        assert_eq!(sum.totals, start, "all is free in the end");
    }
}

/// A file unlinked while open: its link drops at once if its name never
/// reached the disk, but it is whole and usable until its last close
/// frees it (FreeBSD's `ufs_inactive`), at once if the inode still never
/// reached the disk and in order if it did meanwhile. The kernel's way is
/// used: [`Ufs::release_inode`], then the group's free once nothing in
/// core holds the inode.
#[test]
fn an_unlinked_file_still_open_goes_at_its_last_close() {
    let Some(image) = an_image() else {
        std::eprintln!("no soft-test.img or ufs-test.img (run fetch-base): skipped");
        return;
    };
    for written_meanwhile in [false, true] {
        let (fs, mut sum) = soft_on(image.clone());
        let start = sum.totals;
        let (ino, mut ip) = make(&fs, &mut sum, b"a");
        fill(&fs, &mut sum, ino, &mut ip, 3000);
        let mut root = fs.inode(&(), ROOT_INO).unwrap();
        fs.unlink(&(), ROOT_INO, &mut root, b"a", ino, &mut ip, NOW)
            .unwrap();
        assert_eq!((ip.effnlink, ip.nlink), (0, 0));
        // Open still: nothing is freed, and it can be written.
        assert_ne!(ip.mode, 0);
        assert_eq!(sum.totals.nifree, start.nifree - 1);
        fill(&fs, &mut sum, ino, &mut ip, 3 * 4096 + 7);
        crash_ok(&fs, "while unlinked and open");
        let mut got = alloc::vec![0u8; 3000];
        fs.io().write_data_out(usize::MAX);
        assert_eq!(fs.read(&(), &ip, 0, &mut got).unwrap(), 3000);
        assert!(got.iter().enumerate().all(|(i, b)| *b == (i % 249) as u8));
        if written_meanwhile {
            let (cg, block, _) = buffers_of(&fs, ino);
            fs.io().write_buf(cg);
            fs.io().write_buf(block);
            crash_ok(&fs, "with the unlinked inode on the disk");
        }
        // The last close.
        let freed = fs
            .release_inode(&(), &mut sum, ino, &mut ip, Reserve::Allowed)
            .unwrap();
        assert_eq!(freed.is_some(), !written_meanwhile);
        crash_ok(&fs, "after the last close");
        if let Some(mode) = freed {
            // Its blocks are free and the tracker has forgotten it; the
            // group still holds the inode, until the caller lets it go.
            assert_eq!(sum.totals.nifree, start.nifree - 1);
            assert_eq!(
                (sum.totals.nbfree, sum.totals.nffree),
                (start.nbfree, start.nffree)
            );
            nothing_held(&fs, "after the last close");
            fs.freefile(&(), &mut sum, ino, mode).unwrap();
            assert_eq!(sum.totals, start);
            crash_ok(&fs, "after the group's free");
        } else {
            assert!(!fs.io().deps.borrow().is_idle());
        }
        settled(&fs, &mut sum, "in the end");
        assert_eq!(sum.totals, start, "all is free in the end");
    }
}

/// A file made, written, removed, and another made in its place, with a
/// crash at every write: for each point between the operations, and each
/// order and number of the buffers and data then written there, every
/// image on the way (and on the way to settling afterwards) passes
/// [`check_crash`], and in the end the disk holds the second file alone.
#[test]
fn a_crash_at_each_write_of_a_create_and_remove_is_consistent() {
    let Some(image) = an_image() else {
        std::eprintln!("no soft-test.img or ufs-test.img (run fetch-base): skipped");
        return;
    };
    // What is waiting to go out: file data (`None`), then each buffer.
    let waiting = |fs: &Ufs<Sim>| {
        let io = fs.io();
        let mut v: Vec<Option<u64>> = Vec::new();
        if !io.data.borrow().is_empty() {
            v.push(None);
        }
        v.extend(io.dirty.borrow().iter().copied().map(Some));
        v
    };
    let write = |fs: &Ufs<Sim>, what: Option<u64>| match what {
        None => fs.io().write_data_out(usize::MAX),
        Some(at) => fs.io().write_buf(at),
    };
    // The operations, each leaving the file system as the kernel would.
    type State = (u32, Inode);
    let step = |fs: &Ufs<Sim>, sum: &mut Summary, n: usize, st: &mut Option<State>| match n {
        0 => {
            let (ino, mut ip) = make(fs, sum, b"a");
            fill(fs, sum, ino, &mut ip, 9000);
            *st = Some((ino, ip));
        }
        1 => {
            let (ino, mut ip) = st.take().unwrap();
            // Read again, as the work may have written it since.
            let mut now = fs.inode(&(), ino).unwrap();
            now.effnlink = ip.effnlink;
            ip = now;
            remove(fs, sum, b"a", ino, &mut ip);
        }
        _ => {
            let (ino, mut ip) = make(fs, sum, b"b");
            fill(fs, sum, ino, &mut ip, 700);
        }
    };
    let mut runs = 0;
    for point in 0..=3 {
        // How many things wait at this point, with nothing written before.
        let n = {
            let (fs, mut sum) = soft_on(image.clone());
            let mut st = None;
            for k in 0..point {
                step(&fs, &mut sum, k, &mut st);
            }
            waiting(&fs).len()
        };
        for first in 0..n.max(1) {
            for count in 0..=n {
                if n == 0 && count > 0 {
                    continue;
                }
                let (fs, mut sum) = soft_on(image.clone());
                let start = sum.totals;
                let what = std::format!("at point {point}, from {first}, {count} written");
                let mut st = None;
                for k in 0..point {
                    step(&fs, &mut sum, k, &mut st);
                    crash_ok(&fs, &what);
                }
                let mut list = waiting(&fs);
                assert_eq!(list.len(), n);
                list.rotate_left(first);
                for w in list.into_iter().take(count) {
                    write(&fs, w);
                    crash_ok(&fs, &what);
                }
                // The work, half the time.
                if (first + count) % 2 == 0 {
                    do_work(&fs, &mut sum);
                    crash_ok(&fs, &what);
                }
                for k in point..3 {
                    step(&fs, &mut sum, k, &mut st);
                    crash_ok(&fs, &what);
                }
                // To the end, a write at a time.
                for _ in 0..100 {
                    let list = waiting(&fs);
                    if list.is_empty() && fs.io().deps.borrow().is_idle() {
                        break;
                    }
                    for w in list {
                        write(&fs, w);
                        crash_ok(&fs, &what);
                    }
                    do_work(&fs, &mut sum);
                    crash_ok(&fs, &what);
                }
                settled(&fs, &mut sum, &what);
                let disk = fs.io().disk.borrow().clone();
                let back = Ufs::open(&(), &disk[..], 512).unwrap();
                let root = back.inode(&(), ROOT_INO).unwrap();
                assert_eq!(back.lookup(&(), &root, b"a").unwrap(), None, "{what}");
                let b = back.lookup(&(), &root, b"b").unwrap().expect("b");
                let on_disk = back.inode(&(), b).unwrap();
                assert_eq!((on_disk.nlink, on_disk.size), (1, 700), "{what}");
                // One inode and one fragment or so are in use: the first
                // file's are all back.
                assert_eq!(sum.totals.nifree, start.nifree - 1, "{what}");
                let frags =
                    |t: &sb::Totals| t.nbfree * i64::from(back.superblock().frag) + t.nffree;
                let used = frags(&start) - frags(&sum.totals);
                let one = i64::from(
                    back.superblock()
                        .numfrags(700u64.next_multiple_of(u64::from(back.superblock().fsize))),
                );
                assert_eq!(used, one, "{what}: fragments in use");
                runs += 1;
            }
        }
    }
    std::eprintln!("{runs} schedules of writes around a create and a remove checked");
}

/// Writes under way when a file goes (the kernel's cache writes a buffer
/// from a copy, and the buffer changes meanwhile). A directory block on
/// its way out with the entry in it puts the entry on the disk: the
/// removal is not of an entry that never got there, and the link waits
/// for the removal to be written.
#[test]
fn an_entry_going_out_as_it_is_removed_is_on_the_disk() {
    let Some(image) = an_image() else {
        std::eprintln!("no soft-test.img or ufs-test.img (run fetch-base): skipped");
        return;
    };
    let (fs, mut sum) = soft_on(image);
    let start = sum.totals;
    let (ino, mut ip) = make(&fs, &mut sum, b"a");
    let (cg, block, dir) = buffers_of(&fs, ino);
    let io = fs.io();
    // The inode is on the disk, so the entry may go; its block's write
    // begins.
    io.write_buf(cg);
    io.write_buf(block);
    io.begin(dir);
    assert!(!remove(&fs, &mut sum, b"a", ino, &mut ip), "the link stays");
    assert_eq!((ip.effnlink, ip.nlink), (0, 1));
    crash_ok(&fs, "with the entry going out");
    io.land(dir);
    // The entry is on the disk, naming an inode that must stay.
    let disk = io.disk.borrow().clone();
    let back = Ufs::open(&(), &disk[..], 512).unwrap();
    let root = back.inode(&(), ROOT_INO).unwrap();
    assert_eq!(back.lookup(&(), &root, b"a").unwrap(), Some(ino));
    do_work(&fs, &mut sum);
    io.write_buf(block);
    crash_ok(&fs, "with the entry on the disk");
    assert_ne!(fs.inode(&(), ino).unwrap().mode, 0);
    // Its removal written, the link drops and the file goes.
    io.write_buf(dir);
    do_work(&fs, &mut sum);
    crash_ok(&fs, "with the removal on the disk");
    assert_eq!(fs.inode(&(), ino).unwrap().mode, 0);
    settled(&fs, &mut sum, "in the end");
    assert_eq!(sum.totals, start);
}

/// Writes of the group and of the inode's block under way when a file
/// that never reached the disk goes: the inode's block carries it zeroed
/// and the group claims what no inode on the disk holds, which fsck takes
/// back; the file goes at once all the same, the next one takes its
/// inode, and when the writes land that one is still unwritten (the group
/// that went out is older than it).
#[test]
fn a_file_goes_at_once_under_writes_of_its_group_and_inode() {
    let Some(image) = an_image() else {
        std::eprintln!("no soft-test.img or ufs-test.img (run fetch-base): skipped");
        return;
    };
    let (fs, mut sum) = soft_on(image);
    let start = sum.totals;
    let (ino, mut ip) = make(&fs, &mut sum, b"a");
    fill(&fs, &mut sum, ino, &mut ip, 9000);
    let (cg, block, dir) = buffers_of(&fs, ino);
    let io = fs.io();
    io.begin(cg);
    io.begin(block);
    io.begin(dir);
    assert!(io.deps.borrow().unwritten(ino));
    assert!(remove(&fs, &mut sum, b"a", ino, &mut ip));
    assert_eq!(sum.totals, start);
    assert!(io.deps.borrow().is_empty());
    crash_ok(&fs, "after the removal, the writes under way");
    let (again, mut ip) = make(&fs, &mut sum, b"b");
    assert_eq!(again, ino);
    fill(&fs, &mut sum, again, &mut ip, 9000);
    crash_ok(&fs, "after making the next file, the writes under way");
    io.land(block);
    crash_ok(&fs, "with the inode's block landed");
    io.land(cg);
    crash_ok(&fs, "with the group landed");
    io.land(dir);
    crash_ok(&fs, "with the directory landed");
    assert!(io.deps.borrow().unwritten(ino), "the new file is unwritten");
    // The new file's inode goes out zeroed still.
    io.write_data_out(usize::MAX);
    io.write_buf(block);
    io.write_buf(dir);
    crash_ok(&fs, "with the new file's blocks written before its group");
    let disk = io.disk.borrow().clone();
    let back = Ufs::open(&(), &disk[..], 512).unwrap();
    assert_eq!(back.inode(&(), ino).unwrap().mode, 0);
    settled(&fs, &mut sum, "in the end");
    let disk = io.disk.borrow().clone();
    let back = Ufs::open(&(), &disk[..], 512).unwrap();
    let root = back.inode(&(), ROOT_INO).unwrap();
    assert_eq!(back.lookup(&(), &root, b"b").unwrap(), Some(ino));
    assert_eq!(back.inode(&(), ino).unwrap().size, 9000);
}

/// When the tracker forgets an inode being freed ([`Deps::free_unwritten`],
/// FreeBSD's `check_inode_unwritten`): only one still zero on the disk,
/// that nothing names, waits on, or points from, and with no link still
/// to drop.
#[test]
fn an_inode_is_forgotten_only_with_nothing_of_it_left() {
    let Some(sb) = a_superblock() else {
        std::eprintln!("no image (not FreeBSD, no fetch-base): skipped");
        return;
    };
    let f = u64::from(sb.fsize);
    let ino = 5;
    let cg_at = sb.cgtod(0) * f;
    let dir_at = (sb.cgdmin(0) + 512) * f;
    let (at, within) = sb.inode_location(ino);
    let slot = softdep::Slot::Inode { ino, index: 0 };
    let bno = sb.cgdmin(0) + 64;
    let none = softdep::Removal::default();
    let born = || {
        let mut d = Deps::new(&sb);
        d.new_inode(ino, cg_at);
        d
    };
    // Its block, the inode in it with a mode.
    let mut block = alloc::vec![0u8; idx64(u64::from(sb.bsize))];
    block[within] = 0o100;
    let zeroed = |d: &mut Deps| {
        let out = d.write_start(at, &block);
        d.write_done(at, true);
        out.is_some_and(|b| b[within] == 0)
    };

    // Never allocated here: nothing is known of it.
    let mut d = Deps::new(&sb);
    assert!(!d.unwritten(ino) && !d.free_unwritten(ino));

    // Just allocated: it goes out zeroed, and is forgotten whole.
    let mut d = born();
    assert!(d.unwritten(ino));
    assert!(zeroed(&mut d));
    assert!(d.free_unwritten(ino));
    assert!(d.is_idle() && d.is_empty() && d.most() == 0);
    assert!(!d.unwritten(ino) && !d.free_unwritten(ino));
    assert!(!zeroed(&mut d), "its block goes out as it is from now on");

    // A write of its group under way claims it; it is unwritten still.
    let mut d = born();
    let group = alloc::vec![0u8; idx64(u64::from(sb.bsize))];
    assert!(d.write_start(cg_at, &group).is_none());
    assert!(d.unwritten(ino));
    d.write_done(cg_at, true);
    // The group written: no longer (FreeBSD's `DEPCOMPLETE`).
    assert!(!d.unwritten(ino) && !d.free_unwritten(ino));

    // An entry names it, not yet written: its removal drops the link
    // now, and the entry's own wait for the inode is left behind.
    let mut d = born();
    let e = d.new_entry(ino);
    d.wait_copy(e, ino);
    d.inode_copied(ino);
    d.place_entry(e, 2, 0, dir_at, 24, (0, 0), softdep::Gone::Link, &none);
    assert!(!d.free_unwritten(ino), "named");
    assert!(d.unwritten(ino));
    let r = d.remove_entry(2, 0, dir_at, 24, ino, softdep::Gone::Link);
    assert!(r.drops_now());
    assert_eq!(d.nlink_delta(ino), 0);
    assert!(d.take_work().is_empty(), "no work for the link");
    assert!(d.free_unwritten(ino));
    assert!(d.is_idle());

    // A directory's entry removed unwritten: its links are the work's.
    let mut d = born();
    let e = d.new_entry(ino);
    d.place_entry(e, 2, 0, dir_at, 24, (0, 0), softdep::Gone::Link, &none);
    let r = d.remove_entry(2, 0, dir_at, 24, ino, softdep::Gone::Dir(2));
    assert!(!r.drops_now());
    assert_eq!(d.nlink_delta(ino), 2);
    assert!(!d.free_unwritten(ino), "links still to drop");
    assert_eq!(
        d.take_work(),
        [softdep::Work::Unlink {
            ino,
            gone: softdep::Gone::Dir(2)
        }]
    );
    // The work taken is not done yet.
    assert!(!d.free_unwritten(ino));
    d.nlink_applied(ino, 2);
    assert!(d.free_unwritten(ino));

    // An entry on the disk (nothing held of it) removed: the link waits
    // for the removal.
    let mut d = born();
    let r = d.remove_entry(2, 0, dir_at, 24, ino, softdep::Gone::Link);
    assert!(!r.drops_now());
    assert!(!d.free_unwritten(ino), "a link still to drop");

    // A pointer pending.
    let mut d = born();
    d.new_block(bno, 0, f, cg_at, ino);
    d.place(bno, slot, 0, 0, None);
    assert!(!d.free_unwritten(ino), "a pointer");
    d.cancel_slot(slot);
    assert!(d.free_unwritten(ino));

    // Work waiting for its next copy, then for that copy's write.
    let mut d = born();
    let w = d.defer(softdep::Work::Free(alloc::vec![(bno, f)]));
    d.wait_copy(w, ino);
    d.arm(w);
    assert!(!d.free_unwritten(ino), "work waits for its copy");
    d.inode_copied(ino);
    assert!(!d.free_unwritten(ino), "work waits for its write");
    // Written zeroed, nothing is on the disk yet: the work waits on.
    assert!(zeroed(&mut d));
    assert!(!d.free_unwritten(ino) && d.take_work().is_empty());
}

/// Short-lived files, as a tree extracted and removed at once makes them:
/// a few names made, written, linked, renamed, cut, and removed again
/// (some while still open), with the cache writing back now and then, so
/// most of them go before anything of them reached the disk and some with
/// part of it there or on its way. Every so often each image a crash may
/// leave is checked; in the end the disk holds what is left, and nothing
/// else is in use.
#[test]
fn short_lived_files_pass_the_local_checks() {
    let Some(image) = an_image() else {
        std::eprintln!("no soft-test.img or ufs-test.img (run fetch-base): skipped");
        return;
    };
    let r = Reserve::Allowed;
    // `UFS_SOFT_SEEDS` runs more.
    let seeds: u64 = std::env::var("UFS_SOFT_SEEDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);
    for seed in (0..seeds).map(|i| 41 + 2 * i) {
        let (fs, mut sum) = soft_on(image.clone());
        let start = sum.totals;
        let bsize = u64::from(fs.superblock().bsize);
        let mut rng = Rng(seed * 15_485_863);
        // Names and what they name; each file's contents; files unlinked
        // and still open.
        let mut names: BTreeMap<Vec<u8>, u32> = BTreeMap::new();
        let mut files: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
        let mut open: Vec<u32> = Vec::new();
        let (mut at_once, mut in_order, mut crashes) = (0, 0, 0);
        for step in 0..1500 {
            let name = std::format!("f{}", rng.below(6)).into_bytes();
            let existing = names.get(&name).copied();
            let mut root = fs.inode(&(), ROOT_INO).unwrap();
            let what = std::format!("seed {seed} step {step}");
            match (rng.below(10), existing) {
                (0..=3, None) => {
                    let (ino, mut ip) = make(&fs, &mut sum, &name);
                    let len = match rng.below(4) {
                        0 => 0,
                        1 => 1 + rng.below(bsize),
                        2 => 1 + rng.below(3 * bsize),
                        _ => 12 * bsize + rng.below(3 * bsize),
                    };
                    let data: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
                    fs.write_data_range(&(), &mut sum, ino, &mut ip, 0, &data, r)
                        .unwrap();
                    fs.write_inode(&(), ino, &ip, Mode::Delayed).unwrap();
                    names.insert(name, ino);
                    files.insert(ino, data);
                }
                (4, Some(ino)) => {
                    // Cut: to nothing, or somewhere inside.
                    let mut ip = fs.inode(&(), ino).unwrap();
                    let to = if rng.below(2) == 0 {
                        0
                    } else {
                        rng.below(ip.size + 1)
                    };
                    fs.truncate(&(), &mut sum, ino, &mut ip, to, r, false)
                        .unwrap();
                    fs.write_inode(&(), ino, &ip, Mode::Delayed).unwrap();
                    files.get_mut(&ino).unwrap().truncate(to as usize);
                }
                (5, Some(ino)) => {
                    let to = std::format!("f{}", rng.below(6)).into_bytes();
                    if names.contains_key(&to) {
                        continue;
                    }
                    let mut ip = fs.inode(&(), ino).unwrap();
                    if rng.below(2) == 0 {
                        fs.link(
                            &(),
                            &mut sum,
                            ROOT_INO,
                            &mut root,
                            &to,
                            ino,
                            &mut ip,
                            NOW,
                            r,
                        )
                        .unwrap();
                    } else {
                        fs.rename(
                            &(),
                            &mut sum,
                            ROOT_INO,
                            &mut root,
                            &name,
                            ino,
                            &mut ip,
                            None,
                            &to,
                            None,
                            NOW,
                            r,
                        )
                        .unwrap();
                        names.remove(&name);
                    }
                    names.insert(to, ino);
                }
                (6..=9, Some(ino)) => {
                    let mut ip = fs.inode(&(), ino).unwrap();
                    fs.unlink(&(), ROOT_INO, &mut root, &name, ino, &mut ip, NOW)
                        .unwrap();
                    names.remove(&name);
                    if ip.effnlink != 0 {
                        continue;
                    }
                    files.remove(&ino);
                    if ip.nlink != 0 {
                        // The work frees it, once the removal is written.
                        in_order += 1;
                    } else if rng.below(4) == 0 {
                        open.push(ino);
                    } else {
                        let free = sum.totals.nifree;
                        fs.free_inode(&(), &mut sum, ino, &mut ip, r).unwrap();
                        if sum.totals.nifree > free {
                            at_once += 1;
                        } else {
                            in_order += 1;
                        }
                    }
                }
                _ => {}
            }
            // A file unlinked and open is closed.
            if !open.is_empty() && rng.below(3) == 0 {
                let ino = open.swap_remove(rng.below(open.len() as u64) as usize);
                let mut ip = fs.inode(&(), ino).unwrap();
                assert_eq!((ip.nlink, ip.effnlink), (0, 0), "{what}: open, unlinked");
                assert_ne!(ip.mode, 0, "{what}: open, not freed");
                fs.free_inode(&(), &mut sum, ino, &mut ip, r).unwrap();
            }
            let io = fs.io();
            if rng.below(4) == 0 {
                io.write_some(&mut rng);
            }
            if rng.below(4) == 0 {
                do_work(&fs, &mut sum);
            }
            if rng.below(8) == 0 {
                crash_ok(&fs, &what);
                crashes += 1;
            }
        }
        for ino in open {
            let mut ip = fs.inode(&(), ino).unwrap();
            fs.free_inode(&(), &mut sum, ino, &mut ip, r).unwrap();
        }
        let what = std::format!("seed {seed} in the end");
        settled(&fs, &mut sum, &what);
        nothing_held(&fs, &what);
        let disk = fs.io().disk.borrow().clone();
        let back = Ufs::open(&(), &disk[..], 512).unwrap();
        let root = back.inode(&(), ROOT_INO).unwrap();
        let on_disk: BTreeMap<Vec<u8>, u32> = back.names(&(), &root).unwrap().into_iter().collect();
        assert_eq!(on_disk, names, "{what}: names");
        for (&ino, want) in &files {
            let ip = back.inode(&(), ino).unwrap();
            let links = names.values().filter(|i| **i == ino).count();
            assert_eq!(usize::from(ip.nlink), links, "{what}: links of {ino}");
            assert_eq!(ip.size, want.len() as u64, "{what}: size of {ino}");
            let mut got = alloc::vec![0u8; want.len()];
            back.read(&(), &ip, 0, &mut got).unwrap();
            assert!(got == *want, "{what}: contents of {ino}");
        }
        // Nothing but those files is allocated.
        assert_eq!(
            sum.totals.nifree,
            start.nifree - files.len() as i64,
            "{what}: inodes in use"
        );
        std::eprintln!(
            "seed {seed}: {at_once} files gone at once, {in_order} in order, \
             {crashes} crash points checked"
        );
        assert!(at_once > 10 && in_order > 5, "{what}: both ways taken");
    }
}

/// An indirect block freed while a write of it is under way (the kernel's
/// cache writes from a copy): that write went out with no pointers, but
/// says nothing of what the block holds next. A data block there later
/// has its pointer go out like any other.
#[test]
fn an_indirect_block_freed_under_its_write_leaves_no_mark() {
    let Some(sb) = a_superblock() else {
        std::eprintln!("no image (not FreeBSD, no fetch-base): skipped");
        return;
    };
    let f = u64::from(sb.fsize);
    let bsize = u64::from(sb.bsize);
    let ino = 4;
    let cg_at = sb.cgtod(0) * f;
    let (bno, other) = (sb.cgdmin(0) + 64, sb.cgdmin(0) + 128);
    let (at, other_at) = (bno * f, other * f);
    let zeros = alloc::vec![0u8; idx64(bsize)];
    let mut d = Deps::new(&sb);
    // The indirect block's write begins, empty; the file is cut meanwhile
    // and the block freed; then the write lands.
    d.indirect(at);
    assert!(d.write_start(at, &zeros).is_none());
    d.forget_buffer(at, ino);
    d.write_done(at, true);
    // The block is data now, under another indirect block.
    d.new_block(bno, 0, bsize, cg_at, ino);
    d.indirect(other_at);
    d.place(
        bno,
        softdep::Slot::Indirect {
            at: other_at,
            index: 0,
        },
        0,
        0,
        None,
    );
    d.data_written(at, bsize);
    assert!(d.write_start(cg_at, &zeros).is_none());
    d.write_done(cg_at, true);
    let mut block = zeros.clone();
    block[..8].copy_from_slice(&bno.to_le_bytes());
    assert!(
        d.write_start(other_at, &block).is_none(),
        "the pointer is held back:\n{}",
        d.describe()
    );
    d.write_done(other_at, true);
    assert!(d.is_idle(), "{}", d.describe());
}
