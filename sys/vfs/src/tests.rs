// SPDX-License-Identifier: BSD-2-Clause
//! tarfs over the ustar sample bsdtar wrote (see lib/ustar's tests), a
//! second file system mounted on it, and lookups across both.

extern crate std;

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::errno::Errno;
use abi::time::Timespec;

use super::namei::{Found, Lookup, Namespace};
use super::tree::{Builder, Leaf, ROOT_INO};
use super::*;

const SAMPLE: &[u8] = include_bytes!("../../../lib/ustar/src/testdata/sample.tar");
const DEEP: &[u8] = b"a/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/\
cccccccccccccccccccccccccccccccccccccccccccccccccc/file.txt";

fn fs(id: u64, name: &'static str) -> Arc<FileSystem> {
    Arc::new(FileSystem {
        id,
        type_name: name,
        type_num: 0,
        flags: abi::mount::MNT_RDONLY,
        from: Vec::new(),
        io_size: 4096,
    })
}

/// The sample archive at `/`, and a small tree mounted on `/rescue/mnt`
/// (made for the purpose: `mnt/inner/leaf`, and `up -> ..`).
fn namespace() -> Namespace {
    let root = tarfs::mount(ustar::Archive::new(SAMPLE), fs(1, "tarfs")).unwrap();
    let mut ns = Namespace::new(Arc::clone(&root));
    let zero = Timespec::default();
    let mut b = Builder::new(fs(2, "devfs"), Meta::root(0o555, zero));
    b.dir(b"inner", Meta::root(0o755, zero)).unwrap();
    b.leaf(
        b"inner/up",
        Meta::root(0o777, zero),
        Leaf::Symlink(b"..".as_slice().into()),
        Meta::root(0o755, zero),
    )
    .unwrap();
    let other = b.finish();
    // The archive has no spare directory to mount on; `a` will do.
    let covered = lookup(&ns, b"/a", true).unwrap();
    ns.mount(covered, other, b"/a").unwrap();
    ns
}

fn lookup(ns: &Namespace, path: &[u8], follow: bool) -> Result<Arc<Vnode>, Errno> {
    let root = Arc::clone(ns.root());
    ns.lookup(
        &Lookup {
            start: &root,
            root: &root,
            follow,
            beneath: false,
        },
        path,
    )
}

fn read_all(v: &Vnode) -> Vec<u8> {
    let VnodeKind::Regular(f) = v.kind() else {
        panic!("not a file");
    };
    let mut buf = alloc::vec![0; 4096];
    let mut r = ReadBuf::new(&mut buf);
    f.read(0, &mut r).unwrap();
    r.filled().to_vec()
}

#[test]
fn tarfs_files_links_and_directories() {
    let ns = namespace();
    let sh = lookup(&ns, b"/rescue/sh", true).unwrap();
    assert_eq!(read_all(&sh), b"#!/bin/sh\necho crunched\n");
    let st = sh.stat();
    assert_eq!(st.st_mode, abi::stat::S_IFREG | 0o555);
    assert_eq!(st.st_nlink, 3);
    assert_eq!(st.st_size, 24);
    assert_eq!(st.st_dev, 1);
    // Hard links are the same vnode.
    let ls = lookup(&ns, b"rescue/ls", true).unwrap();
    assert_eq!(ls.id(), sh.id());
    // The symlink is followed unless asked not to.
    assert_eq!(lookup(&ns, b"/shell", true).unwrap().id(), sh.id());
    let link = lookup(&ns, b"/shell", false).unwrap();
    assert_eq!(link.stat().st_mode & abi::stat::S_IFMT, abi::stat::S_IFLNK);
    // The root is inode 2, and a directory's links count its subdirectories.
    let root = lookup(&ns, b"/", true).unwrap();
    assert_eq!(root.ino(), ROOT_INO);
    let rescue = lookup(&ns, b"/rescue", true).unwrap();
    assert_eq!(rescue.stat().st_nlink, 2);
    assert_eq!(rescue.stat().st_mode, abi::stat::S_IFDIR | 0o755);
}

#[test]
fn implied_directories_and_long_paths() {
    let root = tarfs::mount(ustar::Archive::new(SAMPLE), fs(1, "tarfs")).unwrap();
    let ns = Namespace::new(root);
    let deep = lookup(&ns, DEEP, true).unwrap();
    assert_eq!(read_all(&deep), b"deep\n");
}

#[test]
fn lookup_errors() {
    let ns = namespace();
    assert_eq!(lookup(&ns, b"", true).unwrap_err(), Errno::ENOENT);
    assert_eq!(lookup(&ns, b"/nope", true).unwrap_err(), Errno::ENOENT);
    assert_eq!(lookup(&ns, b"/nope/x", true).unwrap_err(), Errno::ENOENT);
    assert_eq!(
        lookup(&ns, b"/rescue/sh/x", true).unwrap_err(),
        Errno::ENOTDIR
    );
    assert_eq!(
        lookup(&ns, b"/rescue/sh/", true).unwrap_err(),
        Errno::ENOTDIR
    );
    let long = [b'x'; 256];
    assert_eq!(lookup(&ns, &long, true).unwrap_err(), Errno::ENAMETOOLONG);
    // A trailing slash follows a symlink to a directory... and the
    // archive's symlink is to a file.
    assert_eq!(lookup(&ns, b"/shell/", false).unwrap_err(), Errno::ENOTDIR);
}

#[test]
fn dot_dot_and_mounts() {
    let ns = namespace();
    // Slashes and dots collapse; `..` stops at the root.
    let a = lookup(&ns, b"//rescue/./../../../rescue//sh", true).unwrap();
    assert_eq!(a.ino(), lookup(&ns, b"/rescue/sh", true).unwrap().ino());
    // `/a` is now the mounted tree's root.
    let mounted = lookup(&ns, b"/a", true).unwrap();
    assert_eq!(mounted.fs().id, 2);
    assert_eq!(mounted.ino(), ROOT_INO);
    let inner = lookup(&ns, b"/a/inner", true).unwrap();
    // `..` from the mounted root goes to the parent of the covered directory.
    let up = lookup(&ns, b"/a/inner/../..", true).unwrap();
    assert_eq!(up.id(), ns.root().id());
    // A relative symlink resolves from its own directory.
    assert_eq!(
        lookup(&ns, b"/a/inner/up", true).unwrap().id(),
        mounted.id()
    );
    assert_eq!(
        lookup(&ns, b"/a/inner/up/inner", true).unwrap().id(),
        inner.id()
    );
    // What was under the covered directory is hidden.
    assert_eq!(
        lookup(
            &ns,
            b"/a/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            true
        )
        .unwrap_err(),
        Errno::ENOENT
    );
    assert_eq!(ns.mount_of(&inner).unwrap().on, b"/a");
}

#[test]
fn symlink_loops() {
    let zero = Timespec::default();
    let mut b = Builder::new(fs(3, "t"), Meta::root(0o755, zero));
    let m = Meta::root(0o777, zero);
    b.leaf(b"loop", m, Leaf::Symlink(b"loop".as_slice().into()), m)
        .unwrap();
    b.leaf(b"empty", m, Leaf::Symlink(b"".as_slice().into()), m)
        .unwrap();
    let ns = Namespace::new(b.finish());
    assert_eq!(lookup(&ns, b"/loop", true).unwrap_err(), Errno::ELOOP);
    assert!(lookup(&ns, b"/loop", false).is_ok());
    assert_eq!(lookup(&ns, b"/empty", true).unwrap_err(), Errno::ENOENT);
}

#[test]
fn missing_last_component() {
    let ns = namespace();
    let root = Arc::clone(ns.root());
    let l = Lookup {
        start: &root,
        root: &root,
        follow: true,
        beneath: false,
    };
    match ns.resolve(&l, b"/rescue/new").unwrap() {
        Found::Missing { dir, name } => {
            assert_eq!(dir.id(), lookup(&ns, b"/rescue", true).unwrap().id());
            assert_eq!(name, b"new");
        }
        Found::Vnode(_) => panic!("found a file that does not exist"),
    }
    assert!(ns.resolve(&l, b"/nope/new").is_err());
}

#[test]
fn beneath() {
    let ns = namespace();
    let rescue = lookup(&ns, b"/rescue", true).unwrap();
    let root = Arc::clone(ns.root());
    let l = Lookup {
        start: &rescue,
        root: &root,
        follow: true,
        beneath: true,
    };
    assert!(ns.lookup(&l, b"sh").is_ok());
    assert!(ns.lookup(&l, b"./../rescue/sh").is_err());
    assert_eq!(
        ns.lookup(&l, b"/rescue/sh").unwrap_err(),
        Errno::ENOTCAPABLE
    );
    assert_eq!(ns.lookup(&l, b"..").unwrap_err(), Errno::ENOTCAPABLE);
}

#[test]
fn paths_of_directories() {
    let ns = namespace();
    let root = Arc::clone(ns.root());
    let path = |p: &[u8]| ns.path_of(&lookup(&ns, p, true).unwrap(), &root).unwrap();
    assert_eq!(path(b"/"), b"/");
    assert_eq!(path(b"/rescue"), b"/rescue");
    assert_eq!(path(b"/a"), b"/a");
    assert_eq!(path(b"/a/inner/up/inner"), b"/a/inner");
    // Under `/a` in the archive itself, without the mount.
    let plain = Namespace::new(tarfs::mount(ustar::Archive::new(SAMPLE), fs(1, "tarfs")).unwrap());
    let dir = &DEEP[..DEEP.len() - 9];
    let v = lookup(&plain, dir, true).unwrap();
    assert_eq!(
        plain.path_of(&v, plain.root()).unwrap(),
        [b"/", dir].concat()
    );
}

#[test]
fn directory_entries_in_order() {
    let ns = namespace();
    let rescue = lookup(&ns, b"/rescue", true).unwrap();
    let mut names = Vec::new();
    rescue.as_dir().unwrap().entries(1, &mut |e| {
        names.push((e.name.to_vec(), e.vnode.dtype()));
        true
    });
    assert_eq!(
        names,
        [
            (b"rescue".to_vec(), abi::dirent::DT_REG),
            (b"sh".to_vec(), abi::dirent::DT_REG)
        ]
    );
}

#[test]
fn type_numbers_match_freebsd() {
    // As `lsvfs` shows them on FreeBSD 15.1.
    assert_eq!(type_num("ufs"), 0x35);
    assert_eq!(type_num("devfs"), 0x71);
    assert_eq!(type_num("tmpfs"), 0x87);
}
