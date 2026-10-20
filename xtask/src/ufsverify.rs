// SPDX-License-Identifier: BSD-2-Clause
//! `cargo xtask ufs-verify`: `lib/ufs` against FreeBSD's own UFS2 images
//! of the whole base system (P5.4, docs/VFS.md §14). On the build server:
//!
//! 1. `base.txz` is unpacked whole.
//! 2. `makefs` makes an image of the tree.
//! 3. `newfs -U` makes a file system on an `md` device over an image file;
//!    the tree is copied in through FreeBSD's UFS, and a sparse file with
//!    data past a hole is added.
//! 4. Each image is walked through `lib/ufs`: every entry's mode, owner,
//!    size, modification time, contents, and link target must match the
//!    tree, and the sparse file must read as written with its hole
//!    unallocated.
//!
//! The `md` device and its mount are torn down whatever happens.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use crate::arch::Arch;
use crate::base;
use crate::cli::Args;
use crate::fs::Md;
use crate::util::{self, Result};

/// An image file as a block device.
struct Image(fs::File);

impl ufs::BlockIo for Image {
    type Cx = ();

    fn read(
        &self,
        _: &(),
        offset: u64,
        len: usize,
        f: &mut dyn FnMut(&[u8]),
    ) -> std::result::Result<(), ufs::Error> {
        let mut b = vec![0; len];
        self.0
            .read_exact_at(&mut b, offset)
            .map_err(|_| ufs::Error::Io)?;
        f(&b);
        Ok(())
    }
}

/// The sparse file's size, and where its data sits past the hole.
const SPARSE_SIZE: u64 = 300 << 20;
const SPARSE_AT: u64 = 200 << 20;
const SPARSE_DATA: &[u8] = b"data past a hole\n";

pub(crate) fn cmd(mut args: Args) -> Result<()> {
    let keep = args.take_flag("--keep");
    args.finish()?;
    if !cfg!(target_os = "freebsd") {
        return Err(
            "ufs-verify needs FreeBSD's tools: run it on the build server (--remote)".into(),
        );
    }
    let tree = full_tree()?;
    let work = base::dir(Arch::X86_64).join("ufs-verify");
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work)?;

    let made = work.join("makefs.img");
    let t = Instant::now();
    util::run(
        Command::new("makefs")
            .args(["-t", "ffs", "-o", "version=2"])
            .arg(&made)
            .arg(&tree),
    )?;
    eprintln!("xtask: ufs-verify: makefs image in {:.1?}", t.elapsed());
    let n = verify(&made, &tree, false)?;
    eprintln!("xtask: ufs-verify: makefs image: {n} entries match");

    let newfs = work.join("newfs.img");
    let size = fs::metadata(&made)?.len() * 3 / 2 + (512 << 20);
    newfs_copy(&newfs, size, &tree, &work.join("mnt"))?;
    let n = verify(&newfs, &tree, true)?;
    eprintln!("xtask: ufs-verify: newfs image: {n} entries match, and the sparse file");

    if !keep {
        let _ = fs::remove_dir_all(&work);
    }
    Ok(())
}

/// The whole of `base.txz`, unpacked once beside it.
fn full_tree() -> Result<PathBuf> {
    // Fetches and checks base.txz if need be.
    base::root(Arch::X86_64)?;
    let dir = base::dir(Arch::X86_64);
    let full = dir.join("full");
    if full.join("bin/sh").exists() {
        return Ok(full);
    }
    let tmp = dir.join("full.tmp");
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp)?;
    eprintln!("xtask: ufs-verify: unpacking all of base.txz");
    util::run(
        Command::new("tar")
            .arg("-xf")
            .arg(dir.join("base.txz"))
            .arg("-C")
            .arg(&tmp),
    )?;
    fs::rename(&tmp, &full)?;
    Ok(full)
}

/// A `newfs -U` file system of `size` bytes in `img`, holding a copy of
/// `tree` and the sparse file, written through FreeBSD's UFS.
fn newfs_copy(img: &Path, size: u64, tree: &Path, mnt: &Path) -> Result<()> {
    util::run(
        Command::new("truncate")
            .args(["-s", &size.to_string()])
            .arg(img),
    )?;
    let mut md = Md::attach(img, false)?;
    util::run(Command::new("doas").args(["newfs", "-U", &md.dev()]))?;
    md.mount(mnt, false)?;
    let t = Instant::now();
    let copy = format!(
        "tar -C '{}' -cf - . | doas tar -C '{}' -xpf -",
        tree.display(),
        mnt.display()
    );
    util::run(Command::new("sh").args(["-c", &copy]))?;
    let sparse = mnt.join("abyss-sparse");
    util::run(
        Command::new("doas")
            .args(["truncate", "-s", &SPARSE_SIZE.to_string()])
            .arg(&sparse),
    )?;
    let write = format!(
        "printf '{}' | doas dd of='{}' bs=512 oseek={} conv=notrunc status=none",
        String::from_utf8_lossy(SPARSE_DATA).trim_end().to_owned() + "\\n",
        sparse.display(),
        SPARSE_AT / 512
    );
    util::run(Command::new("sh").args(["-c", &write]))?;
    eprintln!(
        "xtask: ufs-verify: newfs image filled in {:.1?}",
        t.elapsed()
    );
    // Unmount (cleanly) before reading the image.
    drop(md);
    Ok(())
}

/// Walks image `img` against `tree`; with `extras`, the sparse file must
/// be there too. Returns how many entries matched.
fn verify(img: &Path, tree: &Path, extras: bool) -> Result<usize> {
    let t = Instant::now();
    let image = ufs::Ufs::open(&(), Image(fs::File::open(img)?), 512)
        .map_err(|e| format!("{}: {e:?}", img.display()))?;
    let sb = image.superblock();
    eprintln!(
        "xtask: ufs-verify: {}: {}-byte blocks, {}-byte fragments, {} groups, clean {}",
        img.display(),
        sb.bsize,
        sb.fsize,
        sb.ncg,
        sb.clean
    );
    let mut errors = Vec::new();
    let n = walk(&image, tree, ufs::ROOT_INO, extras, &mut errors);
    if extras {
        check_sparse(&image, &mut errors);
    }
    if !errors.is_empty() {
        for e in errors.iter().take(40) {
            eprintln!("  {e}");
        }
        return Err(format!("{}: {} mismatches", img.display(), errors.len()).into());
    }
    eprintln!(
        "xtask: ufs-verify: {} walked in {:.1?}",
        img.display(),
        t.elapsed()
    );
    Ok(n)
}

fn walk(
    image: &ufs::Ufs<Image>,
    dir: &Path,
    ino: u32,
    top_extras: bool,
    errors: &mut Vec<String>,
) -> usize {
    let ip = match image.inode(&(), ino) {
        Ok(ip) => ip,
        Err(e) => {
            errors.push(format!("{}: inode {ino}: {e:?}", dir.display()));
            return 0;
        }
    };
    let mut entries = Vec::new();
    if let Err(e) = image.read_dir(&(), &ip, 0, &mut |e| {
        // At the root of the newfs image: the sparse file, and the
        // snapshot directory newfs makes.
        let extra = top_extras && (e.name.starts_with(b"abyss-") || e.name == b".snap");
        if e.name != b"." && e.name != b".." && !extra {
            entries.push((e.name.to_vec(), e.ino));
        }
        true
    }) {
        errors.push(format!("{}: reading the directory: {e:?}", dir.display()));
        return 0;
    }
    let want: BTreeSet<Vec<u8>> = match fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().as_bytes().to_vec())
            .collect(),
        Err(e) => {
            errors.push(format!("{}: {e}", dir.display()));
            return 0;
        }
    };
    let got: BTreeSet<Vec<u8>> = entries.iter().map(|(n, _)| n.clone()).collect();
    if want != got {
        let missing: Vec<_> = want
            .difference(&got)
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .collect();
        let extra: Vec<_> = got
            .difference(&want)
            .map(|n| String::from_utf8_lossy(n).into_owned())
            .collect();
        errors.push(format!(
            "{}: missing {missing:?}, extra {extra:?}",
            dir.display()
        ));
    }
    let mut n = 0;
    for (name, child) in entries {
        let path = dir.join(OsStr::from_bytes(&name));
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        let cip = match image.inode(&(), child) {
            Ok(c) => c,
            Err(e) => {
                errors.push(format!("{}: inode {child}: {e:?}", path.display()));
                continue;
            }
        };
        let p = path.display();
        if u32::from(cip.mode) != meta.mode() {
            errors.push(format!("{p}: mode {:o}, want {:o}", cip.mode, meta.mode()));
        }
        if (cip.uid, cip.gid) != (meta.uid(), meta.gid()) {
            errors.push(format!(
                "{p}: owner {}:{}, want {}:{}",
                cip.uid,
                cip.gid,
                meta.uid(),
                meta.gid()
            ));
        }
        if cip.mtime.sec != meta.mtime() {
            errors.push(format!(
                "{p}: mtime {}, want {}",
                cip.mtime.sec,
                meta.mtime()
            ));
        }
        if !meta.is_dir() && cip.size != meta.size() {
            errors.push(format!("{p}: size {}, want {}", cip.size, meta.size()));
        }
        match cip.kind() {
            ufs::Kind::Directory => n += walk(image, &path, child, false, errors),
            // A file unreadable to this user has nothing to compare with.
            ufs::Kind::Regular => {
                if let Ok(want) = fs::read(&path) {
                    let mut got = vec![0u8; want.len()];
                    match image.read(&(), &cip, 0, &mut got) {
                        Ok(k) if k == want.len() && got == want => {}
                        Ok(k) => errors.push(format!("{p}: contents differ ({k} bytes read)")),
                        Err(e) => errors.push(format!("{p}: {e:?}")),
                    }
                }
            }
            ufs::Kind::Symlink => {
                let want = fs::read_link(&path).map(|t| t.as_os_str().as_bytes().to_vec());
                match (image.readlink(&(), &cip), want) {
                    (Ok(got), Ok(want)) if got == want => {}
                    (got, want) => errors.push(format!("{p}: link {got:?}, want {want:?}")),
                }
            }
            k => errors.push(format!("{p}: unexpected {k:?}")),
        }
        n += 1;
    }
    n
}

/// The sparse file reads as written, and its hole holds no blocks.
fn check_sparse(image: &ufs::Ufs<Image>, errors: &mut Vec<String>) {
    let ip = match image.namei(&(), b"/abyss-sparse") {
        Ok(Some((_, ip))) => ip,
        other => {
            errors.push(format!("abyss-sparse: {other:?}"));
            return;
        }
    };
    if ip.size != SPARSE_SIZE {
        errors.push(format!("abyss-sparse: size {}", ip.size));
    }
    // One block of data (and the indirect blocks above it), not 300 MiB.
    if ip.blocks * 512 > 1 << 20 {
        errors.push(format!("abyss-sparse: {} blocks held", ip.blocks));
    }
    let mut buf = vec![0xffu8; 64 << 10];
    let ok = image.read(&(), &ip, SPARSE_AT - 1024, &mut buf).is_ok()
        && buf[..1024].iter().all(|&b| b == 0)
        && buf[1024..1024 + SPARSE_DATA.len()] == *SPARSE_DATA
        && buf[1024 + SPARSE_DATA.len()..].iter().all(|&b| b == 0);
    if !ok {
        errors.push("abyss-sparse: contents around the data differ".into());
    }
    let mut start = [0xffu8; 4096];
    if image.read(&(), &ip, 0, &mut start).ok() != Some(4096) || start.iter().any(|&b| b != 0) {
        errors.push("abyss-sparse: the hole does not read as zeros".into());
    }
}
