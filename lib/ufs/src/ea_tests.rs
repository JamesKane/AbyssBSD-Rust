// SPDX-License-Identifier: BSD-2-Clause
//! Extended attributes: the record layout, an area FreeBSD wrote, and
//! areas this engine writes, grown, shrunk, and freed with their files,
//! with and without soft updates. On FreeBSD the images come from `newfs`
//! and end checked by `fsck_ffs`; elsewhere `UFS_EA_IMG` names a directory
//! of images the build server made (`ea-<bsize>.img`, `eas-<bsize>.img`
//! with soft updates, and `fb.img` with attributes FreeBSD set), and
//! `UFS_EA_OUT` a directory to leave the written images in, for `fsck_ffs`
//! there.

#![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

extern crate std;

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::cell::RefCell;
use std::path::PathBuf;
use std::process::Command;

use super::extattr::{self, NAMESPACE_SYSTEM as SYS, NAMESPACE_USER as USER};
use super::soft_tests::{Sim, check_crash, do_work, fsck_preens, settle};
use super::tests::{Mem, Rng, fsck_clean, have_makefs, scratch};
use super::*;

const NOW: Timestamp = Timestamp {
    sec: 1_760_000_000,
    nsec: 0,
};

fn maker() -> ops::Maker {
    ops::Maker {
        perm: 0o644,
        uid: 1001,
        gid: 1001,
        now: Timestamp { sec: 1, nsec: 0 },
        random: || 0x5eed_1234,
        reserve: Reserve::Allowed,
    }
}

/// Blank images to write in, `(name, bytes)`: with soft updates (`newfs
/// -U`) or without, for three block sizes.
fn blank_images(soft: bool) -> Vec<(std::string::String, Vec<u8>)> {
    let prefix = if soft { "eas" } else { "ea" };
    let sizes = [(4096u32, 512u32), (32768, 4096), (8192, 1024)];
    let mut out = Vec::new();
    if have_makefs() {
        let work = scratch(prefix);
        for (b, f) in sizes {
            let img = work.join(std::format!("{prefix}-{b}.img"));
            let mut newfs = Command::new("newfs");
            if soft {
                newfs.arg("-U");
            }
            let ok = Command::new("truncate")
                .args(["-s", "32m"])
                .arg(&img)
                .status()
                .unwrap()
                .success()
                && newfs
                    .args(["-b", &std::format!("{b}"), "-f", &std::format!("{f}")])
                    .arg(&img)
                    .stdout(std::process::Stdio::null())
                    .status()
                    .unwrap()
                    .success();
            assert!(ok, "newfs");
            out.push((std::format!("{prefix}-{b}"), std::fs::read(&img).unwrap()));
        }
        return out;
    }
    let Ok(dir) = std::env::var("UFS_EA_IMG") else {
        return out;
    };
    for (b, _) in sizes {
        let p = PathBuf::from(&dir).join(std::format!("{prefix}-{b}.img"));
        if let Ok(bytes) = std::fs::read(&p) {
            out.push((std::format!("{prefix}-{b}"), bytes));
        }
    }
    out
}

/// A written image checked: by `fsck_ffs -f -n` on FreeBSD, else left in
/// `UFS_EA_OUT` for it.
fn finish(name: &str, bytes: &[u8]) {
    if have_makefs() {
        let work = scratch(&std::format!("ea-out-{name}"));
        let img = work.join(std::format!("{name}.img"));
        std::fs::write(&img, bytes).unwrap();
        fsck_clean(&img);
        let _ = std::fs::remove_dir_all(&work);
    } else if let Ok(dir) = std::env::var("UFS_EA_OUT") {
        std::fs::write(PathBuf::from(dir).join(std::format!("{name}.img")), bytes).unwrap();
    }
}

#[test]
fn records_are_laid_out_as_freebsd_lays_them() {
    let mut area = Vec::new();
    extattr::set(&mut area, USER, b"a", b"hello").unwrap();
    // Length 16: the header and name padded to 8, the content to 8 with
    // 3 bytes of padding.
    assert_eq!(
        area,
        [
            16, 0, 0, 0, 1, 3, 1, b'a', b'h', b'e', b'l', b'l', b'o', 0, 0, 0
        ]
    );
    extattr::set(&mut area, SYS, b"sys", b"").unwrap();
    extattr::set(&mut area, USER, b"longer-name", b"12345678").unwrap();
    assert_eq!(area.len(), 16 + 16 + 32);
    assert_eq!(
        extattr::names(&area, USER).unwrap(),
        [b"a".to_vec(), b"longer-name".to_vec()]
    );
    assert_eq!(extattr::names(&area, SYS).unwrap(), [b"sys".to_vec()]);
    assert_eq!(extattr::find(&area, SYS, b"sys").unwrap(), Some(Vec::new()));
    assert_eq!(extattr::find(&area, SYS, b"a").unwrap(), None);
    // Replaced where it is, the records after it moved.
    extattr::set(&mut area, USER, b"a", b"a value past eight").unwrap();
    assert_eq!(
        extattr::names(&area, USER).unwrap(),
        [b"a".to_vec(), b"longer-name".to_vec()]
    );
    assert_eq!(
        extattr::find(&area, USER, b"a").unwrap().unwrap(),
        b"a value past eight"
    );
    assert_eq!(
        extattr::find(&area, USER, b"longer-name").unwrap().unwrap(),
        b"12345678"
    );
    assert!(extattr::remove(&mut area, SYS, b"sys").unwrap());
    assert!(!extattr::remove(&mut area, SYS, b"sys").unwrap());
    assert_eq!(area.len(), 32 + 32);
    // A zeroed tail ends the records; a record past the end, or whose
    // name overruns it, is corrupt.
    let mut tail = area.clone();
    tail.resize(200, 0);
    assert_eq!(extattr::names(&tail, USER).unwrap().len(), 2);
    let mut bad = area.clone();
    bad[0] = 200;
    assert!(matches!(
        extattr::names(&bad, USER),
        Err(Error::Integrity(_))
    ));
    let mut bad = area.clone();
    bad[6] = 40;
    assert!(extattr::find(&bad, USER, b"x").is_err());
    let mut bad = area;
    bad[5] = 30;
    assert!(extattr::find(&bad, USER, b"x").is_err());
}

/// The 5000-byte value `mkimg.sh` sets: the numbers from 1 run together.
fn big_value() -> Vec<u8> {
    let s: std::string::String = (1..=2000).map(|i| std::format!("{i}")).collect();
    s.as_bytes()[..5000].to_vec()
}

#[test]
fn attributes_freebsd_set_read_back() {
    let Some(img) = std::env::var("UFS_EA_IMG")
        .ok()
        .map(|d| PathBuf::from(d).join("fb.img"))
        .filter(|p| p.exists())
    else {
        std::eprintln!("no UFS_EA_IMG/fb.img: skipped");
        return;
    };
    let bytes = std::fs::read(img).unwrap();
    let fs = Ufs::open(&(), &bytes[..], 512).unwrap();
    let get = |path: &[u8], ns, name: &[u8]| {
        let (_, ip) = fs.namei(&(), path).unwrap().unwrap();
        fs.get_extattr(&(), &ip, ns, name).unwrap()
    };
    let (_, f1) = fs.namei(&(), b"/f1").unwrap().unwrap();
    // Replaced in place: `a` stays first.
    assert_eq!(
        fs.list_extattr(&(), &f1, USER).unwrap(),
        [b"a".to_vec(), b"b".to_vec()]
    );
    assert_eq!(fs.list_extattr(&(), &f1, SYS).unwrap(), [b"s".to_vec()]);
    assert_eq!(get(b"/f1", USER, b"a").unwrap(), b"hello-again");
    assert_eq!(get(b"/f1", USER, b"b").unwrap(), big_value());
    assert_eq!(get(b"/f1", SYS, b"s").unwrap(), b"sys");
    assert_eq!(get(b"/f1", USER, b"tmp"), None);
    // Past one block: two of them, the second its fragments.
    let bsize = u64::from(fs.superblock().bsize);
    assert!(u64::from(f1.extsize) > bsize && f1.extb[1] != 0);
    assert_eq!(get(b"/d", USER, b"x").unwrap(), b"1");
    assert_eq!(get(b"/p", USER, b"y").unwrap(), b"2");
    assert_eq!(get(b"/l", USER, b"z").unwrap(), b"3");
    assert_eq!(get(b"/f2", USER, b"e").unwrap(), b"");
    // What this engine would write for that area is what FreeBSD wrote.
    let mut area = Vec::new();
    for (ns, n, v) in [
        (USER, &b"a"[..], &b"hello"[..]),
        (USER, b"b", &big_value()),
        (SYS, b"s", b"sys"),
        (USER, b"tmp", b"gone"),
        (USER, b"a", b"hello-again"),
    ] {
        extattr::set(&mut area, ns, n, v).unwrap();
    }
    extattr::remove(&mut area, USER, b"tmp").unwrap();
    assert_eq!(fs.read_extattrs(&(), &f1).unwrap(), area);
}

/// The space an area of `extsize` bytes holds, in `di_blocks` units.
fn ext_units(sb: &Superblock, extsize: u32) -> u64 {
    let size = u64::from(extsize);
    let bsize = u64::from(sb.bsize);
    (0..size.div_ceil(bsize))
        .map(|i| u64::from(sb.block_size(size, i)) / 512)
        .sum()
}

#[test]
fn areas_grow_shrink_and_go_with_their_files() {
    let images = blank_images(false);
    if images.is_empty() {
        std::eprintln!("no images (not FreeBSD, no UFS_EA_IMG): skipped");
        return;
    }
    for (name, bytes) in images {
        let fs = Ufs::open(&(), Mem(RefCell::new(bytes)), 512).unwrap();
        let sb = fs.superblock().clone();
        let bsize = sb.bsize as usize;
        let mut sum = fs.load_summary(&()).unwrap();
        let start = sum.totals;
        let mut root = fs.inode(&(), ROOT_INO).unwrap();
        let mut made = Vec::new();
        for (n, kind) in [
            (&b"file"[..], ops::NewKind::Regular),
            (b"dir", ops::NewKind::Directory),
            (b"fifo", ops::NewKind::Fifo),
            (b"sock", ops::NewKind::Socket),
            (b"link", ops::NewKind::Symlink(b"short")),
        ] {
            let (ino, ip) = fs
                .create(&(), &mut sum, ROOT_INO, &mut root, n, kind, &maker())
                .unwrap();
            made.push((n, ino, ip.blocks));
        }
        let r = Reserve::Allowed;
        for &(_, ino, base) in &made {
            let mut ip = fs.inode(&(), ino).unwrap();
            assert_eq!(
                fs.list_extattr(&(), &ip, USER).unwrap(),
                Vec::<Vec<u8>>::new()
            );
            fs.set_extattr(&(), &mut sum, ino, &mut ip, USER, b"one", b"1", r, NOW)
                .unwrap();
            assert_eq!(ip.ctime, NOW);
            assert_eq!(ip.extsize, 24);
            assert_eq!(ip.blocks, base + ext_units(&sb, 24));
            // Growing across the first block into the second.
            let big = alloc::vec![7u8; bsize - 100];
            fs.set_extattr(&(), &mut sum, ino, &mut ip, SYS, b"big", &big, r, NOW)
                .unwrap();
            fs.set_extattr(
                &(),
                &mut sum,
                ino,
                &mut ip,
                USER,
                b"two",
                &big[..300],
                r,
                NOW,
            )
            .unwrap();
            assert!(ip.extsize as usize > bsize && ip.extb[1] != 0);
            assert_eq!(ip.blocks, base + ext_units(&sb, ip.extsize));
            let back = fs.inode(&(), ino).unwrap();
            assert_eq!(back.extsize, ip.extsize);
            assert_eq!(
                fs.get_extattr(&(), &back, SYS, b"big").unwrap().unwrap(),
                big
            );
            assert_eq!(
                fs.get_extattr(&(), &back, USER, b"two").unwrap().unwrap(),
                &big[..300]
            );
            assert_eq!(
                fs.list_extattr(&(), &back, USER).unwrap(),
                [b"one".to_vec(), b"two".to_vec()]
            );
            // Replacing keeps the order; past the area's limit is no space,
            // a value past it invalid.
            fs.set_extattr(&(), &mut sum, ino, &mut ip, USER, b"one", b"uno!", r, NOW)
                .unwrap();
            assert_eq!(
                fs.list_extattr(&(), &ip, USER).unwrap(),
                [b"one".to_vec(), b"two".to_vec()]
            );
            let huge = alloc::vec![1u8; bsize * 2 - 64];
            assert_eq!(
                fs.set_extattr(&(), &mut sum, ino, &mut ip, USER, b"huge", &huge, r, NOW),
                Err(Error::NoSpace)
            );
            let huger = alloc::vec![1u8; bsize * 2 + 1];
            assert_eq!(
                fs.set_extattr(&(), &mut sum, ino, &mut ip, USER, b"huge", &huger, r, NOW),
                Err(Error::Invalid)
            );
            // Shrinking keeps the area (FreeBSD's `ffs_close_ea`), its
            // tail zeroed.
            let size = ip.extsize;
            assert!(
                fs.delete_extattr(&(), &mut sum, ino, &mut ip, SYS, b"big", NOW)
                    .unwrap()
            );
            assert!(
                !fs.delete_extattr(&(), &mut sum, ino, &mut ip, SYS, b"big", NOW)
                    .unwrap()
            );
            assert_eq!(ip.extsize, size);
            assert_eq!(fs.get_extattr(&(), &ip, SYS, b"big").unwrap(), None);
            assert_eq!(
                fs.get_extattr(&(), &ip, USER, b"one").unwrap().unwrap(),
                b"uno!"
            );
        }
        // The last attribute gone, the area goes.
        let &(_, ino, base) = &made[0];
        let mut ip = fs.inode(&(), ino).unwrap();
        for n in [&b"one"[..], b"two"] {
            assert!(
                fs.delete_extattr(&(), &mut sum, ino, &mut ip, USER, n, NOW)
                    .unwrap()
            );
        }
        assert_eq!((ip.extsize, ip.extb, ip.blocks), (0, [0; NXADDR], base));
        assert_eq!(fs.inode(&(), ino).unwrap().extb, [0; NXADDR]);
        // The rest go with their files.
        for &(n, ino, _) in &made {
            let mut ip = fs.inode(&(), ino).unwrap();
            if ip.kind() == Kind::Directory {
                fs.rmdir(&(), &mut sum, ROOT_INO, &mut root, n, ino, &mut ip, NOW)
                    .unwrap();
            } else {
                fs.unlink(&(), ROOT_INO, &mut root, n, ino, &mut ip, NOW)
                    .unwrap();
            }
            fs.free_inode(&(), &mut sum, ino, &mut ip, r).unwrap();
            let gone = fs.inode(&(), ino).unwrap();
            assert_eq!((gone.mode, gone.extsize, gone.extb), (0, 0, [0; NXADDR]));
        }
        assert_eq!(sum.totals.nbfree, start.nbfree, "{name}: blocks leaked");
        assert_eq!(sum.totals.nffree, start.nffree, "{name}: fragments leaked");
        // One file keeps its attributes, for fsck to see.
        let (ino, mut ip) = fs
            .create(
                &(),
                &mut sum,
                ROOT_INO,
                &mut root,
                b"kept",
                ops::NewKind::Regular,
                &maker(),
            )
            .unwrap();
        fs.set_extattr(
            &(),
            &mut sum,
            ino,
            &mut ip,
            USER,
            b"k",
            &big_value(),
            r,
            NOW,
        )
        .unwrap();
        fs.write_summary(&(), &mut sum, true, 1_760_000_100)
            .unwrap();
        let bytes = fs.io().0.borrow().clone();
        finish(&std::format!("grow-{name}"), &bytes);
        std::eprintln!("{name}: areas grown, shrunk, and freed");
    }
}

/// What the files' attributes should be: in each file's record order.
type Attrs = BTreeMap<u32, Vec<(u8, Vec<u8>, Vec<u8>)>>;

/// The engine's attributes of every file in `want` are as it says.
fn check_attrs<B: BlockIo<Cx = ()>>(fs: &Ufs<B>, want: &Attrs, what: &str) {
    for (&ino, list) in want {
        let ip = fs.inode(&(), ino).unwrap();
        for ns in [USER, SYS] {
            let names: Vec<Vec<u8>> = list
                .iter()
                .filter(|(n, _, _)| *n == ns)
                .map(|(_, k, _)| k.clone())
                .collect();
            assert_eq!(
                fs.list_extattr(&(), &ip, ns).unwrap(),
                names,
                "{what}: {ino}"
            );
        }
        for (ns, k, v) in list {
            assert_eq!(
                fs.get_extattr(&(), &ip, *ns, k).unwrap().as_ref(),
                Some(v),
                "{what}: {ino}"
            );
        }
        if list.is_empty() {
            assert_eq!(ip.extsize, 0, "{what}: {ino} keeps an area");
        }
    }
}

/// Random attribute changes on files of every kind, mirrored in a model,
/// files removed and made again; `after` runs after each step.
fn ea_workload<B: BlockIo<Cx = ()>>(
    fs: &Ufs<B>,
    sum: &mut Summary,
    seed: u64,
    steps: usize,
    after: &mut dyn FnMut(&Ufs<B>, &mut Summary),
) -> Attrs {
    let soft = fs.soft_updates();
    let bsize = u64::from(fs.superblock().bsize);
    let mut rng = Rng(seed);
    let mut want: Attrs = BTreeMap::new();
    let mut names: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
    let r = Reserve::Allowed;
    for step in 0..steps {
        let mut root = fs.inode(&(), ROOT_INO).unwrap();
        let op = rng.below(100);
        let files: Vec<u32> = want.keys().copied().collect();
        let pick = (!files.is_empty()).then(|| files[rng.below(files.len() as u64) as usize]);
        match (op, pick) {
            (0..=9, _) | (_, None) if want.len() < 12 => {
                let n = std::format!("f{step}").into_bytes();
                let kind = match rng.below(4) {
                    0 => ops::NewKind::Regular,
                    1 => ops::NewKind::Directory,
                    2 => ops::NewKind::Fifo,
                    _ => ops::NewKind::Symlink(b"target"),
                };
                match fs.create(&(), sum, ROOT_INO, &mut root, &n, kind, &maker()) {
                    Ok((ino, _)) => {
                        want.insert(ino, Vec::new());
                        names.insert(ino, n);
                    }
                    Err(Error::NoSpace) => {}
                    Err(e) => panic!("create: {e:?}"),
                }
            }
            (10..=14, Some(ino)) => {
                let n = names.remove(&ino).unwrap();
                want.remove(&ino);
                let mut ip = fs.inode(&(), ino).unwrap();
                if ip.kind() == Kind::Directory {
                    fs.rmdir(&(), sum, ROOT_INO, &mut root, &n, ino, &mut ip, NOW)
                        .unwrap();
                } else {
                    fs.unlink(&(), ROOT_INO, &mut root, &n, ino, &mut ip, NOW)
                        .unwrap();
                }
                if !soft {
                    fs.free_inode(&(), sum, ino, &mut ip, r).unwrap();
                }
            }
            (15..=39, Some(ino)) => {
                let list = want.get_mut(&ino).unwrap();
                let Some(i) = (!list.is_empty()).then(|| rng.below(list.len() as u64) as usize)
                else {
                    continue;
                };
                let (ns, k, _) = list[i].clone();
                let mut ip = fs.inode(&(), ino).unwrap();
                assert!(
                    fs.delete_extattr(&(), sum, ino, &mut ip, ns, &k, NOW)
                        .unwrap()
                );
                list.remove(i);
            }
            (_, Some(ino)) => {
                let ns = if rng.below(4) == 0 { SYS } else { USER };
                let k = std::format!("attr{}", rng.below(6)).into_bytes();
                let len = match rng.below(4) {
                    0 => rng.below(bsize),
                    _ => rng.below(64),
                };
                let v: Vec<u8> = (0..len).map(|_| rng.next() as u8).collect();
                let mut ip = fs.inode(&(), ino).unwrap();
                let list = want.get_mut(&ino).unwrap();
                match fs.set_extattr(&(), sum, ino, &mut ip, ns, &k, &v, r, NOW) {
                    Ok(()) => match list.iter_mut().find(|(n, kk, _)| *n == ns && *kk == k) {
                        Some(e) => e.2 = v,
                        None => list.push((ns, k, v)),
                    },
                    Err(Error::NoSpace) => {
                        // Too big for the area, or the disk: nothing lost.
                    }
                    Err(e) => panic!("set: {e:?}"),
                }
            }
            _ => {}
        }
        after(fs, sum);
        if step % 50 == 0 {
            check_attrs(fs, &want, &std::format!("step {step}"));
        }
    }
    want
}

#[test]
fn random_attribute_changes_stay_consistent() {
    let images = blank_images(false);
    if images.is_empty() {
        std::eprintln!("no images (not FreeBSD, no UFS_EA_IMG): skipped");
        return;
    }
    for (name, bytes) in images {
        let fs = Ufs::open(&(), Mem(RefCell::new(bytes)), 512).unwrap();
        let mut sum = fs.load_summary(&()).unwrap();
        let want = ea_workload(&fs, &mut sum, 17, 1500, &mut |_, _| {});
        check_attrs(&fs, &want, &name);
        fs.write_summary(&(), &mut sum, true, 1_760_000_100)
            .unwrap();
        let bytes = fs.io().0.borrow().clone();
        finish(&std::format!("random-{name}"), &bytes);
        std::eprintln!("{name}: {} files with attributes, consistent", want.len());
    }
}

#[test]
fn soft_updates_order_attribute_blocks() {
    let images = blank_images(true);
    if images.is_empty() {
        std::eprintln!("no images (not FreeBSD, no UFS_EA_IMG): skipped");
        return;
    }
    for (name, bytes) in images {
        for seed in [5u64, 7, 9] {
            let mut fs = Ufs::open(&(), Sim::new(bytes.clone()), 512).unwrap();
            assert!(fs.use_soft_updates(&()));
            let mut sum = fs.load_summary(&()).unwrap();
            fs.write_summary(&(), &mut sum, false, 1_760_000_000)
                .unwrap();
            let mut rng = Rng(seed * 7919);
            let mut step = 0;
            let mut crashes = 0;
            let want = ea_workload(&fs, &mut sum, seed, 1200, &mut |fs, sum| {
                step += 1;
                let io = fs.io();
                if rng.below(2) == 0 {
                    io.write_some(&mut rng);
                }
                if rng.below(3) == 0 {
                    do_work(fs, sum);
                }
                if rng.below(15) == 0 {
                    let image = io.disk.borrow().clone();
                    if let Err(e) = check_crash(&image) {
                        panic!("{name} seed {seed}: crash after step {step}:\n{e}");
                    }
                    // Some go to `fsck_ffs -p` too.
                    if crashes % 10 == 0 {
                        let crash = std::format!("crash-{name}-{seed}-{step}");
                        if have_makefs() {
                            let work = scratch(&crash);
                            let img = work.join("crash.img");
                            std::fs::write(&img, &image).unwrap();
                            if let Err(e) = fsck_preens(&img) {
                                panic!("{crash}:\n{e}");
                            }
                            let _ = std::fs::remove_dir_all(&work);
                        } else if let Ok(dir) = std::env::var("UFS_EA_OUT") {
                            std::fs::write(PathBuf::from(dir).join(crash + ".img"), &image)
                                .unwrap();
                        }
                    }
                    crashes += 1;
                }
            });
            settle(&fs, &mut sum);
            check_attrs(&fs, &want, &name);
            fs.write_summary(&(), &mut sum, true, 1_760_000_100)
                .unwrap();
            let bytes = fs.io().disk.borrow().clone();
            check_crash(&bytes).unwrap();
            let back = Ufs::open(&(), &bytes[..], 512).unwrap();
            check_attrs(&back, &want, &name);
            finish(&std::format!("soft-{name}-{seed}"), &bytes);
            std::eprintln!(
                "{name} seed {seed}: {} files, {crashes} crash images checked",
                want.len()
            );
        }
    }
}
