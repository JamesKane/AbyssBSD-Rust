// SPDX-License-Identifier: BSD-2-Clause
//! `cargo xtask --remote kyua-baseline`: FreeBSD 15.1's own results for
//! its kyua suite, per test case, in bhyve, where AbyssBSD's runs are
//! compared (docs/MULTIUSER.md §14, ADR-0016's kyua target). Not under
//! QEMU: emulated, the suite takes days, and its timing-sensitive tests
//! measure the emulator. arm64's baseline is taken on the Orange Pi 6 Plus.
//!
//! The release VM image carries `kyua` and `/usr/tests`. A copy of the
//! multi-user image ([`crate::perf::freebsd_multiuser_image`]) gets an
//! `rc.local` that runs the suite one directory at a time (`bin/cat`,
//! `sys/kern`, ...) and keeps each report on the guest's disk. A test that
//! panics the guest ends that VM; the harness boots it again, and the
//! script records the directory it was in as crashed and goes on. When
//! the script is done the guest powers off, and the reports are read off
//! its disk into one line per test case:
//!
//! ```text
//! <program>:<case>\t<result>[\t<reason>]
//! ```
//!
//! `result` is kyua's (`passed`, `skipped`, `expected_failure`, `broken`,
//! `failed`), or `crashed` for a case in a directory whose run panicked.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::arch::Arch;
use crate::base::RELEASE;
use crate::cli::Args;
use crate::expect::Session;
use crate::qemu::{Vm, Vmm};

/// The machine, as the results file is named.
const MACHINE: &str = "x86_64-bhyve";
use crate::util::Result;

/// The guest's vCPUs, and kyua's parallelism: the same in every run, on
/// both kernels.
const CPUS: u32 = 8;

/// The guest's script. Each directory's results are kept as
/// `<dir with / as _>.report`; a `.started` mark left over from an earlier
/// boot means that directory's run took the guest down.
fn rc_local() -> String {
    format!(
        r#"#!/bin/sh
# cargo xtask kyua-baseline: run kyua's suite one directory at a time.
out=/var/db/kyua-baseline
mkdir -p $out
cd /usr/tests || exit 1
say() {{ echo "kyua-baseline: $*" > /dev/console; }}
if [ ! -f $out/chunks.txt ]; then
	say "listing"
	kyua list -k /usr/tests/Kyuafile > $out/list.txt 2> $out/list.err
	cut -d: -f1 $out/list.txt | cut -d/ -f1-2 | sort -u > $out/chunks.tmp
	mv $out/chunks.tmp $out/chunks.txt
	date +%s > $out/start
	sync
fi
n=$(wc -l < $out/chunks.txt | tr -d ' ')
i=0
while read c; do
	i=$((i + 1))
	f=$(echo "$c" | tr / _)
	[ -f "$out/$f.report" ] && continue
	if [ -f "$out/$f.started" ]; then
		say "crashed in $c"
		echo "$c" >> $out/crashed.txt
		: > "$out/$f.report"
		rm "$out/$f.started"
		sync
		continue
	fi
	say "$i/$n $c"
	: > "$out/$f.started"
	sync
	kyua --logfile=/tmp/kyua.log -v parallelism={CPUS} test -k /usr/tests/Kyuafile \
	    -r "$out/$f.db" "$c" < /dev/null > "$out/$f.out" 2>&1
	kyua report -r "$out/$f.db" \
	    --results-filter=passed,skipped,xfail,broken,failed \
	    > "$out/$f.tmp" 2>&1 < /dev/null
	mv "$out/$f.tmp" "$out/$f.report"
	rm "$out/$f.started"
	sync
done < $out/chunks.txt
date +%s > $out/end
sync
say "done"
shutdown -p now
"#
    )
}

/// The loader settings beyond the multi-user image's: a panic reboots at
/// once, without entering the debugger, so the harness can carry on.
const LOADER_CONF: &str = "debug.debugger_on_panic=\"0\"\n\
                           kern.panic_reboot_wait_time=\"0\"\n";

/// How many times the guest may go down before the run is given up.
const MAX_BOOTS: u32 = 20;

/// `cargo xtask --remote kyua-baseline [--resume]`. `--resume` carries on
/// with the last run's disk: it boots it again if the suite is not done,
/// then collects. Results go to
/// `~/abyss/kyua/freebsd-<release>-x86_64-bhyve.txt` on the server
/// (outside the mirrored tree, which a sync would delete), to be copied
/// into `tests/kyua/freebsd/`.
pub(crate) fn cmd_kyua_baseline(mut args: Args) -> Result<()> {
    let resume = args.take_flag("--resume");
    args.finish()?;
    if !cfg!(target_os = "freebsd") {
        return Err("kyua-baseline needs the build server (bhyve, mdconfig)".into());
    }
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    // Eight busy vCPUs would skew a benchmark run: take perf's
    // and CI's lock, as they do.
    if std::env::var_os("ABYSS_PERF_LOCKED").is_none() {
        let lock = home.join("abyss/ci/.lock");
        eprintln!("xtask: kyua-baseline: waiting for {}", lock.display());
        return crate::util::run(
            std::process::Command::new("lockf")
                .arg("-k")
                .arg(lock)
                .arg(std::env::current_exe()?)
                .args(std::env::args().skip(1))
                .env("ABYSS_PERF_LOCKED", "1"),
        );
    }
    let results = home.join("abyss/kyua");
    fs::create_dir_all(&results)?;
    let out = results.join(format!("freebsd-{RELEASE}-{MACHINE}.txt"));
    run(&results.join(MACHINE), &out, resume)?;
    println!("kyua-baseline: results in {}", out.display());
    Ok(())
}

fn run(work: &Path, out: &Path, resume: bool) -> Result<()> {
    fs::create_dir_all(work)?;
    let disk = work.join("disk.img");
    let mnt = work.join("mnt");
    let results = |root: &Path| root.join("var/db/kyua-baseline");
    let mut done = false;
    if resume && disk.exists() {
        crate::perf::mounted_root(&disk, &mnt, |root| {
            done = results(root).join("end").exists();
            Ok(())
        })?;
    } else {
        let (image, _) = crate::perf::freebsd_multiuser_image(Arch::X86_64)?;
        let _ = fs::remove_file(&disk);
        crate::util::run(std::process::Command::new("cp").arg(&image).arg(&disk))?;
        crate::perf::mounted_root(&disk, &mnt, |root| {
            let conf = root.join("boot/loader.conf.local");
            let loader = fs::read_to_string(&conf)? + LOADER_CONF;
            crate::perf::install_files(
                root,
                &[
                    ("etc/rc.local", 0o755, rc_local().into_bytes()),
                    ("boot/loader.conf.local", 0o644, loader.into_bytes()),
                ],
            )
        })?;
    }

    let started = Instant::now();
    // Console logs from earlier boots are kept.
    let first = (1..)
        .find(|b| !work.join(format!("console-{b}.log")).exists())
        .unwrap_or(1);
    for boot in first..first + MAX_BOOTS {
        if done {
            break;
        }
        let log = work.join(format!("console-{boot}.log"));
        eprintln!(
            "xtask: kyua-baseline: boot {boot}, console in {}",
            log.display()
        );
        let vm = Vm {
            smp: CPUS,
            mem: "8G".into(),
            ..Vm::new(Vmm::Bhyve, Arch::X86_64, disk.clone())
        };
        let mut session = Session::start(vm, &log)?;
        // Each directory reports when it starts. kyua's own timeouts bound
        // each test, so this long without a new directory means the guest
        // hung.
        let quiet = Duration::from_secs(6 * 3600);
        loop {
            match session.expect_any(&["kyua-baseline: done", "kyua-baseline: "], quiet) {
                Ok(0) => {
                    done = true;
                    break;
                }
                Ok(_) => {}
                // The VM ended (a panic reboots it) or went quiet.
                Err(e) => {
                    eprintln!("xtask: kyua-baseline: boot {boot}: {e}");
                    break;
                }
            }
        }
        // `shutdown -p` after done; otherwise the VM is gone or stuck.
        let wait = if done { 600 } else { 5 };
        let _ = session.finish(Duration::from_secs(wait));
        // The console can split "done" from its prefix across reads, so the
        // guest's own mark decides.
        crate::perf::mounted_root(&disk, &mnt, |root| {
            done = results(root).join("end").exists();
            Ok(())
        })?;
    }
    if !done {
        return Err(format!("kyua-baseline: not done after {MAX_BOOTS} boots").into());
    }

    let mut text = String::new();
    crate::perf::mounted_root(&disk, &mnt, |root| {
        text = collect(&results(root), MACHINE, started.elapsed())?;
        Ok(())
    })?;
    fs::write(out, text)?;
    Ok(())
}

/// One test case's outcome.
#[derive(Debug, PartialEq, Eq)]
struct Outcome {
    result: String,
    reason: String,
}

/// Parses a `kyua report` line, `<program>:<case>  ->  <result>[: <reason>]
/// [<time>]`, into the test case and its outcome.
fn parse_report_line(line: &str) -> Option<(String, Outcome)> {
    let (case, rest) = line.split_once("  ->  ")?;
    if case.contains(char::is_whitespace) || !case.contains(':') {
        return None;
    }
    // The time is last, in brackets.
    let rest = rest.trim_end();
    let rest = match rest.rfind("  [") {
        Some(at) if rest.ends_with("s]") => &rest[..at],
        _ => rest,
    };
    let (result, reason) = rest.split_once(": ").unwrap_or((rest, ""));
    if result.is_empty() || result.contains(char::is_whitespace) {
        return None;
    }
    Some((
        case.to_owned(),
        Outcome {
            result: result.to_owned(),
            reason: reason.replace(['\t', '\n'], " "),
        },
    ))
}

/// Reads the guest's reports in `dir` into the results file's text.
fn collect(dir: &Path, machine: &str, wall: Duration) -> Result<String> {
    let read = |name: &str| fs::read_to_string(dir.join(name)).unwrap_or_default();
    let listed: Vec<String> = read("list.txt")
        .lines()
        .map(str::trim)
        .filter(|l| l.contains(':'))
        .map(str::to_owned)
        .collect();
    let crashed: Vec<String> = read("crashed.txt").lines().map(str::to_owned).collect();
    let mut cases: BTreeMap<String, Outcome> = BTreeMap::new();
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "report") {
            for line in fs::read_to_string(&path)?.lines() {
                if let Some((case, outcome)) = parse_report_line(line) {
                    cases.insert(case, outcome);
                }
            }
        }
    }
    for case in &listed {
        let chunk = case.split(':').next().unwrap_or("");
        let chunk: String = chunk.split('/').take(2).collect::<Vec<_>>().join("/");
        let result = if crashed.contains(&chunk) {
            "crashed"
        } else {
            "missing"
        };
        cases.entry(case.clone()).or_insert_with(|| Outcome {
            result: result.into(),
            reason: String::new(),
        });
    }
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for o in cases.values() {
        *counts.entry(o.result.as_str()).or_default() += 1;
    }
    let guest_wall = read("end")
        .trim()
        .parse::<u64>()
        .ok()
        .zip(read("start").trim().parse::<u64>().ok())
        .map(|(e, s)| e.saturating_sub(s));
    let mut text = format!(
        "# kyua on FreeBSD {RELEASE}, {machine}, {CPUS} CPUs, parallelism={CPUS}\n\
         # written by cargo xtask kyua-baseline; {} test cases\n",
        cases.len()
    );
    let summary: Vec<String> = counts.iter().map(|(r, n)| format!("{r} {n}")).collect();
    let _ = writeln!(text, "# {}", summary.join(", "));
    let _ = writeln!(
        text,
        "# wall time: suite {} s, harness {} s",
        guest_wall.map_or_else(|| "?".into(), |s| s.to_string()),
        wall.as_secs()
    );
    if !crashed.is_empty() {
        let _ = writeln!(text, "# crashed in: {}", crashed.join(" "));
    }
    for (case, o) in &cases {
        if o.reason.is_empty() {
            let _ = writeln!(text, "{case}\t{}", o.result);
        } else {
            let _ = writeln!(text, "{case}\t{}\t{}", o.result, o.reason);
        }
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_lines() {
        let (case, o) = parse_report_line("bin/cat/cat_test:align  ->  passed  [0.008s]").unwrap();
        assert_eq!(case, "bin/cat/cat_test:align");
        assert_eq!(o.result, "passed");
        assert_eq!(o.reason, "");
        let (_, o) = parse_report_line(
            "sys/net/if_bridge_test:span  ->  skipped: Required kmod if_bridge not loaded  [0.011s]",
        )
        .unwrap();
        assert_eq!(o.result, "skipped");
        assert_eq!(o.reason, "Required kmod if_bridge not loaded");
        let (_, o) = parse_report_line("lib/libc/gen/x:y  ->  failed: a: b  [1.5s]").unwrap();
        assert_eq!((o.result.as_str(), o.reason.as_str()), ("failed", "a: b"));
        assert!(parse_report_line("===> Summary").is_none());
        assert!(parse_report_line("Test cases: 3 total, 0 skipped").is_none());
    }
}
