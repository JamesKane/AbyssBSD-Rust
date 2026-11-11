// SPDX-License-Identifier: BSD-2-Clause
//! The ISO 9660 test volume and FreeBSD's view of it, made by
//! `fetch-base` on the build server like the FAT test disk:
//!
//! - `iso-test.iso`: `bsdtar --format iso9660` (Rock Ridge and Joliet) of a tree
//!   with long and mixed-case names, a directory deep enough for Rock
//!   Ridge to relocate, symbolic links, modes, and fixed times;
//! - `iso-test-<view>.ls`: FreeBSD's `ls -aliRnT` (file numbers
//!   included) of it attached with `mdconfig -t vnode` and mounted by
//!   `mount_cd9660` each way the kernel test mounts it: Rock Ridge
//!   (`rr`), Joliet (`-r`), plain ISO 9660 (`-r -j`), with versions
//!   (`-r -j -g`), and Rock Ridge with an owner and masks
//!   (`-U 5 -G 6 -m 640 -M 755`);
//! - `iso-test.files` and `iso-test.md5`: its files and `md5 -r` of them
//!   (Rock Ridge).
//!
//! The memory disk carries them all under `/usr/tests/iso/`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::util::{self, Result};

/// The views of the volume, each `mount_cd9660`'s flags.
pub(crate) const VIEWS: &[(&str, &[&str])] = &[
    ("rr", &[]),
    ("joliet", &["-r"]),
    ("iso", &["-r", "-j"]),
    ("gens", &["-r", "-j", "-g"]),
    ("masks", &["-U", "5", "-G", "6", "-m", "640", "-M", "755"]),
];

/// The files the test volume consists of, the image first.
pub(crate) fn files() -> Vec<String> {
    let mut v = vec!["iso-test.iso".to_owned()];
    v.extend(VIEWS.iter().map(|(n, _)| format!("iso-test-{n}.ls")));
    v.push("iso-test.files".into());
    v.push("iso-test.md5".into());
    v
}

/// A test volume file, if it has been made (on the server) or copied.
pub(crate) fn file(name: &str) -> Option<PathBuf> {
    crate::base::ufs_test_file(name)
}

/// The tree: (path, what) where what is a file's contents, `/` for a
/// directory, or `@target` for a symbolic link; and its mode.
const TREE: &[(&str, &str, u32)] = &[
    ("README.TXT", "read me\n", 0o644),
    ("Mixed Case Long File Name.text", "long name\n", 0o644),
    ("lower.case", "lower\n", 0o600),
    ("exec.sh", "#!/bin/sh\necho hi\n", 0o751),
    ("empty", "", 0o444),
    ("one", "1", 0o644),
    ("dots.in.the.name.tar.gz", "dots\n", 0o644),
    ("a", "/", 0o755),
    ("a/b", "/", 0o705),
    ("a/b/c", "/", 0o755),
    ("a/b/c/d", "/", 0o755),
    ("a/b/c/d/e", "/", 0o755),
    ("a/b/c/d/e/f", "/", 0o755),
    ("a/b/c/d/e/f/g", "/", 0o755),
    ("a/b/c/d/e/f/g/h", "/", 0o755),
    ("a/b/c/d/e/f/g/h/i", "/", 0o755),
    ("a/b/c/d/e/f/g/h/i/deep.txt", "deep\n", 0o644),
    ("a/up", "@../README.TXT", 0o755),
    ("a/abs", "@/etc/rc.conf", 0o755),
    ("link", "@README.TXT", 0o755),
    ("dangling", "@no/such/file", 0o755),
    ("Dir With Spaces", "/", 0o755),
    ("Dir With Spaces/inner file", "inner\n", 0o644),
];

/// A single-quoted shell word.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Makes the [`files`] in `dir`, unless they are there.
pub(crate) fn build(dir: &Path) -> Result<()> {
    if files().iter().all(|f| dir.join(f).exists()) {
        return Ok(());
    }
    eprintln!("xtask: fetch-base: making the ISO 9660 test volume");
    let src = dir.join("iso-src");
    let _ = fs::remove_dir_all(&src);
    fs::create_dir_all(&src)?;
    let mut script = format!("set -e\ncd {}\n", quote(&src.to_string_lossy()));
    for &(path, what, mode) in TREE {
        let p = quote(path);
        if what == "/" {
            script += &format!("mkdir {p}\n");
        } else if let Some(t) = what.strip_prefix('@') {
            script += &format!("ln -s {} {p}\n", quote(t));
            continue;
        } else {
            script += &format!("printf '%s' {} > {p}\n", quote(what));
        }
        script += &format!("chmod {mode:o} {p}\n");
    }
    // Several blocks and a partial page.
    script += "dd if=/rescue/rescue bs=1k count=70 status=none of=big.bin\n";
    // Fixed times, children before their directories.
    for &(path, _, _) in TREE.iter().rev() {
        script += &format!("touch -h -t 202401151230.42 {}\n", quote(path));
    }
    script += "touch -t 202401151230.42 big.bin .\n";
    let script_path = dir.join("iso-test.sh");
    fs::write(&script_path, &script)?;
    util::run(Command::new("sh").arg(&script_path))?;
    let tmp = dir.join("iso-test.iso.tmp");
    let _ = fs::remove_file(&tmp);
    // libarchive's writer (`makefs` has no Joliet): Rock Ridge with the
    // files' own owners and modes, and Joliet.
    util::run(
        Command::new("bsdtar")
            .args(["-c", "-f"])
            .arg(&tmp)
            .args([
                "--format",
                "iso9660",
                "--options",
                "iso9660:rockridge=strict,iso9660:joliet,iso9660:volume-id=ABYSSTEST",
                "-C",
            ])
            .arg(&src)
            .arg("."),
    )?;
    let mnt = dir.join("iso-mnt");
    let _ = fs::create_dir_all(&mnt);
    {
        let md = crate::fs::Md::attach(&tmp, true)?;
        let dev = md.dev();
        for (name, flags) in VIEWS {
            util::run(
                Command::new("doas")
                    .arg("mount_cd9660")
                    .args(*flags)
                    .arg(&dev)
                    .arg(&mnt),
            )?;
            let mut list = format!(
                "cd '{m}' && TZ=UTC BLOCKSIZE=512 ls -aliRnT . > '{d}/iso-test-{name}.ls'",
                m = mnt.display(),
                d = dir.display()
            );
            if *name == "rr" {
                list += &format!(
                    " && find . -type f | sort > '{d}/iso-test.files' && \
                     while IFS= read -r f; do md5 -r \"$f\"; done < '{d}/iso-test.files' \
                     > '{d}/iso-test.md5'",
                    d = dir.display()
                );
            }
            let r = util::run(Command::new("sh").arg("-c").arg(&list));
            let _ = Command::new("doas").arg("umount").arg(&mnt).status();
            r?;
        }
        drop(md);
    }
    let _ = fs::remove_dir_all(&src);
    fs::rename(&tmp, dir.join("iso-test.iso"))?;
    Ok(())
}
