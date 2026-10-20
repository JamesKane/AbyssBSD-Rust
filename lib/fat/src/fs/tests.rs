// SPDX-License-Identifier: BSD-2-Clause
//! The engine against FreeBSD's FAT images: `makefs -t msdos` builds them
//! (run where `makefs` is: FreeBSD). Its long names, times, and layout are
//! FreeBSD's; its short names come from an older conversion than the
//! kernel's, so they are left to `cargo xtask fat-verify`.

#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

extern crate std;

use alloc::vec::Vec;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::Command;

use super::conv::{self, Conv};
use super::*;

#[test]
fn short_names_follow_freebsd() {
    let dos = |n: &[u8], g| {
        let (d, c) = conv::unix2dosfn(n, g);
        (std::string::String::from_utf8_lossy(&d).into_owned(), c)
    };
    assert_eq!(dos(b"README.TXT", 0), ("README  TXT".into(), Conv::Exact));
    assert_eq!(dos(b"readme.txt", 0), ("README  TXT".into(), Conv::Long));
    assert_eq!(
        dos(b"verylongfilename.txt", 1),
        ("VERYLO~1TXT".into(), Conv::LongNumbered)
    );
    assert_eq!(
        dos(b"verylongfilename.txt", 12),
        ("VERYL~12TXT".into(), Conv::LongNumbered)
    );
    assert_eq!(dos(b"a+b", 1), ("A_B~1      ".into(), Conv::LongNumbered));
    assert_eq!(dos(b"a.b.c", 1), ("AB~1    C  ".into(), Conv::LongNumbered));
    assert_eq!(
        dos(b".hidden", 1),
        ("HIDDEN~1   ".into(), Conv::LongNumbered)
    );
    assert_eq!(dos(b"a/b", 0).1, Conv::Invalid);
    assert_eq!(dos(b"...", 0).1, Conv::Invalid);
    assert_eq!(dos(b"x", 2).1, Conv::Invalid);
    assert_eq!(conv::win_slots(b"verylongfilename.txt"), 2);
    assert_eq!(
        conv::win_chksum(b"README  TXT"),
        conv::win_chksum(b"README  TXT")
    );
}

fn have_makefs() -> bool {
    Command::new("makefs").arg("-h").output().is_ok()
}

fn scratch(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(std::format!("abyss-fat-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A tree of awkward names: case, long names, numbered collisions,
/// characters DOS replaces or drops, Latin-1, and a directory spanning
/// clusters.
fn build_tree(root: &Path) {
    let names: &[&[u8]] = &[
        b"README.TXT",
        b"readme2.txt",
        b"Mixed Case Name.html",
        b"a.b.c",
        b"verylongfilename.txt",
        b"verylongfilename2.txt",
        b"verylongfilename3.txt",
        b"+plus=sign;",
        b".dotfile",
        b"caf\xe9",
        b"x",
        b"exactly13char",
    ];
    for n in names {
        let p = root.join(std::ffi::OsStr::from_bytes(n));
        std::fs::write(&p, n).unwrap();
    }
    std::fs::create_dir_all(root.join("Sub Dir/deeper")).unwrap();
    std::fs::write(
        root.join("Sub Dir/deeper/big.bin"),
        (0..300_000u32).map(|i| i as u8).collect::<Vec<_>>(),
    )
    .unwrap();
    for i in 0..120 {
        std::fs::write(
            root.join(std::format!("Sub Dir/entry number {i:03}")),
            [i as u8],
        )
        .unwrap();
    }
}

/// Every entry of the tree is in the image, with its contents and its
/// FreeBSD short name.
fn compare(fat: &Fat<&[u8]>, tree: &Path, start: u32) -> usize {
    let mut names = Vec::new();
    fat.entries(&(), start, 0, &mut |n| {
        names.push(n.clone());
        false
    })
    .unwrap();
    let mut want: Vec<Vec<u8>> = std::fs::read_dir(tree)
        .unwrap()
        .map(|e| e.unwrap().file_name().as_bytes().to_vec())
        .collect();
    want.sort();
    let mut got: Vec<Vec<u8>> = names
        .iter()
        .filter(|n| n.name != b"." && n.name != b"..")
        .map(|n| n.name.clone())
        .collect();
    got.sort();
    assert_eq!(got, want, "names in {}", tree.display());
    // (Short names are not compared: makefs has its own, older name
    // conversion. The kernel's is checked against a FreeBSD mount by
    // `cargo xtask fat-verify`.)
    let mut count = 0;
    for n in &names {
        if n.name == b"." || n.name == b".." {
            continue;
        }
        // Found again by name, in another case for long names.
        let again = fat.lookup(&(), start, &n.name).unwrap().unwrap();
        assert_eq!(again.loc, n.loc);
        let path = tree.join(std::ffi::OsStr::from_bytes(&n.name));
        let meta = std::fs::metadata(&path).unwrap();
        if n.entry.kind() == Kind::Directory {
            count += compare(fat, &path, n.entry.start);
        } else {
            let data = std::fs::read(&path).unwrap();
            assert_eq!(u64::from(n.entry.size), meta.len());
            let mut got = Vec::new();
            let mut off = 0u64;
            while off < meta.len() {
                let (at, run) = fat.bmap(&(), n.entry.start, off).unwrap().unwrap();
                let len = run.min(meta.len() - off) as usize;
                fat.io()
                    .read(&(), at, len, &mut |b| got.extend_from_slice(b))
                    .unwrap();
                off += len as u64;
            }
            assert!(got == data, "contents of {}", path.display());
            // The modification time FreeBSD would have written.
            use std::os::unix::fs::MetadataExt;
            let t = time::to_fat(meta.mtime(), 0);
            assert_eq!(
                (n.entry.mtime.date, n.entry.mtime.time),
                (t.date, t.time),
                "{}",
                path.display()
            );
        }
        count += 1;
    }
    count
}

#[test]
fn makefs_images_read_back_with_freebsd_names() {
    if !have_makefs() {
        std::eprintln!("no makefs (not FreeBSD): skipped");
        return;
    }
    let work = scratch("read");
    let tree = work.join("tree");
    std::fs::create_dir(&tree).unwrap();
    build_tree(&tree);
    // FAT32 needs 65,525 clusters: one sector each.
    for (fat_type, size, spc) in [(12, "4m", 0), (16, "32m", 0), (32, "64m", 1)] {
        let img = work.join(std::format!("fat{fat_type}.img"));
        let mut opts = std::format!("fat_type={fat_type}");
        if spc != 0 {
            opts += &std::format!(",sectors_per_cluster={spc}");
        }
        let ok = Command::new("makefs")
            .args(["-t", "msdos", "-s", size, "-o", &opts])
            .arg(&img)
            .arg(&tree)
            .stdout(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "makefs");
        let image = std::fs::read(&img).unwrap();
        let fat = Fat::open(&(), &image[..], image.len() as u64).unwrap();
        let want = match fat_type {
            12 => FatType::Fat12,
            16 => FatType::Fat16,
            _ => FatType::Fat32,
        };
        assert_eq!(fat.geometry().fat_type, want);
        let n = compare(&fat, &tree, ROOT);
        std::eprintln!("FAT{fat_type}: {n} entries match");
        assert!(fat.lookup(&(), ROOT, b"no such name").unwrap().is_none());
        // Case does not matter.
        assert!(
            fat.lookup(&(), ROOT, b"MIXED CASE NAME.HTML")
                .unwrap()
                .is_some()
        );
    }
    let _ = std::fs::remove_dir_all(&work);
}

// ---- Writing ----

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

#[derive(Clone, Debug)]
enum MNode {
    File(Vec<u8>),
    Dir(std::collections::BTreeMap<Vec<u8>, MNode>),
}

/// A name of each style: exact short names, long names, and case.
fn pick_name(rng: &mut Rng) -> Vec<u8> {
    let i = rng.below(30);
    match rng.below(4) {
        0 => std::format!("FILE{i}.TXT"),
        1 => std::format!("a long file name {i}.data"),
        2 => std::format!("Mixed{i}"),
        _ => std::format!("very-long-name-to-force-numbers-{i}.extension"),
    }
    .into_bytes()
}

/// The model directory at `path`, and its start cluster on the volume.
fn walk_to<'m>(
    m: &'m mut MNode,
    fat: &Fat<Mem>,
    path: &[Vec<u8>],
) -> (&'m mut std::collections::BTreeMap<Vec<u8>, MNode>, u32) {
    let mut start = ROOT;
    let mut cur = m;
    for p in path {
        let MNode::Dir(d) = cur else { unreachable!() };
        start = fat.lookup(&(), start, p).unwrap().unwrap().entry.start;
        cur = d.get_mut(p).unwrap();
    }
    let MNode::Dir(d) = cur else { unreachable!() };
    (d, start)
}

/// The directories of the model, as paths.
fn dirs(m: &MNode, prefix: &mut Vec<Vec<u8>>, out: &mut Vec<Vec<Vec<u8>>>) {
    if let MNode::Dir(d) = m {
        out.push(prefix.clone());
        for (k, v) in d {
            prefix.push(k.clone());
            dirs(v, prefix, out);
            prefix.pop();
        }
    }
}

#[allow(clippy::too_many_lines)]
fn workload(fat: &Fat<Mem>, a: &mut Alloc, seed: u64, n: usize) -> MNode {
    let mut rng = Rng(seed);
    let mut m = MNode::Dir(std::collections::BTreeMap::new());
    let now = (1_760_000_000i64, 0u32);
    let cl = u64::from(fat.geometry().cluster);
    for step in 0..n {
        let mut all = Vec::new();
        dirs(&m, &mut Vec::new(), &mut all);
        let path = all[rng.below(all.len() as u64) as usize].clone();
        let name = pick_name(&mut rng);
        let op = rng.below(100);
        let (d, start) = walk_to(&mut m, fat, &path);
        let existing = fat.lookup(&(), start, &name).unwrap();
        assert_eq!(
            existing.is_some(),
            d.contains_key(&name),
            "step {step}: {:?}",
            std::string::String::from_utf8_lossy(&name)
        );
        let res: Result<(), Error> = (|| {
            match (op, &existing) {
                (0..=34, None) => {
                    let kind = if rng.below(5) == 0 {
                        Kind::Directory
                    } else {
                        Kind::File
                    };
                    fat.create(&(), a, start, &name, kind, now)?;
                    walk_to(&mut m, fat, &path).0.insert(
                        name.clone(),
                        if kind == Kind::Directory {
                            MNode::Dir(Default::default())
                        } else {
                            MNode::File(Vec::new())
                        },
                    );
                }
                (35..=64, Some(e)) if e.entry.kind() == Kind::File => {
                    let mut e = e.clone();
                    let off = rng.below(cl * 6);
                    let len = 1 + rng.below(cl * 3);
                    let data: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
                    let r = fat.write_data(&(), a, &mut e.entry, off, &data);
                    fat.update(&(), &e, Mode::Sync)?;
                    r?;
                    if let Some(MNode::File(f)) = walk_to(&mut m, fat, &path).0.get_mut(&name) {
                        let end = (off + len) as usize;
                        if f.len() < end {
                            f.resize(end, 0);
                        }
                        f[off as usize..end].copy_from_slice(&data);
                    }
                }
                (65..=74, Some(e)) if e.entry.kind() == Kind::File => {
                    let mut e = e.clone();
                    let new = rng.below(u64::from(e.entry.size) * 2 + cl);
                    fat.resize(&(), a, &mut e.entry, new, true)?;
                    fat.update(&(), &e, Mode::Sync)?;
                    if let Some(MNode::File(f)) = walk_to(&mut m, fat, &path).0.get_mut(&name) {
                        f.resize(new as usize, 0);
                    }
                }
                (75..=87, Some(e)) => {
                    if e.entry.kind() == Kind::Directory {
                        if !matches!(walk_to(&mut m, fat, &path).0.get(&name), Some(MNode::Dir(c)) if c.is_empty())
                        {
                            return Ok(());
                        }
                        fat.rmdir(&(), a, start, e)?;
                    } else {
                        fat.remove(&(), start, e)?;
                        let mut ent = e.entry.clone();
                        fat.resize(&(), a, &mut ent, 0, false)?;
                    }
                    walk_to(&mut m, fat, &path).0.remove(&name);
                }
                (88..=99, Some(e)) => {
                    // Anywhere: to a fresh name, or over a file (or empty
                    // directory) of the same kind; never into itself.
                    let to_path = all[rng.below(all.len() as u64) as usize].clone();
                    let mut moved = path.clone();
                    moved.push(name.clone());
                    if e.entry.kind() == Kind::Directory && to_path.starts_with(&moved) {
                        return Ok(());
                    }
                    let to = pick_name(&mut rng);
                    let e = e.clone();
                    let v = walk_to(&mut m, fat, &path).0.remove(&name).unwrap();
                    let (td, tstart) = walk_to(&mut m, fat, &to_path);
                    let target = fat.lookup(&(), tstart, &to).unwrap();
                    let ok = match (&target, td.get(&to)) {
                        (None, _) => true,
                        (Some(t), Some(MNode::Dir(c))) => {
                            t.entry.kind() == e.entry.kind() && c.is_empty() && t.loc != e.loc
                        }
                        (Some(t), _) => t.entry.kind() == e.entry.kind() && t.loc != e.loc,
                    };
                    // Back into the model as it was, if skipped.
                    let restore = |m: &mut MNode, v: MNode| {
                        walk_to(m, fat, &path).0.insert(name.clone(), v);
                    };
                    if !ok {
                        restore(&mut m, v);
                        return Ok(());
                    }
                    match fat.rename(&(), a, start, &e, tstart, &to, target.as_ref()) {
                        Ok(_) => {
                            if let Some(t) = &target
                                && t.entry.kind() == Kind::File
                            {
                                let mut ent = t.entry.clone();
                                fat.resize(&(), a, &mut ent, 0, false)?;
                            }
                            let (td, _) = walk_to(&mut m, fat, &to_path);
                            td.remove(&to);
                            td.insert(to, v);
                        }
                        Err(err) => {
                            restore(&mut m, v);
                            return Err(err);
                        }
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
    }
    m
}

/// The volume holds the model.
fn check(fat: &Fat<&[u8]>, m: &MNode, start: u32) {
    let MNode::Dir(want) = m else { unreachable!() };
    let mut got = std::collections::BTreeMap::new();
    fat.entries(&(), start, 0, &mut |n| {
        if n.name != b"." && n.name != b".." {
            got.insert(n.name.clone(), n.clone());
        }
        false
    })
    .unwrap();
    let names: Vec<_> = got.keys().cloned().collect();
    let wanted: Vec<_> = want.keys().cloned().collect();
    assert_eq!(names, wanted);
    for (k, v) in want {
        let n = &got[k];
        match v {
            MNode::Dir(_) => check(fat, v, n.entry.start),
            MNode::File(data) => {
                assert_eq!(n.entry.size as usize, data.len());
                let mut out = Vec::new();
                let mut off = 0u64;
                while off < data.len() as u64 {
                    let (at, run) = fat.bmap(&(), n.entry.start, off).unwrap().unwrap();
                    let len = run.min(data.len() as u64 - off) as usize;
                    fat.io()
                        .read(&(), at, len, &mut |b| out.extend_from_slice(b))
                        .unwrap();
                    off += len as u64;
                }
                assert!(
                    out == *data,
                    "contents of {:?}",
                    std::string::String::from_utf8_lossy(k)
                );
            }
        }
    }
}

fn fsck_clean(path: &Path) {
    let out = Command::new("fsck_msdosfs")
        .arg("-n")
        .arg(path)
        .output()
        .unwrap();
    let text = std::string::String::from_utf8_lossy(&out.stdout).into_owned()
        + &std::string::String::from_utf8_lossy(&out.stderr);
    let bad: Vec<&str> = text
        .lines()
        .filter(|l| l.contains('?') || l.contains("not correct"))
        .collect();
    assert!(
        bad.is_empty() && out.status.success(),
        "fsck_msdosfs:\n{text}"
    );
}

#[test]
fn random_workloads_leave_fsck_clean_volumes() {
    if !have_makefs() {
        std::eprintln!("no newfs_msdos (not FreeBSD): skipped");
        return;
    }
    let work = scratch("write");
    for (fat_type, size, seed) in [
        (12, "2m", 1u64),
        (16, "24m", 2),
        (32, "80m", 3),
        (16, "6m", 4),
    ] {
        let img = work.join(std::format!("w{fat_type}-{seed}.img"));
        let mut c = Command::new("newfs_msdos");
        c.args(["-C", size, "-F", &std::format!("{fat_type}")]);
        // Small clusters: FAT32 needs 65,525 of them, and the small FAT16
        // volume, which fills up, needs 4,085.
        if fat_type == 32 || size == "6m" {
            c.args(["-c", "1"]);
        }
        let ok = c
            .arg(&img)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "newfs_msdos");
        let bytes = std::fs::read(&img).unwrap();
        let len = bytes.len() as u64;
        let fat = Fat::open(&(), Mem(core::cell::RefCell::new(bytes)), len).unwrap();
        let mut a = fat.load_alloc(&()).unwrap();
        fat.set_clean(&(), false).unwrap();
        let model = workload(&fat, &mut a, seed, 2000);
        fat.write_fsinfo(&(), &mut a).unwrap();
        fat.set_clean(&(), true).unwrap();
        let out = fat.io().0.borrow().clone();
        let back = Fat::open(&(), &out[..], len).unwrap();
        check(&back, &model, ROOT);
        std::fs::write(&img, &out).unwrap();
        fsck_clean(&img);
        std::eprintln!("FAT{fat_type} {size} seed {seed}: clean");
    }
    let _ = std::fs::remove_dir_all(&work);
}
