// SPDX-License-Identifier: BSD-2-Clause
//! kyua's suite in bhyve, per test case (docs/MULTIUSER.md §14, ADR-0016's
//! kyua target). `cargo xtask --remote kyua-baseline` records FreeBSD
//! 15.1's own results; `cargo xtask --remote kyua` runs the same suite on
//! AbyssBSD and holds its results to those. Not under QEMU: emulated, the
//! suite takes days, and its timing-sensitive tests measure the emulator.
//! arm64's runs are on the Orange Pi 6 Plus.
//!
//! FreeBSD's release VM image carries `kyua` and `/usr/tests`; AbyssBSD's
//! is the installed root with all of `tests.txz`
//! ([`crate::image::build_kyua_root`]). On either, an `rc.local` runs the
//! suite one directory at a time (`bin/cat`, `sys/kern`, ...) and keeps
//! each report on the guest's disk. A test that panics the guest ends
//! that VM; the harness boots it again, and the script records the
//! directory it was in as crashed and goes on. When the script is done
//! the guest powers off, and the reports are read off its disk into one
//! line per test case:
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
use crate::util::Result;

/// The machine, as the results files are named.
const MACHINE: &str = "x86_64-bhyve";

/// The guest's vCPUs, and kyua's parallelism: the same in every run, on
/// both kernels.
const CPUS: u32 = 8;

/// Where the guest keeps its results.
const GUEST_DIR: &str = "var/db/kyua-run";

/// What the guest's script says on the console before each directory, and
/// when it is done.
const SAY: &str = "kyua-run: ";
const DONE: &str = "kyua-run: done";

/// FreeBSD's results, and the cases whose results may differ on AbyssBSD,
/// with why (in the tree).
pub(crate) const BASELINE: &str = "tests/kyua/freebsd/freebsd-15.1-RELEASE-x86_64-bhyve.txt";
pub(crate) const KNOWN_FAILURES: &str = "tests/kyua/known-failures.txt";

/// The areas ADR-0016's target is set for, as directories of
/// `/usr/tests`, and the share of FreeBSD's passing cases that must pass.
const AREAS: &[&str] = &["sys/kern", "lib/libc", "bin", "usr.bin"];
const TARGET: f64 = 0.95;

/// Test programs left out of the target until the phase that brings what
/// they test (ADR-0016): their cases are counted on their own line, and
/// fail nothing.
const DEFERRED: &[(&str, &str)] = &[("sys/kern/ktls_test", "P7, kernel TLS")];

/// The guest's script. Each directory's results are kept as
/// `<dir with / as _>.report`; a `.started` mark left over from an earlier
/// boot means that directory's run took the guest down. `only.txt`, if
/// there, limits the run to the directories under the ones it lists; a
/// smaller part it names (`lib/libc/sys`, `sys/kern/ptrace_test`,
/// `...:case`) runs on its own.
/// kyua's progress goes to the console too, so the harness can tell a
/// busy guest from a hung one.
fn rc_local() -> String {
    format!(
        r#"#!/bin/sh
# cargo xtask kyua: run kyua's suite one directory at a time.
# The packages' programs (python3, jq, ...) are in /usr/local, which rc's
# PATH leaves out.
PATH=$PATH:/usr/local/sbin:/usr/local/bin
export PATH
out=/{GUEST_DIR}
mkdir -p $out
cd /usr/tests || exit 1
say() {{ echo "{SAY}$*" > /dev/console; }}
if [ ! -f $out/chunks.txt ]; then
	say "listing"
	kyua list -k /usr/tests/Kyuafile > $out/list.txt 2> $out/list.err
	cut -d: -f1 $out/list.txt | cut -d/ -f1-2 | sort -u > $out/chunks.all
	if [ -f $out/only.txt ]; then
		while read p; do
			# A directory or more of whole chunks, else a part of
			# one (a directory, program, or case) on its own.
			if grep -qE "^$p(/|\$)" $out/chunks.all; then
				grep -E "^$p(/|\$)" $out/chunks.all
			elif grep -qE "^$p(:|/|\$)" $out/list.txt; then
				echo "$p"
			fi
		done < $out/only.txt | sort -u > $out/chunks.tmp
	else
		cp $out/chunks.all $out/chunks.tmp
	fi
	mv $out/chunks.tmp $out/chunks.txt
	date +%s > $out/start
	sync
fi
n=$(wc -l < $out/chunks.txt | tr -d ' ')
i=0
# Free memory and the test programs running, every 30 s: what a hung or
# starved guest was doing.
(say "watching"
while :; do
	sleep 30
	say "free $(sysctl -n vm.stats.vm.v_free_count) pages"
	say "running" $(ps -axo command= | grep -o '^/usr/tests/[^ ]*' | sort -u)
done) < /dev/null > /dev/null 2>&1 &
while read c; do
	i=$((i + 1))
	f=$(echo "$c" | tr / _)
	[ -f "$out/$f.report" ] && continue
	if [ -f "$out/$f.started" ]; then
		say "crashed in $c"
		echo "$c" >> $out/crashed.txt
		# The cases that ended before the crash, as kyua printed them.
		grep -a ' ->  ' "$out/$f.out" > "$out/$f.report"
		rm "$out/$f.started"
		sync
		continue
	fi
	say "$i/$n $c"
	: > "$out/$f.started"
	sync
	kyua --logfile=/tmp/kyua.log -v parallelism={CPUS} test -k /usr/tests/Kyuafile \
	    -r "$out/$f.db" "$c" < /dev/null 2>&1 | tee "$out/$f.out" > /dev/console
	kyua report -r "$out/$f.db" \
	    --results-filter=passed,skipped,xfail,broken,failed \
	    > "$out/$f.tmp" 2>&1 < /dev/null
	kyua report -r "$out/$f.db" --verbose --results-filter=broken,failed \
	    > "$out/$f.failed" 2>&1 < /dev/null
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

/// The loader settings beyond FreeBSD's multi-user image's: a panic
/// reboots at once, without entering the debugger, so the harness can
/// carry on (AbyssBSD's kernel halts, and the harness sees the panic);
/// and FreeBSD's CI's three FIBs ([`FIBS`]).
const LOADER_CONF: &str = "debug.debugger_on_panic=\"0\"\n\
                           kern.panic_reboot_wait_time=\"0\"\n\
                           net.fibs=\"3\"\n";

/// The test environment of FreeBSD's own CI (`tests/ci/tools/ci.conf` in
/// its tree), on both systems: routing tables 1 and 2 for the tests (the
/// kernel's `net.fibs`, from the loader on FreeBSD and the command line on
/// AbyssBSD), tests that change sysctls allowed, and the `ci` property,
/// with which tests known to be unreliable there skip themselves.
const FIBS: &str = "net.fibs=3";
const KYUA_CONF: &str = "test_suites.FreeBSD.ci = 'true'\n\
                         test_suites.FreeBSD.fibs = '1 2'\n\
                         test_suites.FreeBSD.allow_sysctl_side_effects = '1'\n";

/// Of the modules FreeBSD's CI loads for the tests (its `kld_list`), those
/// of P7's scope: `epair`, which the network tests build their topologies
/// from, and which kyua's `required_kmods` wants loaded before a test.
const RC_CONF: &str = "kld_list=\"if_epair\"\n";

/// The packages of FreeBSD's CI that tests in P7's directories need
/// (Python with `pytest`, `scapy`, and `dpkt`; `jq`; `ndisc6`), installed
/// into both systems' roots. Fetched once with their dependencies and kept
/// ([`packages`]), so every run, on either system, has the same builds.
const PACKAGES: &[&str] = &[
    "python",
    "python3",
    "py312-pytest",
    "py312-scapy",
    "py312-dpkt",
    "jq",
    "ndisc6",
];

/// What AbyssBSD's kernel prints when it panics (Rust's panic message
/// follows `panic: `) or takes a fatal trap. Matched without the newline
/// before them, which the harness's line-by-line reading may have taken.
/// Two CPUs panicking at once interleave their reports, so the marker
/// that ends a report counts too.
const ABYSS_PANIC: &str = "panic: panicked at ";
const ABYSS_TRAP: &str = "fatal trap:";
const ABYSS_FAIL: &str = "ABYSS-TEST-RESULT: FAIL";

/// How many times the guest may go down before the run is given up.
const MAX_BOOTS: u32 = 20;

/// The system a run is of.
#[derive(Clone, Copy, PartialEq, Eq)]
enum System {
    FreeBsd,
    Abyss,
}

impl System {
    fn name(self) -> String {
        match self {
            Self::FreeBsd => format!("FreeBSD {RELEASE}"),
            Self::Abyss => format!(
                "AbyssBSD {}",
                std::env::var("ABYSS_REV").unwrap_or_else(|_| "unknown".into())
            ),
        }
    }
}

/// Runs the rest of the command under perf's and CI's lock, as they do:
/// eight busy vCPUs would skew a benchmark run. Returns `None` once
/// locked, `Some` with the locked run's outcome otherwise.
fn locked() -> Result<Option<()>> {
    if std::env::var_os("ABYSS_PERF_LOCKED").is_some() {
        return Ok(None);
    }
    let lock = home().join("abyss/ci/.lock");
    eprintln!("xtask: kyua: waiting for {}", lock.display());
    crate::util::run(
        std::process::Command::new("lockf")
            .arg("-k")
            .arg(lock)
            .arg(std::env::current_exe()?)
            .args(std::env::args().skip(1))
            .env("ABYSS_PERF_LOCKED", "1"),
    )?;
    Ok(Some(()))
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from)
}

/// `cargo xtask --remote kyua-baseline [--resume] [--only DIR,...]`.
/// `--resume` carries on with the last run's disk: it boots it again if
/// the suite is not done, then collects. Results go to
/// `~/abyss/kyua/freebsd-<release>-x86_64-bhyve.txt` on the server
/// (outside the mirrored tree, which a sync would delete), to be copied
/// into `tests/kyua/freebsd/`. `--only`, as for `kyua`, runs part of the
/// suite, into `...-bhyve-only.txt`: for checking the harness, not a
/// baseline.
pub(crate) fn cmd_kyua_baseline(mut args: Args) -> Result<()> {
    let resume = args.take_flag("--resume");
    let only: Vec<String> = args
        .take_value("--only")?
        .map(|v| v.split(',').map(str::to_owned).collect())
        .unwrap_or_default();
    args.finish()?;
    if !cfg!(target_os = "freebsd") {
        return Err("kyua-baseline needs the build server (bhyve, mdconfig)".into());
    }
    if locked()?.is_some() {
        return Ok(());
    }
    let results = home().join("abyss/kyua");
    fs::create_dir_all(&results)?;
    let part = if only.is_empty() { "" } else { "-only" };
    let out = results.join(format!("freebsd-{RELEASE}-{MACHINE}{part}.txt"));
    let fresh = || -> Result<PathBuf> {
        let (image, _) = crate::perf::freebsd_multiuser_image(Arch::X86_64)?;
        Ok(image)
    };
    let work = results.join(MACHINE);
    run(System::FreeBsd, &work, &out, resume, &only, &fresh)?;
    println!("kyua-baseline: results in {}", out.display());
    Ok(())
}

/// `cargo xtask --remote kyua [--resume] [--only DIR,...]`: the suite on
/// AbyssBSD in bhyve, against FreeBSD's results ([`BASELINE`]). `--only`
/// runs the directories under those given (`bin`, `sys/kern`), or the
/// test programs or cases given (`sys/kern/ptrace_test`), instead of all. Results go to `~/abyss/kyua/abyss-x86_64-bhyve.txt` on the server,
/// the comparison to `abyss-x86_64-bhyve-compare.txt` beside it. Fails if
/// an area misses ADR-0016's target or a case FreeBSD passes fails there
/// without a line in [`KNOWN_FAILURES`].
pub(crate) fn cmd_kyua(mut args: Args) -> Result<()> {
    let resume = args.take_flag("--resume");
    let only: Vec<String> = args
        .take_value("--only")?
        .map(|v| v.split(',').map(str::to_owned).collect())
        .unwrap_or_default();
    args.finish()?;
    if !cfg!(target_os = "freebsd") {
        return Err("kyua needs the build server (bhyve, mdconfig)".into());
    }
    if locked()?.is_some() {
        return Ok(());
    }
    let results = home().join("abyss/kyua");
    fs::create_dir_all(&results)?;
    let out = results.join(format!("abyss-{MACHINE}.txt"));
    let fresh = || {
        let opts = crate::build::Options {
            release: false,
            ktest: false,
        };
        crate::image::build_kyua_root(
            Arch::X86_64,
            opts,
            &format!("boot_multicons=YES boot_serial=YES {FIBS}"),
        )
    };
    let work = results.join(format!("abyss-{MACHINE}"));
    run(System::Abyss, &work, &out, resume, &only, &fresh)?;
    println!("kyua: results in {}", out.display());
    let root = crate::util::root();
    let report = compare(
        &fs::read_to_string(root.join(BASELINE))?,
        &fs::read_to_string(&out)?,
        &fs::read_to_string(root.join(KNOWN_FAILURES))?,
    );
    let text = report.text();
    let compared = out.with_file_name(format!("abyss-{MACHINE}-compare.txt"));
    fs::write(&compared, &text)?;
    print!("{}", report.summary());
    println!("kyua: comparison in {}", compared.display());
    report.verdict()
}

fn run(
    system: System,
    work: &Path,
    out: &Path,
    resume: bool,
    only: &[String],
    fresh: &dyn Fn() -> Result<PathBuf>,
) -> Result<()> {
    fs::create_dir_all(work)?;
    let disk = work.join("disk.img");
    let mnt = work.join("mnt");
    let results = |root: &Path| root.join(GUEST_DIR);
    let mut done = false;
    let packages = packages()?;
    if resume && disk.exists() {
        crate::perf::mounted_root(&disk, &mnt, |root| {
            done = results(root).join("end").exists();
            Ok(())
        })?;
    } else {
        let image = fresh()?;
        let _ = fs::remove_file(&disk);
        // Console logs of an earlier run are not this one's.
        for e in fs::read_dir(work)? {
            let p = e?.path();
            if p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("console-"))
            {
                fs::remove_file(p)?;
            }
        }
        crate::util::run(std::process::Command::new("cp").arg(&image).arg(&disk))?;
        crate::perf::mounted_root(&disk, &mnt, |root| {
            let mut files = vec![("etc/rc.local", 0o755, rc_local().into_bytes())];
            if system == System::FreeBsd {
                let conf = root.join("boot/loader.conf.local");
                let loader = fs::read_to_string(&conf)? + LOADER_CONF;
                files.push(("boot/loader.conf.local", 0o644, loader.into_bytes()));
            }
            let conf = fs::read_to_string(root.join("etc/kyua/kyua.conf"))? + KYUA_CONF;
            files.push(("etc/kyua/kyua.conf", 0o644, conf.into_bytes()));
            let rc = fs::read_to_string(root.join("etc/rc.conf"))? + RC_CONF;
            files.push(("etc/rc.conf", 0o644, rc.into_bytes()));
            install_packages(root, &packages)?;
            let list = only.join("\n") + "\n";
            let only_txt = format!("{GUEST_DIR}/only.txt");
            if !only.is_empty() {
                crate::perf::doas(&["mkdir", "-p", &root.join(GUEST_DIR).to_string_lossy()])?;
                files.push((only_txt.as_str(), 0o644, list.into_bytes()));
            }
            crate::perf::install_files(root, &files)
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
        eprintln!("xtask: kyua: boot {boot}, console in {}", log.display());
        let vm = Vm {
            smp: CPUS,
            mem: "8G".into(),
            nvme: system == System::Abyss,
            ..Vm::new(Vmm::Bhyve, Arch::X86_64, disk.clone())
        };
        let mut session = Session::start(vm, &log)?;
        // FreeBSD's console says only when each directory starts, and
        // kyua's own timeouts bound each test, so a long quiet means the
        // guest hung; AbyssBSD's carries each case's result as well.
        let quiet = Duration::from_secs(6 * 3600);
        let mut panicked = false;
        loop {
            match session.expect_any(&[DONE, ABYSS_PANIC, ABYSS_TRAP, ABYSS_FAIL, "\n"], quiet) {
                Ok(0) => {
                    done = true;
                    break;
                }
                // AbyssBSD's kernel halts after a panic.
                Ok(1..=3) => {
                    panicked = true;
                    break;
                }
                Ok(_) => {}
                // The VM ended (FreeBSD's panic reboots it) or went quiet.
                Err(e) => {
                    eprintln!("xtask: kyua: boot {boot}: {e}");
                    break;
                }
            }
        }
        if panicked {
            eprintln!(
                "xtask: kyua: boot {boot}: the kernel panicked (in {})",
                log.display()
            );
        }
        // `shutdown -p` after done; otherwise the VM is gone or stuck (the
        // backtrace still to come after a panic).
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
        return Err(format!("kyua: not done after {MAX_BOOTS} boots").into());
    }

    let mut text = String::new();
    // What failed, with its output, per directory.
    let details = work.join("failed");
    let _ = fs::remove_dir_all(&details);
    fs::create_dir_all(&details)?;
    crate::perf::mounted_root(&disk, &mnt, |root| {
        text = collect(
            &results(root),
            &system.name(),
            started.elapsed(),
            &package_names(&packages)?,
        )?;
        for e in fs::read_dir(results(root))? {
            let p = e?.path();
            if p.extension().is_some_and(|x| x == "failed")
                && let Some(name) = p.file_name()
            {
                fs::copy(&p, details.join(name))?;
            }
        }
        Ok(())
    })?;
    fs::write(out, text)?;
    Ok(())
}

/// The directory of [`PACKAGES`] and their dependencies as
/// `<name>-<version>.pkg`, fetched from the server's repository the first
/// time and kept.
fn packages() -> Result<PathBuf> {
    let dir = home().join("abyss/kyua/packages");
    let mark = dir.join(".fetched");
    if fs::read_to_string(&mark).is_ok_and(|m| m == PACKAGES.join(" ")) {
        return Ok(dir);
    }
    let tmp = dir.with_extension("tmp");
    let _ = crate::perf::doas(&["rm", "-rf", &tmp.to_string_lossy(), &dir.to_string_lossy()]);
    fs::create_dir_all(&tmp)?;
    let mut fetch = vec!["pkg", "fetch", "-d", "-y", "-o"];
    let tmp_name = tmp.to_string_lossy().into_owned();
    fetch.push(&tmp_name);
    fetch.extend_from_slice(PACKAGES);
    crate::perf::doas(&fetch)?;
    fs::create_dir_all(&dir)?;
    // `pkg fetch` names them `<name>-<version>~<hash>.pkg`; `pkg add`
    // finds a package's dependencies beside it by name and version.
    let found = crate::util::output(
        std::process::Command::new("find")
            .arg(&tmp)
            .args(["-name", "*.pkg"]),
    )?;
    for path in found.lines() {
        let file = Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or("odd package name")?;
        let name = file.split_once('~').map_or(file, |(n, _)| n);
        let name = name.strip_suffix(".pkg").unwrap_or(name);
        fs::copy(path, dir.join(format!("{name}.pkg")))?;
    }
    let _ = crate::perf::doas(&["rm", "-rf", &tmp_name]);
    fs::write(&mark, PACKAGES.join(" "))?;
    Ok(dir)
}

/// The packages in `dir`, as `<name>-<version>`, sorted: for the results'
/// header.
fn package_names(dir: &Path) -> Result<String> {
    let mut names: Vec<String> = fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            e.file_name()
                .to_str()?
                .strip_suffix(".pkg")
                .map(str::to_owned)
        })
        .collect();
    names.sort();
    Ok(names.join(" "))
}

/// Installs [`PACKAGES`], with their dependencies, from `dir` into the
/// root mounted at `root`.
fn install_packages(root: &Path, dir: &Path) -> Result<()> {
    let files: Vec<PathBuf> = fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    for name in PACKAGES {
        let prefix = format!("{name}-");
        let file = files
            .iter()
            .find(|f| {
                f.file_name()
                    .and_then(|n| n.to_str())
                    .and_then(|n| n.strip_prefix(&prefix))
                    .is_some_and(|v| v.starts_with(|c: char| c.is_ascii_digit()))
            })
            .ok_or_else(|| format!("no {name} package in {}", dir.display()))?;
        crate::perf::doas(&[
            "pkg",
            "-r",
            &root.to_string_lossy(),
            "add",
            &file.to_string_lossy(),
        ])?;
    }
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

/// Whether `case` (`<program>:<case>`) is in the run of `chunk`: a
/// directory, a program, or the case itself.
fn in_chunk(case: &str, chunk: &str) -> bool {
    case.strip_prefix(chunk)
        .is_some_and(|r| r.is_empty() || r.starts_with(':') || r.starts_with('/'))
}

/// Reads the guest's reports in `dir` into the results file's text.
fn collect(dir: &Path, system: &str, wall: Duration, packages: &str) -> Result<String> {
    let read = |name: &str| fs::read_to_string(dir.join(name)).unwrap_or_default();
    let listed: Vec<String> = read("list.txt")
        .lines()
        .map(str::trim)
        .filter(|l| l.contains(':'))
        .map(str::to_owned)
        .collect();
    let crashed: Vec<String> = read("crashed.txt").lines().map(str::to_owned).collect();
    let chunks: Vec<String> = read("chunks.txt").lines().map(str::to_owned).collect();
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
        // Not in this run (`--only`).
        let Some(chunk) = chunks.iter().find(|c| in_chunk(case, c)) else {
            continue;
        };
        let result = if crashed.contains(chunk) {
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
        "# kyua on {system}, {MACHINE}, {CPUS} CPUs, parallelism={CPUS}\n\
         # written by cargo xtask kyua; {} test cases\n",
        cases.len()
    );
    let summary: Vec<String> = counts.iter().map(|(r, n)| format!("{r} {n}")).collect();
    let _ = writeln!(text, "# {}", summary.join(", "));
    let _ = writeln!(text, "# packages: {packages}");
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

/// A results file's cases: `<program>:<case>` to its result and reason.
fn parse_results(text: &str) -> BTreeMap<&str, (&str, &str)> {
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| {
            let mut f = l.split('\t');
            Some((f.next()?, (f.next()?, f.next().unwrap_or(""))))
        })
        .collect()
}

/// `known-failures.txt`'s cases, each with its reason (the comment).
pub(crate) fn parse_known(text: &str) -> BTreeMap<&str, &str> {
    text.lines()
        .filter_map(|l| {
            let (case, why) = l.split_once('#').unwrap_or((l, ""));
            let case = case.trim();
            (!case.is_empty()).then(|| (case, why.trim()))
        })
        .collect()
}

/// The area of ADR-0016's target a case is in, if any.
fn area(case: &str) -> Option<&'static str> {
    AREAS
        .iter()
        .copied()
        .find(|a| case.strip_prefix(a).is_some_and(|r| r.starts_with('/')))
}

/// One area's count: FreeBSD's passing cases, and how many of them pass
/// on AbyssBSD.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Tally {
    freebsd: usize,
    abyss: usize,
}

impl Tally {
    fn rate(self) -> f64 {
        if self.freebsd == 0 {
            1.0
        } else {
            self.abyss as f64 / self.freebsd as f64
        }
    }
}

/// AbyssBSD's results against FreeBSD's.
#[derive(Debug, Default)]
struct Comparison {
    /// Per area of the target, over the cases this run had.
    areas: BTreeMap<&'static str, Tally>,
    /// Everything else, the same way.
    rest: Tally,
    /// The [`DEFERRED`] programs' cases.
    deferred: Tally,
    /// Cases FreeBSD passes that do not pass here and are not listed, in
    /// an area of the target (these fail the run) or not.
    unlisted: Vec<String>,
    unlisted_elsewhere: Vec<String>,
    /// Listed cases that end as on FreeBSD now: the list can lose them.
    stale: Vec<String>,
    /// Cases whose result differs from FreeBSD's otherwise (FreeBSD's not
    /// a pass), for the record.
    other: Vec<String>,
    /// AbyssBSD's results, counted.
    counts: BTreeMap<String, usize>,
}

fn compare(freebsd: &str, abyss: &str, known: &str) -> Comparison {
    let freebsd = parse_results(freebsd);
    let abyss = parse_results(abyss);
    let known = parse_known(known);
    let mut c = Comparison::default();
    for a in AREAS {
        c.areas.insert(a, Tally::default());
    }
    for (case, &(result, reason)) in &abyss {
        *c.counts.entry((*result).to_owned()).or_default() += 1;
        let line = |want: &str| {
            if reason.is_empty() {
                format!("{case}: {result} (FreeBSD: {want})")
            } else {
                format!("{case}: {result}: {reason} (FreeBSD: {want})")
            }
        };
        let Some(&(want, _)) = freebsd.get(case) else {
            c.other.push(line("no such case"));
            continue;
        };
        if DEFERRED.iter().any(|(p, _)| in_chunk(case, p)) {
            if want == "passed" {
                c.deferred.freebsd += 1;
                c.deferred.abyss += usize::from(result == "passed");
            }
            continue;
        }
        let listed = known.contains_key(case);
        if want == result {
            if listed {
                c.stale.push((*case).to_owned());
            }
        } else if want != "passed" {
            c.other.push(line(want));
        } else if !listed {
            match area(case) {
                Some(_) => c.unlisted.push(line(want)),
                None => c.unlisted_elsewhere.push(line(want)),
            }
        }
        if want == "passed" {
            let t = area(case).map_or(&mut c.rest, |a| c.areas.entry(a).or_default());
            t.freebsd += 1;
            t.abyss += usize::from(result == "passed");
        }
    }
    c
}

impl Comparison {
    fn summary(&self) -> String {
        let mut s = String::new();
        let counts: Vec<String> = self
            .counts
            .iter()
            .map(|(r, n)| format!("{r} {n}"))
            .collect();
        let _ = writeln!(s, "kyua: {}", counts.join(", "));
        let mut row = |name: &str, t: Tally, gated: bool| {
            let mark = if !gated {
                ""
            } else if t.rate() >= TARGET {
                "  ok"
            } else {
                "  below the target"
            };
            let _ = writeln!(
                s,
                "kyua: {name:<10} {:>5} of {:>5} of FreeBSD's passes ({:.1}%){mark}",
                t.abyss,
                t.freebsd,
                t.rate() * 100.0
            );
        };
        for (a, t) in &self.areas {
            row(a, *t, true);
        }
        row("the rest", self.rest, false);
        row("deferred", self.deferred, false);
        for (p, until) in DEFERRED {
            let _ = writeln!(s, "kyua:   {p}, until {until}");
        }
        let _ = writeln!(
            s,
            "kyua: {} unlisted failures in the target's areas, {} elsewhere; {} listed cases now end as on FreeBSD",
            self.unlisted.len(),
            self.unlisted_elsewhere.len(),
            self.stale.len()
        );
        s
    }

    fn text(&self) -> String {
        let mut s = self.summary();
        let mut list = |title: &str, v: &[String]| {
            if !v.is_empty() {
                let _ = writeln!(s, "\n# {title} ({})", v.len());
                for l in v {
                    let _ = writeln!(s, "{l}");
                }
            }
        };
        list(
            "FreeBSD passes, AbyssBSD does not, not in known-failures.txt (target areas)",
            &self.unlisted,
        );
        list(
            "FreeBSD passes, AbyssBSD does not, not in known-failures.txt (elsewhere)",
            &self.unlisted_elsewhere,
        );
        list("In known-failures.txt, but ends as on FreeBSD", &self.stale);
        list("Otherwise different from FreeBSD", &self.other);
        s
    }

    /// Fails if an area misses the target, or a case FreeBSD passes in
    /// one fails here unlisted.
    fn verdict(&self) -> Result<()> {
        let below: Vec<&str> = self
            .areas
            .iter()
            .filter(|(_, t)| t.rate() < TARGET)
            .map(|(a, _)| *a)
            .collect();
        if below.is_empty() && self.unlisted.is_empty() {
            return Ok(());
        }
        Err(format!(
            "kyua: below the target in [{}]; {} unlisted failures in its areas",
            below.join(", "),
            self.unlisted.len()
        )
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks() {
        assert!(in_chunk("sys/kern/ptrace_test:a", "sys/kern"));
        assert!(in_chunk("sys/kern/ptrace_test:a", "sys/kern/ptrace_test"));
        assert!(in_chunk("sys/kern/ptrace_test:a", "sys/kern/ptrace_test:a"));
        assert!(!in_chunk(
            "sys/kern/ptrace_test:ab",
            "sys/kern/ptrace_test:a"
        ));
        assert!(!in_chunk("sys/kernel/x:a", "sys/kern"));
    }

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

    #[test]
    fn comparison() {
        let freebsd = "# header\n\
                       bin/cat/t:a\tpassed\n\
                       bin/cat/t:b\tpassed\n\
                       bin/cat/t:c\tpassed\n\
                       bin/cat/t:d\tskipped\tno\n\
                       sys/kern/x:y\tpassed\n\
                       sys/kern/ktls_test:a\tpassed\n\
                       sys/net/z:w\tpassed\n";
        let abyss = "# header\n\
                     bin/cat/t:a\tpassed\n\
                     bin/cat/t:b\tfailed\tbad\n\
                     bin/cat/t:c\tbroken\n\
                     bin/cat/t:d\tfailed\n\
                     sys/kern/x:y\tpassed\n\
                     sys/kern/ktls_test:a\tskipped\n\
                     sys/net/z:w\tskipped\n";
        let known = "# why\nbin/cat/t:c   # P7\nsys/kern/x:y # was broken\n";
        let c = compare(freebsd, abyss, known);
        assert_eq!(
            c.areas["bin"],
            Tally {
                freebsd: 3,
                abyss: 1
            }
        );
        assert_eq!(
            c.areas["sys/kern"],
            Tally {
                freebsd: 1,
                abyss: 1
            }
        );
        assert_eq!(c.areas["lib/libc"], Tally::default());
        assert_eq!(
            c.rest,
            Tally {
                freebsd: 1,
                abyss: 0
            }
        );
        assert_eq!(
            c.deferred,
            Tally {
                freebsd: 1,
                abyss: 0
            }
        );
        assert_eq!(c.unlisted, ["bin/cat/t:b: failed: bad (FreeBSD: passed)"]);
        assert_eq!(c.unlisted_elsewhere.len(), 1);
        assert_eq!(c.stale, ["sys/kern/x:y"]);
        assert_eq!(c.other, ["bin/cat/t:d: failed (FreeBSD: skipped)"]);
        assert!(c.verdict().is_err());
        assert_eq!(area("bin/cat/t:a"), Some("bin"));
        assert_eq!(area("binx/t:a"), None);
        assert_eq!(area("lib/libcasper/t:a"), None);
    }
}
