// SPDX-License-Identifier: BSD-2-Clause
//! The format pieces on their own, and the engine against images FreeBSD's
//! `makefs` made (where it exists: on FreeBSD).

#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

extern crate std;

use alloc::vec::Vec;
use std::path::Path;
use std::process::Command;

use super::*;

/// A directory chunk holding `entries`, each padded to its `DIRSIZ`, the
/// last stretched to the chunk's end.
fn chunk(entries: &[(u32, u8, &[u8])]) -> Vec<u8> {
    let mut b = alloc::vec![0u8; dir::DIRBLKSIZ];
    let mut at = 0;
    for (i, (ino, kind, name)) in entries.iter().enumerate() {
        let len = if i + 1 == entries.len() {
            dir::DIRBLKSIZ - at
        } else {
            dir::dirsiz(name.len())
        };
        b[at..at + 4].copy_from_slice(&ino.to_le_bytes());
        b[at + 4..at + 6].copy_from_slice(&(len as u16).to_le_bytes());
        b[at + 6] = *kind;
        b[at + 7] = name.len() as u8;
        b[at + 8..at + 8 + name.len()].copy_from_slice(name);
        at += len;
    }
    b
}

fn names(block: &[u8], skip: usize) -> Result<Vec<(Vec<u8>, u64)>, Error> {
    let mut out = Vec::new();
    dir::entries(block, 1024, skip, &mut |e| {
        out.push((e.name.to_vec(), e.offset));
        true
    })?;
    Ok(out)
}

#[test]
fn directory_chunks_parse_and_bad_entries_are_refused() {
    let mut b = chunk(&[
        (2, 4, b"."),
        (2, 4, b".."),
        (0, 0, b"gone"),
        (7, 8, b"file"),
    ]);
    b.extend(chunk(&[(9, 4, b"sub")]));
    let all = names(&b, 0).unwrap();
    let got: Vec<&[u8]> = all.iter().map(|(n, _)| n.as_slice()).collect();
    assert_eq!(got, [&b"."[..], b"..", b"file", b"sub"]);
    // Offsets count from the block's place in the directory.
    assert_eq!(all[0].1, 1024);
    assert_eq!(all[3].1, 1024 + 512);
    // From the second chunk on.
    assert_eq!(names(&b, 512).unwrap().len(), 1);
    // A record length past its chunk, or not a multiple of 4.
    let mut bad = b.clone();
    bad[4..6].copy_from_slice(&600u16.to_le_bytes());
    assert_eq!(
        names(&bad, 0),
        Err(Error::Corrupt("directory entry length"))
    );
    let mut bad = b.clone();
    bad[4..6].copy_from_slice(&13u16.to_le_bytes());
    assert!(names(&bad, 0).is_err());
    // A name longer than its record.
    let mut bad = b.clone();
    bad[7] = 200;
    assert!(names(&bad, 0).is_err());
    // A block that is not whole chunks.
    assert!(names(&b[..700], 0).is_err());
}

#[test]
fn a_block_that_is_not_a_superblock_is_not_ufs2() {
    let image = alloc::vec![0u8; 1 << 20];
    assert_eq!(
        Ufs::open(&(), &image[..], 512).map(|_| ()),
        Err(Error::NotUfs2)
    );
    // Too small to hold one.
    assert_eq!(
        Ufs::open(&(), &image[..4096], 512).map(|_| ()),
        Err(Error::NotUfs2)
    );
}

// ---- Against makefs ----

fn have_makefs() -> bool {
    Command::new("makefs").arg("-h").output().is_ok()
}

/// A scratch directory under the target directory, emptied.
fn scratch(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(std::format!("abyss-ufs-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Builds the test tree: files of many sizes (a fragment tail, indirect
/// and double-indirect blocks at a 4 KiB block size), short and long
/// symbolic links, and a directory spanning several blocks.
fn build_tree(root: &Path) {
    use std::os::unix::fs::symlink;
    let fs = |p: &str, data: &[u8]| std::fs::write(root.join(p), data).unwrap();
    std::fs::create_dir_all(root.join("dir/sub")).unwrap();
    fs("empty", b"");
    fs("small", b"hello, ufs\n");
    let pattern = |n: usize| -> Vec<u8> { (0..n).map(|i| (i * 7 + i / 4096) as u8).collect() };
    fs("frag", &pattern(4096 * 3 + 1000));
    fs("indirect", &pattern(4096 * 40 + 123));
    fs("double", &pattern(4096 * 600 + 17));
    fs("dir/sub/deep", b"deep\n");
    symlink("small", root.join("short-link")).unwrap();
    let long: std::string::String = core::iter::repeat_n("abcdefghij/", 20).collect();
    symlink(&long, root.join("long-link")).unwrap();
    for i in 0..300 {
        fs(
            &std::format!("dir/entry-with-a-longish-name-{i:04}"),
            &[i as u8],
        );
    }
}

/// Every file under `tree` matches what the image says, through `ufs`.
fn compare<B: BlockIo<Cx = ()>>(ufs: &Ufs<B>, tree: &Path, ino: u32) -> usize {
    use std::os::unix::fs::MetadataExt;
    let ip = ufs.inode(&(), ino).unwrap();
    let mut seen = 0;
    let mut entries = Vec::new();
    ufs.read_dir(&(), &ip, 0, &mut |e| {
        entries.push((e.name.to_vec(), e.ino, e.kind));
        true
    })
    .unwrap();
    let on_disk: std::collections::BTreeSet<Vec<u8>> = std::fs::read_dir(tree)
        .unwrap()
        .map(|e| {
            use std::os::unix::ffi::OsStrExt;
            e.unwrap().file_name().as_bytes().to_vec()
        })
        .collect();
    let in_image: std::collections::BTreeSet<Vec<u8>> = entries
        .iter()
        .filter(|(n, _, _)| n != b"." && n != b"..")
        .map(|(n, _, _)| n.clone())
        .collect();
    assert_eq!(on_disk, in_image, "entries of {}", tree.display());
    for (name, child, _) in entries {
        if name == b"." || name == b".." {
            continue;
        }
        use std::os::unix::ffi::OsStrExt;
        let path = tree.join(std::ffi::OsStr::from_bytes(&name));
        let meta = std::fs::symlink_metadata(&path).unwrap();
        let cip = ufs.inode(&(), child).unwrap();
        assert_eq!(u32::from(cip.mode), meta.mode(), "{}", path.display());
        // A directory's size is its file system's business.
        if !meta.is_dir() {
            assert_eq!(cip.size, meta.size(), "{}", path.display());
        }
        assert_eq!(cip.mtime.sec, meta.mtime(), "{}", path.display());
        assert_eq!(ufs.lookup(&(), &ip, &name).unwrap(), Some(child));
        match cip.kind() {
            Kind::Directory => seen += compare(ufs, &path, child),
            Kind::Regular => {
                let want = std::fs::read(&path).unwrap();
                let mut got = alloc::vec![0u8; want.len() + 100];
                let n = ufs.read(&(), &cip, 0, &mut got).unwrap();
                assert_eq!(n, want.len(), "{}", path.display());
                assert!(got[..n] == want[..], "{} differs", path.display());
                // And from an unaligned offset.
                if want.len() > 5000 {
                    let mut part = alloc::vec![0u8; 3000];
                    assert_eq!(ufs.read(&(), &cip, 4999, &mut part).unwrap(), 3000);
                    assert!(part[..] == want[4999..7999]);
                }
            }
            Kind::Symlink => {
                use std::os::unix::ffi::OsStrExt;
                let want = std::fs::read_link(&path).unwrap();
                assert_eq!(
                    ufs.readlink(&(), &cip).unwrap(),
                    want.as_os_str().as_bytes()
                );
            }
            _ => {}
        }
        seen += 1;
    }
    seen
}

#[test]
fn makefs_images_read_back_whole() {
    if !have_makefs() {
        std::eprintln!("no makefs (not FreeBSD): skipped");
        return;
    }
    let work = scratch("makefs");
    let tree = work.join("tree");
    std::fs::create_dir(&tree).unwrap();
    build_tree(&tree);
    for (bsize, fsize) in [(4096, 512), (32768, 4096)] {
        let img = work.join(std::format!("ufs-{bsize}.img"));
        let ok = Command::new("makefs")
            .args(["-t", "ffs", "-o", "version=2"])
            .args(["-o", &std::format!("bsize={bsize},fsize={fsize}")])
            .arg(&img)
            .arg(&tree)
            .status()
            .unwrap()
            .success();
        assert!(ok, "makefs");
        let image = std::fs::read(&img).unwrap();
        let ufs = Ufs::open(&(), &image[..], 512).unwrap();
        assert_eq!(
            (ufs.superblock().bsize, ufs.superblock().fsize),
            (bsize, fsize)
        );
        assert_eq!(compare(&ufs, &tree, ROOT_INO), 310);
        let (ino, ip) = ufs.namei(&(), b"/dir/sub/deep").unwrap().unwrap();
        assert!(ino > ROOT_INO);
        let mut b = [0u8; 16];
        assert_eq!(ufs.read(&(), &ip, 0, &mut b).unwrap(), 5);
        assert!(ufs.namei(&(), b"/dir/nothing").unwrap().is_none());
        // The double-indirect file runs contiguously, as makefs lays it.
        let (_, big) = ufs.namei(&(), b"/double").unwrap().unwrap();
        let (addr, run) = ufs.bmap_run(&(), &big, 0, 8).unwrap();
        assert!(addr != 0 && run >= 1);
        let s = ufs.stats();
        assert!(s.bfree < s.blocks && s.ffree > 0);
        // A flipped superblock byte fails its checks or its check-hash.
        let mut broken = image.clone();
        broken[65536 + 48] ^= 1;
        assert!(Ufs::open(&(), &broken[..], 512).is_err());
    }
    let _ = std::fs::remove_dir_all(&work);
}

// ---- Writing, against a model and fsck_ffs ----

/// An image in memory that the engine may write.
struct Mem(core::cell::RefCell<Vec<u8>>);

impl BlockIo for Mem {
    type Cx = ();

    fn read(&self, _: &(), offset: u64, len: usize, f: &mut dyn FnMut(&[u8])) -> Result<(), Error> {
        let b = self.0.borrow();
        let o = offset as usize;
        f(b.get(o..o + len).ok_or(Error::Io)?);
        Ok(())
    }

    fn modify(
        &self,
        _: &(),
        offset: u64,
        len: usize,
        fill: Fill,
        _: Mode,
        f: &mut dyn FnMut(&mut [u8]),
    ) -> Result<(), Error> {
        let mut b = self.0.borrow_mut();
        let o = offset as usize;
        let s = b.get_mut(o..o + len).ok_or(Error::Io)?;
        if fill == Fill::Zero {
            s.fill(0);
        }
        f(s);
        Ok(())
    }

    fn write_data(&self, _: &(), offset: u64, data: &[u8]) -> Result<(), Error> {
        let mut b = self.0.borrow_mut();
        let o = offset as usize;
        b.get_mut(o..o + data.len())
            .ok_or(Error::Io)?
            .copy_from_slice(data);
        Ok(())
    }
}

/// A small PRNG (xorshift).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
}

/// The model of what the file system should hold.
#[derive(Clone, Debug)]
enum MNode {
    File(Vec<u8>),
    Dir(std::collections::BTreeMap<Vec<u8>, u32>),
    Link(Vec<u8>),
}

struct Model {
    nodes: std::collections::BTreeMap<u32, MNode>,
    /// Each directory's parent.
    parent: std::collections::BTreeMap<u32, u32>,
}

impl Model {
    fn dirs(&self) -> Vec<u32> {
        self.nodes
            .iter()
            .filter(|(_, n)| matches!(n, MNode::Dir(_)))
            .map(|(i, _)| *i)
            .collect()
    }

    fn entries(&self, d: u32) -> Vec<(Vec<u8>, u32)> {
        match &self.nodes[&d] {
            MNode::Dir(m) => m.iter().map(|(n, i)| (n.clone(), *i)).collect(),
            _ => Vec::new(),
        }
    }

    fn dir_mut(&mut self, d: u32) -> &mut std::collections::BTreeMap<Vec<u8>, u32> {
        match self.nodes.get_mut(&d) {
            Some(MNode::Dir(m)) => m,
            _ => panic!("not a directory"),
        }
    }

    /// How many entries name `ino` (and for a directory, its `.` and its
    /// subdirectories' `..`).
    fn links(&self, ino: u32) -> u16 {
        let named = self
            .nodes
            .values()
            .filter_map(|n| match n {
                MNode::Dir(m) => Some(m.values().filter(|&&i| i == ino).count()),
                _ => None,
            })
            .sum::<usize>();
        match &self.nodes[&ino] {
            MNode::Dir(m) => {
                let subdirs = m
                    .values()
                    .filter(|i| matches!(self.nodes[i], MNode::Dir(_)))
                    .count();
                // The root's `..` is itself.
                let root = usize::from(ino == ROOT_INO);
                (named + 1 + subdirs + root) as u16
            }
            _ => named as u16,
        }
    }

    fn is_under(&self, mut d: u32, ancestor: u32) -> bool {
        loop {
            if d == ancestor {
                return true;
            }
            if d == ROOT_INO {
                return false;
            }
            d = self.parent[&d];
        }
    }
}

fn maker(rng: &mut Rng, reserve: Reserve) -> ops::Maker {
    ops::Maker {
        perm: 0o644,
        uid: 1001,
        gid: 1001,
        now: Timestamp {
            sec: 1_760_000_000,
            nsec: 0,
        },
        random: rng.next() as u32,
        reserve,
    }
}

/// Runs `n` random operations on the file system in `img`, mirrored in a
/// model; returns the model. Out of space is fine.
fn workload(fs: &Ufs<Mem>, sum: &mut Summary, seed: u64, n: usize) -> Model {
    let mut rng = Rng(seed);
    let mut m = Model {
        nodes: std::collections::BTreeMap::new(),
        parent: std::collections::BTreeMap::new(),
    };
    m.nodes
        .insert(ROOT_INO, MNode::Dir(std::collections::BTreeMap::new()));
    m.parent.insert(ROOT_INO, ROOT_INO);
    // What the image starts with (newfs makes an empty `.snap`).
    let root = fs.inode(&(), ROOT_INO).unwrap();
    for (name, ino) in fs.names(&(), &root).unwrap() {
        assert_eq!(
            fs.inode(&(), ino).unwrap().kind(),
            Kind::Directory,
            "unexpected starting entry"
        );
        m.dir_mut(ROOT_INO).insert(name, ino);
        m.nodes
            .insert(ino, MNode::Dir(std::collections::BTreeMap::new()));
        m.parent.insert(ino, ROOT_INO);
    }
    let bsize = u64::from(fs.superblock().bsize);
    let now = Timestamp {
        sec: 1_760_000_000,
        nsec: 0,
    };
    let r = Reserve::Allowed;
    for step in 0..n {
        let dirs = m.dirs();
        let d = dirs[rng.below(dirs.len() as u64) as usize];
        let name = std::format!("n{}", rng.below(40)).into_bytes();
        let op = rng.below(100);
        let mut dip = fs.inode(&(), d).unwrap();
        // Half the time through the directory index.
        let hashed = rng.below(2) == 0;
        if hashed {
            fs.dirhash_build(&(), &mut dip, 0).unwrap();
        }
        let existing = m
            .entries(d)
            .into_iter()
            .find(|(n, _)| *n == name)
            .map(|(_, i)| i);
        let res: Result<(), Error> = (|| {
            match op {
                // Make something.
                0..=29 if existing.is_none() => {
                    let kind = match rng.below(10) {
                        0..=5 => ops::NewKind::Regular,
                        6..=7 => ops::NewKind::Directory,
                        _ => ops::NewKind::Symlink(b""),
                    };
                    let long = rng.below(2) == 0;
                    let target: Vec<u8> = (0..1 + rng.below(if long { 400 } else { 100 }))
                        .map(|i| b'a' + (i % 26) as u8)
                        .collect();
                    let kind = match kind {
                        ops::NewKind::Symlink(_) => ops::NewKind::Symlink(&target),
                        k => k,
                    };
                    let (ino, _) =
                        fs.create(&(), sum, d, &mut dip, &name, kind, &maker(&mut rng, r))?;
                    m.dir_mut(d).insert(name.clone(), ino);
                    let node = match kind {
                        ops::NewKind::Directory => {
                            m.parent.insert(ino, d);
                            MNode::Dir(std::collections::BTreeMap::new())
                        }
                        ops::NewKind::Symlink(t) => MNode::Link(t.to_vec()),
                        _ => MNode::File(Vec::new()),
                    };
                    m.nodes.insert(ino, node);
                }
                // Write into a file.
                30..=59 => {
                    let Some(ino) = existing.filter(|i| matches!(m.nodes[i], MNode::File(_)))
                    else {
                        return Ok(());
                    };
                    let mut ip = fs.inode(&(), ino).unwrap();
                    let span = if rng.below(4) == 0 {
                        bsize * 600
                    } else {
                        bsize * 30
                    };
                    let off = rng.below(span);
                    let len = 1 + rng.below(bsize * 3);
                    let data: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
                    let res = fs.write_data_range(&(), sum, ino, &mut ip, off, &data, r);
                    fs.write_inode(&(), ino, &ip, Mode::Sync)?;
                    res?;
                    if let MNode::File(f) = m.nodes.get_mut(&ino).unwrap() {
                        let end = (off + len) as usize;
                        if f.len() < end {
                            f.resize(end, 0);
                        }
                        f[off as usize..end].copy_from_slice(&data);
                    }
                }
                // Truncate a file.
                60..=69 => {
                    let Some(ino) = existing.filter(|i| matches!(m.nodes[i], MNode::File(_)))
                    else {
                        return Ok(());
                    };
                    let mut ip = fs.inode(&(), ino).unwrap();
                    let new = rng.below((ip.size * 2).max(bsize * 20));
                    fs.truncate(&(), sum, ino, &mut ip, new, r, false)?;
                    fs.write_inode(&(), ino, &ip, Mode::Sync)?;
                    if let MNode::File(f) = m.nodes.get_mut(&ino).unwrap() {
                        f.resize(new as usize, 0);
                    }
                }
                // Link a file elsewhere.
                70..=74 => {
                    let Some(ino) = existing.filter(|i| matches!(m.nodes[i], MNode::File(_)))
                    else {
                        return Ok(());
                    };
                    let to = dirs[rng.below(dirs.len() as u64) as usize];
                    let to_name = std::format!("l{}", rng.below(40)).into_bytes();
                    if m.entries(to).iter().any(|(n, _)| *n == to_name) {
                        return Ok(());
                    }
                    // One in-core inode per directory, as in the kernel.
                    let mut tip = fs.inode(&(), to).unwrap();
                    let tdir = if to == d { &mut dip } else { &mut tip };
                    let mut ip = fs.inode(&(), ino).unwrap();
                    fs.link(&(), sum, to, tdir, &to_name, ino, &mut ip, now, r)?;
                    m.dir_mut(to).insert(to_name, ino);
                }
                // Remove something.
                75..=87 => {
                    let Some(ino) = existing else {
                        return Ok(());
                    };
                    let mut ip = fs.inode(&(), ino).unwrap();
                    if let MNode::Dir(c) = &m.nodes[&ino] {
                        if !c.is_empty() {
                            return Ok(());
                        }
                        fs.rmdir(&(), sum, d, &mut dip, &name, ino, &mut ip, now)?;
                        fs.free_inode(&(), sum, ino, &mut ip, r)?;
                        m.nodes.remove(&ino);
                        m.parent.remove(&ino);
                    } else {
                        fs.unlink(&(), d, &mut dip, &name, ino, &mut ip, now)?;
                        if ip.nlink == 0 {
                            fs.free_inode(&(), sum, ino, &mut ip, r)?;
                            m.nodes.remove(&ino);
                        }
                    }
                    m.dir_mut(d).remove(&name);
                }
                // Rename something.
                88..=99 => {
                    let Some(ino) = existing else {
                        return Ok(());
                    };
                    let to = dirs[rng.below(dirs.len() as u64) as usize];
                    let to_name = std::format!("n{}", rng.below(40)).into_bytes();
                    let is_dir = matches!(m.nodes[&ino], MNode::Dir(_));
                    if (is_dir && m.is_under(to, ino)) || (to == d && to_name == name) {
                        return Ok(());
                    }
                    let target = m
                        .entries(to)
                        .into_iter()
                        .find(|(n, _)| *n == to_name)
                        .map(|(_, i)| i);
                    if let Some(t) = target {
                        let t_dir = matches!(m.nodes[&t], MNode::Dir(_));
                        if t == ino || t_dir != is_dir || (t_dir && !m.entries(t).is_empty()) {
                            return Ok(());
                        }
                    }
                    let mut ip = fs.inode(&(), ino).unwrap();
                    let mut tdir = fs.inode(&(), to).unwrap();
                    let mut tip = target.map(|t| fs.inode(&(), t).unwrap());
                    let to_dir = if to == d { None } else { Some((to, &mut tdir)) };
                    let tgt = target.zip(tip.as_mut());
                    fs.rename(
                        &(),
                        sum,
                        d,
                        &mut dip,
                        &name,
                        ino,
                        &mut ip,
                        to_dir,
                        &to_name,
                        tgt,
                        now,
                        r,
                    )?;
                    if let (Some(t), Some(tip)) = (target, tip.as_mut())
                        && tip.nlink == 0
                    {
                        fs.free_inode(&(), sum, t, tip, r)?;
                        m.nodes.remove(&t);
                        m.parent.remove(&t);
                    }
                    m.dir_mut(d).remove(&name);
                    m.dir_mut(to).insert(to_name, ino);
                    if is_dir {
                        m.parent.insert(ino, to);
                    }
                }
                _ => {}
            }
            Ok(())
        })();
        match res {
            Ok(()) | Err(Error::NoSpace) => {}
            Err(e) => panic!("step {step} (op {op}): {e:?}"),
        }
        // The index the operations kept matches one built afresh.
        if hashed && m.nodes.contains_key(&d) {
            let mut fresh = fs.inode(&(), d).unwrap();
            fresh.dirhash = None;
            fs.dirhash_build(&(), &mut fresh, 0).unwrap();
            if dip.size == fresh.size {
                assert_eq!(
                    dip.dirhash, fresh.dirhash,
                    "step {step} (op {op}): index of {d}"
                );
            }
        }
    }
    m
}

/// The image holds what the model says, link counts included.
fn check_model(fs: &Ufs<&[u8]>, m: &Model, ino: u32) {
    let ip = fs.inode(&(), ino).unwrap();
    assert_eq!(ip.nlink, m.links(ino), "links of {ino}");
    match &m.nodes[&ino] {
        MNode::Dir(want) => {
            let got: std::collections::BTreeMap<Vec<u8>, u32> =
                fs.names(&(), &ip).unwrap().into_iter().collect();
            assert_eq!(&got, want, "entries of {ino}");
            let dotdot = fs.lookup(&(), &ip, b"..").unwrap().unwrap();
            assert_eq!(dotdot, m.parent[&ino], ".. of {ino}");
            for &c in want.values() {
                if matches!(m.nodes[&c], MNode::Dir(_)) && m.parent[&c] != ino {
                    continue;
                }
                check_model(fs, m, c);
            }
        }
        MNode::File(want) => {
            assert_eq!(ip.size, want.len() as u64, "size of {ino}");
            let mut got = alloc::vec![0u8; want.len()];
            assert_eq!(fs.read(&(), &ip, 0, &mut got).unwrap(), want.len());
            assert!(got == *want, "contents of {ino}");
        }
        MNode::Link(want) => assert_eq!(&fs.readlink(&(), &ip).unwrap(), want, "link {ino}"),
    }
}

/// `fsck_ffs -f -n` finds nothing wrong with the image at `path` (but the
/// questions FreeBSD asks of any makefs image).
fn fsck_clean(path: &Path) {
    let out = Command::new("fsck_ffs")
        .args(["-f", "-n"])
        .arg(path)
        .output()
        .unwrap();
    let text = std::string::String::from_utf8_lossy(&out.stdout).into_owned()
        + &std::string::String::from_utf8_lossy(&out.stderr);
    let benign = ["CONTINUE?", "UPDATE FILESYSTEM TO TRACK DIRECTORY DEPTH?"];
    let bad: Vec<&str> = text
        .lines()
        .filter(|l| l.contains('?') && !benign.iter().any(|b| l.contains(b)))
        .collect();
    assert!(bad.is_empty() && out.status.success(), "fsck_ffs:\n{text}");
}

#[test]
fn random_workloads_leave_fsck_clean_images() {
    if !have_makefs() {
        std::eprintln!("no makefs (not FreeBSD): skipped");
        return;
    }
    let work = scratch("write");
    let empty = work.join("empty");
    std::fs::create_dir(&empty).unwrap();
    // makefs images, and newfs images (which carry the superblock,
    // group, and inode check-hashes).
    let cases = [
        ("makefs", 4096u32, 512u32, "24m", 1u64),
        ("makefs", 32768, 4096, "48m", 7),
        ("makefs", 4096, 512, "6m", 3),
        ("newfs", 32768, 4096, "64m", 11),
        ("newfs", 4096, 512, "32m", 13),
    ];
    for (tool, bsize, fsize, size, seed) in cases {
        let img = work.join(std::format!("w-{tool}-{bsize}-{seed}.img"));
        let ok = if tool == "makefs" {
            Command::new("makefs")
                .args(["-t", "ffs", "-o", "version=2", "-s", size])
                .args(["-o", &std::format!("bsize={bsize},fsize={fsize}")])
                .arg(&img)
                .arg(&empty)
                .status()
                .unwrap()
                .success()
        } else {
            Command::new("truncate")
                .args(["-s", size])
                .arg(&img)
                .status()
                .unwrap()
                .success()
                && Command::new("newfs")
                    .args([
                        "-b",
                        &std::format!("{bsize}"),
                        "-f",
                        &std::format!("{fsize}"),
                    ])
                    .arg(&img)
                    .stdout(std::process::Stdio::null())
                    .status()
                    .unwrap()
                    .success()
        };
        assert!(ok, "{tool}");
        let fs = Ufs::open(
            &(),
            Mem(core::cell::RefCell::new(std::fs::read(&img).unwrap())),
            512,
        )
        .unwrap();
        let mut sum = fs.load_summary(&()).unwrap();
        let model = workload(&fs, &mut sum, seed, 3000);
        fs.write_summary(&(), &mut sum, true, 1_760_000_100)
            .unwrap();
        let bytes = fs.io().0.borrow().clone();
        let back = Ufs::open(&(), &bytes[..], 512).unwrap();
        check_model(&back, &model, ROOT_INO);
        std::fs::write(&img, &bytes).unwrap();
        fsck_clean(&img);
        let ck = fs.superblock().metackhash;
        std::eprintln!(
            "{tool} {bsize}/{fsize} seed {seed} (check-hashes {ck:#x}): {} nodes, clean",
            model.nodes.len()
        );
    }
    let _ = std::fs::remove_dir_all(&work);
}

#[test]
fn large_directories_through_the_index() {
    if !have_makefs() {
        std::eprintln!("no makefs (not FreeBSD): skipped");
        return;
    }
    let work = scratch("dirhash");
    let empty = work.join("empty");
    std::fs::create_dir(&empty).unwrap();
    let img = work.join("big.img");
    let ok = Command::new("makefs")
        .args([
            "-t",
            "ffs",
            "-o",
            "version=2",
            "-s",
            "32m",
            "-o",
            "bsize=4096,fsize=512",
        ])
        .arg(&img)
        .arg(&empty)
        .status()
        .unwrap()
        .success();
    assert!(ok, "makefs");
    let fs = Ufs::open(
        &(),
        Mem(core::cell::RefCell::new(std::fs::read(&img).unwrap())),
        512,
    )
    .unwrap();
    let mut sum = fs.load_summary(&()).unwrap();
    let mut rng = Rng(5);
    let mut root = fs.inode(&(), ROOT_INO).unwrap();
    let name = |i: usize| std::format!("a-rather-long-file-name-number-{i:05}").into_bytes();
    let mut inos = std::collections::BTreeMap::new();
    for i in 0..3000 {
        if i == 200 {
            fs.dirhash_build(&(), &mut root, dirhash::MIN_SIZE).unwrap();
            assert!(root.dirhash.is_some(), "a large directory gets an index");
        }
        let (ino, _) = fs
            .create(
                &(),
                &mut sum,
                ROOT_INO,
                &mut root,
                &name(i),
                ops::NewKind::Regular,
                &maker(&mut rng, Reserve::Allowed),
            )
            .unwrap();
        inos.insert(i, ino);
    }
    let full = root.size;
    // Every other name goes, then half of those come back.
    let now = Timestamp {
        sec: 1_760_000_000,
        nsec: 0,
    };
    for i in (0..3000).step_by(2) {
        let ino = inos.remove(&i).unwrap();
        let mut ip = fs.inode(&(), ino).unwrap();
        fs.unlink(&(), ROOT_INO, &mut root, &name(i), ino, &mut ip, now)
            .unwrap();
        fs.free_inode(&(), &mut sum, ino, &mut ip, Reserve::Allowed)
            .unwrap();
    }
    for i in (0..3000).step_by(4) {
        let (ino, _) = fs
            .create(
                &(),
                &mut sum,
                ROOT_INO,
                &mut root,
                &name(i),
                ops::NewKind::Regular,
                &maker(&mut rng, Reserve::Allowed),
            )
            .unwrap();
        inos.insert(i, ino);
    }
    // Lookups through the index and by scanning agree.
    let mut plain = fs.inode(&(), ROOT_INO).unwrap();
    plain.dirhash = None;
    for i in 0..3000 {
        let want = inos.get(&i).copied();
        assert_eq!(
            fs.lookup(&(), &root, &name(i)).unwrap(),
            want,
            "indexed {i}"
        );
        assert_eq!(
            fs.lookup(&(), &plain, &name(i)).unwrap(),
            want,
            "scanned {i}"
        );
    }
    let mut fresh = plain.clone();
    fs.dirhash_build(&(), &mut fresh, 0).unwrap();
    assert_eq!(root.dirhash, fresh.dirhash);
    // Freed slots were reused: the directory did not grow for the
    // second round.
    assert_eq!(root.size, full, "directory size");
    fs.write_summary(&(), &mut sum, true, 1_760_000_100)
        .unwrap();
    std::fs::write(&img, &*fs.io().0.borrow()).unwrap();
    fsck_clean(&img);
    let _ = std::fs::remove_dir_all(&work);
}
