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
use crate::base::{self, RELEASE};
use crate::build;
use crate::cli::Args;
use crate::image;
use crate::qemu::{Outcome, Vm, Vmm};
use crate::util::{self, Result};
const VM_IMAGE: &str = "FreeBSD-15.1-RELEASE-amd64-ufs.raw.xz";
const VM_IMAGE_ARM64: &str = "FreeBSD-15.1-RELEASE-arm64-aarch64-ufs.raw.xz";
/// Bumped whenever [`prepare_image`] changes what it writes.
const PREP_VERSION: &str = "17";
/// The bound on a component's geometric mean of AbyssBSD's medians over
/// FreeBSD's (ADR-0014); a single benchmark over it is only reported.
const BOUND: f64 = 1.02;
/// FreeBSD-side coefficient of variation above which a benchmark cannot
/// resolve the bound.
const MAX_CV: f64 = 0.01;
const BOOTSTRAP: usize = 2000;

const USAGE: &str = "usage: cargo xtask --remote perf [--suite p2|p3|p4|p5|p6|p6.thr|p6.event|p6.unix|p6.pty|vt|diag] [--runs N] [--prepare] \
                     [--freebsd-only [--prof] | --abyss-only [--prof [--prof-cpu N]]]";

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
    // FreeBSD's numbers alone: a baseline for a suite AbyssBSD cannot run
    // yet (p4 until its system calls are in).
    let freebsd_only = args.take_flag("--freebsd-only");
    // AbyssBSD's numbers alone, for investigating; with `--prof`, under
    // the kernel's sampling profiler (`abyss.prof`), whose reports, one
    // per benchmark, are printed.
    let abyss_only = args.take_flag("--abyss-only");
    // With `--freebsd-only`: the userland suite under DTrace's profile
    // provider, each benchmark's busiest kernel functions printed as
    // `prof fbsd` lines, to set beside AbyssBSD's.
    let prof = args.take_flag("--prof");
    // With `--prof`: the functions of one CPU's samples only (ubench runs
    // bound to CPU 1).
    let prof_cpu = args.take_value("--prof-cpu")?;
    args.finish()?;
    if freebsd_only && abyss_only
        || prof && !(abyss_only || freebsd_only)
        || prof_cpu.is_some() && !(prof && abyss_only)
    {
        return Err(USAGE.into());
    }
    // `diag` holds AbyssBSD-side diagnostics, some with FreeBSD
    // counterparts; it is for investigation, not gating.
    if ![
        "p2", "p3", "p4", "p5", "p6", "p6.thr", "p6.event", "p6.unix", "p6.pty", "vt", "diag",
    ]
    .contains(&suite.as_str())
    {
        return Err(format!(
            "unknown suite `{suite}` (p2, p3, p4, p5, p6, p6.thr, p6.event, p6.unix, p6.pty, vt, \
             diag)"
        )
        .into());
    }
    // AbyssBSD runs all of p6 from the installed root once base runs;
    // `p6.thr`, `p6.event`, `p6.unix`, and `p6.pty`, its threads, events,
    // UNIX-domain sockets, and pseudo-terminals components, need only the
    // static ubench.
    if suite == "p6" && !freebsd_only {
        return Err(
            "suite p6 runs on FreeBSD alone for now: add --freebsd-only, or run p6.thr or p6.event"
                .into(),
        );
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
    let ubench = if suite.starts_with("p6") {
        build_ubench_p6(&dirs)?
    } else {
        build_ubench(&dirs)?
    };
    if prepare_only {
        eprintln!(
            "xtask: perf: prepared {}, {} and {}",
            freebsd.display(),
            kbench.display(),
            ubench.display()
        );
        return Ok(());
    }

    let rev = std::env::var("ABYSS_REV").unwrap_or_else(|_| "unknown".into());
    let stamp = util::output(Command::new("date").arg("+%Y%m%d-%H%M%S"))?;
    let out = home()
        .join("abyss/ci/perf")
        .join(format!("{}-{}", stamp.trim(), rev));
    fs::create_dir_all(out.join("logs"))?;

    // The userland suites run ubench; the earlier ones, kbench. The file
    // systems' suite also needs its tree and fsx beside it.
    let userland = [
        "p4", "p5", "p6", "p6.thr", "p6.event", "p6.unix", "p6.pty", "vt",
    ]
    .contains(&suite.as_str());
    let bench = if userland { &ubench } else { &kbench };
    let extras = if suite == "p5" {
        vec![build_tree(&dirs)?, build_fsx(&dirs)?]
    } else {
        Vec::new()
    };
    let freebsd_prof = prof && freebsd_only;
    if freebsd_prof && !userland {
        return Err("--freebsd-only --prof profiles the userland suites (p4, p5)".into());
    }
    let bench_disk = make_bench_disk(&out, bench, &suite, &extras, freebsd_prof)?;
    let opts = build::Options {
        release: true,
        ktest: false,
    };
    // The userland suite runs the same ubench on AbyssBSD, from the memory
    // disk (no UFS yet to read the bench disk).
    let extra: &[(&str, &Path)] = if suite == "p4" || suite == "vt" {
        &[("usr/bench/ubench", ubench.as_path())]
    } else {
        &[]
    };
    let cmdline = if let Some(cpu) = &prof_cpu {
        format!("abyss.bench={suite} abyss.prof abyss.prof.cpu={cpu}")
    } else if prof {
        format!("abyss.bench={suite} abyss.prof")
    } else {
        format!("abyss.bench={suite}")
    };
    let abyss = image::build_with(Arch::X86_64, opts, &cmdline, extra)?;

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
            if freebsd_only && side == Side::Abyss || abyss_only && side == Side::FreeBsd {
                continue;
            }
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
            if prof {
                let text = fs::read_to_string(&log)?;
                for line in text.lines().filter(|l| l.starts_with("prof ")) {
                    println!("{line}");
                }
            }
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

    if abyss_only {
        for (name, sides) in &samples {
            let v = &sides[&Side::Abyss];
            println!("{name:<24} {:>14.1} {}", median(v), units[name]);
        }
        eprintln!("xtask: perf: results in {}", out.display());
        return Ok(());
    }
    let report = Report::new(&samples, &units);
    let text = if freebsd_only {
        report.render_baseline(&suite, runs)
    } else {
        report.render(&suite, &rev, runs)
    };
    fs::write(out.join("report.txt"), &text)?;
    print!("{text}");
    eprintln!("xtask: perf: results in {}", out.display());
    if freebsd_only {
        return Ok(());
    }
    if report.failures() > 0 {
        return Err(format!("{} component(s) over the bound", report.failures()).into());
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
pub(crate) fn doas(args: &[&str]) -> Result<String> {
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

/// The prepared FreeBSD VM image booting single-user, and a directory for
/// work beside it. In single-user mode init gives the shell the console as
/// its controlling terminal, as AbyssBSD's boot-to-shell does.
pub(crate) fn freebsd_single_user_image() -> Result<(PathBuf, PathBuf)> {
    let dirs = Dirs::new();
    fs::create_dir_all(&dirs.perf)?;
    let base = prepare_image(&dirs)?;
    let img = dirs.perf.join(format!("freebsd-{RELEASE}-single.img"));
    let stamp = img.with_extension("img.prepared");
    if img.exists() && fs::read_to_string(&stamp).is_ok_and(|s| s.trim() == PREP_VERSION) {
        return Ok((img, dirs.perf));
    }
    let tmp = img.with_extension("img.tmp");
    fs::copy(&base, &tmp)?;
    with_root(&dirs, &tmp, |mnt| {
        let conf = mnt.join("boot/loader.conf.local");
        let text = fs::read_to_string(&conf)? + "boot_single=\"YES\"\n";
        let staging = mnt.with_file_name("staging");
        fs::create_dir_all(&staging)?;
        let src = staging.join("loader.conf.local");
        fs::write(&src, text)?;
        doas(&[
            "install",
            "-o",
            "root",
            "-g",
            "wheel",
            "-m",
            "644",
            &src.to_string_lossy(),
            &conf.to_string_lossy(),
        ])?;
        Ok(())
    })?;
    fs::rename(&tmp, &img)?;
    fs::write(&stamp, PREP_VERSION)?;
    Ok((img, dirs.perf))
}

/// Bumped whenever [`freebsd_multiuser_image`] changes what it writes.
const MULTIUSER_PREP_VERSION: &str = "1";

/// FreeBSD 15.1's release VM image for `arch`, configured as its installer
/// would leave it (docs/MULTIUSER.md §14, [`crate::multiuser`]): the
/// installer's `rc.conf` and time zone, both consoles (`boot_multicons`,
/// the serial one primary, so `getty` runs on `ttyu0` and `ttyv0`), and
/// no first-boot scripts. Returns the image and the directory to work in.
pub(crate) fn freebsd_multiuser_image(arch: Arch) -> Result<(PathBuf, PathBuf)> {
    let dirs = Dirs::new();
    fs::create_dir_all(&dirs.perf)?;
    let (xz, sums) = match arch {
        Arch::X86_64 => (dirs.dist.join(VM_IMAGE), dirs.dist.join("CHECKSUM.SHA256")),
        Arch::Aarch64 => (
            dirs.dist.join("arm64").join(VM_IMAGE_ARM64),
            dirs.dist.join("arm64/CHECKSUM.SHA256"),
        ),
    };
    let img = dirs
        .perf
        .join(format!("freebsd-{RELEASE}-{arch}-multiuser.img"));
    let stamp = img.with_extension("img.prepared");
    if img.exists() && fs::read_to_string(&stamp).is_ok_and(|s| s.trim() == MULTIUSER_PREP_VERSION)
    {
        return Ok((img, dirs.perf));
    }
    let name = xz.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    eprintln!("xtask: preparing {} from {}", img.display(), xz.display());
    verify(&xz, name, &sums)?;
    let tmp = img.with_extension("img.tmp");
    let status = Command::new("xz")
        .arg("-dc")
        .arg(&xz)
        .stdout(fs::File::create(&tmp)?)
        .status()?;
    if !status.success() {
        return Err("xz failed".into());
    }
    with_root(&dirs, &tmp, |mnt| {
        let loader = "autoboot_delay=\"0\"\nbeastie_disable=\"YES\"\n\
                      boot_multicons=\"YES\"\nboot_serial=\"YES\"\n\
                      console=\"comconsole,efi\"\n";
        let mut files = crate::multiuser::installed_files(mnt)?;
        files.push(("boot/loader.conf.local", 0o644, loader.as_bytes().to_vec()));
        install_files(mnt, &files)?;
        doas(&["rm", "-f", &mnt.join("firstboot").to_string_lossy()])?;
        Ok(())
    })?;
    fs::rename(&tmp, &img)?;
    fs::write(&stamp, MULTIUSER_PREP_VERSION)?;
    Ok((img, dirs.perf))
}

/// Installs `files` (path under the root, mode, contents) into the root
/// mounted at `mnt`, owned by root.
pub(crate) fn install_files(mnt: &Path, files: &[(&str, u32, Vec<u8>)]) -> Result<()> {
    let staging = mnt.with_file_name("staging");
    fs::create_dir_all(&staging)?;
    for (path, mode, data) in files {
        let src = staging.join(path.replace('/', "_"));
        fs::write(&src, data)?;
        let dst = mnt.join(path);
        doas(&[
            "install",
            "-o",
            "root",
            "-g",
            "wheel",
            "-m",
            &format!("{mode:o}"),
            &src.to_string_lossy(),
            &dst.to_string_lossy(),
        ])?;
    }
    Ok(())
}

/// Runs `f` on the root file system of raw disk image `img`, mounted.
fn with_root(dirs: &Dirs, img: &Path, f: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    mounted_root(img, &dirs.perf.join("mnt"), f)
}

/// Runs `f` on the root file system of raw disk image `img`, mounted on
/// `mnt` (created if need be).
pub(crate) fn mounted_root(
    img: &Path,
    mnt: &Path,
    f: impl FnOnce(&Path) -> Result<()>,
) -> Result<()> {
    let md = doas(&[
        "mdconfig",
        "-a",
        "-t",
        "vnode",
        "-f",
        &img.to_string_lossy(),
    ])?;
    let md = md.trim().to_owned();
    fs::create_dir_all(mnt)?;
    let result = (|| -> Result<()> {
        let part = rootfs_partition(&md)?;
        doas(&["mount", &format!("/dev/{part}"), &mnt.to_string_lossy()])?;
        let r = f(mnt);
        doas(&["umount", &mnt.to_string_lossy()])?;
        r
    })();
    let _ = doas(&["mdconfig", "-d", "-u", &md]);
    result
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

    with_root(dirs, &tmp, configure_guest)?;
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
///
/// PTI is off, as stock FreeBSD sets it on this hardware: it decides from
/// `IA32_ARCH_CAPABILITIES.RDCL_NO`, which bhyve does not pass through, so
/// a guest on a CPU immune to Meltdown would otherwise isolate its page
/// tables anyway and pay for it on every kernel entry (`getpid` 240 ns
/// against 67 ns on the host). `rc.local` prints the mitigations in force.
fn configure_guest(mnt: &Path) -> Result<()> {
    let files: [(&str, &str, &str); 3] = [
        (
            "boot/loader.conf.local",
            "644",
            "autoboot_delay=\"0\"\nbeastie_disable=\"YES\"\nconsole=\"comconsole\"\n\
             vm.pmap.pti=\"0\"\n",
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
             sysctl vm.pmap.pti hw.ibrs_active hw.mds_disable_state\n\
             if mount -o ro /dev/vtbd1 /mnt; then\n\
             \tif [ -f /mnt/run.sh ]; then\n\
             \t\tsh /mnt/run.sh\n\
             \t\tumount /mnt\n\
             \telse\n\
             \t\trm -f /tmp/kbench.ko /tmp/ubench /tmp/tree.tar /tmp/fsx\n\
             \t\tcp /mnt/kbench.ko /mnt/ubench /mnt/suite /mnt/tree.tar /mnt/fsx /tmp/ 2>/dev/null\n\
             \t\tumount /mnt\n\
             \t\tfor irq in $(vmstat -ai | awk '/virtio_pci|nvme/ { sub(/^irq/, \"\", $1); sub(/:$/, \"\", $1); print $1 }'); do\n\
             \t\t\tcpuset -l 0 -x \"$irq\"\n\
             \t\tdone\n\
             \t\tvmstat -ai | grep -E 'virtio_pci|nvme'\n\
             \t\tif [ -f /tmp/ubench ]; then\n\
             \t\t\t/tmp/ubench \"$(cat /tmp/suite)\"\n\
             \t\telse\n\
             \t\t\tkldload /tmp/kbench.ko && sysctl debug.kbench.run=\"$(cat /tmp/suite)\" >/dev/null\n\
             \t\tfi\n\
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

/// Extracts one file or directory of the release's `src.txz` into the
/// perf directory's `src/`, once.
fn release_source(dirs: &Dirs, path: &str) -> Result<PathBuf> {
    let src = dirs.perf.join("src");
    let at = src.join(path);
    if !at.exists() {
        let txz = dirs.dist.join("src.txz");
        verify(&txz, "src.txz", &dirs.dist.join("MANIFEST"))?;
        fs::create_dir_all(&src)?;
        eprintln!("xtask: perf: extracting {path}");
        util::run(
            Command::new("tar")
                .arg("-xf")
                .arg(&txz)
                .arg("-C")
                .arg(&src)
                .arg(path),
        )?;
    }
    Ok(at)
}

/// Builds FreeBSD's `fsx` (from the release's sources: it is under the
/// APSL, and never copied into this repository) as a static binary.
fn build_fsx(dirs: &Dirs) -> Result<PathBuf> {
    let source = release_source(dirs, "usr/src/tools/regression/fsx")?.join("fsx.c");
    let out = dirs.perf.join("fsx");
    base::cc_static_foreign(Arch::X86_64, &source, &out)?;
    Ok(out)
}

/// The tree `fs.*.extract` unpacks and `fs.*.find` walks: `base.txz`'s
/// `/usr/share` and `/usr/include` (about 25,600 files, 124 MiB) as an
/// uncompressed tar, so the benchmark measures the file system, not xz.
fn build_tree(dirs: &Dirs) -> Result<PathBuf> {
    let out = dirs.perf.join("tree.tar");
    if out.exists() {
        return Ok(out);
    }
    let txz = base::dir(Arch::X86_64).join("base.txz");
    let work = dirs.perf.join("tree");
    let _ = fs::remove_dir_all(&work);
    fs::create_dir_all(&work)?;
    eprintln!("xtask: perf: building {}", out.display());
    util::run(
        Command::new("tar")
            .arg("-xf")
            .arg(&txz)
            .arg("-C")
            .arg(&work)
            .args(["./usr/share", "./usr/include"]),
    )?;
    let tmp = out.with_extension("tar.tmp");
    util::run(
        Command::new("tar")
            .arg("-cf")
            .arg(&tmp)
            .arg("-C")
            .arg(&work)
            .arg("usr"),
    )?;
    fs::rename(&tmp, &out)?;
    let _ = fs::remove_dir_all(&work);
    Ok(out)
}

/// Builds `ubench`, the userland suites' static binary, for the bhyve guests.
fn build_ubench(dirs: &Dirs) -> Result<PathBuf> {
    let out = dirs.perf.join("ubench");
    base::cc_static(
        Arch::X86_64,
        &util::root().join("tests/perf/ubench/ubench.c"),
        &out,
    )?;
    Ok(out)
}

/// `ubench` with suite `p6`, which needs threads: built apart, with
/// libthr (`-lthr`: the trimmed sysroot has no `libpthread.a` link), so
/// the earlier suites' binary stays as it was. Same name, so
/// the guests' scripts run it unchanged.
fn build_ubench_p6(dirs: &Dirs) -> Result<PathBuf> {
    let dir = dirs.perf.join("ubench-p6");
    fs::create_dir_all(&dir)?;
    let out = dir.join("ubench");
    base::cc_static_with(
        Arch::X86_64,
        &util::root().join("tests/perf/ubench/ubench.c"),
        &["-DUB_P6", "-lthr"],
        &out,
    )?;
    Ok(out)
}

/// `cargo xtask ubench [--arch A] [--runs N]`: the P4 suite on AbyssBSD
/// alone, in local QEMU (KVM where it can), for comparing AbyssBSD with
/// itself while working on it. The numbers that count come from `perf`,
/// against FreeBSD on the same bhyve host.
pub(crate) fn cmd_ubench(mut args: Args) -> Result<()> {
    let arches = Arch::from_args(&mut args)?;
    let runs: usize = args
        .take_value("--runs")?
        .map_or(Ok(1), |s| s.parse())
        .map_err(|_| "bad --runs")?;
    let bhyve = args.take_flag("--bhyve");
    args.finish()?;
    if bhyve && !cfg!(target_os = "freebsd") {
        return Err("ubench --bhyve needs bhyve: run it on the build server".into());
    }
    for arch in arches {
        let dir = util::root().join("build").join(arch.to_string());
        fs::create_dir_all(&dir)?;
        let bin = dir.join("ubench");
        base::cc_static(arch, &util::root().join("tests/perf/ubench/ubench.c"), &bin)?;
        let opts = build::Options {
            release: true,
            ktest: false,
        };
        let disk = image::build_with(
            arch,
            opts,
            "abyss.bench=p4 abyss.qemu_exit=1",
            &[("usr/bench/ubench", bin.as_path())],
        )?;
        let mut all: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        let mut units = BTreeMap::new();
        for run in 0..runs {
            let log = dir.join(format!("ubench-{run}.log"));
            let vm = if bhyve {
                // As `perf` runs AbyssBSD, without its disks.
                Vm {
                    smp: 8,
                    mem: "2G".into(),
                    pin: true,
                    ..Vm::new(Vmm::Bhyve, arch, disk.clone())
                }
            } else {
                Vm {
                    smp: 8,
                    mem: "2G".into(),
                    test_devices: true,
                    ..Vm::new(Vmm::Qemu, arch, disk.clone())
                }
            };
            vm.run_until(Duration::from_secs(900), &log, false, &|line: &str| {
                (line.contains("panic:") || line.contains("fatal trap"))
                    .then(|| Outcome::Failed(line.to_owned()))
            })?;
            let text = fs::read_to_string(&log)?;
            for (name, value, unit) in
                parse(&text).map_err(|e| format!("{}: {e}", log.display()))?
            {
                units.insert(name.clone(), unit);
                all.entry(name).or_default().push(value);
            }
        }
        let vmm = if bhyve { "bhyve" } else { "local QEMU" };
        println!("ubench p4 on AbyssBSD ({arch}, {vmm}), median of {runs}");
        for (name, mut v) in all {
            v.sort_by(f64::total_cmp);
            let unit = units.get(&name).map_or("", String::as_str);
            println!("{name:<24} {:>12.1} {unit}", v[v.len() / 2]);
        }
    }
    Ok(())
}

/// The second disk: a UFS file system holding the benchmark (`kbench.ko`,
/// or the `ubench` binary), the suite name, and the suite's `extras`.
/// Attached to both kernels' VMs, so the configurations match.
fn make_bench_disk(
    out: &Path,
    bench: &Path,
    suite: &str,
    extras: &[PathBuf],
    freebsd_prof: bool,
) -> Result<PathBuf> {
    let dir = out.join("bench-disk");
    fs::create_dir_all(&dir)?;
    for file in core::iter::once(bench).chain(extras.iter().map(PathBuf::as_path)) {
        let name = file.file_name().ok_or("bench file has no name")?;
        fs::copy(file, dir.join(name))?;
    }
    fs::write(dir.join("suite"), suite)?;
    if freebsd_prof {
        // FreeBSD's rc.local runs `run.sh` in place of the suite.
        fs::write(dir.join("run.sh"), FREEBSD_PROF_RUN)?;
        fs::write(dir.join("prof.d"), FREEBSD_PROF_D)?;
    }
    // The NVMe disk: the P3 suite reads it (zeros are fine), the P5 suite
    // formats it. Sparse.
    let nvme = if suite == "p5" { 4 << 30 } else { 64 << 20 };
    fs::File::create(out.join("nvme.img"))?.set_len(nvme)?;
    let disk = out.join("bench-disk.img");
    // Sized to fit, with room for makefs's own overhead. UFS2 (makefs
    // makes UFS1 by default), which AbyssBSD's suites mount too.
    util::run(
        Command::new("makefs")
            .args([
                "-t",
                "ffs",
                "-o",
                "version=2",
                "-M",
                "16m",
                "-f",
                "64",
                "-b",
                "10%",
            ])
            .arg(&disk)
            .arg(&dir),
    )?;
    Ok(disk)
}

/// `perf --freebsd-only --prof`'s script on the FreeBSD guest: the suite
/// as usual (interrupts on CPU 0), with DTrace watching it (started apart:
/// `dtrace -c` fails to take control of it).
const FREEBSD_PROF_RUN: &str = "#!/bin/sh\n\
    cp /mnt/ubench /mnt/suite /mnt/tree.tar /mnt/fsx /mnt/prof.d /tmp/ 2>/dev/null\n\
    for irq in $(vmstat -ai | awk '/virtio_pci|nvme/ { sub(/^irq/, \"\", $1); sub(/:$/, \"\", $1); print $1 }'); do\n\
    \tcpuset -l 0 -x \"$irq\"\n\
    done\n\
    kldload dtraceall\n\
    dtrace -q -x bufsize=16m -x aggsize=16m -s /tmp/prof.d &\n\
    sleep 5\n\
    UB_PROF=1 /tmp/ubench \"$(cat /tmp/suite)\"\n\
    sleep 2\n\
    kill -INT $!\n\
    wait\n";

/// The D script: kernel PCs sampled on CPU 1 (where ubench binds itself)
/// while the benchmark samples (its `kprof pause` and `kprof resume`
/// lines, as for `abyss.prof`); at each result line, the busiest
/// functions since the last, printed and cleared.
const FREEBSD_PROF_D: &str = "\
BEGIN { on = 1; }
profile-4999 /on && cpu == 1/ { @total = count(); }
profile-4999 /on && cpu == 1 && arg0 != 0/ { @fn[func(arg0)] = count(); }
syscall::write:entry /execname == \"ubench\"/ { self->s = copyinstr(arg1, arg2 < 120 ? arg2 : 120); }
syscall::write:entry /execname == \"ubench\" && strstr(self->s, \"kprof pause\") != NULL/ { on = 0; }
syscall::write:entry /execname == \"ubench\" && strstr(self->s, \"kprof resume\") != NULL/ { on = 1; }
syscall::write:entry /execname == \"ubench\" && strstr(self->s, \"kbench \") != NULL/ {
\tprintf(\"prof fbsd %s\", self->s);
\tprinta(\"prof fbsd %@d samples on CPU 1\\n\", @total);
\ttrunc(@fn, 40);
\tprinta(\"prof fbsd %@8d %a\\n\", @fn);
\ttrunc(@fn);
\tclear(@total);
\ton = 1;
}
";

/// The commands `freebsd-record` captures, with the file each lands in.
const RECORDINGS: &[(&str, &str)] = &[
    ("devinfo -rv", "devinfo-rv.txt"),
    ("pciconf -lv", "pciconf-lv.txt"),
    ("sysctl dev", "sysctl-dev.txt"),
    ("devinfo -u", "devinfo-u.txt"),
];

/// The commands `freebsd-record --usb` captures: how FreeBSD names and
/// describes the USB and input devices (ROADMAP U.0).
const USB_RECORDINGS: &[(&str, &str)] = &[
    ("dmesg", "dmesg.txt"),
    ("devinfo -rv", "devinfo-rv.txt"),
    ("usbconfig", "usbconfig.txt"),
    ("usbconfig dump_device_desc", "usbconfig-device-desc.txt"),
    (
        "usbconfig dump_curr_config_desc",
        "usbconfig-config-desc.txt",
    ),
    (
        "ls -l /dev | grep -E ' (ukbd|hkbd|kbd|kbdmux|atkbd|da|ugen|usbctl|uhid|ums|hms|sysmouse|psm|pass|uinput)[0-9.]*( |$)'",
        "dev.txt",
    ),
    ("ls -lR /dev/input /dev/usb", "dev-input-usb.txt"),
    (
        "sysctl kern.evdev hw.usb | grep -v '\\.debug'",
        "sysctl.txt",
    ),
    ("kbdcontrol -i < /dev/ttyv0", "kbdcontrol-i.txt"),
    // devd's events for each device as it goes away and comes back: devd
    // stopped, /dev/devctl read directly, each device's port powered off
    // and on.
    (
        "service devd onestop >/dev/null; cat /dev/devctl > /tmp/devctl & cat=$!; \
         for u in $(usbconfig | sed -n 's/^\\(ugen[0-9]*\\.[0-9]*\\):.*/\\1/p' | grep -v '\\.1$'); do \
         usbconfig -d $u power_off; sleep 2; usbconfig -d $u power_on; sleep 4; done; \
         kill $cat; cat /tmp/devctl",
        "devctl.txt",
    ),
];

/// `cargo xtask --remote freebsd-record [--out DIR] [--usb]`: boots
/// FreeBSD 15.1 in bhyve with the P3 reference devices (virtio-blk, NVMe,
/// AHCI, virtio-rng) and the boot tests' (another of each disk) and
/// records how it names and describes them, for `lib/devtree`'s
/// differential tests. Writes into `DIR` (default
/// `tests/corpus/freebsd/bhyve`) on the server.
///
/// With `--usb`, it records the USB and input devices instead, twice: in
/// QEMU with the boot tier's `--usb` set (`DIR/qemu`), and in bhyve with
/// its xHCI tablet (`DIR/bhyve`). `DIR` defaults to
/// `tests/corpus/freebsd/usb`.
pub(crate) fn cmd_record(mut args: Args) -> Result<()> {
    let usb = args.take_flag("--usb");
    let default = if usb {
        "tests/corpus/freebsd/usb"
    } else {
        "tests/corpus/freebsd/bhyve"
    };
    let out = PathBuf::from(args.take_value("--out")?.unwrap_or_else(|| default.into()));
    args.finish()?;
    if !cfg!(target_os = "freebsd") {
        return Err("freebsd-record needs bhyve: run it on the build server".into());
    }
    let dirs = Dirs::new();
    fs::create_dir_all(&dirs.perf)?;
    let freebsd = prepare_image(&dirs)?;
    let work = dirs.perf.join("record");
    fs::create_dir_all(&work)?;
    if usb {
        let script = script_disk(&work, USB_RECORDINGS)?;
        // QEMU emulates the CPU on a FreeBSD host: slow, but this only
        // names devices.
        let qemu = Vm {
            smp: 2,
            mem: "2G".into(),
            extra_disk: Some(script.clone()),
            usb: true,
            ..Vm::new(Vmm::Qemu, Arch::X86_64, freebsd.clone())
        };
        record(
            &qemu,
            USB_RECORDINGS,
            &work.join("qemu.log"),
            &out.join("qemu"),
        )?;
        let bhyve = Vm {
            smp: 4,
            mem: "2G".into(),
            extra_disk: Some(script),
            usb: true,
            ..Vm::new(Vmm::Bhyve, Arch::X86_64, freebsd)
        };
        return record(
            &bhyve,
            USB_RECORDINGS,
            &work.join("bhyve.log"),
            &out.join("bhyve"),
        );
    }
    let disk = script_disk(&work, RECORDINGS)?;
    let ahci = work.join("ahci.img");
    fs::File::create(&ahci)?.set_len(64 << 20)?;
    // The boot tests' read-only UFS2 disk, on virtio-blk, NVMe, and AHCI
    // (slots 7 to 9), their read-write one (slot 10), and the FAT test
    // disk (slot 11), as they attach them.
    let ufs = work.join("ufs.img");
    fs::File::create(&ufs)?.set_len(16 << 20)?;
    let vm = Vm {
        smp: 4,
        mem: "2G".into(),
        extra_disk: Some(disk),
        devices: vec![
            "5,nvme,ram=64".into(),
            format!("6,ahci-hd,{}", ahci.display()),
            format!("7,virtio-blk,{},ro", ufs.display()),
            format!("8,nvme,{},ro", ufs.display()),
            format!("9,ahci-hd,{},ro", ufs.display()),
            format!("10,virtio-blk,{}", ufs.display()),
            format!("11,virtio-blk,{},ro", ufs.display()),
        ],
        ..Vm::new(Vmm::Bhyve, Arch::X86_64, freebsd)
    };
    record(&vm, RECORDINGS, &work.join("console.log"), &out)
}

/// A UFS disk whose `run.sh` runs `commands`, framing each one's output
/// (by its index, as a command may hold quotes) for [`split_recordings`];
/// the guest's `rc.local` runs it.
fn script_disk(work: &Path, commands: &[(&str, &str)]) -> Result<PathBuf> {
    let script_dir = work.join("disk");
    let _ = fs::remove_dir_all(&script_dir);
    fs::create_dir_all(&script_dir)?;
    let mut script = String::from("#!/bin/sh\n");
    for (i, (cmd, _)) in commands.iter().enumerate() {
        let _ = writeln!(
            script,
            "echo 'record: begin {i}'\n( {cmd} ) 2>&1 | sed 's/^/record: | /'\necho 'record: end'"
        );
    }
    script.push_str("echo 'record: done'\n");
    fs::write(script_dir.join("run.sh"), script)?;
    let disk = work.join("script.img");
    let _ = fs::remove_file(&disk);
    util::run(
        Command::new("makefs")
            .args(["-t", "ffs", "-s", "16m"])
            .arg(&disk)
            .arg(&script_dir),
    )?;
    Ok(disk)
}

/// Boots `vm`, which runs a [`script_disk`], and writes each command's
/// output into `out`.
fn record(vm: &Vm, commands: &[(&str, &str)], log: &Path, out: &Path) -> Result<()> {
    let done = |line: &str| line.contains("record: done").then_some(Outcome::Passed);
    let run = vm.run_until(Duration::from_secs(1800), log, false, &done)?;
    if run.outcome != Outcome::Passed {
        return Err(format!(
            "recording incomplete ({:?}); see {}",
            run.outcome,
            log.display()
        )
        .into());
    }
    let recorded = split_recordings(&fs::read_to_string(log)?);
    fs::create_dir_all(out)?;
    for (i, (cmd, file)) in commands.iter().enumerate() {
        let text = recorded
            .get(&i.to_string())
            .ok_or_else(|| format!("no output for `{cmd}` in {}", log.display()))?;
        fs::write(out.join(file), text)?;
        eprintln!("xtask: freebsd-record: {}", out.join(file).display());
    }
    Ok(())
}

/// The output of each recorded command, by its index, from the console
/// log.
fn split_recordings(log: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut cur: Option<(String, String)> = None;
    for line in log.lines() {
        if let Some(index) = line.split("record: begin ").nth(1) {
            cur = Some((index.to_owned(), String::new()));
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
    /// Over the bound: a diagnostic (components gate, ADR-0014).
    Slower,
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
                        cv: cv(f),
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
                    Verdict::Slower
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

    /// Each component's benchmarks and the geometric mean of their ratios,
    /// and whether any of them is missing (ADR-0014).
    fn components(&self) -> Vec<(String, usize, f64, bool)> {
        let mut by: BTreeMap<&str, (usize, f64, bool)> = BTreeMap::new();
        for r in &self.rows {
            let e = by.entry(component(&r.name)).or_default();
            if r.verdict == Verdict::Missing || !(r.ratio.is_finite() && r.ratio > 0.0) {
                e.2 = true;
            } else {
                e.0 += 1;
                e.1 += r.ratio.ln();
            }
        }
        by.into_iter()
            .map(|(c, (n, logs, missing))| {
                (c.to_owned(), n, (logs / n.max(1) as f64).exp(), missing)
            })
            .collect()
    }

    /// Components over the bound or missing a benchmark: what fails a run.
    fn failures(&self) -> usize {
        self.components()
            .iter()
            .filter(|(_, _, geo, missing)| *geo > BOUND || *missing)
            .count()
    }

    /// FreeBSD's side alone, for `--freebsd-only`.
    fn render_baseline(&self, suite: &str, runs: usize) -> String {
        let mut s = format!(
            "perf suite {suite}, FreeBSD {RELEASE} baseline, bhyve, {runs} runs\n\
             noisy = CV > {:.0}%\n\n",
            MAX_CV * 100.0
        );
        let _ = writeln!(s, "{:<22} {:>14} {:>6}", "benchmark", "FreeBSD", "CV");
        for r in &self.rows {
            let _ = writeln!(
                s,
                "{:<22} {:>14} {:>5.1}%",
                r.name,
                format!("{:.1} {}", r.freebsd, r.unit),
                r.cv * 100.0
            );
        }
        s
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
            "\ngeometric mean ratio {geo:.3}; {} pass, {} win, {} slower, {} noisy, {} missing",
            count(Verdict::Pass),
            count(Verdict::Win),
            count(Verdict::Slower),
            count(Verdict::Noisy),
            count(Verdict::Missing)
        );
        let _ = writeln!(s, "\ncomponents (gate: geometric mean <= {BOUND:.2}):");
        for (c, n, geo, missing) in self.components() {
            let verdict = if missing {
                "FAIL (missing)"
            } else if geo > BOUND {
                "FAIL"
            } else {
                "pass"
            };
            let _ = writeln!(s, "{c:<22} {n:>3} benchmarks {geo:>7.3}  {verdict}");
        }
        s
    }
}

/// The component a benchmark measures (ADR-0014): its name's first part,
/// or for file systems its first two (`fs.ufs.create` is `fs.ufs`).
fn component(name: &str) -> &str {
    let mut ends = name.match_indices('.').map(|(i, _)| i);
    let first = ends.next().unwrap_or(name.len());
    if &name[..first] == "fs" {
        &name[..ends.next().unwrap_or(name.len())]
    } else {
        &name[..first]
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
    fn components_gate() {
        assert_eq!(component("fs.ufs.seq_read.warm"), "fs.ufs");
        assert_eq!(component("malloc.hit.t1"), "malloc");
        assert_eq!(component("solo"), "solo");
        let mut s = BTreeMap::new();
        let mut units = BTreeMap::new();
        let mut add = |name: &str, f: f64, a: f64| {
            let e: &mut BTreeMap<Side, Vec<f64>> = s.entry(name.to_owned()).or_default();
            e.insert(Side::FreeBsd, vec![f, f, f]);
            e.insert(Side::Abyss, vec![a, a, a]);
            units.insert(name.to_owned(), "ns".to_owned());
        };
        // One slow microbenchmark, the component well ahead: a pass.
        add("fs.x.slow", 100.0, 110.0);
        add("fs.x.fast", 100.0, 80.0);
        // Over overall: a failure.
        add("vm.a", 100.0, 105.0);
        add("vm.b", 100.0, 101.0);
        let r = Report::new(&s, &units);
        assert_eq!(r.failures(), 1);
        let c = r.components();
        assert_eq!(c[0].0, "fs.x");
        assert!(c[0].2 < 1.0 && c[1].2 > BOUND);
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
                ("slow", Verdict::Slower),
            ]
        );
        // One component (each name its own), "slow" over the bound.
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
