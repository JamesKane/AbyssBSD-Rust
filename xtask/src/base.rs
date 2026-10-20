// SPDX-License-Identifier: BSD-2-Clause
//! `cargo xtask fetch-base`: FreeBSD's `base.txz`, the userland AbyssBSD
//! runs (on the build server).
//!
//! Each architecture's archive is downloaded once, checked against the
//! SHA-256 pinned below (from the release's `MANIFEST`), and partly
//! unpacked into a root: `/rescue` for the memory-disk image, and the
//! headers and libraries that make it a sysroot for building static test
//! binaries (`tests/user`, `tests/perf/ubench`) for either architecture.
//!
//! Elsewhere (a Linux workstation), `fetch-base` copies what the server
//! built: `rescue.tar`, and a trimmed sysroot (headers, start files, and
//! the static libraries a test program links) for clang and lld to build
//! the same programs with.
//!
//! The memory disk is `rescue.tar` with the test programs appended under
//! `usr/tests/abyss/`, rebuilt with each image.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::arch::Arch;
use crate::cli::Args;
use crate::util::{self, Result};

pub(crate) const RELEASE: &str = "15.1-RELEASE";

/// What a workstation copies of the root to build test programs.
const SYSROOT_MEMBERS: &[&str] = &[
    "usr/include",
    "usr/lib/crt1.o",
    "usr/lib/crti.o",
    "usr/lib/crtn.o",
    "usr/lib/crtbeginT.o",
    "usr/lib/crtend.o",
    "usr/lib/libc.a",
    "usr/lib/libsys.a",
    "usr/lib/libcompiler_rt.a",
    "usr/lib/libgcc.a",
    "usr/lib/libgcc_eh.a",
    "usr/lib/libthr.a",
    "usr/lib/libm.a",
];

/// Where the memory disk puts the test programs.
const TESTS_DIR: &str = "usr/tests/abyss";

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
    if !cfg!(target_os = "freebsd") {
        // Elsewhere, copy the memory-disk images and sysroots the server
        // built.
        for arch in arches {
            let local = local_rescue_image(arch);
            fs::create_dir_all(local.parent().ok_or("no parent")?)?;
            let remote = format!(
                "{}:abyss/dist/{RELEASE}/{}/rescue.tar",
                crate::remote::host(),
                arch.freebsd_name()
            );
            util::run(Command::new("scp").arg("-q").arg(&remote).arg(&local))
                .map_err(|e| format!("{e} (run `cargo xtask --remote fetch-base` first)"))?;
            let sysroot = local_sysroot(arch);
            let tmp = sysroot.with_extension("tmp");
            let _ = fs::remove_dir_all(&tmp);
            fs::create_dir_all(&tmp)?;
            let copy = format!(
                "ssh -n {} 'cd abyss/dist/{RELEASE}/{}/root && tar -cf - {}' | tar -xf - -C {}",
                crate::remote::host(),
                arch.freebsd_name(),
                SYSROOT_MEMBERS.join(" "),
                tmp.display()
            );
            util::run(Command::new("sh").arg("-c").arg(&copy))?;
            let _ = fs::remove_dir_all(&sysroot);
            fs::rename(&tmp, &sysroot)?;
            println!(
                "fetch-base: {arch}: {} and {}",
                local.display(),
                sysroot.display()
            );
        }
        return Ok(());
    }
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

/// Where a non-FreeBSD host keeps its copy of `arch`'s `rescue.tar`.
fn local_rescue_image(arch: Arch) -> PathBuf {
    local_dir(arch).join("rescue.tar")
}

/// Where a non-FreeBSD host keeps its copies of `arch`'s base.
fn local_dir(arch: Arch) -> PathBuf {
    util::root()
        .join("build/dist")
        .join(RELEASE)
        .join(arch.freebsd_name())
}

/// Where a non-FreeBSD host keeps `arch`'s trimmed sysroot.
fn local_sysroot(arch: Arch) -> PathBuf {
    local_dir(arch).join("sysroot")
}

/// The sysroot to build `arch`'s FreeBSD programs against.
fn sysroot(arch: Arch) -> Result<PathBuf> {
    if cfg!(target_os = "freebsd") {
        return root(arch);
    }
    let s = local_sysroot(arch);
    if s.join("usr/lib/libc.a").exists() {
        Ok(s)
    } else {
        Err(format!("{}: no sysroot (run `cargo xtask fetch-base`)", s.display()).into())
    }
}

/// `arch`'s memory-disk image, if it has been fetched (on the server, by
/// `fetch-base`; elsewhere, copied by it).
fn existing_rescue_image(arch: Arch) -> Option<PathBuf> {
    let p = if cfg!(target_os = "freebsd") {
        dir(arch).join("rescue.tar")
    } else {
        local_rescue_image(arch)
    };
    p.exists().then_some(p)
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
/// architecture's base as the sysroot (with the base system's `cc` on
/// FreeBSD, clang and lld elsewhere).
pub(crate) fn cc_static(arch: Arch, source: &Path, out: &Path) -> Result<()> {
    let root = sysroot(arch)?;
    let mut cc = if cfg!(target_os = "freebsd") {
        Command::new("cc")
    } else {
        let mut c = Command::new("clang");
        c.arg("-fuse-ld=lld");
        c
    };
    util::run(
        cc.arg(format!("--target={}", arch.clang_target()))
            .arg(format!("--sysroot={}", root.display()))
            .args(["-static", "-s", "-O2", "-Wall", "-Wextra", "-Werror", "-o"])
            .arg(out)
            .arg(source),
    )
}

/// [`cc_static`] for third-party sources (FreeBSD's test programs): their
/// warnings are not ours to fix.
pub(crate) fn cc_static_foreign(arch: Arch, source: &Path, out: &Path) -> Result<()> {
    let root = sysroot(arch)?;
    let mut cc = if cfg!(target_os = "freebsd") {
        Command::new("cc")
    } else {
        let mut c = Command::new("clang");
        c.arg("-fuse-ld=lld");
        c
    };
    util::run(
        cc.arg(format!("--target={}", arch.clang_target()))
            .arg(format!("--sysroot={}", root.display()))
            .args(["-static", "-s", "-O2", "-w", "-o"])
            .arg(out)
            .arg(source),
    )
}

/// The memory-disk image for `arch`, if `fetch-base` has run: `/rescue`,
/// an empty `/dev`, and each `tests/user/*.c` built into
/// `usr/tests/abyss/` (rebuilt when its source or xtask changes), then
/// the `extra` files (path on the memory disk, file here).
pub(crate) fn md_image(arch: Arch, extra: &[(&str, &Path)]) -> Result<Option<Vec<u8>>> {
    let Some(rescue) = existing_rescue_image(arch) else {
        return Ok(None);
    };
    let mut md = fs::read(&rescue).map_err(|e| format!("{}: {e}", rescue.display()))?;
    let end = ustar::Archive::new(&md)
        .entries_len()
        .map_err(|e| format!("{}: {e:?}", rescue.display()))?;
    md.truncate(end);
    let out_dir = util::root()
        .join("build")
        .join(arch.to_string())
        .join("user");
    fs::create_dir_all(&out_dir)?;
    let mut sources: Vec<PathBuf> = fs::read_dir(util::root().join("tests/user"))?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<_>>()?;
    sources.retain(|p| p.extension().is_some_and(|x| x == "c"));
    sources.sort();
    // devfs's mount point, and an empty one for tests to mount on.
    append(&mut md, "dev/", b'5', 0o555, &[])?;
    append(&mut md, "mnt/", b'5', 0o755, &[])?;
    let mut dir = String::new();
    for part in TESTS_DIR.split('/') {
        dir = if dir.is_empty() {
            part.to_string()
        } else {
            format!("{dir}/{part}")
        };
        append(&mut md, &format!("{dir}/"), b'5', 0o755, &[])?;
    }
    let built_by = std::env::current_exe()
        .and_then(fs::metadata)
        .and_then(|m| m.modified())
        .ok();
    for src in sources {
        let name = src
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or("bad test name")?;
        let bin = out_dir.join(name);
        // Stale if older than its source or than xtask itself, whose
        // build flags may have changed.
        let stale = match (
            fs::metadata(&src)?.modified(),
            fs::metadata(&bin).and_then(|m| m.modified()),
        ) {
            (Ok(s), Ok(b)) => s > b || built_by.is_none_or(|x| x > b),
            _ => true,
        };
        if stale {
            cc_static(arch, &src, &bin)?;
        }
        let data = fs::read(&bin)?;
        append(&mut md, &format!("{TESTS_DIR}/{name}"), b'0', 0o555, &data)?;
    }
    for (path, file) in extra {
        if let Some((dir, _)) = path.rsplit_once('/') {
            append(&mut md, &format!("{dir}/"), b'5', 0o755, &[])?;
        }
        let data = fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
        append(&mut md, path, b'0', 0o555, &data)?;
    }
    md.resize(md.len() + 2 * ustar::BLOCK, 0);
    Ok(Some(md))
}

/// Appends a ustar entry (root-owned, time zero, so images are
/// reproducible).
fn append(tar: &mut Vec<u8>, path: &str, kind: u8, mode: u32, data: &[u8]) -> Result<()> {
    let mut h = [0u8; ustar::BLOCK];
    if path.len() > 100 {
        return Err(format!("{path}: name too long for ustar").into());
    }
    h[..path.len()].copy_from_slice(path.as_bytes());
    let octal = |field: &mut [u8], v: u64| {
        let s = format!("{v:0w$o}\0", w = field.len() - 1);
        field.copy_from_slice(s.as_bytes());
    };
    octal(&mut h[100..108], u64::from(mode));
    octal(&mut h[108..116], 0);
    octal(&mut h[116..124], 0);
    octal(&mut h[124..136], data.len() as u64);
    octal(&mut h[136..148], 0);
    h[156] = kind;
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h[265..269].copy_from_slice(b"root");
    h[297..302].copy_from_slice(b"wheel");
    h[148..156].fill(b' ');
    let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
    h[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
    tar.extend_from_slice(&h);
    tar.extend_from_slice(data);
    tar.resize(tar.len().next_multiple_of(ustar::BLOCK), 0);
    Ok(())
}
