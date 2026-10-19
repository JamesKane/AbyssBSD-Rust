// SPDX-License-Identifier: BSD-2-Clause
//! `cargo xtask fetch-base`: FreeBSD's `base.txz`, the userland AbyssBSD
//! runs (on the build server).
//!
//! Each architecture's archive is downloaded once, checked against the
//! SHA-256 pinned below (from the release's `MANIFEST`), and partly
//! unpacked into a root: `/rescue` for the memory-disk image, and the
//! headers and libraries that make it a sysroot for building static test
//! binaries (`tests/perf/ubench`) for either architecture.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::arch::Arch;
use crate::cli::Args;
use crate::util::{self, Result};

pub(crate) const RELEASE: &str = "15.1-RELEASE";

/// What is unpacked from `base.txz`.
const MEMBERS: &[&str] = &["./rescue", "./lib", "./usr/include", "./usr/lib"];

/// `base.txz`'s SHA-256, from each architecture's release `MANIFEST`.
const fn base_sha256(arch: Arch) -> &'static str {
    match arch {
        Arch::X86_64 => "3768988b151c20f965679062b065c63a977d6bbb9f47fd83695ec2c40790c18f",
        Arch::Aarch64 => "5b7a46a0abfbe23a1d4454b5600e2efcfce16b705bc7e9c851c37470c035ef98",
    }
}

/// The release's distribution directory for `arch`.
fn url(arch: Arch) -> String {
    let platform = match arch {
        Arch::X86_64 => "amd64/amd64",
        Arch::Aarch64 => "arm64/aarch64",
    };
    format!("https://download.freebsd.org/releases/{platform}/{RELEASE}")
}

/// Where `arch`'s base lives: `base.txz`, `root/`, and `rescue.tar`.
pub(crate) fn dir(arch: Arch) -> PathBuf {
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    home.join("abyss/dist")
        .join(RELEASE)
        .join(arch.freebsd_name())
}

pub(crate) fn cmd(mut args: Args) -> Result<()> {
    let arches = Arch::from_args(&mut args)?;
    args.finish()?;
    for arch in arches {
        let root = root(arch)?;
        let tar = rescue_image(arch)?;
        println!(
            "fetch-base: {arch}: {} and {}",
            root.display(),
            tar.display()
        );
    }
    Ok(())
}

/// `arch`'s unpacked base, fetching and verifying it on first use.
pub(crate) fn root(arch: Arch) -> Result<PathBuf> {
    if !cfg!(target_os = "freebsd") {
        return Err("fetch-base runs on the build server (use --remote)".into());
    }
    let dir = dir(arch);
    let root = dir.join("root");
    if root.join("rescue/sh").exists() {
        return Ok(root);
    }
    fs::create_dir_all(&dir)?;
    let txz = dir.join("base.txz");
    if !txz.exists() {
        let tmp = dir.join("base.txz.tmp");
        eprintln!("xtask: fetch-base: downloading {arch} base.txz");
        util::run(
            Command::new("fetch")
                .arg("-q")
                .arg("-o")
                .arg(&tmp)
                .arg(format!("{}/base.txz", url(arch))),
        )?;
        fs::rename(&tmp, &txz)?;
    }
    let got = util::output(Command::new("sha256").arg("-q").arg(&txz))?;
    if got.trim() != base_sha256(arch) {
        // A bad download is not kept: the next run fetches it again.
        fs::remove_file(&txz)?;
        return Err(format!("{}: SHA-256 mismatch (removed)", txz.display()).into());
    }
    // Unpack beside the final path and rename on success, so an
    // interrupted run never leaves a half-populated root.
    let tmp = dir.join("root.tmp");
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp)?;
    eprintln!("xtask: fetch-base: unpacking {arch} base.txz");
    util::run(
        Command::new("tar")
            .arg("-xf")
            .arg(&txz)
            .arg("-C")
            .arg(&tmp)
            .args(MEMBERS),
    )?;
    fs::rename(&tmp, &root)?;
    Ok(root)
}

/// `/rescue` as a ustar archive: the memory-disk image the loader preloads
/// (its hard links stay links, so the 150-odd names cost one binary).
pub(crate) fn rescue_image(arch: Arch) -> Result<PathBuf> {
    let root = root(arch)?;
    let tar = dir(arch).join("rescue.tar");
    if !tar.exists() {
        let tmp = tar.with_extension("tar.tmp");
        util::run(
            Command::new("tar")
                .args(["--format", "ustar", "-cf"])
                .arg(&tmp)
                .arg("-C")
                .arg(&root)
                .arg("rescue"),
        )?;
        fs::rename(&tmp, &tar)?;
    }
    Ok(tar)
}

/// Builds a static FreeBSD program from C `source` for `arch`, against that
/// architecture's base as the sysroot.
pub(crate) fn cc_static(arch: Arch, source: &Path, out: &Path) -> Result<()> {
    let root = root(arch)?;
    util::run(
        Command::new("cc")
            .arg(format!("--target={}", arch.clang_target()))
            .arg(format!("--sysroot={}", root.display()))
            .args(["-static", "-O2", "-Wall", "-Wextra", "-Werror", "-o"])
            .arg(out)
            .arg(source),
    )
}
