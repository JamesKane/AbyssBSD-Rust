// SPDX-License-Identifier: BSD-2-Clause
//! `cargo xtask perf`: the performance comparison against FreeBSD
//! (docs/PERFORMANCE.md).
//!
//! Runs on the FreeBSD build server (`cargo xtask --remote perf`). Each
//! run boots one kernel in a pinned bhyve VM and collects the in-kernel
//! benchmark output (tests/perf/README.md): AbyssBSD with
//! `abyss.bench=<suite>`, and stock FreeBSD 15.1 with the `kbench` module,
//! loaded from a second disk by `/etc/rc.local`. Runs alternate between
//! the two kernels. The report applies PERFORMANCE.md's rules and is
//! written, with the raw samples and console logs, to
//! `~/abyss/ci/perf/<time>-<rev>/`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use crate::arch::Arch;
use crate::build;
use crate::cli::Args;
use crate::image;
use crate::qemu::{Outcome, Vm, Vmm};
use crate::util::{self, Result};

const RELEASE: &str = "15.1-RELEASE";
const VM_IMAGE: &str = "FreeBSD-15.1-RELEASE-amd64-ufs.raw.xz";
/// Bumped whenever [`prepare_image`] changes what it writes.
const PREP_VERSION: &str = "14";
/// The bound: AbyssBSD's median over FreeBSD's.
const BOUND: f64 = 1.02;
/// FreeBSD-side coefficient of variation above which a benchmark cannot
/// resolve the bound.
const MAX_CV: f64 = 0.01;
const BOOTSTRAP: usize = 2000;

const USAGE: &str = "usage: cargo xtask --remote perf [--suite p2|p3|diag] [--runs N] [--prepare]";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Side {
    FreeBsd,
    Abyss,
}

impl Side {
    fn name(self) -> &'static str {
        match self {
            Self::FreeBsd => "freebsd",
            Self::Abyss => "abyss",
        }
    }
}

pub(crate) fn cmd(mut args: Args) -> Result<()> {
    let suite = args.take_value("--suite")?.unwrap_or_else(|| "p2".into());
    let runs: usize = args
        .take_value("--runs")?
        .map_or(Ok(10), |s| s.parse())
        .map_err(|_| "bad --runs")?;
    let prepare_only = args.take_flag("--prepare");
    args.finish()?;
    // `diag` holds AbyssBSD-side diagnostics, some with FreeBSD
    // counterparts; it is for investigation, not gating.
    if !["p2", "p3", "diag"].contains(&suite.as_str()) {
        return Err(format!("unknown suite `{suite}` (p2, p3, diag)").into());
    }
    if !cfg!(target_os = "freebsd") {
        return Err(format!("perf needs bhyve: run it on the build server\n{USAGE}").into());
    }
    // A perf run must have the server to itself: share CI's lock.
    if std::env::var_os("ABYSS_PERF_LOCKED").is_none() {
        let lock = home().join("abyss/ci/.lock");
        eprintln!("xtask: perf: waiting for {}", lock.display());
        return util::run(
            Command::new("lockf")
                .arg("-k")
                .arg(lock)
                .arg(std::env::current_exe()?)
                .args(std::env::args().skip(1))
                .env("ABYSS_PERF_LOCKED", "1"),
        );
    }

    let dirs = Dirs::new();
    fs::create_dir_all(&dirs.perf)?;
    let freebsd = prepare_image(&dirs)?;
    let kbench = build_kbench(&dirs)?;
    if prepare_only {
        eprintln!(
            "xtask: perf: prepared {} and {}",
            freebsd.display(),
            kbench.display()
        );
        return Ok(());
    }

    let rev = std::env::var("ABYSS_REV").unwrap_or_else(|_| "unknown".into());
    let stamp = util::output(Command::new("date").arg("+%Y%m%d-%H%M%S"))?;
    let out = home()
        .join("abyss/ci/perf")
        .join(format!("{}-{}", stamp.trim(), rev));
    fs::create_dir_all(out.join("logs"))?;

    let bench_disk = make_bench_disk(&out, &kbench, &suite)?;
    let opts = build::Options {
        release: true,
        ktest: false,
    };
    let abyss = image::build(Arch::X86_64, opts, &format!("abyss.bench={suite}"))?;

    let mut samples: BTreeMap<String, BTreeMap<Side, Vec<f64>>> = BTreeMap::new();
    let mut raw = String::from("side\trun\tname\tvalue\tunit\n");
    let mut units = BTreeMap::new();
    for run in 0..runs {
        // Alternate which kernel goes first, so drift hits both equally.
        let order = if run % 2 == 0 {
            [Side::FreeBsd, Side::Abyss]
        } else {
            [Side::Abyss, Side::FreeBsd]
        };
        for side in order {
            let disk = match side {
                Side::FreeBsd => &freebsd,
                Side::Abyss => &abyss,
            };
            let log = out
                .join("logs")
                .join(format!("{}-{run:02}.log", side.name()));
            eprintln!("xtask: perf: run {}/{runs}: {}", run + 1, side.name());
            // One retry: a VM run can fail for reasons outside the kernels
            // (a console glitch, a host hiccup); the first log is kept.
            let results = match run_one(disk, &bench_disk, &log) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("xtask: perf: {e}; retrying once");
                    run_one(disk, &bench_disk, &log.with_extension("retry.log"))?
                }
            };
            for (name, value, unit) in results {
                let _ = writeln!(raw, "{}\t{run}\t{name}\t{value}\t{unit}", side.name());
                units.insert(name.clone(), unit);
                samples
                    .entry(name)
                    .or_default()
                    .entry(side)
                    .or_default()
                    .push(value);
            }
        }
    }
    fs::write(out.join("samples.tsv"), raw)?;
    fs::write(out.join("environment.txt"), environment(&rev))?;

    let report = Report::new(&samples, &units);
    let text = report.render(&suite, &rev, runs);
    fs::write(out.join("report.txt"), &text)?;
    print!("{text}");
    eprintln!("xtask: perf: results in {}", out.display());
    if report.failures() > 0 {
        return Err(format!("{} benchmark(s) over the bound", report.failures()).into());
    }
    Ok(())
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from)
}

/// Where perf keeps its inputs.
struct Dirs {
    perf: PathBuf,
    dist: PathBuf,
}

impl Dirs {
    fn new() -> Self {
        let perf = home().join("abyss/perf");
        Self {
            dist: perf.join("dist"),
            perf,
        }
    }
}

/// `doas <cmd> <args>`, failing on a non-zero exit.
fn doas(args: &[&str]) -> Result<String> {
    util::output(Command::new("doas").args(args))
}

/// Checks `file` against the SHA-256 recorded for `name` in `sums`, a
/// `CHECKSUM.SHA256` file (`SHA256 (name) = hex`) or a release `MANIFEST`
/// (`name<TAB>hex<TAB>…`).
fn verify(file: &Path, name: &str, sums: &Path) -> Result<()> {
    let text = fs::read_to_string(sums).map_err(|e| format!("{}: {e}", sums.display()))?;
    let want = text
        .lines()
        .find_map(|l| {
            l.strip_prefix(&format!("SHA256 ({name}) = "))
                .or_else(|| {
                    let mut f = l.split('\t');
                    (f.next() == Some(name)).then(|| f.next()).flatten()
                })
                .map(str::trim)
        })
        .ok_or_else(|| format!("{name} not listed in {}", sums.display()))?;
    let got = util::output(Command::new("sha256").arg("-q").arg(file))?;
    if got.trim() != want {
        return Err(format!("{}: SHA-256 mismatch", file.display()).into());
    }
    Ok(())
}

/// The FreeBSD VM image, configured to run the benchmark at boot and power
/// off: serial console, no daemons that need not run, and an `rc.local`
/// that loads `kbench.ko` from the second disk.
fn prepare_image(dirs: &Dirs) -> Result<PathBuf> {
    let img = dirs.perf.join(format!("freebsd-{RELEASE}-kbench.img"));
    let stamp = img.with_extension("img.prepared");
    if img.exists() && fs::read_to_string(&stamp).is_ok_and(|s| s.trim() == PREP_VERSION) {
        return Ok(img);
    }
    let xz = dirs.dist.join(VM_IMAGE);
    eprintln!(
        "xtask: perf: preparing {} from {}",
        img.display(),
        xz.display()
    );
    verify(&xz, VM_IMAGE, &dirs.dist.join("CHECKSUM.SHA256"))?;
    let tmp = img.with_extension("img.tmp");
    let out = fs::File::create(&tmp)?;
    let status = Command::new("xz")
        .arg("-dc")
        .arg(&xz)
        .stdout(out)
        .status()?;
    if !status.success() {
        return Err("xz failed".into());
    }

    let md = doas(&[
        "mdconfig",
        "-a",
        "-t",
        "vnode",
        "-f",
        &tmp.to_string_lossy(),
    ])?;
    let md = md.trim().to_owned();
    let mnt = dirs.perf.join("mnt");
    fs::create_dir_all(&mnt)?;
    let result = (|| -> Result<()> {
        let part = rootfs_partition(&md)?;
        doas(&["mount", &format!("/dev/{part}"), &mnt.to_string_lossy()])?;
        let r = configure_guest(&mnt);
        doas(&["umount", &mnt.to_string_lossy()])?;
        r
    })();
    let _ = doas(&["mdconfig", "-d", "-u", &md]);
    result?;
    fs::rename(&tmp, &img)?;
    fs::write(&stamp, PREP_VERSION)?;
    Ok(img)
}

/// The image's UFS partition on memory disk `md`.
fn rootfs_partition(md: &str) -> Result<String> {
    let table = util::output(Command::new("gpart").args(["show", "-p", md]))?;
    table
        .lines()
        .find(|l| l.contains("freebsd-ufs"))
        .and_then(|l| l.split_whitespace().nth(2))
        .map(str::to_owned)
        .ok_or_else(|| format!("no freebsd-ufs partition on {md}").into())
}

/// Writes the guest configuration into the mounted root at `mnt`.
fn configure_guest(mnt: &Path) -> Result<()> {
    let files: [(&str, &str, &str); 3] = [
        (
            "boot/loader.conf.local",
            "644",
            "autoboot_delay=\"0\"\nbeastie_disable=\"YES\"\nconsole=\"comconsole\"\n",
        ),
        (
            "etc/rc.conf.local",
            "644",
            "growfs_enable=\"NO\"\nsendmail_enable=\"NONE\"\nsshd_enable=\"NO\"\n\
             ntpd_enable=\"NO\"\ncron_enable=\"NO\"\nsyslogd_enable=\"NO\"\n\
             dumpdev=\"NO\"\nupdate_motd=\"NO\"\n",
        ),
        (
            "etc/rc.local",
            "755",
            "#!/bin/sh\n# AbyssBSD cargo xtask perf / freebsd-record: run the second disk's\n\
             # script, or the kbench suite it names; then power off.\n\
             # kbench runs with vtbd1 unmounted: it reads and detaches it.\n\
             # Device interrupts go to CPU 0, where AbyssBSD puts them.\n\
             if mount -o ro /dev/vtbd1 /mnt; then\n\
             \tif [ -f /mnt/run.sh ]; then\n\
             \t\tsh /mnt/run.sh\n\
             \t\tumount /mnt\n\
             \telse\n\
             \t\tcp /mnt/kbench.ko /mnt/suite /tmp/\n\
             \t\tumount /mnt\n\
             \t\tfor irq in $(vmstat -ai | awk '/virtio_pci|nvme/ { sub(/^irq/, \"\", $1); sub(/:$/, \"\", $1); print $1 }'); do\n\
             \t\t\tcpuset -l 0 -x \"$irq\"\n\
             \t\tdone\n\
             \t\tvmstat -ai | grep -E 'virtio_pci|nvme'\n\
             \t\tkldload /tmp/kbench.ko && sysctl debug.kbench.run=\"$(cat /tmp/suite)\" >/dev/null\n\
             \tfi\n\
             fi\npoweroff\n",
        ),
    ];
    let staging = mnt.with_file_name("staging");
    fs::create_dir_all(&staging)?;
    for (path, mode, text) in files {
        let src = staging.join(path.replace('/', "_"));
        fs::write(&src, text)?;
        let dst = mnt.join(path);
        doas(&[
            "install",
            "-o",
            "root",
            "-g",
            "wheel",
            "-m",
            mode,
            &src.to_string_lossy(),
            &dst.to_string_lossy(),
        ])?;
    }
    // First-boot scripts would make the first run unlike the others.
    doas(&["rm", "-f", &mnt.join("firstboot").to_string_lossy()])?;
    Ok(())
}

/// Builds `kbench.ko` against the release's kernel sources.
fn build_kbench(dirs: &Dirs) -> Result<PathBuf> {
    let src = dirs.perf.join("src");
    let sys = src.join("usr/src/sys");
    if !sys.join("kern").is_dir() {
        let txz = dirs.dist.join("src.txz");
        verify(&txz, "src.txz", &dirs.dist.join("MANIFEST"))?;
        fs::create_dir_all(&src)?;
        eprintln!("xtask: perf: extracting kernel sources");
        util::run(
            Command::new("tar")
                .arg("-xf")
                .arg(&txz)
                .arg("-C")
                .arg(&src)
                .arg("usr/src/sys"),
        )?;
    }
    let obj = dirs.perf.join("kbench-obj");
    fs::create_dir_all(&obj)?;
    util::run(
        Command::new("make")
            .arg("-C")
            .arg(util::root().join("tests/perf/kbench"))
            .arg(format!("SYSDIR={}", sys.display()))
            .arg(format!("MAKEOBJDIR={}", obj.display())),
    )?;
    Ok(obj.join("kbench.ko"))
}

/// The second disk: a UFS file system holding `kbench.ko` and the suite
/// name. Attached to both kernels' VMs, so the configurations match.
fn make_bench_disk(out: &Path, kbench: &Path, suite: &str) -> Result<PathBuf> {
    let dir = out.join("bench-disk");
    fs::create_dir_all(&dir)?;
    fs::copy(kbench, dir.join("kbench.ko"))?;
    fs::write(dir.join("suite"), suite)?;
    // The NVMe disk the P3 suite reads (zeros are fine for reads).
    fs::File::create(out.join("nvme.img"))?.set_len(64 << 20)?;
    let disk = out.join("bench-disk.img");
    util::run(
        Command::new("makefs")
            .args(["-t", "ffs", "-s", "16m"])
            .arg(&disk)
            .arg(&dir),
    )?;
    Ok(disk)
}

/// The commands `freebsd-record` captures, with the file each lands in.
const RECORDINGS: &[(&str, &str)] = &[
    ("devinfo -rv", "devinfo-rv.txt"),
    ("pciconf -lv", "pciconf-lv.txt"),
    ("sysctl dev", "sysctl-dev.txt"),
    ("devinfo -u", "devinfo-u.txt"),
];

/// `cargo xtask --remote freebsd-record [--out DIR]`: boots FreeBSD 15.1 in
/// bhyve with the P3 reference devices (virtio-blk, NVMe, AHCI, virtio-rng)
/// and records how it names and describes them, for `lib/devtree`'s
/// differential tests. Writes into `DIR` (default
/// `tests/corpus/freebsd/bhyve`) on the server.
pub(crate) fn cmd_record(mut args: Args) -> Result<()> {
    let out = PathBuf::from(
        args.take_value("--out")?
            .unwrap_or_else(|| "tests/corpus/freebsd/bhyve".into()),
    );
    args.finish()?;
    if !cfg!(target_os = "freebsd") {
        return Err("freebsd-record needs bhyve: run it on the build server".into());
    }
    let dirs = Dirs::new();
    fs::create_dir_all(&dirs.perf)?;
    let freebsd = prepare_image(&dirs)?;
    let work = dirs.perf.join("record");
    let script_dir = work.join("disk");
    fs::create_dir_all(&script_dir)?;
    let mut script = String::from("#!/bin/sh\n");
    for (cmd, _) in RECORDINGS {
        let _ = writeln!(
            script,
            "echo 'record: begin {cmd}'\n{cmd} 2>&1 | sed 's/^/record: | /'\necho 'record: end'"
        );
    }
    script.push_str("echo 'record: done'\n");
    fs::write(script_dir.join("run.sh"), script)?;
    let disk = work.join("script.img");
    util::run(
        Command::new("makefs")
            .args(["-t", "ffs", "-s", "16m"])
            .arg(&disk)
            .arg(&script_dir),
    )?;
    let ahci = work.join("ahci.img");
    fs::File::create(&ahci)?.set_len(64 << 20)?;
    let vm = Vm {
        smp: 4,
        mem: "2G".into(),
        extra_disk: Some(disk),
        devices: vec![
            "5,nvme,ram=64".into(),
            format!("6,ahci-hd,{}", ahci.display()),
        ],
        ..Vm::new(Vmm::Bhyve, Arch::X86_64, freebsd)
    };
    let log = work.join("console.log");
    let done = |line: &str| line.contains("record: done").then_some(Outcome::Passed);
    let run = vm.run_until(Duration::from_secs(600), &log, false, &done)?;
    if run.outcome != Outcome::Passed {
        return Err(format!(
            "recording incomplete ({:?}); see {}",
            run.outcome,
            log.display()
        )
        .into());
    }
    let recorded = split_recordings(&fs::read_to_string(&log)?);
    fs::create_dir_all(&out)?;
    for (cmd, file) in RECORDINGS {
        let text = recorded
            .get(*cmd)
            .ok_or_else(|| format!("no output for `{cmd}` in {}", log.display()))?;
        fs::write(out.join(file), text)?;
        eprintln!("xtask: freebsd-record: {}", out.join(file).display());
    }
    Ok(())
}

/// The output of each recorded command, from the console log.
fn split_recordings(log: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut cur: Option<(String, String)> = None;
    for line in log.lines() {
        if let Some(cmd) = line.split("record: begin ").nth(1) {
            cur = Some((cmd.to_owned(), String::new()));
        } else if line.ends_with("record: end") {
            if let Some((cmd, text)) = cur.take() {
                out.insert(cmd, text);
            }
        } else if let (Some((_, text)), Some(rest)) =
            (cur.as_mut(), line.split("record: | ").nth(1))
        {
            text.push_str(rest);
            text.push('\n');
        } else if let (Some((_, text)), true) = (cur.as_mut(), line.ends_with("record: |")) {
            text.push('\n');
        }
    }
    out
}

/// Boots `disk` in the benchmark VM and returns its results.
fn run_one(disk: &Path, bench_disk: &Path, log: &Path) -> Result<Vec<(String, f64, String)>> {
    let vm = Vm {
        smp: 8,
        mem: "2G".into(),
        extra_disk: Some(bench_disk.to_path_buf()),
        // The P3 suite's NVMe disk, beside the bench disk; the same on
        // both sides. File-backed: bhyve's RAM-backed namespaces fail
        // 128 KiB reads (EIO, under FreeBSD too).
        devices: vec![format!(
            "5,nvme,{}",
            bench_disk.with_file_name("nvme.img").display()
        )],
        pin: true,
        ..Vm::new(Vmm::Bhyve, Arch::X86_64, disk.to_path_buf())
    };
    // Both kernels power off when done; stop early only on a panic.
    let panic = |line: &str| {
        (line.contains("panic:") || line.contains("fatal trap"))
            .then(|| Outcome::Failed(line.to_owned()))
    };
    let run = vm.run_until(Duration::from_secs(900), log, false, &panic)?;
    let text = fs::read_to_string(log)?;
    let results =
        parse(&text).map_err(|e| format!("{}: {e} (outcome {:?})", log.display(), run.outcome))?;
    Ok(results)
}

/// Extracts `kbench <name> <value> <unit>` lines between `kbench begin` and
/// `kbench end`.
fn parse(log: &str) -> Result<Vec<(String, f64, String)>> {
    let mut results = Vec::new();
    let (mut begun, mut ended) = (false, false);
    for line in log.lines() {
        let Some(at) = line.find("kbench ") else {
            continue;
        };
        let words: Vec<&str> = line[at..].split_whitespace().collect();
        match words.as_slice() {
            ["kbench", "begin", ..] => begun = true,
            ["kbench", "end", ..] => ended = true,
            ["kbench", name, value, unit] if begun && !ended => {
                let v: f64 = value
                    .parse()
                    .map_err(|_| format!("bad value in `{line}`"))?;
                results.push(((*name).to_owned(), v, (*unit).to_owned()));
            }
            _ => {}
        }
    }
    if !begun || !ended {
        return Err("benchmark output incomplete".into());
    }
    Ok(results)
}

/// Host facts for the record.
fn environment(rev: &str) -> String {
    let run = |c: &mut Command| util::output(c).unwrap_or_else(|e| format!("({e})\n"));
    let mut s = format!("abyss revision: {rev}\n");
    s += &run(Command::new("uname").arg("-a"));
    s += &run(Command::new("sysctl").args(["hw.model", "hw.ncpu", "hw.physmem"]));
    s += &run(Command::new("pkg").args(["query", "%n-%v", "edk2-bhyve"]));
    // The microcode revision, when cpuctl(4) is loaded.
    s += &run(Command::new("doas").args(["cpucontrol", "-m", "0x8b", "/dev/cpuctl0"]));
    s
}

/// One benchmark's comparison.
#[derive(Debug)]
struct Row {
    name: String,
    unit: String,
    freebsd: f64,
    abyss: f64,
    ratio: f64,
    ci: (f64, f64),
    cv: f64,
    verdict: Verdict,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    Pass,
    Win,
    Fail,
    /// FreeBSD's own noise is too high to resolve the bound.
    Noisy,
    /// Only one side reported it.
    Missing,
}

struct Report {
    rows: Vec<Row>,
}

impl Report {
    fn new(
        samples: &BTreeMap<String, BTreeMap<Side, Vec<f64>>>,
        units: &BTreeMap<String, String>,
    ) -> Self {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let rows = samples
            .iter()
            .map(|(name, sides)| {
                let empty = Vec::new();
                let f = sides.get(&Side::FreeBsd).unwrap_or(&empty);
                let a = sides.get(&Side::Abyss).unwrap_or(&empty);
                let unit = units.get(name).cloned().unwrap_or_default();
                if f.is_empty() || a.is_empty() {
                    return Row {
                        name: name.clone(),
                        unit,
                        freebsd: median(f),
                        abyss: median(a),
                        ratio: f64::NAN,
                        ci: (f64::NAN, f64::NAN),
                        cv: f64::NAN,
                        verdict: Verdict::Missing,
                    };
                }
                let (fm, am) = (median(f), median(a));
                let ratio = am / fm;
                let ci = bootstrap_ci(f, a, &mut rng);
                let cv = cv(f);
                let verdict = if cv > MAX_CV {
                    Verdict::Noisy
                } else if ratio > BOUND {
                    Verdict::Fail
                } else if ci.1 < 1.0 {
                    Verdict::Win
                } else {
                    Verdict::Pass
                };
                Row {
                    name: name.clone(),
                    unit,
                    freebsd: fm,
                    abyss: am,
                    ratio,
                    ci,
                    cv,
                    verdict,
                }
            })
            .collect();
        Self { rows }
    }

    fn failures(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| matches!(r.verdict, Verdict::Fail | Verdict::Missing))
            .count()
    }

    fn render(&self, suite: &str, rev: &str, runs: usize) -> String {
        let mut s = format!(
            "perf suite {suite}, AbyssBSD {rev} vs FreeBSD {RELEASE}, bhyve, {runs} runs per side\n\
             ratio = median(AbyssBSD) / median(FreeBSD); pass <= {BOUND:.2}; \
             noisy = FreeBSD CV > {:.0}%\n\n",
            MAX_CV * 100.0
        );
        let _ = writeln!(
            s,
            "{:<22} {:>14} {:>14} {:>7}  {:<15} {:>6}  verdict",
            "benchmark", "FreeBSD", "AbyssBSD", "ratio", "95% CI", "CV"
        );
        for r in &self.rows {
            let _ = writeln!(
                s,
                "{:<22} {:>14} {:>14} {:>7.3}  [{:.3}, {:.3}] {:>5.1}%  {:?}",
                r.name,
                format!("{:.1} {}", r.freebsd, r.unit),
                format!("{:.1} {}", r.abyss, r.unit),
                r.ratio,
                r.ci.0,
                r.ci.1,
                r.cv * 100.0,
                r.verdict
            );
        }
        let ratios: Vec<f64> = self
            .rows
            .iter()
            .map(|r| r.ratio)
            .filter(|r| r.is_finite() && *r > 0.0)
            .collect();
        let geo = (ratios.iter().map(|r| r.ln()).sum::<f64>() / ratios.len().max(1) as f64).exp();
        let count = |v: Verdict| self.rows.iter().filter(|r| r.verdict == v).count();
        let _ = writeln!(
            s,
            "\ngeometric mean ratio {geo:.3}; {} pass, {} win, {} fail, {} noisy, {} missing",
            count(Verdict::Pass),
            count(Verdict::Win),
            count(Verdict::Fail),
            count(Verdict::Noisy),
            count(Verdict::Missing)
        );
        s
    }
}

fn median(v: &[f64]) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let mut s = v.to_vec();
    s.sort_by(f64::total_cmp);
    let n = s.len();
    if n % 2 == 1 {
        s[n / 2]
    } else {
        (s[n / 2 - 1] + s[n / 2]) / 2.0
    }
}

/// Coefficient of variation (sample standard deviation over the mean).
fn cv(v: &[f64]) -> f64 {
    if v.len() < 2 {
        return 0.0;
    }
    let n = v.len() as f64;
    let mean = v.iter().sum::<f64>() / n;
    let var = v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0);
    var.sqrt() / mean
}

/// The 95% percentile-bootstrap interval of median(a) / median(f).
fn bootstrap_ci(f: &[f64], a: &[f64], rng: &mut Rng) -> (f64, f64) {
    let mut resample = |v: &[f64]| {
        let r: Vec<f64> = (0..v.len()).map(|_| v[rng.below(v.len())]).collect();
        median(&r)
    };
    let mut ratios: Vec<f64> = (0..BOOTSTRAP).map(|_| resample(a) / resample(f)).collect();
    ratios.sort_by(f64::total_cmp);
    (
        ratios[BOOTSTRAP * 25 / 1000],
        ratios[BOOTSTRAP * 975 / 1000 - 1],
    )
}

/// xorshift64*: reproducible resampling.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        let x = self.0.wrapping_mul(0x2545_f491_4f6c_dd1d);
        usize::try_from(x % n as u64).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_between_markers() {
        let log = "boot noise\nkbench mtx.t1 1.0 ns/op\n\
                   kbench begin p2 cpus=8\nkbench mtx.t1 18.250 ns/op\n\
                   [  1.0] kbench callout.latency.p50 4164 ns\nkbench end p2\n\
                   kbench late 1 ns\n";
        let r = parse(log).unwrap();
        assert_eq!(
            r,
            vec![
                ("mtx.t1".to_owned(), 18.25, "ns/op".to_owned()),
                ("callout.latency.p50".to_owned(), 4164.0, "ns".to_owned()),
            ]
        );
        assert!(parse("kbench begin p2\n").is_err());
    }

    #[test]
    fn statistics() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 2.0, 3.0]), 2.5);
        assert!(cv(&[10.0, 10.0, 10.0]) < 1e-12);
        let f = [100.0, 101.0, 99.0, 100.5, 99.5];
        let a = [90.0, 91.0, 89.0, 90.5, 89.5];
        let (lo, hi) = bootstrap_ci(&f, &a, &mut Rng(1));
        assert!(lo <= 0.9 && (0.9..1.0).contains(&hi), "({lo}, {hi})");
    }

    #[test]
    fn verdicts() {
        let mut s = BTreeMap::new();
        let mut units = BTreeMap::new();
        let mut add = |name: &str, f: &[f64], a: &[f64]| {
            let e: &mut BTreeMap<Side, Vec<f64>> = s.entry(name.to_owned()).or_default();
            e.insert(Side::FreeBsd, f.to_vec());
            e.insert(Side::Abyss, a.to_vec());
            units.insert(name.to_owned(), "ns".to_owned());
        };
        add("same", &[100.0, 100.1, 99.9], &[100.0, 100.1, 99.9]);
        add("slow", &[100.0, 100.1, 99.9], &[110.0, 110.1, 109.9]);
        add("fast", &[100.0, 100.1, 99.9], &[50.0, 50.1, 49.9]);
        add("noisy", &[50.0, 100.0, 150.0], &[100.0, 100.0, 100.0]);
        let r = Report::new(&s, &units);
        let v: Vec<_> = r
            .rows
            .iter()
            .map(|r| (r.name.as_str(), r.verdict))
            .collect();
        assert_eq!(
            v,
            [
                ("fast", Verdict::Win),
                ("noisy", Verdict::Noisy),
                ("same", Verdict::Pass),
                ("slow", Verdict::Fail),
            ]
        );
        assert_eq!(r.failures(), 1);
    }

    #[test]
    fn checksum_formats() {
        let dir = std::env::temp_dir().join(format!("abyss-perf-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let sums = dir.join("CHECKSUM");
        fs::write(&sums, "SHA256 (x.xz) = abc\n").unwrap();
        // Lookup works; the hash itself is checked with sha256(1) on FreeBSD.
        assert!(verify(&dir.join("nope"), "y.xz", &sums).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
