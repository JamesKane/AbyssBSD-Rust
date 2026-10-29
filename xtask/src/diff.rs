// SPDX-License-Identifier: BSD-2-Clause
//! Differential tests of command output (docs/BUILD-AND-TEST.md §4):
//! the same scenario on AbyssBSD and on FreeBSD 15.1, its output
//! normalized the same way, compared line for line.
//!
//! A scenario is a shell script, `tests/diff/scenarios/NAME.sh`, that sets
//! up a known situation (processes asleep in known places, open
//! descriptors, mappings) and runs the programs under test through
//! `tests/diff/lib.sh`'s `show`. `tests/diff/run.sh` runs each in a
//! directory and on a terminal of its own and prints its output twice: as
//! printed (`diff-raw: | `), and through `tests/diff/normalize.awk`
//! (`diff: | `), which masks what legitimately differs between machines
//! (pids, times, sizes, addresses) and nothing else. The normalizer runs
//! inside each guest, with FreeBSD's own `awk`. Helper programs
//! (`tests/diff/helpers/*.c`) are built static once and run on both.
//!
//! `cargo xtask --remote freebsd-diff` runs the scenarios on FreeBSD 15.1
//! (its release image, as the installer leaves it, from `rc.local`) and
//! records their normalized outputs in `tests/corpus/freebsd/diff/ARCH/`.
//! The shell cases run them on AbyssBSD from the boot shell ([`DIFF`]),
//! and [`verdict`] compares: a scenario whose output differs fails the
//! case with the differing lines, unless `tests/diff/known-differences.txt`
//! lists it, with why. Adding a scenario is adding its script and
//! recording FreeBSD's output. `ABYSS_DIFF` (names, blank- or
//! comma-separated) runs and checks only those scenarios.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::arch::Arch;
use crate::cli::Args;
use crate::expect::{Step, step};
use crate::qemu::{Outcome, Vm, Vmm};
use crate::util::{self, Result};

/// Where the scenarios live in the tree, and in a guest's root.
const SOURCE: &str = "tests/diff";
pub(crate) const GUEST_DIR: &str = "usr/tests/diff";
/// FreeBSD's recorded outputs, per architecture.
const CORPUS: &str = "tests/corpus/freebsd/diff";
/// Scenarios whose outputs may differ yet, with why.
const KNOWN: &str = "tests/diff/known-differences.txt";

/// The shell cases' step: every scenario (or `ABYSS_DIFF`'s), from the
/// boot shell.
pub(crate) fn steps() -> Vec<Step> {
    let send = match only() {
        Some(names) => format!("/bin/sh /usr/tests/diff/run.sh {}\n", names.join(" ")),
        None => "/bin/sh /usr/tests/diff/run.sh\n".into(),
    };
    // A step's text lives as long as the run.
    let send: &'static str = Box::leak(send.into_boxed_str());
    vec![step(send, "\ndiff: done\r\n", "the differential scenarios")]
}

/// The scenarios `ABYSS_DIFF` names, if it names any.
pub(crate) fn only() -> Option<Vec<String>> {
    let v = std::env::var("ABYSS_DIFF").ok()?;
    let names: Vec<String> = v
        .split(|c: char| c == ',' || c.is_ascii_whitespace())
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
        .collect();
    (!names.is_empty()).then_some(names)
}

/// The scenarios' names, sorted (only `ABYSS_DIFF`'s, if it names any).
fn scenarios() -> Result<Vec<String>> {
    if let Some(names) = only() {
        return Ok(names);
    }
    all_scenarios()
}

/// Every scenario's name, sorted.
fn all_scenarios() -> Result<Vec<String>> {
    let mut names: Vec<String> = fs::read_dir(util::root().join(SOURCE).join("scenarios"))?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            e.file_name()
                .to_str()
                .and_then(|n| n.strip_suffix(".sh"))
                .map(str::to_owned)
        })
        .collect();
    names.sort();
    Ok(names)
}

/// Copies the harness into `stage` (a root being built, at
/// [`GUEST_DIR`]), with its helpers built static for `arch` in `bin/`.
pub(crate) fn stage(arch: Arch, stage: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let src = util::root().join(SOURCE);
    let dst = stage.join(GUEST_DIR);
    fs::create_dir_all(dst.join("scenarios"))?;
    fs::create_dir_all(dst.join("bin"))?;
    for f in ["lib.sh", "normalize.awk", "run.sh"] {
        fs::copy(src.join(f), dst.join(f))?;
    }
    fs::set_permissions(dst.join("run.sh"), fs::Permissions::from_mode(0o555))?;
    for name in all_scenarios()? {
        let f = format!("{name}.sh");
        fs::copy(
            src.join("scenarios").join(&f),
            dst.join("scenarios").join(&f),
        )?;
    }
    let out = util::root()
        .join("build")
        .join(arch.to_string())
        .join("diff-helpers");
    fs::create_dir_all(&out)?;
    for e in fs::read_dir(src.join("helpers"))? {
        let path = e?.path();
        if path.extension().is_none_or(|x| x != "c") {
            continue;
        }
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or("bad helper name")?;
        let bin = out.join(name);
        let libs = crate::base::link_args(&path);
        let libs: Vec<&str> = libs.iter().map(String::as_str).collect();
        crate::base::cc_static_with(arch, &path, &libs, &bin)?;
        fs::copy(&bin, dst.join("bin").join(name))?;
    }
    Ok(())
}

/// Each scenario's normalized output in a console log, by name.
fn outputs(console: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut cur: Option<(String, String)> = None;
    for line in console.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(name) = line.split("diff: begin ").nth(1) {
            cur = Some((name.trim().to_owned(), String::new()));
        } else if line.contains("diff: end ") {
            if let Some(done) = cur.take() {
                out.push(done);
            }
        } else if let Some((_, text)) = cur.as_mut() {
            if let Some(rest) = line.split("diff: | ").nth(1) {
                text.push_str(rest);
                text.push('\n');
            } else if line.ends_with("diff: |") {
                text.push('\n');
            }
        }
    }
    out
}

/// The scenarios `tests/diff/known-differences.txt` lists for `arch`:
/// `NAME` or `ARCH:NAME`, then a `#` and why.
fn known(arch: Arch) -> Result<Vec<String>> {
    let text = fs::read_to_string(util::root().join(KNOWN)).unwrap_or_default();
    Ok(text
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .filter_map(|l| match l.split_once(':') {
            Some((a, n)) if a == arch.to_string() => Some(n.to_owned()),
            Some(_) => None,
            None => Some(l.to_owned()),
        })
        .collect())
}

/// The lines of `got` and `want` that differ, as a short report.
fn report(want: &str, got: &str) -> String {
    let (w, g): (Vec<&str>, Vec<&str>) = (want.lines().collect(), got.lines().collect());
    let mut out = Vec::new();
    for i in 0..w.len().max(g.len()) {
        let (a, b) = (w.get(i), g.get(i));
        if a != b {
            out.push(format!(
                "    line {}:\n      FreeBSD:  {}\n      AbyssBSD: {}",
                i + 1,
                a.unwrap_or(&"(none)"),
                b.unwrap_or(&"(none)")
            ));
            if out.len() == 12 {
                out.push("    ...".into());
                break;
            }
        }
    }
    out.join("\n")
}

/// Compares the scenarios' outputs in `console` with FreeBSD's for
/// `arch`: every scenario must have run and match, but those
/// `known-differences.txt` lists. Returns how many matched.
pub(crate) fn verdict(arch: Arch, console: &str) -> Result<usize> {
    let dir = util::root().join(CORPUS).join(arch.to_string());
    let known = known(arch)?;
    let got = outputs(console);
    let mut matched = 0;
    let mut wrong = Vec::new();
    for name in scenarios()? {
        let Ok(want) = fs::read_to_string(dir.join(format!("{name}.txt"))) else {
            println!("diff: {name}: no FreeBSD recording for {arch}; skipped");
            continue;
        };
        match got.iter().find(|(n, _)| *n == name) {
            None => wrong.push(format!("  {name}: did not run")),
            Some((_, text)) if *text == want => matched += 1,
            Some((_, text)) if known.contains(&name) => {
                println!(
                    "diff: {name}: differs from FreeBSD, as known-differences.txt says\n{}",
                    report(&want, text)
                );
            }
            Some((_, text)) => wrong.push(format!("  {name}:\n{}", report(&want, text))),
        }
    }
    if !wrong.is_empty() {
        return Err(format!("output differs from FreeBSD's:\n{}", wrong.join("\n")).into());
    }
    Ok(matched)
}

/// `cargo xtask --remote freebsd-diff [--arch A] [--scenario N,...]`:
/// boots FreeBSD 15.1's release image (bhyve on amd64, QEMU on arm64)
/// with the scenarios in its root and an `rc.local` that runs them, and
/// writes each one's normalized output to `tests/corpus/freebsd/diff/`
/// in the mirror on the server (copy them back from there).
pub(crate) fn cmd_freebsd_diff(mut args: Args) -> Result<()> {
    let arches = Arch::from_args(&mut args)?;
    let only = args.take_value("--scenario")?;
    args.finish()?;
    if !cfg!(target_os = "freebsd") {
        return Err("freebsd-diff needs the build server (bhyve, mdconfig)".into());
    }
    let wanted: Vec<String> = match &only {
        Some(list) => list.split(',').map(str::to_owned).collect(),
        None => scenarios()?,
    };
    for arch in arches {
        let (image, work) = crate::perf::freebsd_multiuser_image(arch)?;
        let dir = work.join(format!("diff-{arch}"));
        fs::create_dir_all(&dir)?;
        let disk = dir.join("disk.img");
        crate::multiuser::copy_sparse(&image, &disk)?;
        let staging = dir.join("stage");
        let _ = fs::remove_dir_all(&staging);
        stage(arch, &staging)?;
        crate::perf::mounted_root(&disk, &dir.join("mnt"), |mnt| {
            install_tree(&staging, mnt)?;
            let rc = format!(
                "#!/bin/sh\n# AbyssBSD cargo xtask freebsd-diff\n\
                 sh /{GUEST_DIR}/run.sh {} > /dev/console 2>&1\npoweroff\n",
                wanted.join(" ")
            );
            crate::perf::install_files(mnt, &[("etc/rc.local", 0o755, rc.into_bytes())])
        })?;
        let vmm = if arch == Arch::X86_64 {
            Vmm::Bhyve
        } else {
            Vmm::Qemu
        };
        let vm = Vm {
            smp: 4,
            mem: "2G".into(),
            ..Vm::new(vmm, arch, disk)
        };
        let log = dir.join("console.log");
        let done = |line: &str| line.contains("diff: done").then_some(Outcome::Passed);
        let run = vm.run_until(Duration::from_secs(3600), &log, false, &done)?;
        if run.outcome != Outcome::Passed {
            return Err(format!(
                "freebsd-diff ({arch}): incomplete ({:?}); see {}",
                run.outcome,
                log.display()
            )
            .into());
        }
        let console = fs::read_to_string(&log)?;
        let out: PathBuf = util::root().join(CORPUS).join(arch.to_string());
        fs::create_dir_all(&out)?;
        for (name, text) in outputs(&console) {
            let f = out.join(format!("{name}.txt"));
            fs::write(&f, text)?;
            println!("freebsd-diff ({arch}): {}", f.display());
        }
    }
    Ok(())
}

/// Copies the staged tree into the mounted root `mnt`, owned by root.
fn install_tree(staging: &Path, mnt: &Path) -> Result<()> {
    crate::perf::doas(&[
        "cp",
        "-R",
        &staging.join("usr").to_string_lossy(),
        &mnt.to_string_lossy(),
    ])?;
    crate::perf::doas(&[
        "chown",
        "-R",
        "root:wheel",
        &mnt.join(GUEST_DIR).to_string_lossy(),
    ])?;
    Ok(())
}
