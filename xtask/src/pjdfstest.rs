// SPDX-License-Identifier: BSD-2-Clause
//! `cargo xtask pjdfstest`: FreeBSD's file-system conformance suite on
//! AbyssBSD's tmpfs and UFS2, compared with FreeBSD's own results
//! (ROADMAP P5.9).
//!
//! The VM boots the UFS2 root (all of `base.txz`), makes it read-write,
//! and runs every pjdfstest script on `/tmp` (tmpfs) and in `/var/tmp`
//! (UFS2 with soft updates) at once, through `tests/pjdfstest/run.sh`.
//! Each script's line (its plan, how many passed, which failed) must
//! match FreeBSD's in `tests/pjdfstest/freebsd-<fs>.txt`, which
//! `--baseline` records on the FreeBSD host itself with the same program
//! and scripts, or be one `tests/pjdfstest/known-differences.txt` lists
//! with its reason (a known difference that stops happening fails too, so
//! the list stays true). Afterwards the root must be left clean and pass
//! `fsck_ffs`.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use crate::arch::Arch;
use crate::build;
use crate::cli::Args;
use crate::image;
use crate::qemu::{Vm, Vmm};
use crate::util::{self, Result};

const USAGE: &str = "usage: cargo xtask pjdfstest [--arch A] [--vmm qemu|bhyve] [--release] \
                     [--groups chmod,open,...] [--debug group/NN.t|command] [--baseline]";

/// The file systems the suite runs on, and the directory each run uses.
const RUNS: &[(&str, &str)] = &[("tmpfs", "/tmp/pjd"), ("ufs", "/var/tmp/pjd")];

/// How long one script may take: the slowest sleeps a second at a time
/// for its timestamp checks, minutes on FreeBSD. A run that reports
/// nothing for this long has hung.
const SCRIPT_TIMEOUT: Duration = Duration::from_secs(1800);

pub(crate) fn cmd(mut args: Args) -> Result<()> {
    let arch = Arch::one_from_args(&mut args)?;
    let vmm = match args.take_value("--vmm")?.as_deref() {
        None if cfg!(target_os = "freebsd") && arch == Arch::X86_64 => Vmm::Bhyve,
        None | Some("qemu") => Vmm::Qemu,
        Some("bhyve") => Vmm::Bhyve,
        Some(v) => return Err(format!("unknown --vmm `{v}`; {USAGE}").into()),
    };
    let release = args.take_flag("--release");
    let baseline = args.take_flag("--baseline");
    let debug = args.take_value("--debug")?;
    let groups: Vec<String> = args
        .take_value("--groups")?
        .map(|g| g.split(',').map(str::to_string).collect())
        .unwrap_or_default();
    args.finish()?;
    if !cfg!(target_os = "freebsd") {
        return Err("pjdfstest needs a FreeBSD host (makefs, and FreeBSD's results)".into());
    }
    if baseline {
        return record_baseline(arch);
    }
    match run(arch, vmm, release, &groups, debug.as_deref())? {
        Ok(()) => {
            println!("pjdfstest: PASS");
            Ok(())
        }
        Err(why) => Err(format!("pjdfstest: FAIL: {why}").into()),
    }
}

/// A run's results: each script's line, by script.
type Results = BTreeMap<String, String>;

/// The `pjd: ` lines of `text`, by script.
fn parse(text: &str) -> Results {
    text.lines()
        .filter_map(|l| l.trim_end_matches('\r').strip_prefix("pjd: "))
        .filter(|l| *l != "done")
        .filter_map(|l| {
            let (script, rest) = l.split_once(' ')?;
            Some((script.to_string(), rest.to_string()))
        })
        .collect()
}

/// Where FreeBSD's results for file system `fs` are kept.
fn baseline_file(fs_name: &str) -> PathBuf {
    util::root()
        .join("tests/pjdfstest")
        .join(format!("freebsd-{fs_name}.txt"))
}

/// Runs the suite on this FreeBSD host, as root (through `doas`), on a
/// new tmpfs and a new UFS2 memory disk with soft updates, and writes the
/// results where [`baseline_file`] says.
fn record_baseline(arch: Arch) -> Result<()> {
    if arch != Arch::X86_64 {
        return Err("the baseline runs on this host, x86_64".into());
    }
    let suite = crate::base::pjdfstest_suite(arch)?;
    let work = util::root().join("build/pjdfstest-baseline");
    let _ = fs::create_dir_all(&work);
    let script = format!(
        "set -e\n\
         w={w}\n\
         mkdir -p $w/tmpfs $w/ufs\n\
         mount -t tmpfs tmpfs $w/tmpfs\n\
         md=$(mdconfig -a -t swap -s 2g)\n\
         trap 'umount -f $w/tmpfs; umount -f $w/ufs; mdconfig -d -u $md' EXIT\n\
         newfs -U /dev/$md >/dev/null\n\
         mount /dev/$md $w/ufs\n\
         mkdir $w/tmpfs/pjd $w/ufs/pjd\n\
         sh {s}/run.sh {s} $w/tmpfs/pjd > $w/tmpfs.txt 2>&1 &\n\
         sh {s}/run.sh {s} $w/ufs/pjd > $w/ufs.txt 2>&1\n\
         wait\n",
        w = work.display(),
        s = suite.display(),
    );
    util::run(Command::new("doas").args(["sh", "-c", &script]))?;
    for (fs_name, _) in RUNS {
        let text = fs::read_to_string(work.join(format!("{fs_name}.txt")))?;
        let results = parse(&text);
        let mut out = String::new();
        for (script, line) in &results {
            out.push_str(&format!("{script} {line}\n"));
        }
        fs::write(baseline_file(fs_name), out)?;
        println!(
            "pjdfstest: FreeBSD's {fs_name} results ({} scripts) in {}",
            results.len(),
            baseline_file(fs_name).display()
        );
    }
    Ok(())
}

/// FreeBSD's results for `fs_name`.
fn load_baseline(fs_name: &str) -> Result<Results> {
    let path = baseline_file(fs_name);
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(text
        .lines()
        .filter_map(|l| {
            let (script, rest) = l.split_once(' ')?;
            Some((script.to_string(), rest.to_string()))
        })
        .collect())
}

/// Boots the UFS2 root, runs the suite (only `groups`, if any), and
/// compares; or with `debug`, runs that one script under `sh -x` on each
/// file system, its trace on the console.
fn run(
    arch: Arch,
    vmm: Vmm,
    release: bool,
    groups: &[String],
    debug: Option<&str>,
) -> Result<std::result::Result<(), String>> {
    let opts = build::Options {
        release,
        ktest: false,
    };
    let disk = image::build_ufs_root(arch, opts, "")?;
    let disk = {
        let copy = disk.with_file_name("disk-pjdfstest.img");
        fs::copy(&disk, &copy)?;
        copy
    };
    let log = disk.with_file_name("pjdfstest.log");
    let vm = Vm {
        smp: 4,
        nvme: true,
        ..Vm::new(vmm, arch, disk.clone())
    };
    let mut session = crate::expect::Session::start(vm, &log)?;
    let suite = format!("/{}", image::PJDFSTEST);
    let result = (|| -> Result<()> {
        let quick = Duration::from_secs(60);
        session.expect("# ", Duration::from_secs(600))?;
        session.send(&format!(
            "PATH=/{}:/rescue; export PATH; mount -u -o rw / && echo rw-$?\n",
            image::STATIC_TOOLS
        ))?;
        session.expect("\nrw-0\r\n", quick)?;
        if let Some(script) = debug {
            for (_, dir) in RUNS {
                // A command of its own, or a script to trace.
                let run = if script.contains(' ') {
                    script.to_string()
                } else {
                    format!("sh -x {suite}/{script}")
                };
                session.send(&format!(
                    "mkdir -p {dir} && cd {dir} && {run}; echo pjd-debug-done; cd /\n"
                ))?;
                session.expect("\npjd-debug-done\r\n", SCRIPT_TIMEOUT)?;
            }
            return session.send("exit\n");
        }
        // Both runs at once, each script's line on the console as well as
        // in its file.
        let mut cmd = String::new();
        let only = groups.join(" ");
        for (fs_name, dir) in RUNS {
            cmd.push_str(&format!(
                "mkdir -p {dir} && (PJD_LABEL={fs_name} sh {suite}/run.sh {suite} {dir} {only} > {dir}.{fs_name}.txt) & "
            ));
        }
        cmd.push_str("wait; echo pjd-$?-finished\n");
        session.send(&cmd)?;
        // Done when both say so; hung when nothing has been said for a
        // script's time.
        let mut done = 0;
        while done < RUNS.len() {
            // Whole lines: output can arrive cut anywhere.
            if session.expect_any(&["pjd: done\r\n", "\r\n"], SCRIPT_TIMEOUT)? == 0 {
                done += 1;
            }
        }
        // (The line before it ended the loop above, newline and all.)
        session.expect("pjd-0-finished\r\n", SCRIPT_TIMEOUT)?;
        for (fs_name, dir) in RUNS {
            session.send(&format!(
                "echo pjd-results-{fs_name}; cat {dir}.{fs_name}.txt; echo pjd-end-{fs_name}\n"
            ))?;
            session.expect(&format!("\npjd-end-{fs_name}\r\n"), quick)?;
        }
        session.send("exit\n")
    })();
    let exited = session.finish(Duration::from_secs(300))?;
    let console = fs::read_to_string(&log).unwrap_or_default();
    let verdict = match result {
        Err(e) => Err(e.to_string()),
        Ok(()) if !exited => Err("the machine did not power off after `exit`".into()),
        Ok(()) if debug.is_some() => {
            println!("pjdfstest: trace in {}", log.display());
            Ok(())
        }
        Ok(()) => compare(&console, groups).and_then(|()| crate::test::root_left_clean(&disk)),
    };
    if verdict.is_err() {
        println!("pjdfstest: serial log at {}", log.display());
    }
    Ok(verdict)
}

/// The section of `console` between `pjd-results-<fs>` and
/// `pjd-end-<fs>`, as printed (not as echoed).
fn section<'a>(console: &'a str, fs_name: &str) -> &'a str {
    let start = format!("\npjd-results-{fs_name}\r\n");
    let end = format!("\npjd-end-{fs_name}\r\n");
    console
        .rfind(&start)
        .map(|at| &console[at + start.len()..])
        .and_then(|rest| rest.find(&end).map(|e| &rest[..e]))
        .unwrap_or("")
}

/// Known differences from FreeBSD: (file system, script) to AbyssBSD's
/// line and why. Each line of the file is `<fs> <script> | <line> | <why>`.
fn known_differences() -> Result<BTreeMap<(String, String), (String, String)>> {
    let path = util::root().join("tests/pjdfstest/known-differences.txt");
    let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut known = BTreeMap::new();
    for l in text
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
    {
        let mut parts = l.splitn(3, " | ");
        let (Some(at), Some(line), Some(why)) = (parts.next(), parts.next(), parts.next()) else {
            return Err(format!("{}: bad line {l:?}", path.display()).into());
        };
        let Some((fs_name, script)) = at.split_once(' ') else {
            return Err(format!("{}: bad line {l:?}", path.display()).into());
        };
        known.insert(
            (fs_name.to_string(), script.to_string()),
            (line.to_string(), why.to_string()),
        );
    }
    Ok(known)
}

/// Every script's line (in `groups`, if any) must be FreeBSD's, or a known
/// difference.
fn compare(console: &str, groups: &[String]) -> std::result::Result<(), String> {
    let known = known_differences().map_err(|e| e.to_string())?;
    let mut diffs = Vec::new();
    for (fs_name, _) in RUNS {
        let got = parse(section(console, fs_name));
        let mut want = match load_baseline(fs_name) {
            Ok(w) => w,
            Err(e) => {
                diffs.push(format!("{fs_name}: {e}"));
                continue;
            }
        };
        if !groups.is_empty() {
            want.retain(|script, _| {
                groups
                    .iter()
                    .any(|g| script.split('/').next() == Some(g.as_str()))
            });
        }
        if got.is_empty() {
            diffs.push(format!("{fs_name}: no results"));
            continue;
        }
        for (script, line) in &want {
            let k = known.get(&((*fs_name).to_string(), script.clone()));
            match (got.get(script), k) {
                (Some(l), None) if l == line => {}
                (Some(l), Some((kl, why))) if l == kl => {
                    println!("pjdfstest: known: {fs_name} {script}: {why}");
                }
                (Some(l), Some(_)) if l == line => diffs.push(format!(
                    "{fs_name} {script}: now as FreeBSD; remove its known difference"
                )),
                (Some(l), _) => diffs.push(format!("{fs_name} {script}: {l} (FreeBSD: {line})")),
                (None, _) => diffs.push(format!("{fs_name} {script}: missing (FreeBSD: {line})")),
            }
        }
        for script in got.keys().filter(|s| !want.contains_key(*s)) {
            diffs.push(format!("{fs_name} {script}: not in FreeBSD's results"));
        }
        let passed: usize = got.values().filter_map(|l| passed_of(l)).sum();
        let total: usize = want.values().filter_map(|l| passed_of(l)).sum();
        println!(
            "pjdfstest {fs_name}: {} scripts; {passed} tests passed (FreeBSD: {total})",
            got.len()
        );
    }
    for d in &diffs {
        println!("pjdfstest: {d}");
    }
    if diffs.is_empty() {
        Ok(())
    } else {
        Err(format!("{} scripts differ from FreeBSD", diffs.len()))
    }
}

/// How many of a script's tests passed: its line's second word.
fn passed_of(line: &str) -> Option<usize> {
    line.split_ascii_whitespace().nth(1)?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_results_and_sections() {
        let console = "x\r\n# echo pjd-results-ufs; cat f; echo pjd-end-ufs\r\n\
                       pjd-results-ufs\r\n\
                       pjd: chmod/00.t 1..5 5\r\n\
                       pjd: open/01.t 1..3 1 not ok: 2 3\r\n\
                       pjd: done\r\n\
                       pjd-end-ufs\r\n# ";
        let got = parse(section(console, "ufs"));
        assert_eq!(got.len(), 2);
        assert_eq!(got["chmod/00.t"], "1..5 5");
        assert_eq!(got["open/01.t"], "1..3 1 not ok: 2 3");
        assert_eq!(passed_of(&got["open/01.t"]), Some(1));
        assert_eq!(section(console, "tmpfs"), "");
    }

    #[test]
    fn known_differences_parse() {
        let known = known_differences().unwrap();
        for ((fs_name, script), (line, why)) in &known {
            assert!(RUNS.iter().any(|(f, _)| f == fs_name), "{fs_name}");
            assert!(
                script.ends_with(".t") && line.starts_with("1.."),
                "{script}"
            );
            assert!(!why.is_empty());
        }
    }
}
