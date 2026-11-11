// SPDX-License-Identifier: BSD-2-Clause
//! The engine against hand-made records everywhere, and against FreeBSD's
//! `makefs -t cd9660` images (plain, Rock Ridge, Joliet) where `makefs`
//! is: FreeBSD (`cargo xtask --remote test --tier unit`).

#![allow(clippy::cast_possible_truncation)]

extern crate std;

use alloc::vec;
use alloc::vec::Vec;
use std::path::Path;
use std::process::Command;

use super::*;

/// An image in memory.
struct Img(Vec<u8>);

impl BlockIo for Img {
    type Cx = ();

    fn read(&self, _: &(), offset: u64, len: usize, f: &mut dyn FnMut(&[u8])) -> Result<(), Error> {
        let at = usize::try_from(offset).map_err(|_| Error::Io)?;
        f(self.0.get(at..at + len).ok_or(Error::Io)?);
        Ok(())
    }
}

#[test]
fn names_compare_and_translate_as_freebsd() {
    use name::{isofncmp, isofntrans};
    assert_eq!(isofncmp(b"file.txt", b"FILE.TXT;1", 0), 0);
    assert_eq!(isofncmp(b"FILE.TXT", b"FILE.TXT;1", 0), 0);
    assert_eq!(isofncmp(b"file.txt;1", b"FILE.TXT;1", 0), 0);
    assert_ne!(isofncmp(b"file.txt;2", b"FILE.TXT;1", 0), 0);
    // A trailing dot before the version may be left out.
    assert_eq!(isofncmp(b"readme", b"README.;1", 0), 0);
    assert_ne!(isofncmp(b"readm", b"README.;1", 0), 0);
    assert_ne!(isofncmp(b"readmex", b"README.;1", 0), 0);
    assert_eq!(isofntrans(b"FILE.TXT;1", false, false, 0), b"file.txt");
    assert_eq!(isofntrans(b"README.;1", false, false, 0), b"readme");
    assert_eq!(isofntrans(b"FILE.TXT;1", true, false, 0), b"FILE.TXT;1");
    assert_eq!(isofntrans(b"ASSOC;1", false, true, 0), b"=assoc");
    // Joliet: UCS-2, case kept, beyond Latin-1 a `?`.
    let j: Vec<u8> = "Mixed é€;1"
        .encode_utf16()
        .flat_map(u16::to_be_bytes)
        .collect();
    assert_eq!(isofntrans(&j, false, false, 1), b"Mixed \xe9?");
    let j: Vec<u8> = "Mixed;1"
        .encode_utf16()
        .flat_map(u16::to_be_bytes)
        .collect();
    assert_eq!(isofncmp(b"Mixed", &j, 1), 0);
    // Upper case recorded matches either case, Joliet too.
    assert_eq!(isofncmp(b"mixed", &j, 1), 0);
    assert_ne!(isofncmp(b"MIXED", &j, 1), 0);
}

#[test]
fn times_convert_as_freebsd() {
    // 2024-01-15 12:30:42 UTC.
    let (t, ok) = tstamp7(&[124, 1, 15, 12, 30, 42, 0], false);
    assert!(ok);
    assert_eq!(t.sec, 1_705_321_842);
    // An hour east of UTC is an hour earlier.
    assert_eq!(
        tstamp7(&[124, 1, 15, 12, 30, 42, 4], false).0.sec,
        1_705_321_842 - 3600
    );
    // Offsets out of range are ignored, as are those of High Sierra.
    assert_eq!(
        tstamp7(&[124, 1, 15, 12, 30, 42, 100], false).0.sec,
        1_705_321_842
    );
    assert_eq!(
        tstamp7(&[124, 1, 15, 12, 30, 42, 4], true).0.sec,
        1_705_321_842
    );
    assert_eq!(
        tstamp7(&[60, 1, 1, 0, 0, 0, 0], false),
        (Time::default(), false)
    );
    let (t, ok) = tstamp17(b"2024011512304200\0");
    assert!(ok);
    assert_eq!(t.sec, 1_705_321_842);
}

/// A directory record: `name`, extent, size, flags, and System Use.
fn record(name: &[u8], extent: u32, size: u32, flags: u8, su: &[u8]) -> Vec<u8> {
    let mut r = vec![0u8; 33];
    r.extend_from_slice(name);
    if name.len().is_multiple_of(2) {
        r.push(0);
    }
    r.extend_from_slice(su);
    if r.len() % 2 == 1 {
        r.push(0);
    }
    r[0] = r.len() as u8;
    r[2..6].copy_from_slice(&extent.to_le_bytes());
    r[6..10].copy_from_slice(&extent.to_be_bytes());
    r[10..14].copy_from_slice(&size.to_le_bytes());
    r[14..18].copy_from_slice(&size.to_be_bytes());
    r[18..25].copy_from_slice(&[124, 1, 15, 12, 30, 42, 0]);
    r[25] = flags;
    r[28] = 1;
    r[32] = name.len() as u8;
    r
}

/// A SUSP entry.
fn su(kind: &[u8; 2], body: &[u8]) -> Vec<u8> {
    let mut e = vec![kind[0], kind[1], (4 + body.len()) as u8, 1];
    e.extend_from_slice(body);
    e
}

fn both32(n: u32) -> Vec<u8> {
    let mut v = n.to_le_bytes().to_vec();
    v.extend_from_slice(&n.to_be_bytes());
    v
}

/// A small Rock Ridge volume made by hand: the root (block 18) holds
/// `hello` (block 20, "hello, world\n"), a link `ln -> hello`, and an
/// entry whose `NM` overruns its record (malformed: its ISO name is
/// used).
fn handmade(rock_ridge: bool) -> Img {
    let mut img = vec![0u8; 2048 * 24];
    // The primary descriptor, then the terminator.
    let pvd = &mut img[16 * 2048..17 * 2048];
    pvd[0] = 1;
    pvd[1..6].copy_from_slice(b"CD001");
    pvd[6] = 1;
    pvd[80..88].copy_from_slice(&both32(24));
    pvd[128..130].copy_from_slice(&2048u16.to_le_bytes());
    pvd[130..132].copy_from_slice(&2048u16.to_be_bytes());
    let root = record(&[0], 18, 2048, 2, &[]);
    pvd[156..156 + 34].copy_from_slice(&root[..34]);
    img[17 * 2048] = 255;
    img[17 * 2048 + 1..17 * 2048 + 6].copy_from_slice(b"CD001");
    let px = |mode: u32| {
        let mut b = both32(mode);
        b.extend(both32(1));
        b.extend(both32(1001));
        b.extend(both32(20));
        su(b"PX", &b)
    };
    let mut dot_su = Vec::new();
    if rock_ridge {
        dot_su.extend(su(b"SP", &[0xbe, 0xef, 0]));
        let mut er = vec![10, 0, 0, 1];
        er.extend_from_slice(b"RRIP_1991A");
        dot_su.extend(su(b"ER", &er));
        dot_su.extend(px(0o40755));
    }
    let mut dir = Vec::new();
    dir.extend(record(&[0], 18, 2048, 2, &dot_su));
    dir.extend(record(
        &[1],
        18,
        2048,
        2,
        &if rock_ridge { px(0o40755) } else { Vec::new() },
    ));
    let mut hello_su = Vec::new();
    if rock_ridge {
        hello_su.extend(px(0o100_644));
        let mut nm = vec![0];
        nm.extend_from_slice(b"hello");
        hello_su.extend(su(b"NM", &nm));
        // Modified and accessed, 7-byte form.
        let mut tf = vec![TS_MODIFY_FOR_TEST | 0x04];
        tf.extend_from_slice(&[124, 1, 15, 12, 30, 42, 0]);
        tf.extend_from_slice(&[124, 1, 16, 0, 0, 0, 0]);
        hello_su.extend(su(b"TF", &tf));
    }
    dir.extend(record(b"HELLO.;1", 20, 13, 0, &hello_su));
    let mut ln_su = Vec::new();
    if rock_ridge {
        ln_su.extend(px(0o120_777));
        let mut nm = vec![0];
        nm.extend_from_slice(b"ln");
        ln_su.extend(su(b"NM", &nm));
        let mut sl = vec![0, 0, 5];
        sl.extend_from_slice(b"hello");
        ln_su.extend(su(b"SL", &sl));
    }
    dir.extend(record(b"LN.;1", 21, 0, 0, &ln_su));
    if rock_ridge {
        // An `NM` claiming more than its record holds.
        let mut bad = record(b"BAD.;1", 21, 0, 0, &su(b"NM", &[0, b'x']));
        let at = bad.len() - 6 + 2;
        bad[at] = 60;
        dir.extend(bad);
    }
    img[18 * 2048..18 * 2048 + dir.len()].copy_from_slice(&dir);
    img[20 * 2048..20 * 2048 + 13].copy_from_slice(b"hello, world\n");
    Img(img)
}

const TS_MODIFY_FOR_TEST: u8 = 0x02;

fn listing(v: &Volume, io: &Img, dir: &Inode) -> Vec<(Vec<u8>, u64)> {
    let mut out = Vec::new();
    v.readdir(io, &(), dir, 0, &mut |e| {
        out.push((e.name.to_vec(), e.ino));
        true
    })
    .unwrap();
    out
}

#[test]
fn a_handmade_rock_ridge_volume_reads_as_freebsd_reads_it() {
    let img = handmade(true);
    let v = Volume::open(&img, &(), 512, &Options::default()).unwrap();
    assert_eq!(v.ftype, FType::Rrip);
    let root = v.load(&img, &(), v.root_ino(), None).unwrap();
    assert_eq!(root.ino, 18 * 2048);
    assert_eq!(root.kind(), S_IFDIR);
    assert_eq!(v.mode_of(&root), 0o755);
    let names: Vec<Vec<u8>> = listing(&v, &img, &root).into_iter().map(|e| e.0).collect();
    assert_eq!(
        names,
        [&b"."[..], b"..", b"hello", b"ln", b"BAD.;1"].map(<[u8]>::to_vec)
    );
    let f = v.lookup(&img, &(), &root, b"hello").unwrap();
    assert_eq!(v.lookup(&img, &(), &root, b"HELLO"), Err(Error::NotFound));
    let hello = v.load(&img, &(), f.ino, Some(&f)).unwrap();
    assert_eq!(
        (hello.kind(), v.mode_of(&hello), hello.uid, hello.gid),
        (S_IFREG, 0o644, 1001, 20)
    );
    assert_eq!(hello.mtime.sec, 1_705_321_842);
    assert_eq!(hello.ctime, hello.mtime);
    assert_eq!(hello.atime.sec, 1_705_363_200);
    assert_eq!(v.data_offset(&hello, 0), 20 * 2048);
    // Loaded by number alone, the same file.
    assert_eq!(v.load(&img, &(), f.ino, None).unwrap(), hello);
    let l = v.lookup(&img, &(), &root, b"ln").unwrap();
    let ln = v.load(&img, &(), l.ino, Some(&l)).unwrap();
    assert_eq!(ln.kind(), S_IFLNK);
    assert_eq!(v.readlink(&img, &(), &ln, b"/mnt").unwrap(), b"hello");
    // Mounted with `-o uid=,gid=,mask=`.
    let o = Options {
        uid: Some(5),
        gid: Some(6),
        mask: Some(0o700),
        ..Options::default()
    };
    let v2 = Volume::open(&img, &(), 512, &o).unwrap();
    assert_eq!(
        (v2.uid_of(&hello), v2.gid_of(&hello), v2.mode_of(&hello)),
        (5, 6, 0o600)
    );
    // `-r`: the plain names.
    let plain = Volume::open(
        &img,
        &(),
        512,
        &Options {
            norrip: true,
            ..Options::default()
        },
    )
    .unwrap();
    assert_eq!(plain.ftype, FType::Default);
    let root = plain.load(&img, &(), plain.root_ino(), None).unwrap();
    let names: Vec<Vec<u8>> = listing(&plain, &img, &root)
        .into_iter()
        .map(|e| e.0)
        .collect();
    assert_eq!(
        names,
        [&b"."[..], b"..", b"hello", b"ln", b"bad"].map(<[u8]>::to_vec)
    );
    let f = plain.lookup(&img, &(), &root, b"HELLO").unwrap();
    let hello = plain.load(&img, &(), f.ino, Some(&f)).unwrap();
    assert_eq!(plain.mode_of(&hello), 0o555);
    assert_eq!(plain.readlink(&img, &(), &hello, b"/"), Err(Error::Invalid));
}

#[test]
fn garbage_is_refused_not_trusted() {
    let mut img = handmade(true);
    // Not a volume.
    let mut blank = Img(vec![0u8; 2048 * 24]);
    assert_eq!(
        Volume::open(&blank, &(), 512, &Options::default()).unwrap_err(),
        Error::Invalid
    );
    blank.0[16 * 2048 + 1..16 * 2048 + 6].copy_from_slice(b"CD001");
    assert!(Volume::open(&blank, &(), 512, &Options::default()).is_err());
    // A root `ER` too short for its kind: EINVAL (PR 296853).
    let er = img.0[18 * 2048..19 * 2048]
        .windows(2)
        .position(|w| w == b"ER")
        .unwrap()
        + 18 * 2048;
    img.0[er + 2] = 6;
    assert_eq!(
        Volume::open(&img, &(), 512, &Options::default()).unwrap_err(),
        Error::Invalid
    );
    // A sector size ISO blocks are not made of.
    assert_eq!(
        Volume::open(&handmade(false), &(), 4096, &Options::default()).unwrap_err(),
        Error::Invalid
    );
    // Every byte of the root directory flipped in turn: never a panic.
    let base = handmade(true);
    for i in 0..300 {
        let mut img = Img(base.0.clone());
        img.0[18 * 2048 + i] ^= 0xa5;
        if let Ok(v) = Volume::open(&img, &(), 512, &Options::default())
            && let Ok(root) = v.load(&img, &(), v.root_ino(), None)
        {
            let _ = v.readdir(&img, &(), &root, 0, &mut |_| true);
            if let Ok(f) = v.lookup(&img, &(), &root, b"ln")
                && let Ok(ip) = v.load(&img, &(), f.ino, Some(&f))
            {
                let _ = v.readlink(&img, &(), &ip, b"/");
            }
        }
    }
}

fn have_makefs() -> bool {
    Command::new("makefs").arg("-h").output().is_ok()
}

fn scratch(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(std::format!("abyss-cd9660-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A tree to image: files, a deep directory (Rock Ridge relocates past
/// eight levels), a symbolic link, and names ISO 9660 cannot hold.
fn tree(dir: &Path) {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let deep = dir.join("a/b/c/d/e/f/g/h/i/j");
    std::fs::create_dir_all(&deep).unwrap();
    std::fs::write(deep.join("deep.txt"), b"deep\n").unwrap();
    std::fs::write(dir.join("README.TXT"), b"read me\n").unwrap();
    std::fs::write(dir.join("Mixed Case Long Name.text"), b"long\n").unwrap();
    let big: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
    std::fs::write(dir.join("big.bin"), &big).unwrap();
    std::fs::write(dir.join("exec.sh"), b"#!/bin/sh\n").unwrap();
    std::fs::set_permissions(dir.join("exec.sh"), std::fs::Permissions::from_mode(0o751)).unwrap();
    symlink("README.TXT", dir.join("link")).unwrap();
    symlink("../../../README.TXT", dir.join("a/b/c/up")).unwrap();
}

fn makefs(out: &Path, src: &Path, opts: &str) {
    let ok = Command::new("makefs")
        .args(["-t", "cd9660"])
        .args(if opts.is_empty() {
            Vec::new()
        } else {
            std::vec!["-o", opts]
        })
        .arg(out)
        .arg(src)
        .status()
        .unwrap()
        .success();
    assert!(ok, "makefs -o {opts}");
}

/// Reads `path` (components) from the root, as `namei` would.
fn walk_to(v: &Volume, io: &Img, path: &[&[u8]]) -> Result<Inode, Error> {
    let mut cur = v.load(io, &(), v.root_ino(), None)?;
    for c in path {
        let f = v.lookup(io, &(), &cur, c)?;
        cur = v.load(io, &(), f.ino, Some(&f))?;
    }
    Ok(cur)
}

fn contents(v: &Volume, io: &Img, ip: &Inode) -> Vec<u8> {
    let at = usize::try_from(v.data_offset(ip, 0)).unwrap();
    io.0[at..at + usize::try_from(ip.size).unwrap()].to_vec()
}

#[test]
fn makefs_images_read_back() {
    if !have_makefs() {
        std::eprintln!("no makefs (not FreeBSD): skipped");
        return;
    }
    let dir = scratch("tree");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    tree(&src);
    let big: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
    // Rock Ridge.
    let rr = dir.join("rr.iso");
    makefs(&rr, &src, "rockridge");
    let io = Img(std::fs::read(&rr).unwrap());
    let v = Volume::open(&io, &(), 512, &Options::default()).unwrap();
    assert_eq!(v.ftype, FType::Rrip);
    let root = v.load(&io, &(), v.root_ino(), None).unwrap();
    let mut names: Vec<Vec<u8>> = listing(&v, &io, &root).into_iter().map(|e| e.0).collect();
    names.sort();
    let want: Vec<Vec<u8>> = [
        &b"."[..],
        b"..",
        b"Mixed Case Long Name.text",
        b"README.TXT",
        b"a",
        b"big.bin",
        b"exec.sh",
        b"link",
    ]
    .map(<[u8]>::to_vec)
    .into_iter()
    .collect();
    // makefs adds its relocation directory (`.rr_moved`), which shows.
    names.retain(|n| n != b".rr_moved" && n != b"rr_moved");
    assert_eq!(names, want);
    let deep = walk_to(
        &v,
        &io,
        &[
            b"a",
            b"b",
            b"c",
            b"d",
            b"e",
            b"f",
            b"g",
            b"h",
            b"i",
            b"j",
            b"deep.txt",
        ],
    )
    .unwrap();
    assert_eq!(contents(&v, &io, &deep), b"deep\n");
    assert_eq!(
        contents(&v, &io, &walk_to(&v, &io, &[b"big.bin"]).unwrap()),
        big
    );
    let exec = walk_to(&v, &io, &[b"exec.sh"]).unwrap();
    assert_eq!(v.mode_of(&exec), 0o751);
    let link = walk_to(&v, &io, &[b"link"]).unwrap();
    assert_eq!(link.kind(), S_IFLNK);
    assert_eq!(v.readlink(&io, &(), &link, b"/mnt").unwrap(), b"README.TXT");
    let up = walk_to(&v, &io, &[b"a", b"b", b"c", b"up"]).unwrap();
    assert_eq!(
        v.readlink(&io, &(), &up, b"/mnt").unwrap(),
        b"../../../README.TXT"
    );
    // A directory's `..` is its parent, relocated or not.
    let j = walk_to(
        &v,
        &io,
        &[b"a", b"b", b"c", b"d", b"e", b"f", b"g", b"h", b"i", b"j"],
    )
    .unwrap();
    let i = walk_to(
        &v,
        &io,
        &[b"a", b"b", b"c", b"d", b"e", b"f", b"g", b"h", b"i"],
    )
    .unwrap();
    assert_eq!(j.parent, i.ino);
    // Neither (`-r`): ISO 9660 names, lower case.
    let pv = Volume::open(
        &io,
        &(),
        512,
        &Options {
            norrip: true,
            nojoliet: true,
            ..Options::default()
        },
    )
    .unwrap();
    assert_eq!(pv.joliet_level, 0);
    assert_eq!(pv.ftype, FType::Default);
    let r = walk_to(&pv, &io, &[b"readme.txt"]).unwrap();
    assert_eq!(contents(&pv, &io, &r), b"read me\n");
    let root = pv.load(&io, &(), pv.root_ino(), None).unwrap();
    assert!(
        listing(&pv, &io, &root)
            .iter()
            .any(|e| e.0 == b"readme.txt")
    );
    // `-g` keeps versions and case.
    let gv = Volume::open(
        &io,
        &(),
        512,
        &Options {
            norrip: true,
            nojoliet: true,
            gens: true,
            ..Options::default()
        },
    )
    .unwrap();
    assert_eq!(gv.ftype, FType::Iso9660);
    let root = gv.load(&io, &(), gv.root_ino(), None).unwrap();
    assert!(
        listing(&gv, &io, &root)
            .iter()
            .any(|e| e.0 == b"README.TXT;1")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn libarchive_joliet_and_rock_ridge_read_back() {
    // `bsdtar` writes Joliet, which `makefs` does not.
    if Command::new("bsdtar").arg("--version").output().is_err() || !have_makefs() {
        std::eprintln!("no bsdtar or not FreeBSD: skipped");
        return;
    }
    let dir = scratch("joliet");
    let src = dir.join("src");
    std::fs::create_dir_all(&src).unwrap();
    tree(&src);
    let iso = dir.join("j.iso");
    let ok = Command::new("bsdtar")
        .args(["-c", "-f"])
        .arg(&iso)
        .args([
            "--format",
            "iso9660",
            "--options",
            "iso9660:rockridge=strict,iso9660:joliet",
            "-C",
        ])
        .arg(&src)
        .arg(".")
        .status()
        .unwrap()
        .success();
    assert!(ok, "bsdtar");
    let io = Img(std::fs::read(&iso).unwrap());
    // Rock Ridge first.
    let v = Volume::open(&io, &(), 512, &Options::default()).unwrap();
    assert_eq!(v.ftype, FType::Rrip);
    assert_eq!(v.joliet_level, 0);
    let exec = walk_to(&v, &io, &[b"exec.sh"]).unwrap();
    assert_eq!(v.mode_of(&exec), 0o751);
    // Joliet when Rock Ridge is ignored (`-r`): names as given, any case.
    let jv = Volume::open(
        &io,
        &(),
        512,
        &Options {
            norrip: true,
            ..Options::default()
        },
    )
    .unwrap();
    assert!(jv.joliet_level > 0);
    let f = walk_to(&jv, &io, &[b"Mixed Case Long Name.text"]).unwrap();
    assert_eq!(contents(&jv, &io, &f), b"long\n");
    let root = jv.load(&io, &(), jv.root_ino(), None).unwrap();
    assert!(
        listing(&jv, &io, &root)
            .iter()
            .any(|e| e.0 == b"Mixed Case Long Name.text")
    );
    let deep = walk_to(
        &jv,
        &io,
        &[
            b"a",
            b"b",
            b"c",
            b"d",
            b"e",
            b"f",
            b"g",
            b"h",
            b"i",
            b"j",
            b"deep.txt",
        ],
    )
    .unwrap();
    assert_eq!(contents(&jv, &io, &deep), b"deep\n");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Where `fetch-base` keeps the ISO test volume and FreeBSD's listings of
/// it: the workstation's copy, or the server's own.
fn iso_test_dir() -> Option<std::path::PathBuf> {
    let here = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build/dist/15.1-RELEASE/amd64");
    let home = std::env::var_os("HOME")
        .map(|h| Path::new(&h).join("abyss/dist/15.1-RELEASE/amd64"))
        .unwrap_or_default();
    [here, home]
        .into_iter()
        .find(|d| d.join("iso-test.iso").exists() && d.join("iso-test-rr.ls").exists())
}

/// `ls -l`'s mode column.
fn mode_string(kind: u16, mode: u16) -> std::string::String {
    let t = match kind {
        S_IFDIR => 'd',
        S_IFLNK => 'l',
        0o020_000 => 'c',
        0o060_000 => 'b',
        0o010_000 => 'p',
        0o140_000 => 's',
        _ => '-',
    };
    let mut s = std::string::String::from(t);
    for (i, shift) in [6u16, 3, 0].into_iter().enumerate() {
        let bits = (mode >> shift) & 7;
        s.push(if bits & 4 != 0 { 'r' } else { '-' });
        s.push(if bits & 2 != 0 { 'w' } else { '-' });
        let special = [0o4000u16, 0o2000, 0o1000][i] & mode != 0;
        let x = bits & 1 != 0;
        s.push(match (special, x, i) {
            (true, true, 2) => 't',
            (true, false, 2) => 'T',
            (true, true, _) => 's',
            (true, false, _) => 'S',
            (false, true, _) => 'x',
            (false, false, _) => '-',
        });
    }
    s
}

/// `ls -T`'s time, in UTC.
fn ls_time(t: Time) -> std::string::String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = t.sec.div_euclid(86_400);
    let secs = t.sec.rem_euclid(86_400);
    // Civil from days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    std::format!(
        "{} {} {:02}:{:02}:{:02} {}",
        MONTHS[usize::try_from(m - 1).unwrap()],
        d,
        secs / 3600,
        secs / 60 % 60,
        secs % 60,
        y
    )
}

/// What this engine lists of the volume as `ls -aliRnT` would, but for
/// column widths and the root's `..` and total (the mount point's).
fn engine_listing(v: &Volume, io: &Img) -> Vec<std::string::String> {
    let mut out = Vec::new();
    let mut dirs: Vec<(std::string::String, Inode)> =
        std::vec![(".".into(), v.load(io, &(), v.root_ino(), None).unwrap())];
    let mut first = true;
    while !dirs.is_empty() {
        let (path, dir) = dirs.remove(0);
        if !first {
            out.push(std::string::String::new());
            out.push(std::format!("{path}:"));
        }
        let mut entries: Vec<(Vec<u8>, Inode)> = Vec::new();
        for (name, _) in listing(v, io, &dir) {
            let ip = match &name[..] {
                b"." => dir.clone(),
                b".." => v.load(io, &(), dir.parent, None).unwrap(),
                _ => {
                    let f = v.lookup(io, &(), &dir, &name).unwrap();
                    v.load(io, &(), f.ino, Some(&f)).unwrap()
                }
            };
            entries.push((name, ip));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let total: u64 = entries
            .iter()
            .filter(|(n, _)| !(first && n == b".."))
            .map(|(_, ip)| ip.size / 512)
            .sum();
        if !first {
            out.push(std::format!("total {total}"));
        }
        let mut subdirs = Vec::new();
        for (name, ip) in &entries {
            if first && name == b".." {
                continue;
            }
            let name_s = std::string::String::from_utf8_lossy(name).into_owned();
            let mut size = ip.size;
            let mut line = std::format!(
                "{} {} {} {} {}",
                ip.ino,
                mode_string(ip.kind(), v.mode_of(ip)),
                ip.links,
                v.uid_of(ip),
                v.gid_of(ip)
            );
            let mut target = std::string::String::new();
            if ip.kind() == S_IFLNK {
                let t = v.readlink(io, &(), ip, b"/mnt").unwrap_or_default();
                if size == 0 {
                    size = t.len() as u64;
                }
                target = std::format!(" -> {}", std::string::String::from_utf8_lossy(&t));
            }
            line += &std::format!(" {size} {} {name_s}{target}", ls_time(ip.mtime));
            out.push(line);
            if ip.kind() == S_IFDIR && name != b"." && name != b".." {
                subdirs.push((std::format!("{path}/{name_s}"), ip.clone()));
            }
        }
        dirs.splice(0..0, subdirs);
        first = false;
    }
    out
}

#[test]
fn freebsd_listings_of_the_test_volume_match() {
    let Some(dir) = iso_test_dir() else {
        std::eprintln!("no ISO test volume (cargo xtask fetch-base): skipped");
        return;
    };
    let io = Img(std::fs::read(dir.join("iso-test.iso")).unwrap());
    let views: &[(&str, Options)] = &[
        ("rr", Options::default()),
        (
            "joliet",
            Options {
                norrip: true,
                ..Options::default()
            },
        ),
        (
            "iso",
            Options {
                norrip: true,
                nojoliet: true,
                ..Options::default()
            },
        ),
        (
            "gens",
            Options {
                norrip: true,
                nojoliet: true,
                gens: true,
                ..Options::default()
            },
        ),
        (
            "masks",
            Options {
                uid: Some(5),
                gid: Some(6),
                mask: Some(0o640),
                dirmask: Some(0o755),
                ..Options::default()
            },
        ),
    ];
    let mut bad = Vec::new();
    for (name, opts) in views {
        let v = Volume::open(&io, &(), 512, opts).unwrap();
        let text = std::fs::read_to_string(dir.join(std::format!("iso-test-{name}.ls"))).unwrap();
        // FreeBSD's, the root's total and `..` left out, columns joined.
        let mut want = Vec::new();
        let mut first = true;
        for line in text.lines() {
            if line.is_empty() {
                first = false;
            }
            if first && (line.starts_with("total ") || line.ends_with(" ..")) {
                continue;
            }
            want.push(line.split_whitespace().collect::<Vec<_>>().join(" "));
        }
        let got: Vec<std::string::String> = engine_listing(&v, &io)
            .into_iter()
            .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect();
        if got != want {
            let at = got
                .iter()
                .zip(&want)
                .position(|(g, w)| g != w)
                .unwrap_or(got.len().min(want.len()));
            bad.push(std::format!(
                "{name}: line {}: got {:?}, want {:?}",
                at + 1,
                got.get(at),
                want.get(at)
            ));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}
