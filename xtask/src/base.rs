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
        // The UFS2 test disk and FreeBSD's listings of it.
        let local = local_dir(Arch::X86_64);
        fs::create_dir_all(&local)?;
        for name in UFS_TEST {
            let remote = format!(
                "{}:abyss/dist/{RELEASE}/{}/{name}",
                crate::remote::host(),
                Arch::X86_64.freebsd_name()
            );
            util::run(
                Command::new("scp")
                    .arg("-q")
                    .arg(&remote)
                    .arg(local.join(name)),
            )?;
        }
        // And the FAT one, and the empty soft-updates one.
        for name in crate::fattest::FAT_TEST.iter().chain([&SOFT_TEST]) {
            let remote = format!(
                "{}:abyss/dist/{RELEASE}/{}/{name}",
                crate::remote::host(),
                Arch::X86_64.freebsd_name()
            );
            util::run(
                Command::new("scp")
                    .arg("-q")
                    .arg(&remote)
                    .arg(local.join(name)),
            )?;
        }
        println!(
            "fetch-base: the UFS2 and FAT test disks in {}",
            local.display()
        );
        return Ok(());
    }
    ufs_test(&dir(Arch::X86_64), &root(Arch::X86_64)?)?;
    crate::fattest::build(&dir(Arch::X86_64))?;
    soft_test(&dir(Arch::X86_64))?;
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

/// The UFS2 test disk (P5.4) and FreeBSD's view of it: `ufs-test.img`,
/// which `makefs` made of `/rescue`, `/lib`, and `/usr/include`; and from
/// FreeBSD's own tools over it, mounted read-only, `ufs-test.ls`
/// (`ls -lRnT` in UTC, in 512-byte blocks as with no `BLOCKSIZE`),
/// `ufs-test.files` (every regular file), and
/// `ufs-test.md5` (`md5 -r` of those). The kernel test mounts the disk
/// and must print the same.
pub(crate) const UFS_TEST: [&str; 4] = [
    "ufs-test.img",
    "ufs-test.ls",
    "ufs-test.files",
    "ufs-test.md5",
];

/// An empty 32 MiB UFS2 file system with soft updates (`newfs -U`), for
/// `lib/ufs`'s soft-updates tests where FreeBSD's tools are not.
pub(crate) const SOFT_TEST: &str = "soft-test.img";

/// Makes [`SOFT_TEST`] in `dir`, unless it is there.
fn soft_test(dir: &Path) -> Result<()> {
    let img = dir.join(SOFT_TEST);
    if img.exists() {
        return Ok(());
    }
    let tmp = dir.join("soft-test.img.tmp");
    fs::File::create(&tmp)?.set_len(32 << 20)?;
    util::run(
        Command::new("newfs")
            .arg("-U")
            .arg(&tmp)
            .stdout(std::process::Stdio::null()),
    )?;
    fs::rename(&tmp, &img)?;
    Ok(())
}

/// A UFS2 test file, if it has been made (on the server) or copied.
pub(crate) fn ufs_test_file(name: &str) -> Option<PathBuf> {
    let dir = if cfg!(target_os = "freebsd") {
        dir(Arch::X86_64)
    } else {
        local_dir(Arch::X86_64)
    };
    Some(dir.join(name)).filter(|p| p.exists())
}

/// Makes the [`UFS_TEST`] files in `dir` from base tree `root`, unless
/// they are there.
fn ufs_test(dir: &Path, root: &Path) -> Result<()> {
    if UFS_TEST.iter().all(|f| dir.join(f).exists()) {
        return Ok(());
    }
    eprintln!("xtask: fetch-base: making the UFS2 test disk");
    let stage = dir.join("ufs-tree");
    let _ = fs::remove_dir_all(&stage);
    fs::create_dir_all(&stage)?;
    let copy = format!(
        "tar -C '{}' -cf - rescue lib usr/include | tar -C '{}' -xf -",
        root.display(),
        stage.display()
    );
    util::run(Command::new("sh").arg("-c").arg(&copy))?;
    let tmp = dir.join("ufs-test.img.tmp");
    let _ = fs::remove_file(&tmp);
    util::run(
        Command::new("makefs")
            .args(["-t", "ffs", "-o", "version=2"])
            .arg(&tmp)
            .arg(&stage),
    )?;
    {
        let mut md = crate::fs::Md::attach(&tmp, true)?;
        let mnt = dir.join("ufs-mnt");
        md.mount(&mnt, true)?;
        let list = format!(
            "cd '{m}' && TZ=UTC BLOCKSIZE=512 ls -lRnT . > '{d}/ufs-test.ls' && \
             find . -type f | sort > '{d}/ufs-test.files' && \
             md5 -r $(cat '{d}/ufs-test.files') > '{d}/ufs-test.md5'",
            m = mnt.display(),
            d = dir.display()
        );
        util::run(Command::new("sh").arg("-c").arg(&list))?;
    }
    fs::rename(&tmp, dir.join("ufs-test.img"))?;
    let _ = fs::remove_dir_all(&stage);
    Ok(())
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
    cc_static_with(arch, source, &[], out)
}

/// [`cc_static`] with extra compiler `args` (`-D`, libraries), after the
/// source.
pub(crate) fn cc_static_with(arch: Arch, source: &Path, args: &[&str], out: &Path) -> Result<()> {
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
            .arg(source)
            .args(args),
    )
}

/// The libraries a test program's source asks for, on a line
/// "Links with: -lpthread ..." in its header comment.
pub(crate) fn link_args(source: &Path) -> Vec<String> {
    fs::read_to_string(source)
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| {
                l.split_once("Links with:")
                    .map(|(_, rest)| rest.to_string())
            })
        })
        .map(|rest| rest.split_whitespace().map(String::from).collect())
        .unwrap_or_default()
}

/// Builds `tests/user/*.c` again as dynamically linked PIEs into `out`
/// (when stale), against the full base on the server: the user tests,
/// run through rtld and base's shared libraries (ROADMAP P6.1). A test
/// whose source says "Built static only" (it reaches into libc's
/// internals) is left out. Returns each program's name and file.
pub(crate) fn dynamic_user_tests(arch: Arch, out: &Path) -> Result<Vec<(String, PathBuf)>> {
    if !cfg!(target_os = "freebsd") {
        return Err("dynamic test programs need the server's full base".into());
    }
    let root = root(arch)?;
    fs::create_dir_all(out)?;
    let built_by = std::env::current_exe()
        .and_then(fs::metadata)
        .and_then(|m| m.modified())
        .ok();
    let mut sources: Vec<PathBuf> = fs::read_dir(util::root().join("tests/user"))?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<_>>()?;
    sources.retain(|p| {
        p.extension().is_some_and(|x| x == "c")
            && !fs::read_to_string(p).is_ok_and(|s| s.contains("Built static only"))
    });
    sources.sort();
    let mut built = Vec::new();
    for src in sources {
        let name = src
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or("bad test name")?
            .to_string();
        let bin = out.join(&name);
        let stale = match (
            fs::metadata(&src)?.modified(),
            fs::metadata(&bin).and_then(|m| m.modified()),
        ) {
            (Ok(s), Ok(b)) => s > b || built_by.is_none_or(|x| x > b),
            _ => true,
        };
        if stale {
            util::run(
                Command::new("cc")
                    .arg(format!("--target={}", arch.clang_target()))
                    .arg(format!("--sysroot={}", root.display()))
                    .args([
                        "-fPIE", "-pie", "-s", "-O2", "-Wall", "-Wextra", "-Werror", "-o",
                    ])
                    .arg(&bin)
                    .arg(&src)
                    .args(link_args(&src)),
            )?;
        }
        built.push((name, bin));
    }
    Ok(built)
}

/// [`cc_static`] for third-party sources (FreeBSD's test programs): their
/// warnings are not ours to fix.
pub(crate) fn cc_static_foreign(arch: Arch, source: &Path, out: &Path) -> Result<()> {
    cc_static_program(arch, &[source], &[], out)
}

/// A static program from third-party C `sources`, with extra compiler
/// `args` (include directories, libraries last).
fn cc_static_program(arch: Arch, sources: &[&Path], args: &[String], out: &Path) -> Result<()> {
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
            .args(sources)
            .args(args),
    )
}

/// `kbdcontrol` (its scanner made by `lex`) and `vidcontrol`, built
/// static into `out` when stale; none without the FreeBSD tree.
fn console_tools(
    arch: Arch,
    out: &Path,
    built_by: Option<std::time::SystemTime>,
) -> Result<Vec<(&'static str, PathBuf)>> {
    let src = util::root().join("../freebsd-src/usr.sbin");
    if !src.join("kbdcontrol").exists() {
        return Ok(Vec::new());
    }
    let fresh = |bin: &Path, dir: &Path| -> Result<bool> {
        let Ok(b) = fs::metadata(bin).and_then(|m| m.modified()) else {
            return Ok(false);
        };
        let mut newest = built_by.unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        for e in fs::read_dir(dir)? {
            newest = newest.max(e?.metadata()?.modified()?);
        }
        Ok(newest <= b)
    };
    let kbd = src.join("kbdcontrol");
    let kbdcontrol = out.join("kbdcontrol");
    if !fresh(&kbdcontrol, &kbd)? {
        let lex = out.join("kbdcontrol-lex.c");
        util::run(
            Command::new("lex")
                .arg("-o")
                .arg(&lex)
                .arg(kbd.join("lex.l")),
        )?;
        let main = kbd.join("kbdcontrol.c");
        let args = [
            format!("-I{}", kbd.display()),
            // As its Makefile builds it.
            "-DCOMPAT_FREEBSD13".into(),
        ];
        cc_static_program(arch, &[&main, &lex], &args, &kbdcontrol)?;
    }
    let vid = src.join("vidcontrol");
    let vidcontrol = out.join("vidcontrol");
    if !fresh(&vidcontrol, &vid)? {
        let sources = [vid.join("vidcontrol.c"), vid.join("decode.c")];
        let refs: Vec<&Path> = sources.iter().map(PathBuf::as_path).collect();
        cc_static_program(arch, &refs, &[format!("-I{}", vid.display())], &vidcontrol)?;
    }
    Ok(vec![("kbdcontrol", kbdcontrol), ("vidcontrol", vidcontrol)])
}

/// The parts of the FreeBSD tree beside this one that third-party
/// programs built here come from (they are never kept here); the remote
/// mirror copies them beside its tree.
pub(crate) const FREEBSD_SOURCES: &[&str] = &[
    "tools/regression/fsx",
    "usr.sbin/kbdcontrol",
    "usr.sbin/vidcontrol",
    "usr.bin/find",
    "contrib/mtree",
    "contrib/mknod",
    "lib/libnetbsd",
    "contrib/pjdfstest",
    "tests/sys/pjdfstest",
    "contrib/one-true-awk",
    "usr.bin/grep",
    "usr.bin/wc",
    "usr.bin/cut",
    "usr.bin/dirname",
    "usr.bin/tr",
    "usr.bin/mkfifo",
    "usr.bin/touch",
    "usr.bin/which",
    "bin/sleep",
];

/// Programs pjdfstest's scripts run that `/rescue` lacks (or has only
/// crunched), built from
/// these directories of the FreeBSD tree with these sources and
/// libraries: (name, directory, sources, libraries).
const SIMPLE_TOOLS: &[(&str, &str, &[&str], &[&str])] = &[
    (
        "grep",
        "usr.bin/grep",
        &["file.c", "grep.c", "queue.c", "util.c"],
        &["-lregex"],
    ),
    ("wc", "usr.bin/wc", &["wc.c"], &["-lxo", "-lutil"]),
    ("cut", "usr.bin/cut", &["cut.c"], &[]),
    ("dirname", "usr.bin/dirname", &["dirname.c"], &[]),
    (
        "tr",
        "usr.bin/tr",
        &["cmap.c", "cset.c", "str.c", "tr.c"],
        &[],
    ),
    ("mkfifo", "usr.bin/mkfifo", &["mkfifo.c"], &[]),
    ("touch", "usr.bin/touch", &["touch.c"], &[]),
    ("which", "usr.bin/which", &["which.c"], &[]),
    // A program of its own (`/rescue`'s is one crunched binary, which
    // runs as nothing else when copied under another name).
    ("sleep", "bin/sleep", &["sleep.c"], &[]),
];

/// FreeBSD's `find` and `mtree`, and what pjdfstest's scripts need
/// beyond `/rescue` (`awk`, `grep`, `wc`, `cut`, `dirname`, `tr`, `mkfifo`,
/// `touch`, `which`, `sleep`), built
/// static from the FreeBSD tree beside this one, for the UFS2 root's
/// tests until dynamic linking (P6) lets base's own run: (name, program).
/// FreeBSD only (they link `libmd`, `libutil`, `libxo`, and `libregex`
/// from the full base, and `awk` runs a generated helper).
pub(crate) fn static_tools(arch: Arch) -> Result<Vec<(&'static str, PathBuf)>> {
    let src = util::root().join("../freebsd-src");
    let out = util::root()
        .join("build")
        .join(arch.to_string())
        .join("tools");
    fs::create_dir_all(&out)?;
    let built_by = std::env::current_exe()
        .and_then(fs::metadata)
        .and_then(|m| m.modified())
        .ok();
    let fresh = |bin: &Path| {
        fs::metadata(bin)
            .and_then(|m| m.modified())
            .is_ok_and(|b| built_by.is_some_and(|x| x <= b))
    };

    let find = out.join("find");
    if !fresh(&find) {
        let dir = src.join("usr.bin/find");
        let getdate = out.join("getdate.c");
        util::run(
            Command::new("yacc")
                .arg("-o")
                .arg(&getdate)
                .arg(dir.join("getdate.y")),
        )?;
        let mut sources: Vec<PathBuf> = [
            "find.c",
            "function.c",
            "ls.c",
            "main.c",
            "misc.c",
            "operator.c",
            "option.c",
            "printf.c",
        ]
        .iter()
        .map(|f| dir.join(f))
        .collect();
        sources.push(getdate);
        let refs: Vec<&Path> = sources.iter().map(PathBuf::as_path).collect();
        cc_static_program(arch, &refs, &[format!("-I{}", dir.display())], &find)?;
    }

    let mtree = out.join("mtree");
    if !fresh(&mtree) {
        let m = src.join("contrib/mtree");
        let netbsd = src.join("lib/libnetbsd");
        let mut sources: Vec<PathBuf> = [
            "compare.c",
            "crc.c",
            "create.c",
            "excludes.c",
            "getid.c",
            "misc.c",
            "mtree.c",
            "only.c",
            "spec.c",
            "specspec.c",
            "verify.c",
        ]
        .iter()
        .map(|f| m.join(f))
        .collect();
        sources.push(src.join("contrib/mknod/pack_dev.c"));
        for f in ["efun.c", "sockaddr_snprintf.c", "strsuftoll.c", "util.c"] {
            sources.push(netbsd.join(f));
        }
        let refs: Vec<&Path> = sources.iter().map(PathBuf::as_path).collect();
        let args = [
            format!("-I{}", src.join("contrib/mknod").display()),
            format!("-I{}", netbsd.display()),
            "-lmd".into(),
            "-lutil".into(),
        ];
        cc_static_program(arch, &refs, &args, &mtree)?;
    }

    let mut tools = vec![("find", find), ("mtree", mtree)];
    for &(name, dir, files, libs) in SIMPLE_TOOLS {
        let bin = out.join(name);
        if !fresh(&bin) {
            let dir = src.join(dir);
            let sources: Vec<PathBuf> = files.iter().map(|f| dir.join(f)).collect();
            let refs: Vec<&Path> = sources.iter().map(PathBuf::as_path).collect();
            let mut args = vec![format!("-I{}", dir.display())];
            args.extend(libs.iter().map(|l| (*l).to_string()));
            cc_static_program(arch, &refs, &args, &bin)?;
        }
        tools.push((name, bin));
    }

    // awk's parser comes from yacc, and its table of operators from a
    // helper built for this host and run over the parser's header.
    let awk = out.join("awk");
    if !fresh(&awk) {
        let dir = src.join("contrib/one-true-awk");
        let gen_dir = out.join("awk-gen");
        fs::create_dir_all(&gen_dir)?;
        let gram = gen_dir.join("awkgram.tab.c");
        util::run(
            Command::new("yacc")
                .arg("-d")
                .arg("-o")
                .arg(&gram)
                .arg(dir.join("awkgram.y")),
        )?;
        let maketab = gen_dir.join("maketab");
        util::run(
            Command::new("cc")
                .arg("-o")
                .arg(&maketab)
                .arg(format!("-I{}", gen_dir.display()))
                .arg(dir.join("maketab.c")),
        )?;
        let table = Command::new(&maketab)
            .arg(gen_dir.join("awkgram.tab.h"))
            .output()?;
        if !table.status.success() {
            return Err("awk: maketab failed".into());
        }
        let proctab = gen_dir.join("proctab.c");
        fs::write(&proctab, table.stdout)?;
        let mut sources: Vec<PathBuf> = [
            "b.c", "lex.c", "lib.c", "main.c", "parse.c", "run.c", "tran.c",
        ]
        .iter()
        .map(|f| dir.join(f))
        .collect();
        sources.push(gram);
        sources.push(proctab);
        let refs: Vec<&Path> = sources.iter().map(PathBuf::as_path).collect();
        let args = [
            "-DHAS_ISBLANK".to_string(),
            "-DFOPEN_MAX=64".into(),
            format!("-I{}", gen_dir.display()),
            format!("-I{}", dir.display()),
            "-lm".into(),
        ];
        cc_static_program(arch, &refs, &args, &awk)?;
    }
    tools.push(("awk", awk));
    // run-atf's helper for cases that need an unprivileged user.
    let asuser = out.join("asuser");
    if !fresh(&asuser) {
        cc_static_program(
            arch,
            &[&util::root().join("tests/kyua/asuser.c")],
            &[],
            &asuser,
        )?;
    }
    tools.push(("asuser", asuser));
    Ok(tools)
}

/// pjdfstest as FreeBSD installs it (`/usr/tests/sys/pjdfstest`): the
/// program built static from the FreeBSD tree beside this one, FreeBSD's
/// `conf`, `misc.sh`, and each group's scripts, plus our runner
/// (`run.sh`), in a directory of its own. FreeBSD only.
pub(crate) fn pjdfstest_suite(arch: Arch) -> Result<PathBuf> {
    let src = util::root().join("../freebsd-src");
    let out = util::root()
        .join("build")
        .join(arch.to_string())
        .join("pjdfstest");
    let _ = fs::remove_dir_all(&out);
    fs::create_dir_all(&out)?;
    let contrib = src.join("contrib/pjdfstest");
    cc_static_program(
        arch,
        &[&contrib.join("pjdfstest.c")],
        &[format!("-I{}", src.join("tests/sys/pjdfstest").display())],
        &out.join("pjdfstest"),
    )?;
    fs::copy(src.join("tests/sys/pjdfstest/tests/conf"), out.join("conf"))?;
    fs::copy(contrib.join("tests/misc.sh"), out.join("misc.sh"))?;
    fs::copy(
        util::root().join("tests/pjdfstest/run.sh"),
        out.join("run.sh"),
    )?;
    for group in fs::read_dir(contrib.join("tests"))? {
        let group = group?.path();
        if !group.is_dir() {
            continue;
        }
        let name = group.file_name().ok_or("bad pjdfstest group")?;
        fs::create_dir_all(out.join(name))?;
        for t in fs::read_dir(&group)? {
            let t = t?.path();
            if t.extension().is_some_and(|x| x == "t") {
                fs::copy(&t, out.join(name).join(t.file_name().ok_or("bad name")?))?;
            }
        }
    }
    Ok(out)
}

/// The whole of `arch`'s `base.txz`, unpacked once beside it. Unpacked
/// unprivileged, it has lost owners and file flags: [`spec`] has them.
pub(crate) fn full(arch: Arch) -> Result<PathBuf> {
    // Fetches and checks base.txz if need be.
    root(arch)?;
    let dir = dir(arch);
    let full = dir.join("full");
    if full.join("bin/sh").exists() {
        return Ok(full);
    }
    let tmp = dir.join("full.tmp");
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp)?;
    eprintln!("xtask: unpacking all of {arch} base.txz");
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

/// `tests.txz`'s SHA-256, from each architecture's release `MANIFEST`.
const fn tests_sha256(arch: Arch) -> &'static str {
    match arch {
        Arch::X86_64 => "4ea87202ffe0847cb53feaedac824278b5696d907ed4485678a9a86fb41a5531",
        Arch::Aarch64 => "ddd62669f98aeefcfeb38296edbbc6bcb0b6a71edef1c1819f6e0611a3839a0c",
    }
}

/// `arch`'s `tests.txz` (FreeBSD's test suite, `/usr/tests`), fetched,
/// checked, and unpacked once beside the base. Returns the unpacked tree.
pub(crate) fn tests(arch: Arch) -> Result<PathBuf> {
    if !cfg!(target_os = "freebsd") {
        return Err("tests.txz is kept on the build server (use --remote)".into());
    }
    let dir = dir(arch);
    let tree = dir.join("tests");
    if tree.join("usr/tests/Kyuafile").exists() {
        return Ok(tree);
    }
    fs::create_dir_all(&dir)?;
    let txz = dir.join("tests.txz");
    if !txz.exists() {
        let tmp = dir.join("tests.txz.tmp");
        eprintln!("xtask: downloading {arch} tests.txz");
        util::run(
            Command::new("fetch")
                .arg("-q")
                .arg("-o")
                .arg(&tmp)
                .arg(format!("{}/tests.txz", url(arch))),
        )?;
        fs::rename(&tmp, &txz)?;
    }
    let got = util::output(Command::new("sha256").arg("-q").arg(&txz))?;
    if got.trim() != tests_sha256(arch) {
        fs::remove_file(&txz)?;
        return Err(format!("{}: SHA-256 mismatch (removed)", txz.display()).into());
    }
    let tmp = dir.join("tests.tmp");
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp)?;
    eprintln!("xtask: unpacking {arch} tests.txz");
    util::run(Command::new("tar").arg("-xf").arg(&txz).arg("-C").arg(&tmp))?;
    fs::rename(&tmp, &tree)?;
    Ok(tree)
}

/// An mtree specification of `arch`'s base contents: every entry's type,
/// size, link target, and SHA-256, as FreeBSD's `mtree -c` finds them in
/// [`full`] (`base.txz`'s own records give hard links no size).
pub(crate) fn data_spec(arch: Arch) -> Result<PathBuf> {
    let tree = full(arch)?;
    let spec = dir(arch).join("base-data.mtree");
    if spec.exists() {
        return Ok(spec);
    }
    let out = Command::new("mtree")
        .args(["-c", "-k", "type,size,link,sha256digest", "-p"])
        .arg(&tree)
        .output()?;
    if !out.status.success() {
        return Err(format!("mtree -c: {}", String::from_utf8_lossy(&out.stderr)).into());
    }
    fs::write(&spec, out.stdout)?;
    Ok(spec)
}

/// An mtree specification of `arch`'s `base.txz` (every entry's type,
/// owner, mode, file flags, and link target), for `makefs -F` to give a
/// [`full`] unpacking what it lost.
pub(crate) fn spec(arch: Arch) -> Result<PathBuf> {
    root(arch)?;
    let dir = dir(arch);
    let spec = dir.join("base.mtree");
    if spec.exists() {
        return Ok(spec);
    }
    let raw = dir.join("base.mtree.tmp");
    util::run(
        Command::new("tar")
            .arg("-cf")
            .arg(&raw)
            .args([
                "--format=mtree",
                "--options=!all,type,uid,gid,mode,flags,link",
            ])
            .arg(format!("@{}", dir.join("base.txz").display())),
    )?;
    // bsdtar names the top `/.`, which makefs does not take.
    let text = fs::read_to_string(&raw)?;
    let fixed: String = text
        .lines()
        .map(|l| match l.strip_prefix("/. ") {
            Some(rest) => format!(". {rest}\n"),
            None => format!("{l}\n"),
        })
        .collect();
    fs::write(&spec, fixed)?;
    fs::remove_file(&raw)?;
    Ok(spec)
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
    // Mount points: devfs's, tmpfs's, an empty one for tests, and the
    // benchmarks'.
    append(&mut md, "boot/", b'5', 0o755, &[])?;
    append(&mut md, "boot/efi/", b'5', 0o755, &[])?;
    append(&mut md, "dev/", b'5', 0o555, &[])?;
    append(&mut md, "mnt/", b'5', 0o755, &[])?;
    append(&mut md, "tmp/", b'5', 0o1777, &[])?;
    append(&mut md, "bench/", b'5', 0o755, &[])?;
    append(&mut md, "benchtmp/", b'5', 0o755, &[])?;
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
            let libs = link_args(&src);
            let libs: Vec<&str> = libs.iter().map(String::as_str).collect();
            cc_static_with(arch, &src, &libs, &bin)?;
        }
        let data = fs::read(&bin)?;
        append(&mut md, &format!("{TESTS_DIR}/{name}"), b'0', 0o555, &data)?;
    }
    // FreeBSD's fsx, built from the FreeBSD tree beside this one (it is
    // under the APSL and is not kept here), for the kernel tests.
    let fsx_src = util::root().join("../freebsd-src/tools/regression/fsx/fsx.c");
    if fsx_src.exists() {
        let bin = out_dir.join("fsx");
        let stale = match (
            fs::metadata(&fsx_src)?.modified(),
            fs::metadata(&bin).and_then(|m| m.modified()),
        ) {
            (Ok(s), Ok(b)) => s > b || built_by.is_none_or(|x| x > b),
            _ => true,
        };
        if stale {
            cc_static_foreign(arch, &fsx_src, &bin)?;
        }
        append(&mut md, "usr/tests/fsx/", b'5', 0o755, &[])?;
        append(&mut md, "usr/tests/fsx/fsx", b'0', 0o555, &fs::read(&bin)?)?;
    }
    // FreeBSD's `kbdcontrol` and `vidcontrol`, built static from the
    // FreeBSD tree beside this one until dynamic linking (P6) lets base's
    // own run, for `tests/user/vtools.c`.
    let console_tools = console_tools(arch, &out_dir, built_by)?;
    if !console_tools.is_empty() {
        append(&mut md, "usr/sbin/", b'5', 0o755, &[])?;
    }
    for (name, bin) in console_tools {
        append(
            &mut md,
            &format!("usr/sbin/{name}"),
            b'0',
            0o555,
            &fs::read(&bin)?,
        )?;
    }
    // FreeBSD's view of the FAT test disk, likewise.
    if crate::fattest::file(crate::fattest::FAT_TEST[1]).is_some() {
        append(&mut md, "usr/tests/fat/", b'5', 0o755, &[])?;
    }
    for name in &crate::fattest::FAT_TEST[1..] {
        if let Some(file) = crate::fattest::file(name) {
            append(
                &mut md,
                &format!("usr/tests/fat/{name}"),
                b'0',
                0o444,
                &fs::read(file)?,
            )?;
        }
    }
    // FreeBSD's view of the UFS2 test disk, for the kernel test (not
    // beside the test programs, which are all run).
    if ufs_test_file(UFS_TEST[1]).is_some() {
        append(&mut md, "usr/tests/ufs/", b'5', 0o755, &[])?;
    }
    for name in &UFS_TEST[1..] {
        if let Some(file) = ufs_test_file(name) {
            append(
                &mut md,
                &format!("usr/tests/ufs/{name}"),
                b'0',
                0o444,
                &fs::read(file)?,
            )?;
        }
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
