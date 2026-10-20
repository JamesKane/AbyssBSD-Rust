// SPDX-License-Identifier: BSD-2-Clause
//! The VFS on the host: tarfs over the ustar sample bsdtar wrote (see
//! lib/ustar's tests) with a fixed tree mounted on it, and tmpfs for
//! everything that changes, including random operation sequences checked
//! against a model.

#![allow(unsafe_code)]

extern crate std;

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::errno::Errno;
use abi::time::Timespec;
use sched::Sleepable;

use super::namei::{Found, Lookup, Namespace};
use super::tree::{Builder, Leaf, ROOT_INO};
use super::*;

const SAMPLE: &[u8] = include_bytes!("../../../lib/ustar/src/testdata/sample.tar");
const DEEP: &[u8] = b"a/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/\
cccccccccccccccccccccccccccccccccccccccccccccccccc/file.txt";

fn s() -> Sleepable {
    // SAFETY: a host test thread may sleep.
    unsafe { Sleepable::assume() }
}

fn info(name: &'static str, flags: u64) -> MountInfo {
    MountInfo {
        type_name: name,
        flags,
        from: Vec::new(),
        io_size: 4096,
    }
}

/// The sample archive at `/`, and a small tree mounted on `/a` (made for
/// the purpose: `inner/up -> ..`).
fn namespace() -> Namespace {
    let s = s();
    let tar = tarfs::mount(ustar::Archive::new(SAMPLE)).unwrap();
    let root = Mount::new(
        info("tarfs", abi::mount::MNT_RDONLY),
        alloc::boxed::Box::new(tar),
    );
    let ns = Namespace::new(&s, root).unwrap();
    let zero = Timespec::default();
    let mut b = Builder::new(Meta::root(0o555, zero));
    b.dir(b"inner", Meta::root(0o755, zero)).unwrap();
    b.leaf(
        b"inner/up",
        Meta::root(0o777, zero),
        Leaf::Symlink(b"..".as_slice().into()),
        Meta::root(0o755, zero),
    )
    .unwrap();
    let other = Mount::new(
        info("devfs", abi::mount::MNT_RDONLY),
        alloc::boxed::Box::new(b.finish()),
    );
    // The archive has no spare directory to mount on; `a` will do.
    let covered = lookup(&ns, b"/a", true).unwrap();
    ns.mount(&s, covered, other, b"/a").unwrap();
    ns
}

/// A tmpfs as the root.
fn tmp() -> Namespace {
    Namespace::new(
        &s(),
        Mount::new(
            info("tmpfs", 0),
            alloc::boxed::Box::new(tmpfs::TmpFs::new(0o755, 0, 0)),
        ),
    )
    .unwrap()
}

fn lookup_as(ns: &Namespace, cred: &Cred, path: &[u8], follow: bool) -> Result<Arc<Vnode>, Errno> {
    let root = Arc::clone(ns.root());
    ns.lookup(
        &s(),
        &Lookup {
            start: &root,
            root: &root,
            follow,
            beneath: false,
            cred,
        },
        path,
    )
}

fn lookup(ns: &Namespace, path: &[u8], follow: bool) -> Result<Arc<Vnode>, Errno> {
    lookup_as(ns, &Cred::root(), path, follow)
}

fn try_parent(ns: &Namespace, path: &[u8]) -> Result<(Arc<Vnode>, Vec<u8>), Errno> {
    let root = Arc::clone(ns.root());
    let cred = Cred::root();
    let p = ns.parent_of(
        &s(),
        &Lookup {
            start: &root,
            root: &root,
            follow: false,
            beneath: false,
            cred: &cred,
        },
        path,
    )?;
    Ok((p.dir, p.name))
}

fn parent(ns: &Namespace, path: &[u8]) -> (Arc<Vnode>, Vec<u8>) {
    try_parent(ns, path).unwrap()
}

fn mk(ns: &Namespace, path: &[u8], node: NewNode) -> Result<Arc<Vnode>, Errno> {
    mk_as(ns, &Cred::root(), path, node)
}

fn mk_as(ns: &Namespace, cred: &Cred, path: &[u8], node: NewNode) -> Result<Arc<Vnode>, Errno> {
    let (dir, name) = try_parent(ns, path)?;
    let c = Create {
        mode: if node == NewNode::Directory {
            0o755
        } else {
            0o644
        },
        uid: cred.uid,
        gid: cred.gid,
        time: Timespec::default(),
    };
    ops::create(&s(), &dir, &name, &node, &c, cred, true).map(|(v, _)| v)
}

fn rm(ns: &Namespace, path: &[u8], dir_wanted: bool) -> Result<(), Errno> {
    let (dir, name) = try_parent(ns, path)?;
    ops::remove(&s(), &dir, &name, &Cred::root(), dir_wanted)
}

fn mv(ns: &Namespace, from: &[u8], to: &[u8]) -> Result<(), Errno> {
    let (fd, fname) = try_parent(ns, from)?;
    let (td, tname) = try_parent(ns, to)?;
    ops::rename(&s(), &fd, &fname, &td, &tname, &Cred::root())
}

fn read_all(v: &Vnode) -> Vec<u8> {
    let mut buf = alloc::vec![0; 1 << 16];
    let mut r = ReadBuf::new(&mut buf);
    v.read_at(&s(), 0, &mut r).unwrap();
    r.filled().to_vec()
}

fn write_all(v: &Vnode, at: u64, data: &[u8]) {
    let n = v.write_at(&s(), at, &mut WriteBuf::new(data)).unwrap();
    assert_eq!(n, data.len());
}

fn names(v: &Vnode) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    v.readdir(&s(), 0, &mut |e| {
        out.push(e.name.to_vec());
        true
    })
    .unwrap();
    out
}

#[test]
fn tarfs_files_links_and_directories() {
    let ns = namespace();
    let s = s();
    let sh = lookup(&ns, b"/rescue/sh", true).unwrap();
    assert_eq!(read_all(&sh), b"#!/bin/sh\necho crunched\n");
    let st = sh.stat(&s).unwrap();
    assert_eq!(st.st_mode, abi::stat::S_IFREG | 0o555);
    assert_eq!(st.st_nlink, 3);
    assert_eq!(st.st_size, 24);
    assert_eq!(st.st_dev, ns.root().mount().id);
    // Hard links are the same vnode, and lookups meet at one vnode.
    let ls = lookup(&ns, b"rescue/ls", true).unwrap();
    assert!(Arc::ptr_eq(&ls, &sh));
    // The symlink is followed unless asked not to.
    assert!(Arc::ptr_eq(&lookup(&ns, b"/shell", true).unwrap(), &sh));
    let link = lookup(&ns, b"/shell", false).unwrap();
    assert_eq!(
        link.stat(&s).unwrap().st_mode & abi::stat::S_IFMT,
        abi::stat::S_IFLNK
    );
    // The root is inode 2, and a directory's links count its subdirectories.
    let root = lookup(&ns, b"/", true).unwrap();
    assert_eq!(root.ino(), ROOT_INO);
    let rescue = lookup(&ns, b"/rescue", true).unwrap();
    assert_eq!(rescue.stat(&s).unwrap().st_nlink, 2);
    assert_eq!(rescue.stat(&s).unwrap().st_mode, abi::stat::S_IFDIR | 0o755);
    // Read-only.
    assert_eq!(rm(&ns, b"/rescue/sh", false), Err(Errno::EROFS));
}

#[test]
fn implied_directories_and_long_paths() {
    let tar = tarfs::mount(ustar::Archive::new(SAMPLE)).unwrap();
    let ns = Namespace::new(
        &s(),
        Mount::new(
            info("tarfs", abi::mount::MNT_RDONLY),
            alloc::boxed::Box::new(tar),
        ),
    )
    .unwrap();
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
    // A trailing slash follows a symlink... and this one is to a file.
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
    assert_ne!(mounted.mount().id, ns.root().mount().id);
    assert_eq!(mounted.ino(), ROOT_INO);
    let inner = lookup(&ns, b"/a/inner", true).unwrap();
    // `..` from the mounted root goes to the parent of the covered directory.
    let up = lookup(&ns, b"/a/inner/../..", true).unwrap();
    assert!(Arc::ptr_eq(&up, ns.root()));
    // A relative symlink resolves from its own directory.
    assert!(Arc::ptr_eq(
        &lookup(&ns, b"/a/inner/up", true).unwrap(),
        &mounted
    ));
    assert!(Arc::ptr_eq(
        &lookup(&ns, b"/a/inner/up/inner", true).unwrap(),
        &inner
    ));
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
    assert_eq!(inner.mount().on(), b"/a");
}

#[test]
fn symlink_loops() {
    let zero = Timespec::default();
    let mut b = Builder::new(Meta::root(0o755, zero));
    let m = Meta::root(0o777, zero);
    b.leaf(b"loop", m, Leaf::Symlink(b"loop".as_slice().into()), m)
        .unwrap();
    b.leaf(b"empty", m, Leaf::Symlink(b"".as_slice().into()), m)
        .unwrap();
    let ns = Namespace::new(
        &s(),
        Mount::new(info("t", 0), alloc::boxed::Box::new(b.finish())),
    )
    .unwrap();
    assert_eq!(lookup(&ns, b"/loop", true).unwrap_err(), Errno::ELOOP);
    assert!(lookup(&ns, b"/loop", false).is_ok());
    assert_eq!(lookup(&ns, b"/empty", true).unwrap_err(), Errno::ENOENT);
}

#[test]
fn missing_last_component() {
    let ns = namespace();
    let root = Arc::clone(ns.root());
    let cred = Cred::root();
    let l = Lookup {
        start: &root,
        root: &root,
        follow: true,
        beneath: false,
        cred: &cred,
    };
    match ns.resolve(&s(), &l, b"/rescue/new").unwrap() {
        Found::Missing { dir, name } => {
            assert!(Arc::ptr_eq(&dir, &lookup(&ns, b"/rescue", true).unwrap()));
            assert_eq!(name, b"new");
        }
        Found::Vnode(_) => panic!("found a file that does not exist"),
    }
    assert!(ns.resolve(&s(), &l, b"/nope/new").is_err());
}

#[test]
fn beneath() {
    let ns = namespace();
    let rescue = lookup(&ns, b"/rescue", true).unwrap();
    let root = Arc::clone(ns.root());
    let cred = Cred::root();
    let l = Lookup {
        start: &rescue,
        root: &root,
        follow: true,
        beneath: true,
        cred: &cred,
    };
    assert!(ns.lookup(&s(), &l, b"sh").is_ok());
    assert!(ns.lookup(&s(), &l, b"./../rescue/sh").is_err());
    assert_eq!(
        ns.lookup(&s(), &l, b"/rescue/sh").unwrap_err(),
        Errno::ENOTCAPABLE
    );
    assert_eq!(ns.lookup(&s(), &l, b"..").unwrap_err(), Errno::ENOTCAPABLE);
}

#[test]
fn paths_of_directories() {
    let ns = namespace();
    let path = |p: &[u8]| {
        let v = lookup(&ns, p, true).unwrap();
        ns.path_of(&s(), &v, ns.root()).unwrap()
    };
    assert_eq!(path(b"/"), b"/");
    assert_eq!(path(b"/rescue"), b"/rescue");
    assert_eq!(path(b"/a"), b"/a");
    assert_eq!(path(b"/a/inner/up/inner"), b"/a/inner");
}

#[test]
fn directory_entries_in_order_with_dot_entries() {
    let ns = namespace();
    let rescue = lookup(&ns, b"/rescue", true).unwrap();
    let n = names(&rescue);
    assert_eq!(&n[..2], &[b".".to_vec(), b"..".to_vec()]);
    let mut sorted = n[2..].to_vec();
    sorted.sort();
    assert_eq!(n[2..], sorted[..]);
}

#[test]
fn type_numbers_match_freebsd() {
    assert_eq!(type_num("ufs"), 0x35);
    assert_eq!(type_num("devfs"), 0x71);
    assert_eq!(type_num("tmpfs"), 0x87);
}

#[test]
fn tmpfs_create_read_write_truncate() {
    let ns = tmp();
    let s = s();
    let f = mk(&ns, b"/f", NewNode::Regular).unwrap();
    assert!(Arc::ptr_eq(&lookup(&ns, b"/f", true).unwrap(), &f));
    write_all(&f, 0, b"hello");
    write_all(&f, 10, b"world");
    assert_eq!(read_all(&f), b"hello\0\0\0\0\0world");
    assert_eq!(f.stat(&s).unwrap().st_size, 15);
    // O_EXCL on an existing name, and the other answers.
    assert_eq!(mk(&ns, b"/f", NewNode::Regular).unwrap_err(), Errno::EEXIST);
    assert_eq!(
        mk(&ns, b"/f/x", NewNode::Regular).unwrap_err(),
        Errno::ENOTDIR
    );
    ops::setattr(
        &s,
        &f,
        &SetAttr {
            size: Some(3),
            ..SetAttr::default()
        },
        &Cred::root(),
        false,
    )
    .unwrap();
    assert_eq!(read_all(&f), b"hel");
    let d = mk(&ns, b"/d", NewNode::Directory).unwrap();
    assert_eq!(
        ops::setattr(
            &s,
            &d,
            &SetAttr {
                size: Some(0),
                ..SetAttr::default()
            },
            &Cred::root(),
            false
        ),
        Err(Errno::EISDIR)
    );
}

#[test]
fn tmpfs_link_counts_and_removal() {
    let ns = tmp();
    let s = s();
    let root = Arc::clone(ns.root());
    assert_eq!(root.stat(&s).unwrap().st_nlink, 2);
    let d = mk(&ns, b"/d", NewNode::Directory).unwrap();
    assert_eq!(root.stat(&s).unwrap().st_nlink, 3);
    assert_eq!(d.stat(&s).unwrap().st_nlink, 2);
    let f = mk(&ns, b"/d/f", NewNode::Regular).unwrap();
    let (dir, _) = parent(&ns, b"/d/g");
    ops::link(&s, &f, &dir, b"g", &Cred::root()).unwrap();
    assert_eq!(f.stat(&s).unwrap().st_nlink, 2);
    assert!(Arc::ptr_eq(&lookup(&ns, b"/d/g", true).unwrap(), &f));
    // A directory cannot be unlinked, linked, or removed while not empty.
    assert_eq!(rm(&ns, b"/d", false), Err(Errno::EPERM));
    assert_eq!(
        ops::link(&s, &d, &root, b"d2", &Cred::root()),
        Err(Errno::EPERM)
    );
    assert_eq!(rm(&ns, b"/d", true), Err(Errno::ENOTEMPTY));
    assert_eq!(rm(&ns, b"/d/f", true), Err(Errno::ENOTDIR));
    assert_eq!(rm(&ns, b"/d/.", true), Err(Errno::EINVAL));
    assert_eq!(rm(&ns, b"/d/..", true), Err(Errno::EINVAL));
    assert_eq!(rm(&ns, b"/", true), Err(Errno::EISDIR));
    rm(&ns, b"/d/f", false).unwrap();
    assert_eq!(f.stat(&s).unwrap().st_nlink, 1);
    rm(&ns, b"/d/g", false).unwrap();
    rm(&ns, b"/d", true).unwrap();
    assert_eq!(root.stat(&s).unwrap().st_nlink, 2);
    assert_eq!(lookup(&ns, b"/d", true).unwrap_err(), Errno::ENOENT);
}

#[test]
fn an_unlinked_open_file_lives_until_its_last_use() {
    let ns = tmp();
    let s = s();
    let f = mk(&ns, b"/f", NewNode::Regular).unwrap();
    write_all(&f, 0, b"still here");
    let open = f.use_ref();
    drop(f);
    rm(&ns, b"/f", false).unwrap();
    assert_eq!(read_all(&open), b"still here");
    assert_eq!(open.stat(&s).unwrap().st_nlink, 0);
    let v = Arc::clone(open.vnode());
    drop(open);
    // The last use freed it, and its vnode is doomed.
    assert!(v.is_doomed(&s));
    assert_eq!(v.stat(&s).unwrap_err(), Errno::EBADF);
}

#[test]
fn the_name_cache_answers_and_is_kept_right() {
    let ns = tmp();
    assert_eq!(lookup(&ns, b"/x", true).unwrap_err(), Errno::ENOENT);
    let root = Arc::clone(ns.root());
    assert!(matches!(
        cache::lookup(&root, b"x"),
        Some(cache::Hit::Negative)
    ));
    // A creation replaces the negative entry.
    let x = mk(&ns, b"/x", NewNode::Regular).unwrap();
    match cache::lookup(&root, b"x") {
        Some(cache::Hit::Positive(v)) => assert!(Arc::ptr_eq(&v, &x)),
        other => panic!("{other:?}"),
    }
    mv(&ns, b"/x", b"/y").unwrap();
    assert_eq!(lookup(&ns, b"/x", true).unwrap_err(), Errno::ENOENT);
    assert!(Arc::ptr_eq(&lookup(&ns, b"/y", true).unwrap(), &x));
    rm(&ns, b"/y", false).unwrap();
    assert_eq!(lookup(&ns, b"/y", true).unwrap_err(), Errno::ENOENT);
}

#[test]
fn idle_vnodes_are_reclaimed_and_found_again() {
    let ns = tmp();
    let s = s();
    for i in 0..50 {
        let f = mk(&ns, std::format!("/f{i}").as_bytes(), NewNode::Regular).unwrap();
        write_all(&f, 0, std::format!("file {i}").as_bytes());
    }
    let m = Arc::clone(ns.root().mount());
    // The root and fifty files; none in use, so all but the root may go.
    assert_eq!(m.vnodes(), 51);
    let keep = lookup(&ns, b"/f7", true).unwrap();
    // The longest unused go first; one held is never taken.
    assert_eq!(m.prune(&s, 10), 10);
    assert_eq!(m.vnodes(), 41);
    assert_eq!(m.prune(&s, usize::MAX), 39);
    assert_eq!(m.vnodes(), 2);
    assert!(!keep.is_doomed(&s));
    // The name cache does not keep them, and a lookup loads them again.
    for i in [0, 7, 49] {
        let v = lookup(&ns, std::format!("/f{i}").as_bytes(), true).unwrap();
        assert_eq!(read_all(&v), std::format!("file {i}").as_bytes());
    }
    assert!(Arc::ptr_eq(&lookup(&ns, b"/f7", true).unwrap(), &keep));
}

#[test]
fn rename_rules() {
    let ns = tmp();
    let s = s();
    mk(&ns, b"/a", NewNode::Directory).unwrap();
    mk(&ns, b"/a/b", NewNode::Directory).unwrap();
    mk(&ns, b"/f", NewNode::Regular).unwrap();
    mk(&ns, b"/g", NewNode::Regular).unwrap();
    mk(&ns, b"/e", NewNode::Directory).unwrap();
    // A directory may not go under itself.
    assert_eq!(mv(&ns, b"/a", b"/a/b/c"), Err(Errno::EINVAL));
    // Kinds must match; a directory replaces only an empty one.
    assert_eq!(mv(&ns, b"/a", b"/f"), Err(Errno::ENOTDIR));
    assert_eq!(mv(&ns, b"/f", b"/a"), Err(Errno::EISDIR));
    assert_eq!(mv(&ns, b"/e", b"/a"), Err(Errno::ENOTEMPTY));
    assert_eq!(mv(&ns, b"/missing", b"/z"), Err(Errno::ENOENT));
    assert_eq!(mv(&ns, b"/f", b"/."), Err(Errno::EINVAL));
    // A file over a file: the target goes.
    let f = lookup(&ns, b"/f", true).unwrap();
    let g = lookup(&ns, b"/g", true).unwrap();
    mv(&ns, b"/f", b"/g").unwrap();
    assert!(Arc::ptr_eq(&lookup(&ns, b"/g", true).unwrap(), &f));
    assert!(g.is_doomed(&s));
    // A directory into another: `..` and the link counts follow.
    let a = lookup(&ns, b"/a", true).unwrap();
    let e = lookup(&ns, b"/e", true).unwrap();
    mv(&ns, b"/a", b"/e/a").unwrap();
    assert!(Arc::ptr_eq(&lookup(&ns, b"/e/a/..", true).unwrap(), &e));
    assert_eq!(e.stat(&s).unwrap().st_nlink, 3);
    assert_eq!(ns.path_of(&s, &a, ns.root()).unwrap(), b"/e/a");
    // Two names for one file: nothing happens.
    let (dir, _) = parent(&ns, b"/h");
    ops::link(&s, &f, &dir, b"h", &Cred::root()).unwrap();
    mv(&ns, b"/g", b"/h").unwrap();
    assert_eq!(f.stat(&s).unwrap().st_nlink, 2);
}

#[test]
fn renaming_across_mounts_is_exdev() {
    let ns = tmp();
    let s = s();
    let d = mk(&ns, b"/m", NewNode::Directory).unwrap();
    mk(&ns, b"/f", NewNode::Regular).unwrap();
    let other = Mount::new(
        info("tmpfs", 0),
        alloc::boxed::Box::new(tmpfs::TmpFs::new(0o755, 0, 0)),
    );
    ns.mount(&s, d, Arc::clone(&other), b"/m").unwrap();
    assert_eq!(mv(&ns, b"/f", b"/m/f"), Err(Errno::EXDEV));
    // Busy while in use; then gone.
    let inside = mk(&ns, b"/m/x", NewNode::Regular).unwrap();
    assert_eq!(rm(&ns, b"/m", true), Err(Errno::EBUSY));
    let open = inside.use_ref();
    assert_eq!(ns.unmount(&s, &other, false), Err(Errno::EBUSY));
    drop(open);
    drop(inside);
    ns.unmount(&s, &other, false).unwrap();
    assert_eq!(lookup(&ns, b"/m/x", true).unwrap_err(), Errno::ENOENT);
}

#[test]
fn getdirentries_packs_and_resumes_across_changes() {
    let ns = tmp();
    let s = s();
    for i in 0..20 {
        mk(&ns, std::format!("/e{i:02}").as_bytes(), NewNode::Regular).unwrap();
    }
    let root = Arc::clone(ns.root());
    // A small buffer: a few records at a time.
    let mut seen = Vec::new();
    let mut cookie = 0;
    loop {
        let mut out = Vec::new();
        cookie = ops::getdirentries(&s, &root, cookie, 120, &mut out).unwrap();
        if out.is_empty() {
            break;
        }
        let mut at = 0;
        while at < out.len() {
            let reclen = usize::from(u16::from_ne_bytes([out[at + 16], out[at + 17]]));
            let namlen = usize::from(u16::from_ne_bytes([out[at + 20], out[at + 21]]));
            seen.push(out[at + 24..at + 24 + namlen].to_vec());
            at += reclen;
        }
        // Removing what was already returned changes nothing to come.
        if seen.len() == 5 {
            rm(&ns, b"/e00", false).unwrap();
        }
    }
    assert_eq!(seen.len(), 22);
    assert_eq!(seen[0], b".");
    assert_eq!(seen[21], b"e19");
    // Too small for one record.
    assert_eq!(
        ops::getdirentries(&s, &root, 0, 8, &mut Vec::new()),
        Err(Errno::EINVAL)
    );
}

#[test]
fn permissions_for_others() {
    let ns = tmp();
    let s = s();
    let user = Cred {
        uid: 1001,
        gid: 1001,
        groups: Vec::new(),
    };
    let d = mk(&ns, b"/d", NewNode::Directory).unwrap();
    // Root's 0755 directory: others may search it but not create in it.
    assert_eq!(
        mk_as(&ns, &user, b"/d/x", NewNode::Regular).unwrap_err(),
        Errno::EACCES
    );
    ops::setattr(
        &s,
        &d,
        &SetAttr {
            mode: Some(0o1777),
            ..SetAttr::default()
        },
        &Cred::root(),
        false,
    )
    .unwrap();
    let mine = mk_as(&ns, &user, b"/d/mine", NewNode::Regular).unwrap();
    assert_eq!(mine.stat(&s).unwrap().st_uid, 1001);
    let theirs = mk(&ns, b"/d/theirs", NewNode::Regular).unwrap();
    // Sticky: only the owner (or root) removes an entry.
    let (dir, _) = parent(&ns, b"/d/theirs");
    assert_eq!(
        ops::remove(&s, &dir, b"theirs", &user, false),
        Err(Errno::EPERM)
    );
    ops::remove(&s, &dir, b"mine", &user, false).unwrap();
    // Only the owner changes the mode; only root gives a file away.
    assert_eq!(
        ops::setattr(
            &s,
            &theirs,
            &SetAttr {
                mode: Some(0o600),
                ..SetAttr::default()
            },
            &user,
            false
        ),
        Err(Errno::EPERM)
    );
    ops::setattr(
        &s,
        &theirs,
        &SetAttr {
            mode: Some(0o600),
            ..SetAttr::default()
        },
        &Cred::root(),
        false,
    )
    .unwrap();
    let a = theirs.attr(&s).unwrap();
    assert_eq!(
        ops::access(&a, Kind::Regular, &user, ops::VREAD),
        Err(Errno::EACCES)
    );
    assert_eq!(
        ops::access(&a, Kind::Regular, &Cred::root(), ops::VREAD),
        Ok(())
    );
    // Root executes only what some execute bit allows.
    assert_eq!(
        ops::access(&a, Kind::Regular, &Cred::root(), ops::VEXEC),
        Err(Errno::EACCES)
    );
    // A directory others cannot search hides its contents.
    ops::setattr(
        &s,
        &d,
        &SetAttr {
            mode: Some(0o700),
            ..SetAttr::default()
        },
        &Cred::root(),
        false,
    )
    .unwrap();
    assert_eq!(
        lookup_as(&ns, &user, b"/d/theirs", true).unwrap_err(),
        Errno::EACCES
    );
}

#[test]
fn immutable_files_refuse_change() {
    let ns = tmp();
    let s = s();
    let f = mk(&ns, b"/f", NewNode::Regular).unwrap();
    ops::setattr(
        &s,
        &f,
        &SetAttr {
            flags: Some(abi::stat::SF_IMMUTABLE),
            ..SetAttr::default()
        },
        &Cred::root(),
        false,
    )
    .unwrap();
    assert_eq!(rm(&ns, b"/f", false), Err(Errno::EPERM));
    assert_eq!(
        ops::setattr(
            &s,
            &f,
            &SetAttr {
                mode: Some(0o600),
                ..SetAttr::default()
            },
            &Cred::root(),
            false
        ),
        Err(Errno::EPERM)
    );
    ops::setattr(
        &s,
        &f,
        &SetAttr {
            flags: Some(0),
            ..SetAttr::default()
        },
        &Cred::root(),
        false,
    )
    .unwrap();
    rm(&ns, b"/f", false).unwrap();
}

/// What the model says a path is.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Model {
    File(Vec<u8>),
    Dir,
}

/// Random creates, writes, links, removals, and renames on tmpfs, each
/// result (success or the errno) checked against a model of paths, and
/// the whole tree compared with the model at the end.
#[test]
fn random_operations_match_a_model() {
    let ns = tmp();
    let mut model: BTreeMap<Vec<u8>, Model> = BTreeMap::new();
    let mut rng = 0x9e37_79b9_7f4a_7c15_u64;
    let mut next = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        rng
    };
    let paths: Vec<Vec<u8>> = ["/a", "/b", "/c", "/a/x", "/a/y", "/b/x", "/b/z", "/a/x/q"]
        .iter()
        .map(|p| p.as_bytes().to_vec())
        .collect();
    // Why the walk to `p`'s directory fails, if it does: the first
    // ancestor missing (`ENOENT`) or a file (`ENOTDIR`), as namei finds it.
    let walk = |model: &BTreeMap<Vec<u8>, Model>, p: &[u8]| -> Result<(), Errno> {
        for (i, &b) in p.iter().enumerate().skip(1) {
            if b == b'/' {
                match model.get(&p[..i]) {
                    None => return Err(Errno::ENOENT),
                    Some(Model::File(_)) => return Err(Errno::ENOTDIR),
                    Some(Model::Dir) => {}
                }
            }
        }
        Ok(())
    };
    for round in 0..5000 {
        let pick = |r: u64| paths[usize::try_from(r % paths.len() as u64).unwrap_or(0)].clone();
        let p = pick(next());
        let q = pick(next());
        let reach = walk(&model, &p);
        let parent_ok = reach.is_ok();
        match next() % 6 {
            0 | 1 => {
                let dir = next() % 2 == 0;
                let got = mk(
                    &ns,
                    &p,
                    if dir {
                        NewNode::Directory
                    } else {
                        NewNode::Regular
                    },
                )
                .map(drop);
                let want = if let Err(e) = reach {
                    Err(e)
                } else if model.contains_key(&p) {
                    Err(Errno::EEXIST)
                } else {
                    model.insert(
                        p.clone(),
                        if dir {
                            Model::Dir
                        } else {
                            Model::File(Vec::new())
                        },
                    );
                    Ok(())
                };
                assert_eq!(
                    got,
                    want,
                    "round {round}: create {:?}",
                    core::str::from_utf8(&p)
                );
            }
            2 => {
                if let Some(Model::File(data)) = model.get_mut(&p) {
                    let v = lookup(&ns, &p, true).unwrap();
                    let byte = next().to_le_bytes()[0];
                    write_all(&v, data.len() as u64, &[byte]);
                    data.push(byte);
                }
            }
            3 => {
                let got = rm(&ns, &p, false);
                let want = match model.get(&p) {
                    None if !parent_ok => reach,
                    None => Err(Errno::ENOENT),
                    Some(Model::Dir) => Err(Errno::EPERM),
                    Some(Model::File(_)) => {
                        model.remove(&p);
                        Ok(())
                    }
                };
                assert_eq!(
                    got,
                    want,
                    "round {round}: unlink {:?}",
                    core::str::from_utf8(&p)
                );
            }
            4 => {
                let got = rm(&ns, &p, true);
                let has_children = model
                    .keys()
                    .any(|k| k.len() > p.len() && k.starts_with(&p) && k[p.len()] == b'/');
                let want = match model.get(&p) {
                    None if !parent_ok => reach,
                    None => Err(Errno::ENOENT),
                    Some(Model::File(_)) => Err(Errno::ENOTDIR),
                    Some(Model::Dir) if has_children => Err(Errno::ENOTEMPTY),
                    Some(Model::Dir) => {
                        model.remove(&p);
                        Ok(())
                    }
                };
                assert_eq!(
                    got,
                    want,
                    "round {round}: rmdir {:?}",
                    core::str::from_utf8(&p)
                );
            }
            _ => {
                // Renames only between files that exist and fresh names in
                // existing directories, to keep the model simple.
                let qparent_ok = walk(&model, &q).is_ok();
                if let (Some(Model::File(data)), true) = (model.get(&p).cloned(), qparent_ok)
                    && !matches!(model.get(&q), Some(Model::Dir))
                {
                    mv(&ns, &p, &q).unwrap();
                    if p != q {
                        model.remove(&p);
                        model.insert(q.clone(), Model::File(data));
                    }
                }
            }
        }
    }
    // The tree is the model.
    for (p, m) in &model {
        let v = lookup(&ns, p, false).unwrap();
        match m {
            Model::Dir => assert!(v.is_dir()),
            Model::File(data) => assert_eq!(&read_all(&v), data),
        }
    }
    for p in &paths {
        if !model.contains_key(p) {
            assert!(
                lookup(&ns, p, false).is_err(),
                "{:?} should not exist",
                core::str::from_utf8(p)
            );
        }
    }
}
