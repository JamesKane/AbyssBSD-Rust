// SPDX-License-Identifier: BSD-2-Clause
//! The FAT test disk (P5.6) and FreeBSD's view of it, made by
//! `fetch-base` on the build server like the UFS2 one:
//!
//! - `fat-test.img`: a FAT32 volume `newfs_msdos` made, holding a tree of
//!   awkward names written through FreeBSD's own msdosfs (`mount_msdosfs`),
//!   with fixed modification times;
//! - `fat-test.ls`, `fat-test.files`, `fat-test.md5`: FreeBSD's
//!   `ls -aliRnT` (file numbers included), its file list, and `md5 -r` of
//!   them, mounted read-only as root with mode 0755, in the C locale (no
//!   `kiconv`, as AbyssBSD's msdosfs has none).
//!
//! On the way, `lib/fat` reads the volume back and must predict every
//! short name FreeBSD's kernel chose, generation numbers included.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::fs::Md;
use crate::util::{self, Result};

/// The files the test disk consists of.
pub(crate) const FAT_TEST: [&str; 4] = [
    "fat-test.img",
    "fat-test.ls",
    "fat-test.files",
    "fat-test.md5",
];

/// A FAT test file, if it has been made (on the server) or copied.
pub(crate) fn file(name: &str) -> Option<PathBuf> {
    crate::base::ufs_test_file(name)
}

/// The tree, in the order it is made: names that need long names, case
/// changes, generation numbers, characters DOS drops or replaces,
/// ISO 8859-1, and directories (ending in `/`).
const NAMES: &[&[u8]] = &[
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
    b"trailing.",
    b"UPPER.lower",
    b"lower.UPPER",
    b"with space.txt",
    b"dots..ext",
    b"a~1",
    b"1234567890123.txt",
    b"Dir One/",
    b"Dir One/inner file.dat",
    b"Dir One/deeper/",
    b"Dir One/deeper/x.bin",
    b"Dir One/deeper/big.bin",
    b"SUBDIR/",
    b"SUBDIR/FILE.TXT",
    b"SUBDIR/file.txt.bak",
];

/// A single-quoted shell word for `bytes`.
fn quote(bytes: &[u8]) -> Vec<u8> {
    let mut out = vec![b'\''];
    for &b in bytes {
        if b == b'\'' {
            out.extend_from_slice(b"'\\''");
        } else {
            out.push(b);
        }
    }
    out.push(b'\'');
    out
}

/// Makes the [`FAT_TEST`] files in `dir`, unless they are there.
pub(crate) fn build(dir: &Path) -> Result<()> {
    if FAT_TEST.iter().all(|f| dir.join(f).exists()) {
        return Ok(());
    }
    eprintln!("xtask: fetch-base: making the FAT test disk");
    let tmp = dir.join("fat-test.img.tmp");
    let _ = fs::remove_file(&tmp);
    // FAT32 with small clusters, as a small ESP might be.
    util::run(
        Command::new("newfs_msdos")
            .args(["-C", "40m", "-F", "32", "-c", "1"])
            .arg(&tmp)
            .stdout(std::process::Stdio::null()),
    )?;
    let mnt = dir.join("fat-mnt");
    let _ = fs::create_dir_all(&mnt);
    {
        let md = Md::attach(&tmp, false)?;
        let mut script = b"set -e\ncd ".to_vec();
        script.extend(quote(mnt.to_string_lossy().as_bytes()));
        script.push(b'\n');
        for name in NAMES {
            if let Some(d) = name.strip_suffix(b"/") {
                script.extend_from_slice(b"mkdir ");
                script.extend(quote(d));
            } else if name.ends_with(b"big.bin") {
                script.extend_from_slice(b"dd if=/rescue/rescue bs=64k count=5 status=none of=");
                script.extend(quote(name));
            } else {
                script.extend_from_slice(b"printf '%s\\n' ");
                script.extend(quote(name));
                script.extend_from_slice(b" > ");
                script.extend(quote(name));
            }
            script.push(b'\n');
        }
        // Fixed times, children before their directories (making an
        // entry changes no directory time on FAT, but touch does).
        for name in NAMES.iter().rev() {
            script.extend_from_slice(b"touch -t 202401151230.42 ");
            script.extend(quote(name.strip_suffix(b"/").unwrap_or(name)));
            script.push(b'\n');
        }
        let script_path = dir.join("fat-test.sh");
        fs::write(&script_path, &script)?;
        let dev = md.dev();
        util::run(
            Command::new("doas")
                .args(["mount_msdosfs", "-u", "0", "-g", "0", "-m", "755", &dev])
                .arg(&mnt),
        )?;
        let r = util::run(Command::new("doas").arg("sh").arg(&script_path));
        let _ = Command::new("doas").arg("umount").arg(&mnt).status();
        r?;
        // FreeBSD's view, read-only, as the boot test mounts it.
        util::run(
            Command::new("doas")
                .args([
                    "mount_msdosfs",
                    "-o",
                    "ro",
                    "-u",
                    "0",
                    "-g",
                    "0",
                    "-m",
                    "755",
                    &dev,
                ])
                .arg(&mnt),
        )?;
        let list = format!(
            "cd '{m}' && TZ=UTC BLOCKSIZE=512 ls -aliRnT . > '{d}/fat-test.ls' && \
             find . -type f | sort > '{d}/fat-test.files' && \
             while IFS= read -r f; do md5 -r \"$f\"; done < '{d}/fat-test.files' > '{d}/fat-test.md5'",
            m = mnt.display(),
            d = dir.display()
        );
        let r = util::run(Command::new("sh").arg("-c").arg(&list));
        let _ = Command::new("doas").arg("umount").arg(&mnt).status();
        r?;
        drop(md);
    }
    check_short_names(&tmp)?;
    fs::rename(&tmp, dir.join("fat-test.img"))?;
    Ok(())
}

/// `lib/fat` predicts the short name FreeBSD gave each entry: replaying
/// each directory in its on-disk order, the first generation number not
/// yet taken.
fn check_short_names(img: &Path) -> Result<()> {
    let bytes = fs::read(img)?;
    let vol =
        fat::fs::Fat::open(&(), &bytes[..], bytes.len() as u64).map_err(|e| format!("{e:?}"))?;
    let mut bad = Vec::new();
    let mut checked = 0;
    check_dir(&vol, fat::fs::ROOT, &mut bad, &mut checked)?;
    if !bad.is_empty() {
        return Err(format!("short names differ from FreeBSD's: {}", bad.join("; ")).into());
    }
    eprintln!("xtask: fetch-base: {checked} short names match FreeBSD's");
    Ok(())
}

fn check_dir(
    vol: &fat::fs::Fat<&[u8]>,
    start: u32,
    bad: &mut Vec<String>,
    checked: &mut usize,
) -> Result<()> {
    let mut names = Vec::new();
    vol.entries(&(), start, 0, &mut |n| {
        names.push(n.clone());
        false
    })
    .map_err(|e| format!("{e:?}"))?;
    let mut taken: Vec<[u8; 11]> = Vec::new();
    for n in &names {
        if n.name == b"." || n.name == b".." {
            taken.push(n.entry.name);
            continue;
        }
        let want = (1..)
            .map(|g| fat::fs::conv::unix2dosfn(&n.name, g))
            .find(|(d, c)| *c == fat::fs::conv::Conv::Invalid || !taken.contains(d))
            .map(|(d, _)| d)
            .unwrap_or([b'?'; 11]);
        if want != n.entry.name {
            bad.push(format!(
                "{:?}: FreeBSD {:?}, lib/fat {:?}",
                String::from_utf8_lossy(&n.name),
                String::from_utf8_lossy(&n.entry.name),
                String::from_utf8_lossy(&want)
            ));
        }
        taken.push(n.entry.name);
        *checked += 1;
        if n.entry.kind() == fat::fs::Kind::Directory {
            check_dir(vol, n.entry.start, bad, checked)?;
        }
    }
    Ok(())
}
