// SPDX-License-Identifier: BSD-2-Clause
//! Soft updates on the host: random workloads over a cache that writes
//! buffers back in random order and file data late, crashes at random
//! points, and a check of each crash image. On FreeBSD (the build server)
//! `fsck_ffs -p` must also repair each one with nothing but what soft
//! updates allows, and the settled image must be clean.

#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

extern crate std;

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;
use std::process::Command;

use super::softdep::Deps;
use super::tests::{MNode, Rng, check_model, fsck_clean, have_makefs, scratch, workload};
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
/// the tracker's hooks, with file data queued behind it.
pub(crate) struct Sim {
    pub(crate) disk: RefCell<Vec<u8>>,
    bufs: RefCell<BTreeMap<u64, Vec<u8>>>,
    dirty: RefCell<BTreeSet<u64>>,
    /// File data not on the disk yet, in order.
    data: RefCell<Vec<(u64, Vec<u8>)>>,
    deps: RefCell<Deps>,
}

impl Sim {
    pub(crate) fn new(image: Vec<u8>) -> Self {
        let sb = Superblock::find(&(), &image[..], 512).unwrap();
        Self {
            disk: RefCell::new(image),
            bufs: RefCell::new(BTreeMap::new()),
            dirty: RefCell::new(BTreeSet::new()),
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

    /// Writes the buffer at `offset` through the tracker.
    fn write_buf(&self, offset: u64) {
        let Some(bytes) = self.bufs.borrow().get(&offset).cloned() else {
            self.dirty.borrow_mut().remove(&offset);
            return;
        };
        let copy = self.deps.borrow_mut().write_start(offset, &bytes);
        let out = copy.as_ref().unwrap_or(&bytes);
        let o = offset as usize;
        self.disk.borrow_mut()[o..o + out.len()].copy_from_slice(out);
        self.deps.borrow_mut().write_done(offset, true);
        if copy.is_some() {
            self.dirty.borrow_mut().insert(offset);
        } else {
            self.dirty.borrow_mut().remove(&offset);
        }
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

    /// Writes a random part of what is dirty, in a random order.
    pub(crate) fn write_some(&self, rng: &mut Rng) {
        let n = self.data.borrow().len();
        self.write_data_out(rng.below(n as u64 + 1) as usize);
        let mut dirty: Vec<u64> = self.dirty.borrow().iter().copied().collect();
        for i in (1..dirty.len()).rev() {
            dirty.swap(i, rng.below(i as u64 + 1) as usize);
        }
        let keep = rng.below(dirty.len() as u64 + 1) as usize;
        for at in &dirty[keep..] {
            self.write_buf(*at);
        }
    }

    fn idle(&self) -> bool {
        self.dirty.borrow().is_empty()
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
        let mut bufs = self.bufs.borrow_mut();
        let gone: Vec<u64> = bufs
            .iter()
            .filter(|(o, b)| **o < end && **o + b.len() as u64 > offset)
            .map(|(o, _)| *o)
            .collect();
        for o in gone {
            bufs.remove(&o);
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
    out
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
                    let image = io.disk.borrow().clone();
                    if let Err(e) = check_crash(&image) {
                        if std::env::var("UFS_LOG").is_ok() {
                            std::eprintln!("{}", io.deps.borrow().describe());
                        }
                        panic!(
                            "{} seed {seed}: crash after step {step}:\n{e}",
                            src.display()
                        );
                    }
                    crashes += 1;
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
