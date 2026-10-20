// SPDX-License-Identifier: BSD-2-Clause
//! `cargo xtask fs-image` and `cargo xtask fs-check`: file-system images
//! made and checked by FreeBSD's own tools, on the build server. They are
//! the oracle for AbyssBSD's UFS2 and FAT code (docs/VFS.md §14): images
//! FreeBSD made are read, and images AbyssBSD wrote must check clean.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::cli::Args;
use crate::util::{self, Result};

/// The file systems the tools handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fs {
    Ufs2,
    Msdos,
}

impl Fs {
    fn parse(s: &str) -> Result<Self> {
        match s {
            "ufs2" | "ufs" => Ok(Self::Ufs2),
            "msdos" | "fat" => Ok(Self::Msdos),
            _ => Err(format!("unknown file system `{s}` (ufs2, msdos)").into()),
        }
    }
}

fn need_freebsd(cmd: &str) -> Result<()> {
    if cfg!(target_os = "freebsd") {
        Ok(())
    } else {
        Err(format!("{cmd} needs FreeBSD's tools: run it on the build server (--remote)").into())
    }
}

/// `fs-image --fs ufs2|msdos --out IMG [--from DIR] [--size SIZE] [--softdep]`:
/// an image of `DIR`'s tree (`makefs`), or an empty file system of `SIZE`
/// (`newfs`, `newfs_msdos`) as FreeBSD's installer would make it.
pub(crate) fn cmd_image(mut args: Args) -> Result<()> {
    let fs = Fs::parse(&args.take_value("--fs")?.ok_or("--fs is required")?)?;
    let out = PathBuf::from(args.take_value("--out")?.ok_or("--out is required")?);
    let from = args.take_value("--from")?.map(PathBuf::from);
    let size = args.take_value("--size")?;
    let softdep = args.take_flag("--softdep");
    args.finish()?;
    need_freebsd("fs-image")?;
    match (&from, fs) {
        (Some(dir), Fs::Ufs2) => {
            let mut c = Command::new("makefs");
            c.args(["-t", "ffs", "-o", "version=2"]);
            if softdep {
                c.args(["-o", "softupdates=1"]);
            }
            if let Some(size) = &size {
                c.args(["-s", size]);
            }
            util::run(c.arg(&out).arg(dir))?;
        }
        (Some(dir), Fs::Msdos) => {
            let mut c = Command::new("makefs");
            c.args(["-t", "msdos"]);
            if let Some(size) = &size {
                c.args(["-s", size]);
            }
            util::run(c.arg(&out).arg(dir))?;
        }
        (None, fs) => {
            let size = size.ok_or("an empty image needs --size")?;
            util::run(Command::new("truncate").args(["-s", &size]).arg(&out))?;
            match fs {
                Fs::Ufs2 => {
                    let mut c = Command::new("newfs");
                    if softdep {
                        c.arg("-U");
                    }
                    util::run(c.arg(&out))?;
                }
                Fs::Msdos => util::run(Command::new("newfs_msdos").arg(&out))?,
            }
        }
    }
    eprintln!("xtask: fs-image: {}", out.display());
    Ok(())
}

/// `fs-check --fs ufs2|msdos IMG`: `fsck_ffs -f -n` or `fsck_msdosfs -n`,
/// read-only. Clean means a zero exit and no question asked but the
/// upgrades FreeBSD offers on any image `makefs` made.
pub(crate) fn cmd_check(mut args: Args) -> Result<()> {
    let fs = Fs::parse(&args.take_value("--fs")?.ok_or("--fs is required")?)?;
    let img = PathBuf::from(args.next_positional().ok_or("no image given")?);
    args.finish()?;
    need_freebsd("fs-check")?;
    let report = check(fs, &img)?;
    print!("{}", report.output);
    if report.problems.is_empty() {
        eprintln!("xtask: fs-check: {} is clean", img.display());
        Ok(())
    } else {
        Err(format!("{}: {}", img.display(), report.problems.join("; ")).into())
    }
}

/// What a check found.
struct Report {
    output: String,
    /// Why the image is not clean; empty if it is.
    problems: Vec<String>,
}

fn check(fs: Fs, img: &Path) -> Result<Report> {
    let mut c = match fs {
        Fs::Ufs2 => {
            let mut c = Command::new("fsck_ffs");
            c.args(["-f", "-n"]);
            c
        }
        Fs::Msdos => {
            let mut c = Command::new("fsck_msdosfs");
            c.arg("-n");
            c
        }
    };
    let out = c.arg(img).output()?;
    let output =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    Ok(Report {
        problems: problems(&output, out.status.code()),
        output,
    })
}

/// Questions `fsck` asks that are not damage: an image file is not a disk,
/// and `makefs` leaves features newer kernels track unset.
const BENIGN: &[&str] = &["CONTINUE?", "UPDATE FILESYSTEM TO TRACK DIRECTORY DEPTH?"];

fn problems(output: &str, status: Option<i32>) -> Vec<String> {
    let mut found: Vec<String> = output
        .lines()
        .map(str::trim)
        .filter(|l| l.contains('?') && !BENIGN.iter().any(|b| l.contains(b)))
        .map(str::to_owned)
        .collect();
    match status {
        Some(0) => {}
        Some(n) => found.push(format!("exit status {n}")),
        None => found.push("killed by a signal".into()),
    }
    found
}

/// An `md` device over an image file, detached (and unmounted first, if
/// mounted) when dropped, whatever happened meanwhile.
pub(crate) struct Md {
    unit: String,
    mnt: Option<PathBuf>,
}

impl Md {
    /// Attaches `img` (read-only if `readonly`).
    pub(crate) fn attach(img: &Path, readonly: bool) -> Result<Self> {
        let mut c = Command::new("doas");
        c.args(["mdconfig", "-a", "-t", "vnode"]);
        if readonly {
            c.args(["-o", "readonly"]);
        }
        let out = c.arg("-f").arg(img).output()?;
        if !out.status.success() {
            return Err(format!("mdconfig: {}", String::from_utf8_lossy(&out.stderr)).into());
        }
        let unit = String::from_utf8_lossy(&out.stdout).trim().to_owned();
        if !unit.starts_with("md") {
            return Err(format!("mdconfig printed {unit:?}").into());
        }
        Ok(Self { unit, mnt: None })
    }

    /// The device node.
    pub(crate) fn dev(&self) -> String {
        format!("/dev/{}", self.unit)
    }

    /// Mounts the device's UFS on `mnt`.
    pub(crate) fn mount(&mut self, mnt: &Path, readonly: bool) -> Result<()> {
        std::fs::create_dir_all(mnt)?;
        let mut c = Command::new("doas");
        c.arg("mount");
        if readonly {
            c.args(["-o", "ro"]);
        }
        util::run(c.arg(self.dev()).arg(mnt))?;
        self.mnt = Some(mnt.to_owned());
        Ok(())
    }
}

impl Drop for Md {
    fn drop(&mut self) {
        if let Some(m) = self.mnt.take() {
            let _ = Command::new("doas").arg("umount").arg(&m).status();
        }
        let _ = Command::new("doas")
            .args(["mdconfig", "-d", "-u", &self.unit])
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_makefs_image_with_only_upgrades_offered_is_clean() {
        let out = "fstest/u.img IS NOT A DISK DEVICE\n\nCONTINUE? yes\n\n\
                   ** Phase 2 - Check Pathnames\n\n\
                   UPDATE FILESYSTEM TO TRACK DIRECTORY DEPTH? no\n\n\
                   3 files, 3 used, 7892 free\n";
        assert!(problems(out, Some(0)).is_empty());
    }

    #[test]
    fn damage_is_reported() {
        let out = "** Phase 1 - Check Blocks and Sizes\n\
                   INCORRECT BLOCK COUNT I=2 (8 should be 0)\nCORRECT? no\n";
        assert_eq!(problems(out, Some(8)), ["CORRECT? no", "exit status 8"]);
    }
}
