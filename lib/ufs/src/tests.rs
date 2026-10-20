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
