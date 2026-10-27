// SPDX-License-Identifier: BSD-2-Clause
//! `cargo xtask --remote freebsd-usertests`: `tests/user` against FreeBSD
//! 15.1 itself, on either architecture.
//!
//! The test programs check FreeBSD's behavior, so each must pass on
//! FreeBSD. On the build server's amd64 they run as they are; this runs
//! them on FreeBSD's own release image, arm64 included (in QEMU, with the
//! `max` CPU, so SVE too), as the boot tests run them under AbyssBSD:
//! built static into `/usr/tests/abyss`, and dynamic into
//! `/usr/tests/abyss-dynamic`. The script logs in as root on the serial
//! console and runs each in turn, then powers off.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use crate::arch::Arch;
use crate::cli::Args;
use crate::expect::{PROMPT, Session};
use crate::qemu::{Vm, Vmm};
use crate::util::Result;

/// Where the programs go in the image.
const STATIC_DIR: &str = "usr/tests/abyss";
const DYNAMIC_DIR: &str = "usr/tests/abyss-dynamic";

/// `cargo xtask --remote freebsd-usertests [--arch A] [--test NAME,...]`.
pub(crate) fn cmd(mut args: Args) -> Result<()> {
    let arches = Arch::from_args(&mut args)?;
    let only: Option<Vec<String>> = args
        .take_value("--test")?
        .map(|v| v.split(',').map(String::from).collect());
    args.finish()?;
    if !cfg!(target_os = "freebsd") {
        return Err("freebsd-usertests needs the build server (mdconfig, the full base)".into());
    }
    for arch in arches {
        run(arch, only.as_deref())?;
    }
    Ok(())
}

fn run(arch: Arch, only: Option<&[String]>) -> Result<()> {
    let (image, work) = crate::perf::freebsd_multiuser_image(arch)?;
    let dir = work.join(format!("usertests-{arch}"));
    fs::create_dir_all(&dir)?;
    let wanted = |name: &str| only.is_none_or(|o| o.iter().any(|n| n == name));
    // The programs, static and dynamic.
    let static_out = dir.join("static");
    fs::create_dir_all(&static_out)?;
    let mut programs: Vec<(String, PathBuf)> = Vec::new();
    let mut sources: Vec<PathBuf> = fs::read_dir(crate::util::root().join("tests/user"))?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<_>>()?;
    sources.retain(|p| p.extension().is_some_and(|x| x == "c"));
    sources.sort();
    for src in &sources {
        let name = src
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or("bad test name")?
            .to_string();
        if !wanted(&name) {
            continue;
        }
        let bin = static_out.join(&name);
        let libs = crate::base::link_args(src);
        let libs: Vec<&str> = libs.iter().map(String::as_str).collect();
        crate::base::cc_static_with(arch, src, &libs, &bin)?;
        programs.push((format!("{STATIC_DIR}/{name}"), bin));
    }
    for (name, bin) in crate::base::dynamic_user_tests(arch, &dir.join("dynamic"))? {
        if wanted(&name) {
            programs.push((format!("{DYNAMIC_DIR}/{name}"), bin));
        }
    }
    if programs.is_empty() {
        return Err("no test program selected".into());
    }
    let disk = dir.join("disk.img");
    crate::multiuser::copy_sparse(&image, &disk)?;
    crate::perf::mounted_root(&disk, &dir.join("mnt"), |mnt| {
        for d in [STATIC_DIR, DYNAMIC_DIR] {
            crate::perf::doas(&["mkdir", "-p", &mnt.join(d).to_string_lossy()])?;
        }
        let files: Vec<(&str, u32, Vec<u8>)> = programs
            .iter()
            .map(|(path, bin)| Ok((path.as_str(), 0o555, fs::read(bin)?)))
            .collect::<Result<_>>()?;
        crate::perf::install_files(mnt, &files)
    })?;
    let log = dir.join("console.log");
    let vmm = if arch == Arch::X86_64 && Vmm::Bhyve.available(arch) {
        Vmm::Bhyve
    } else {
        Vmm::Qemu
    };
    let vm = Vm {
        smp: 4,
        mem: "2G".into(),
        ..Vm::new(vmm, arch, disk)
    };
    let mut session = Session::start(vm, &log)?;
    let count = programs.len();
    let result = (|| -> Result<()> {
        // FreeBSD under QEMU's emulation boots slowly.
        session
            .expect("login: ", Duration::from_secs(1800))
            .map_err(|e| format!("no login prompt: {e}"))?;
        session.send("root\n")?;
        session.expect(crate::multiuser::ROOT_PROMPT, Duration::from_secs(120))?;
        session.send("PS1='@''sh> '\n")?;
        session.expect(PROMPT, Duration::from_secs(30))?;
        // Each in /tmp, its output kept, and a line per failure (a glob:
        // a terminal line holds 255 bytes).
        session.send(&format!(
            "cd /tmp; for t in /{STATIC_DIR}/* /{DYNAMIC_DIR}/*; do $t > /tmp/out 2>&1 || {{ echo \"FAILED $t:\"; cat /tmp/out; }}; done; echo done-$((6*7))\n"
        ))?;
        session.expect("\ndone-42\r\n", Duration::from_secs(3600))?;
        session.expect(PROMPT, Duration::from_secs(30))?;
        session.send("shutdown -p now\n")
    })();
    let exited = session.finish(Duration::from_secs(300));
    result.map_err(|e| {
        format!(
            "freebsd-usertests ({arch}): {e} (console: {})",
            log.display()
        )
    })?;
    let console = fs::read_to_string(&log).unwrap_or_default();
    let failed: Vec<&str> = console
        .lines()
        .filter(|l| l.starts_with("FAILED "))
        .collect();
    if !exited? {
        eprintln!("freebsd-usertests ({arch}): FreeBSD did not power off");
    }
    if !failed.is_empty() {
        return Err(format!(
            "freebsd-usertests ({arch}): {} of {} failed on FreeBSD (console: {}):\n{}",
            failed.len(),
            count,
            log.display(),
            failed.join("\n")
        )
        .into());
    }
    println!(
        "freebsd-usertests ({arch}): {} programs passed on FreeBSD; console in {}",
        count,
        log.display()
    );
    Ok(())
}
