// SPDX-License-Identifier: BSD-2-Clause
//! Test tiers and the combined `check`. See docs/BUILD-AND-TEST.md §4.

use std::time::Duration;

use crate::arch::Arch;
use crate::build;
use crate::cli::Args;
use crate::image;
use crate::policy;
use crate::qemu::{Outcome, Vm, Vmm};
use crate::util::{self, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tier {
    Unit,
    Boot,
    Stress,
    All,
}

pub(crate) fn cmd(mut args: Args) -> Result<()> {
    let tier = match args.take_value("--tier")?.as_deref() {
        None | Some("all") => Tier::All,
        Some("unit") => Tier::Unit,
        Some("boot") => Tier::Boot,
        Some("stress") => Tier::Stress,
        Some(t) => return Err(format!("unknown tier `{t}` (unit, boot, stress, all)").into()),
    };
    let duration: u64 = args
        .take_value("--duration")?
        .map_or(Ok(600), |s| s.parse())
        .map_err(|_| "bad --duration (seconds)")?;
    let arches = Arch::from_args(&mut args)?;
    let release = args.take_flag("--release");
    let verbose = args.take_flag("--verbose");
    // The USB and input devices on every boot-tier machine (U.6);
    // `--no-usb` leaves them out, for a quicker run.
    let usb = !args.take_flag("--no-usb");
    // The shell cases without the user tests and FreeBSD's (the rest,
    // the differential scenarios included, still runs).
    let quick = args.take_flag("--quick");
    // One boot-tier case, by name (`x86_64-console`).
    let only = args.take_value("--case")?;
    // How many boot-tier cases run at once, each its own xtask process;
    // `--jobs 1` runs them one after another in this one.
    let jobs: usize = args
        .take_value("--jobs")?
        .map_or(Ok(4), |s| s.parse())
        .map_err(|_| "bad --jobs (a count)")?;
    let wanted = |name: &str| only.as_deref().is_none_or(|o| o == name);
    args.finish()?;

    if matches!(tier, Tier::Unit | Tier::All) {
        println!("== unit tests (host)");
        util::run(util::cargo().args(["test", "--quiet"]))?;
    }
    if matches!(tier, Tier::Boot | Tier::All) && only.is_none() && jobs > 1 {
        let mut flags = Vec::new();
        for (on, flag) in [
            (release, "--release"),
            (verbose, "--verbose"),
            (!usb, "--no-usb"),
            (quick, "--quick"),
        ] {
            if on {
                flags.push(flag);
            }
        }
        return side_by_side(&case_names(&arches), jobs, &flags);
    }
    if matches!(tier, Tier::Boot | Tier::All) {
        let mut failed = Vec::new();
        for case in BOOT_CASES
            .iter()
            .filter(|c| arches.contains(&c.arch) && wanted(c.name))
        {
            if !case.vmm.available(case.arch) {
                println!(
                    "== boot tests ({}): skipped, needs a FreeBSD host",
                    case.name
                );
                continue;
            }
            println!("== boot tests ({}, {:?})", case.name, case.vmm);
            match boot(case, release, verbose, usb)? {
                Ok(()) => println!("{}: PASS", case.name),
                Err(why) => {
                    println!("{}: FAIL: {why}", case.name);
                    failed.push(case.name);
                }
            }
        }
        for &arch in &arches {
            for vmm in [Vmm::Qemu, Vmm::Bhyve] {
                let name = match vmm {
                    Vmm::Qemu => format!("{arch}-shell"),
                    Vmm::Bhyve => format!("{arch}-bhyve-shell"),
                };
                if !wanted(&name) {
                    continue;
                }
                if !vmm.available(arch) {
                    if vmm == Vmm::Bhyve && arch == Arch::X86_64 {
                        println!("== boot tests ({name}): skipped, needs a FreeBSD host");
                    }
                    continue;
                }
                println!("== boot tests ({name}, {vmm:?})");
                match shell(arch, vmm, release, usb, quick)? {
                    Ok(()) => println!("{name}: PASS"),
                    Err(why) => {
                        println!("{name}: FAIL: {why}");
                        failed.push(if vmm == Vmm::Qemu {
                            "shell"
                        } else {
                            "bhyve-shell"
                        });
                    }
                }
            }
        }
        for &arch in &arches {
            let name = format!("{arch}-console");
            if !wanted(&name) {
                continue;
            }
            println!("== boot tests ({name}, Qemu)");
            match console(arch, release)? {
                Ok(()) => println!("{name}: PASS"),
                Err(why) => {
                    println!("{name}: FAIL: {why}");
                    failed.push("console");
                }
            }
        }
        // The multi-user boot (ROADMAP P6.0), expected to fail until P6.9.
        for &arch in &arches {
            for vmm in [Vmm::Qemu, Vmm::Bhyve] {
                let name = match vmm {
                    Vmm::Qemu => format!("{arch}-multiuser"),
                    Vmm::Bhyve => format!("{arch}-bhyve-multiuser"),
                };
                if !wanted(&name) || !vmm.available(arch) {
                    continue;
                }
                if !cfg!(target_os = "freebsd") {
                    println!("== boot tests ({name}): skipped, needs a FreeBSD host (makefs)");
                    continue;
                }
                println!("== boot tests ({name}, {vmm:?})");
                match multiuser(arch, vmm, release, usb)? {
                    Ok(()) => println!("{name}: PASS"),
                    Err(why) => {
                        println!("{name}: FAIL: {why}");
                        failed.push("multiuser");
                    }
                }
            }
        }
        // The networked boot (ROADMAP P7.0), passing since P7.6:
        // QEMU on user-mode networking, bhyve on the test bridge.
        for &arch in &arches {
            for vmm in [Vmm::Qemu, Vmm::Bhyve] {
                let name = match vmm {
                    Vmm::Qemu => format!("{arch}-net"),
                    Vmm::Bhyve => format!("{arch}-bhyve-net"),
                };
                if !wanted(&name) || !vmm.available(arch) {
                    continue;
                }
                if !cfg!(target_os = "freebsd") {
                    println!("== boot tests ({name}): skipped, needs a FreeBSD host (makefs)");
                    continue;
                }
                println!("== boot tests ({name}, {vmm:?})");
                match net(arch, vmm, release)? {
                    Ok(()) => println!("{name}: PASS"),
                    Err(why) => {
                        println!("{name}: FAIL: {why}");
                        failed.push("net");
                    }
                }
            }
        }
        if !failed.is_empty() {
            return Err(format!("boot tests failed on {failed:?}; logs in build/<arch>/").into());
        }
    }
    if tier == Tier::Stress {
        let mut failed = Vec::new();
        for &arch in &arches {
            println!("== stress test ({arch}, {duration} s, 8 CPUs)");
            match stress(arch, duration, release, verbose)? {
                Ok(()) => println!("stress-{arch}: PASS"),
                Err(why) => {
                    println!("stress-{arch}: FAIL: {why}");
                    failed.push(arch);
                }
            }
        }
        if !failed.is_empty() {
            return Err(format!("stress test failed on {failed:?}; logs in build/<arch>/").into());
        }
    }
    Ok(())
}

/// Every boot-tier case for `arches`, by the name `--case` takes, longest
/// first (by what an emulated or bhyve run of each has taken), so the
/// long ones start at once and the short ones fill in around them.
fn case_names(arches: &[Arch]) -> Vec<String> {
    let mut names: Vec<String> = BOOT_CASES
        .iter()
        .filter(|c| arches.contains(&c.arch))
        .map(|c| c.name.to_owned())
        .collect();
    for &arch in arches {
        for kind in ["shell", "console", "multiuser", "net"] {
            names.push(format!("{arch}-{kind}"));
            if kind != "console" && Vmm::Bhyve.available(arch) {
                names.push(format!("{arch}-bhyve-{kind}"));
            }
        }
    }
    // Minutes, roughly, from the CI logs of 2026-11-09.
    let minutes = |n: &str| match n {
        "aarch64-shell" => 25,
        "x86_64-shell" => 22,
        "x86_64-bhyve-shell" => 9,
        "aarch64" | "aarch64-el2" | "aarch64-smmu" | "aarch64-smmu-warm-reset" => 6,
        "x86_64" | "aarch64-net" | "aarch64-multiuser" => 5,
        n if n.ends_with("-net") || n.ends_with("-multiuser") => 4,
        _ => 2,
    };
    names.sort_by_key(|n| std::cmp::Reverse(minutes(n)));
    names
}

/// Runs the boot tier's `cases` `jobs` at a time, each as `xtask test
/// --tier boot --case NAME` (its own process, so its own bhyve names,
/// VNC ports, and QMP sockets; images are made under [`image::lock`]).
/// A case's output is printed whole when it ends.
fn side_by_side(cases: &[String], jobs: usize, flags: &[&str]) -> Result<()> {
    let exe = std::env::current_exe()?;
    let out_dir = util::root().join("build").join("boot-tier");
    std::fs::create_dir_all(&out_dir)?;
    let mut queue = cases.iter();
    let mut running: Vec<(
        &String,
        std::process::Child,
        std::path::PathBuf,
        std::time::Instant,
    )> = Vec::new();
    let mut failed = Vec::new();
    loop {
        while running.len() < jobs
            && let Some(name) = queue.next()
        {
            let out = out_dir.join(format!("{name}.out"));
            let file = std::fs::File::create(&out)?;
            let child = std::process::Command::new(&exe)
                .args(["test", "--tier", "boot", "--case", name])
                .args(flags)
                .stdin(std::process::Stdio::null())
                .stdout(file.try_clone()?)
                .stderr(file)
                .spawn()?;
            running.push((name, child, out, std::time::Instant::now()));
        }
        if running.is_empty() {
            break;
        }
        let mut i = 0;
        while i < running.len() {
            if let Some(status) = running[i].1.try_wait()? {
                let (name, _, out, start) = running.swap_remove(i);
                let text = std::fs::read_to_string(&out).unwrap_or_default();
                print!("{text}");
                println!("({name}: {} s)", start.elapsed().as_secs());
                if !status.success() {
                    failed.push(name.as_str());
                }
            } else {
                i += 1;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    if failed.is_empty() {
        Ok(())
    } else {
        Err(format!("boot tests failed on {failed:?}; logs in build/<arch>/").into())
    }
}

/// The Phase 2 stress test: 64 kernel threads on 8 CPUs for `duration`
/// seconds (see kernel/src/stress.rs).
fn stress(
    arch: Arch,
    duration: u64,
    release: bool,
    verbose: bool,
) -> Result<std::result::Result<(), String>> {
    let cmdline = format!("abyss.verifier=1 abyss.qemu_exit=1 abyss.stress={duration}");
    let disk = image::build(
        arch,
        build::Options {
            release,
            ktest: false,
        },
        &cmdline,
    )?;
    let disk = {
        let copy = disk.with_file_name("disk-stress.img");
        std::fs::copy(&disk, &copy)?;
        copy
    };
    let log = disk.with_file_name("stress.log");
    let vm = Vm {
        smp: 8,
        mem: "2G".into(),
        test_devices: true,
        ..Vm::new(Vmm::Qemu, arch, disk)
    };
    let run = vm.run_captured(Duration::from_secs(duration + 300), &log, verbose)?;
    let pass_status = match arch {
        Arch::X86_64 => 33,
        Arch::Aarch64 => 0,
    };
    let verdict = match run.outcome {
        Outcome::Passed if run.exit_code == Some(pass_status) => Ok(()),
        Outcome::Passed => Err(format!("QEMU exit status {:?}", run.exit_code)),
        other => Err(format!("{other:?}")),
    };
    if verdict.is_err() {
        println!("stress-{arch}: serial log at {}", log.display());
    }
    Ok(verdict)
}

/// One boot-test configuration.
#[derive(Debug)]
struct BootCase {
    name: &'static str,
    vmm: Vmm,
    arch: Arch,
    /// aarch64 firmware at EL2, exercising the kernel's EL2 exit.
    el2: bool,
    /// aarch64: an SMMUv3 in front of the PCIe root complex
    /// (docs/IOMMU.md §6).
    iommu: bool,
    /// Boot twice through a warm reset and check that the loader saved the
    /// first boot's log to the ESP.
    warm_reset: bool,
    /// Extra kernel command line options.
    cmdline: &'static str,
    /// QEMU's exit status when the kernel reports a pass.
    pass_status: i32,
    /// A line the log must contain (how the run powered off).
    expect: Option<&'static str>,
    /// A line the first boot's saved log must contain (warm-reset cases).
    expect_saved: Option<&'static str>,
}

const BOOT_CASES: &[BootCase] = &[
    BootCase {
        name: "x86_64",
        vmm: Vmm::Qemu,
        arch: Arch::X86_64,
        el2: false,
        iommu: false,
        warm_reset: false,
        cmdline: "",
        // isa-debug-exit: (0x10 << 1) | 1.
        pass_status: 33,
        expect: None,
        expect_saved: None,
    },
    BootCase {
        name: "aarch64",
        vmm: Vmm::Qemu,
        arch: Arch::Aarch64,
        el2: false,
        iommu: false,
        warm_reset: false,
        cmdline: "",
        pass_status: 0,
        expect: None,
        expect_saved: None,
    },
    BootCase {
        name: "aarch64-el2",
        vmm: Vmm::Qemu,
        arch: Arch::Aarch64,
        el2: true,
        iommu: false,
        warm_reset: false,
        // One attach worker, INTx instead of MSI-X (through _PRT and the
        // root port's swizzle), and a real power-off at the end.
        cmdline: "abyss.bus.serial=1 hw.pci.enable_msix=0 abyss.test.poweroff=1",
        pass_status: 0,
        expect: Some("power: off (PSCI SYSTEM_OFF)"),
        expect_saved: None,
    },
    BootCase {
        name: "aarch64-smmu",
        vmm: Vmm::Qemu,
        arch: Arch::Aarch64,
        el2: false,
        iommu: true,
        warm_reset: false,
        // The SMMU translating (docs/IOMMU.md §8, stage 3): every device's
        // DMA through its own domain, the edu device's stream (which no
        // driver takes) aborting until a test gives it a context. On
        // INTx until MSI goes through the SMMU (stage 4); the user-memory
        // tests are the plain case's.
        cmdline: "hw.pci.enable_msix=0 abyss.test.skip=kernel::tests::user_memory",
        pass_status: 0,
        expect: Some("smmu0: <ARM System Memory Management Unit> on acpi0"),
        expect_saved: None,
    },
    BootCase {
        name: "aarch64-warm-reset",
        vmm: Vmm::Qemu,
        arch: Arch::Aarch64,
        el2: false,
        iommu: false,
        warm_reset: true,
        // The reset is what this case adds: the user-memory tests (most of
        // a run's time) are the plain case's.
        cmdline: "abyss.test.skip=kernel::tests::user_memory",
        pass_status: 0,
        expect: None,
        expect_saved: None,
    },
    BootCase {
        name: "aarch64-smmu-warm-reset",
        vmm: Vmm::Qemu,
        arch: Arch::Aarch64,
        el2: false,
        iommu: true,
        warm_reset: true,
        // The first boot shuts its devices down before the reset, as a
        // reboot does, so the SMMU is quiesced (docs/IOMMU.md §4.11); the
        // second runs the tests with it enabled again.
        cmdline: "abyss.test.orderly_reset=1 abyss.test.skip=kernel::tests::user_memory",
        pass_status: 0,
        expect: Some("smmu0: <ARM System Memory Management Unit> on acpi0"),
        expect_saved: Some("logring: devices shut down before the reset"),
    },
    BootCase {
        name: "x86_64-warm-reset",
        vmm: Vmm::Qemu,
        arch: Arch::X86_64,
        el2: false,
        iommu: false,
        warm_reset: true,
        // A real power-off at the end: QEMU exits 0 on ACPI S5.
        cmdline: "abyss.test.poweroff=1 abyss.test.skip=kernel::tests::user_memory",
        pass_status: 0,
        expect: Some("power: off (ACPI S5)"),
        expect_saved: None,
    },
    BootCase {
        name: "x86_64-watchdog",
        vmm: Vmm::Qemu,
        arch: Arch::X86_64,
        el2: false,
        iommu: false,
        warm_reset: true,
        // The first boot runs watchdogd on /dev/fido (ichwd), which keeps
        // it patted past its timeout, then kills it; the watchdog's expiry
        // must reset the machine, and the second boot runs the tests.
        cmdline: "abyss.test.watchdog_reset=1 abyss.test.skip=kernel::tests::user_memory",
        pass_status: 33,
        expect: None,
        expect_saved: Some("watchdog: patting stopped (watchdogd killed)"),
    },
    BootCase {
        name: "x86_64-bhyve",
        vmm: Vmm::Bhyve,
        arch: Arch::X86_64,
        el2: false,
        iommu: false,
        warm_reset: false,
        // bhyve's status when the guest powers off.
        cmdline: "",
        pass_status: 1,
        expect: Some("power: off (ACPI S5)"),
        expect_saved: None,
    },
];

/// The multi-user boot (docs/MULTIUSER.md §14): the installed root boots
/// to `login:`, and [`crate::multiuser::login_script`] logs in on `ttyv0`
/// (QEMU) and the serial console, checks the daemons, and shuts down;
/// the root must be left clean, and the console may hold no first use of
/// an interface outside `tests/multiuser/absent.txt`. The first uses go
/// to `build/<arch>/multiuser<suffix>-first-use.txt` either way.
fn multiuser(
    arch: Arch,
    vmm: Vmm,
    release: bool,
    usb: bool,
) -> Result<std::result::Result<(), String>> {
    let opts = build::Options {
        release,
        ktest: false,
    };
    let lock = image::lock()?;
    let disk = image::build_installed_root(arch, opts, "boot_multicons=YES boot_serial=YES")?;
    let suffix = match vmm {
        Vmm::Bhyve => "-bhyve",
        Vmm::Qemu => "",
    };
    let disk = {
        let copy = disk.with_file_name(format!("disk-multiuser{suffix}.img"));
        std::fs::copy(&disk, &copy)?;
        copy
    };
    drop(lock);
    let log = disk.with_file_name(format!("multiuser{suffix}.log"));
    let sock = disk.with_file_name(format!("multiuser{suffix}.qmp"));
    let shot = disk.with_file_name(format!("multiuser{suffix}.ppm"));
    let qemu = vmm == Vmm::Qemu;
    let vm = Vm {
        smp: 4,
        mem: "2G".into(),
        nvme: true,
        usb: usb || qemu,
        qmp: qemu.then(|| sock.clone()),
        ..Vm::new(vmm, arch, disk.clone())
    };
    let session = crate::expect::Session::start(vm, &log)?;
    let screen = qemu.then_some(crate::multiuser::Screen {
        qmp: &sock,
        shot: &shot,
    });
    // The kernel says when it starts a shell instead of init.
    let (result, exited) = crate::multiuser::login_script(
        session,
        screen.as_ref(),
        Duration::from_secs(600),
        &["panic:", "(no init)"],
    );
    let console = std::fs::read_to_string(&log).unwrap_or_default();
    let first = crate::multiuser::unexpected_first_uses(&console)?;
    let list = disk.with_file_name(format!("multiuser{suffix}-first-use.txt"));
    std::fs::write(&list, first.join("\n") + "\n")?;
    let verdict = match (result, exited) {
        (Err(e), _) | (_, Err(e)) => Err(e.to_string()),
        (Ok(()), Ok(false)) => Err("the machine did not power off".into()),
        (Ok(()), Ok(true)) if !first.is_empty() => Err(format!(
            "{} first uses outside absent.txt (in {})",
            first.len(),
            list.display()
        )),
        (Ok(()), Ok(true)) => root_left_clean(&disk),
    };
    if verdict.is_err() {
        println!(
            "{arch}{suffix}-multiuser: serial log at {}; {} first uses in {}",
            log.display(),
            first.len(),
            list.display()
        );
    }
    Ok(verdict)
}

/// The networked boot (docs/NETWORKING.md §16.4): the networked root boots
/// on QEMU's user-mode network, and [`crate::net::net_script`] checks the
/// addresses, routes, `ping`, DNS, `fetch`, and `pkg`, then logs in with
/// `ssh` and copies with `scp` from the host, and shuts down.
fn net(arch: Arch, vmm: Vmm, release: bool) -> Result<std::result::Result<(), String>> {
    let opts = build::Options {
        release,
        ktest: false,
    };
    let suffix = match vmm {
        Vmm::Bhyve => "-bhyve",
        Vmm::Qemu => "",
    };
    let lock = image::lock()?;
    let disk = image::build_net_root(arch, opts, "boot_multicons=YES boot_serial=YES")?;
    // Its own copy: the guest writes to it, and the other VMM's case may
    // run beside it.
    let disk = {
        // Not `disk-net.img` itself, which the build writes.
        let vmm_name = match vmm {
            Vmm::Bhyve => "bhyve",
            Vmm::Qemu => "qemu",
        };
        let copy = disk.with_file_name(format!("disk-net-{vmm_name}.img"));
        std::fs::copy(&disk, &copy)?;
        copy
    };
    drop(lock);
    let log = disk.with_file_name(format!("net{suffix}.log"));
    let (vm, lab) = crate::net::networked(Vm {
        smp: 4,
        mem: "2G".into(),
        nvme: true,
        ..Vm::new(vmm, arch, disk.clone())
    })?;
    let session = crate::expect::Session::start(vm, &log)?;
    // The ssh key and the copies, apart from the other VMM's case.
    let dir = disk.with_file_name(format!("net{suffix}"));
    std::fs::create_dir_all(&dir)?;
    let (result, exited) = crate::net::net_script(
        session,
        &lab,
        &log,
        &dir,
        Duration::from_secs(600),
        &["panic:", "(no init)"],
    );
    let verdict = match (result, exited) {
        (Err(e), _) | (_, Err(e)) => Err(e.to_string()),
        (Ok(()), Ok(false)) => Err("the machine did not power off".into()),
        (Ok(()), Ok(true)) => Ok(()),
    };
    if verdict.is_err() {
        println!("{arch}{suffix}-net: serial log at {}", log.display());
    }
    Ok(verdict)
}

/// The line the first boot of a warm-reset case leaves in the log ring.
const WARM_RESET_MARKER: &str = "logring: first boot marker";

/// The boot-to-shell test (ROADMAP P4.7): a normal boot ends in
/// `/rescue/sh` on the serial console. The script `freebsd-shell` proves
/// against FreeBSD runs against it, and `exit` must power the machine off.
///
/// With `ufs_root`, the loader must find the kernel and the memory disk on
/// a UFS2 partition (the ESP holds neither).
fn shell(
    arch: Arch,
    vmm: Vmm,
    release: bool,
    usb: bool,
    quick: bool,
) -> Result<std::result::Result<(), String>> {
    let opts = build::Options {
        release,
        ktest: false,
    };
    // On FreeBSD (which has makefs) the root is UFS2 on an NVMe disk, all
    // of base.txz; elsewhere the memory disk's /rescue.
    let ufs_root = cfg!(target_os = "freebsd");
    let lock = image::lock()?;
    let disk = if ufs_root {
        image::build_ufs_root(arch, opts, "abyss.verifier=1")?
    } else {
        image::build(arch, opts, "abyss.verifier=1")?
    };
    let suffix = match vmm {
        Vmm::Bhyve => "-bhyve",
        Vmm::Qemu => "",
    };
    let disk = {
        let copy = disk.with_file_name(format!("disk-shell{suffix}.img"));
        std::fs::copy(&disk, &copy)?;
        copy
    };
    drop(lock);
    let log = disk.with_file_name(format!("shell{suffix}.log"));
    let vm = Vm {
        smp: 4,
        // As FreeBSD's shell VM (`freebsd-shell`): tests such as
        // `unix_seqpacket_test`'s `random_eor_and_waitall` take 100 MiB.
        mem: "2G".into(),
        nvme: ufs_root,
        usb,
        ..Vm::new(vmm, arch, disk.clone())
    };
    let mut session = crate::expect::Session::start(vm, &log)?;
    let result = (|| -> Result<()> {
        session.expect("# ", Duration::from_secs(240))?;
        session.script(crate::expect::SHELL, Duration::from_secs(30))?;
        if ufs_root {
            if let Some(dirs) = image::atf_only() {
                println!("{arch}-shell{suffix}: ABYSS_ATF: only {}", dirs.join(" "));
                session.script(crate::expect::ATF_SETUP, Duration::from_secs(60))?;
                session.script(crate::expect::FREEBSD_TESTS, Duration::from_secs(3600))?;
            } else {
                // Quick: the root reloaded and made writable, without `mtree`'s checks.
                let ufs = if quick {
                    &crate::expect::UFS_ROOT[..5]
                } else {
                    crate::expect::UFS_ROOT
                };
                session.script(ufs, Duration::from_secs(600))?;
                session.script(crate::expect::DYNAMIC, Duration::from_secs(60))?;
                session.script(&crate::diff::steps(), Duration::from_secs(900))?;
                if !quick {
                    session.script(crate::expect::DYNAMIC_TESTS, Duration::from_secs(900))?;
                    session.script(crate::expect::FREEBSD_TESTS, Duration::from_secs(3600))?;
                }
            }
        }
        session.send("exit\n")
    })();
    let exited = session.finish(Duration::from_secs(120))?;
    let console = std::fs::read_to_string(&log).unwrap_or_default();
    let verdict = match result {
        Err(e) => Err(e.to_string()),
        Ok(()) if !exited => Err("the machine did not power off after `exit`".into()),
        Ok(()) if ufs_root && !console.contains("loader: kernel from UFS2") => {
            Err("the kernel did not come from UFS2".into())
        }
        Ok(()) if ufs_root && !console.contains("Trying to mount root from ufs:") => {
            Err("the root was not UFS2".into())
        }
        Ok(()) if ufs_root && image::atf_only().is_some() => crate::expect::atf_verdict(&console)
            .map(|n| println!("{arch}-shell{suffix}: {n} of FreeBSD's test cases as on FreeBSD"))
            .map_err(|e| e.to_string()),
        Ok(()) if ufs_root => crate::diff::verdict(arch, &console)
            .map(|n| println!("{arch}-shell{suffix}: {n} differential scenarios as on FreeBSD"))
            .and_then(|()| {
                if quick {
                    return Ok(());
                }
                crate::expect::atf_verdict(&console).map(|n| {
                    println!("{arch}-shell{suffix}: {n} of FreeBSD's test cases as on FreeBSD");
                })
            })
            .map_err(|e| e.to_string())
            .and_then(|()| root_left_clean(&disk)),
        Ok(()) => Ok(()),
    };
    if verdict.is_err() {
        println!("{arch}-shell{suffix}: serial log at {}", log.display());
    }
    Ok(verdict)
}

/// The track's QEMU exit (ROADMAP U.6): `/rescue/sh` on `ttyv0` (the
/// video console primary, `boot_multicons`), driven entirely by the USB
/// keyboard over QMP and read back from the screen, with the virtio
/// keyboard and the USB mouse (its evdev node, found by name) working
/// too; `exit` typed on the USB keyboard powers the machine off.
fn console(arch: Arch, release: bool) -> Result<std::result::Result<(), String>> {
    let opts = build::Options {
        release,
        ktest: false,
    };
    let lock = image::lock()?;
    let disk = image::build(arch, opts, "abyss.verifier=1 boot_multicons=YES")?;
    let disk = {
        let copy = disk.with_file_name("disk-console.img");
        std::fs::copy(&disk, &copy)?;
        copy
    };
    drop(lock);
    let log = disk.with_file_name("console.log");
    let shot = disk.with_file_name("console.ppm");
    let sock = disk.with_file_name("console.qmp");
    let vm = Vm {
        smp: 2,
        usb: true,
        qmp: Some(sock.clone()),
        ..Vm::new(Vmm::Qemu, arch, disk)
    };
    let mut session = crate::expect::Session::start(vm, &log)?;
    let mut screen = Vec::new();
    let result = (|| -> Result<()> {
        session.expect(
            "Dual Console: Video Primary, Serial Secondary",
            Duration::from_secs(300),
        )?;
        let mut q = crate::qmp::Qmp::connect(&sock)?;
        // Until a line of the screen is `want` (or starts with it, for a
        // `prefix`), doing `poke` between looks.
        let mut until = |q: &mut crate::qmp::Qmp,
                         want: &str,
                         prefix: bool,
                         poke: &mut dyn FnMut(&mut crate::qmp::Qmp) -> Result<()>|
         -> Result<String> {
            let deadline = std::time::Instant::now() + Duration::from_secs(90);
            loop {
                q.screendump(&shot)?;
                screen = crate::qmp::Ppm::parse(&std::fs::read(&shot)?)?.text();
                let hit = screen.iter().find(|l| {
                    if prefix {
                        l.starts_with(want) && l.len() > want.len()
                    } else {
                        *l == want
                    }
                });
                if let Some(l) = hit {
                    return Ok(l.clone());
                }
                if std::time::Instant::now() > deadline {
                    return Err(format!("{want:?} never appeared on the screen").into());
                }
                poke(q)?;
                std::thread::sleep(Duration::from_millis(500));
            }
        };
        let mut idle = |_: &mut crate::qmp::Qmp| Ok(());
        // A prompt, once the shell runs and the keyboard is attached (the
        // first one scrolls away under the USB devices' messages, as on
        // FreeBSD with a memory-disk root, which waits for no bus).
        let mut looks = 0;
        until(&mut q, "#", false, &mut |q| {
            looks += 1;
            if looks % 6 == 0 {
                q.type_text("\n", None)?;
            }
            Ok(())
        })?;
        q.type_text("echo USB-$((6*7))\n", None)?;
        until(&mut q, "USB-42", false, &mut idle)?;
        q.type_text("echo VIRTIO-$((6*7))\n", Some("video0"))?;
        until(&mut q, "VIRTIO-42", false, &mut idle)?;
        q.type_text(
            "for n in 0 1 2 3 4 5 6 7 8 9; do \
             [ \"$(/rescue/sysctl -n kern.evdev.input.$n.name 2>/dev/null)\" = \
             \"QEMU QEMU USB Mouse\" ] && m=$n; done; echo MOUSE=$m\n",
            None,
        )?;
        until(&mut q, "MOUSE=", true, &mut idle)?;
        q.type_text(
            "/rescue/dd if=/dev/input/event$m of=/dev/null bs=24 count=4 2>/dev/null \
             && echo MOUSE-OK\n",
            None,
        )?;
        until(&mut q, "MOUSE-OK", false, &mut |q| {
            q.move_pointer(5, 5, None)
        })?;
        q.type_text("exit\n", None)?;
        Ok(())
    })();
    let exited = session.finish(Duration::from_secs(120))?;
    let verdict = match result {
        Err(e) => Err(e.to_string()),
        Ok(()) if !exited => Err("the machine did not power off after `exit`".into()),
        Ok(()) => Ok(()),
    };
    if verdict.is_err() {
        let text: Vec<&str> = screen
            .iter()
            .map(String::as_str)
            .filter(|l| !l.is_empty())
            .collect();
        println!(
            "{arch}-console: serial log at {}; the last screen:\n{}",
            log.display(),
            text.join("\n")
        );
    }
    Ok(verdict)
}

/// After a shell session on a UFS2 root and a power-off: the root was
/// unmounted cleanly (its superblock says so, which `dumpfs` shows) and
/// `fsck_ffs -f -n` finds nothing.
pub(crate) fn root_left_clean(disk: &std::path::Path) -> std::result::Result<(), String> {
    let part = image::ufs_partition(disk).map_err(|e| e.to_string())?;
    let stem = disk.file_stem().unwrap_or_default().to_string_lossy();
    let root = disk.with_file_name(format!("{stem}-root.img"));
    let copy = || -> Result<()> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(disk)?;
        f.seek(SeekFrom::Start(part.start))?;
        let mut out = std::fs::File::create(&root)?;
        std::io::copy(&mut f.take(part.end - part.start), &mut out)?;
        Ok(())
    };
    copy().map_err(|e| e.to_string())?;
    let dumpfs = std::process::Command::new("dumpfs")
        .arg("-s")
        .arg(&root)
        .output()
        .map_err(|e| e.to_string())?;
    let sb = String::from_utf8_lossy(&dumpfs.stdout);
    let clean = sb
        .split_ascii_whitespace()
        .skip_while(|w| *w != "clean")
        .nth(1)
        .is_some_and(|v| v == "1");
    let fsck = crate::fs::check_ufs_if_any(&root);
    let _ = std::fs::remove_file(&root);
    if !clean {
        return Err("the root was not left clean (dumpfs: clean 0)".into());
    }
    fsck
}

/// Lines the kernel's deliberate fatal fault must produce: a decoded page
/// fault, a register dump, and a symbolized backtrace through the test.
const FAULT_REPORT: &[&str] = &[
    "fatal trap:",
    "page fault at",
    "unmapped read in kernel mode",
    "backtrace:",
    "kernel::tests::deliberate_fault",
    "ktest: deliberate fatal fault reported",
];

fn boot(
    case: &BootCase,
    release: bool,
    verbose: bool,
    usb: bool,
) -> Result<std::result::Result<(), String>> {
    // bhyve has no exit device: the kernel powers off after a pass.
    // CI runs with the verifier on (DRIVER-MODEL.md §13.1).
    let base = match (case.vmm, case.warm_reset) {
        (Vmm::Bhyve, _) => "abyss.verifier=1",
        // QEMU runs can press the power button (QMP).
        (Vmm::Qemu, true) => {
            "abyss.verifier=1 abyss.qemu_exit=1 abyss.test.warm_reset=1 abyss.test.power_button=1"
        }
        (Vmm::Qemu, false) => "abyss.verifier=1 abyss.qemu_exit=1 abyss.test.power_button=1",
    };
    let cmdline = format!("{base} {}", case.cmdline);
    let cmdline = cmdline.trim();
    let lock = image::lock()?;
    let disk = image::build(
        case.arch,
        build::Options {
            release,
            ktest: true,
        },
        cmdline,
    )?;
    // Its own copy: the guest may write to it, and other cases (with
    // other command lines) build theirs beside it.
    let disk = {
        let copy = disk.with_file_name(format!("disk-{}.img", case.name));
        std::fs::copy(&disk, &copy)?;
        copy
    };
    drop(lock);
    let log = disk.with_file_name(format!("boot-{}.log", case.name));
    // Blank 16 MiB disks the storage tests may write: virtio-blk (vtbd1)
    // and NVMe (nda0), in the bhyve slots FreeBSD's recording uses.
    let blank = |what: &str| -> Result<std::path::PathBuf> {
        let p = disk.with_file_name(format!("{what}-{}.img", case.name));
        std::fs::File::create(&p)?.set_len(16 << 20)?;
        Ok(p)
    };
    let scratch = blank("scratch")?;
    // The UFS2 read-write test's own disk (32 MiB, which tells it apart),
    // which no other test writes.
    let ufsrw = disk.with_file_name(format!("ufsrw-{}.img", case.name));
    std::fs::File::create(&ufsrw)?.set_len(32 << 20)?;
    let nvme = blank("nvme")?;
    let sata = blank("sata")?;
    // The UFS2 test disk, read-only (vtbd2), if fetch-base made it.
    let ufs = crate::base::ufs_test_file("ufs-test.img");
    let mut devices = match case.vmm {
        Vmm::Bhyve => vec![
            format!("5,nvme,{}", nvme.display()),
            format!("6,ahci-hd,{}", sata.display()),
        ],
        Vmm::Qemu => {
            let mut d = vec![
                "-drive".into(),
                format!("if=none,id=nvm0,format=raw,file={}", nvme.display()),
                "-device".into(),
                "nvme,serial=abyss0,drive=nvm0".into(),
                "-drive".into(),
                format!("if=none,id=sata0,format=raw,file={}", sata.display()),
            ];
            // q35 has an ICH9 AHCI controller built in; virt needs one.
            let bus = match case.arch {
                Arch::X86_64 => "ide.0",
                Arch::Aarch64 => {
                    d.extend(["-device".into(), "ahci,id=ahci0".into()]);
                    "ahci0.0"
                }
            };
            d.extend(["-device".into(), format!("ide-hd,drive=sata0,bus={bus}")]);
            // Behind the SMMU, an idle device whose DMA a test drives:
            // QEMU's `edu`, reaching all of RAM (its default mask is 28
            // bits, below virt's RAM).
            if case.iommu {
                d.extend(["-device".into(), "edu,dma_mask=0xffffffffffffffff".into()]);
            }
            d
        }
    };
    // The same disk on NVMe (nda1) and AHCI (ada1) too. QEMU's IDE
    // disks cannot be read-only: a snapshot drops any write instead.
    if let Some(img) = &ufs {
        let img = img.display();
        match case.vmm {
            Vmm::Bhyve => devices.extend([
                format!("7,virtio-blk,{img},ro"),
                format!("8,nvme,{img},ro"),
                format!("9,ahci-hd,{img},ro"),
                format!("10,virtio-blk,{}", ufsrw.display()),
            ]),
            Vmm::Qemu => {
                let ahci_port = match case.arch {
                    Arch::X86_64 => "ide.1",
                    Arch::Aarch64 => "ahci0.1",
                };
                devices.extend([
                    "-drive".into(),
                    format!("if=none,id=ufs0,format=raw,readonly=on,file={img}"),
                    "-device".into(),
                    "virtio-blk-pci,drive=ufs0".into(),
                    "-drive".into(),
                    format!("if=none,id=ufs1,format=raw,readonly=on,file={img}"),
                    "-device".into(),
                    "nvme,serial=abyss1,drive=ufs1".into(),
                    "-drive".into(),
                    format!("if=none,id=ufs2,format=raw,snapshot=on,file={img}"),
                    "-device".into(),
                    format!("ide-hd,drive=ufs2,bus={ahci_port}"),
                    "-drive".into(),
                    format!("if=none,id=ufsrw,format=raw,file={}", ufsrw.display()),
                    "-device".into(),
                    "virtio-blk-pci,drive=ufsrw".into(),
                ]);
            }
        }
    }
    // The FAT test disk, read-only (FreeBSD's listings of it are on the
    // memory disk).
    if let Some(img) = crate::fattest::file("fat-test.img") {
        let img = img.display();
        match case.vmm {
            Vmm::Bhyve => devices.push(format!("11,virtio-blk,{img},ro")),
            Vmm::Qemu => devices.extend([
                "-drive".into(),
                format!("if=none,id=fat0,format=raw,readonly=on,file={img}"),
                "-device".into(),
                "virtio-blk-pci,drive=fat0".into(),
            ]),
        }
    }
    let vm = Vm {
        el2: case.el2,
        iommu: case.iommu,
        test_devices: true,
        allow_reset: case.warm_reset,
        extra_disk: Some(scratch),
        devices,
        usb,
        // QEMU's user-mode network, for the network test (`vtnet0`).
        net: (case.vmm == Vmm::Qemu).then(|| crate::qemu::Net::User {
            forwards: Vec::new(),
        }),
        ..Vm::new(case.vmm, case.arch, disk)
    };
    // Emulated aarch64 is slow to start and slow through the file-system
    // tests (soft updates and msdosfs alone take minutes under TCG); leave
    // generous headroom. The USB set's disk tests add a few minutes more.
    let secs = if usb { 900 } else { 600 };
    let run = vm.run_captured(Duration::from_secs(secs), &log, verbose)?;
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    let verdict = match run.outcome {
        Outcome::Passed => check_log(&text, case)
            .and_then(|()| check_saved_log(&vm, case))
            .and_then(|()| match run.exit_code {
                Some(code) if code == case.pass_status => Ok(()),
                other => Err(format!(
                    "QEMU exit status {other:?}, expected {}",
                    case.pass_status
                )),
            }),
        other => Err(format!("{other:?}")),
    };
    // The kernel test left a UFS2 file system on its disk: on a FreeBSD
    // host, FreeBSD's own fsck_ffs must find it clean too.
    let verdict = verdict.and_then(|()| crate::fs::check_ufs_if_any(&ufsrw));
    if verdict.is_err() {
        println!("{}: serial log at {}", case.name, log.display());
    }
    Ok(verdict)
}

/// For a warm-reset case: the ESP holds the first boot's log.
fn check_saved_log(vm: &Vm, case: &BootCase) -> std::result::Result<(), String> {
    if !case.warm_reset {
        return Ok(());
    }
    let saved =
        image::read_esp_file(&vm.disk, "EFI/ABYSS/PREVLOG.TXT").map_err(|e| e.to_string())?;
    let saved = String::from_utf8_lossy(&saved);
    if let Some(line) = case.expect_saved
        && !saved.contains(line)
    {
        return Err(format!("PREVLOG.TXT lacks {line:?}"));
    }
    if saved.contains(WARM_RESET_MARKER) && saved.contains("AbyssBSD") {
        Ok(())
    } else {
        Err(format!(
            "PREVLOG.TXT lacks the first boot's log ({} bytes)",
            saved.len()
        ))
    }
}

fn check_log(text: &str, case: &BootCase) -> std::result::Result<(), String> {
    let mut missing: Vec<&str> = FAULT_REPORT
        .iter()
        .copied()
        .filter(|l| !text.contains(l))
        .collect();
    if case.el2 && !text.contains("entered at EL2") {
        missing.push("entered at EL2");
    }
    if case.warm_reset && !text.contains("logring: loader recovered") {
        missing.push("logring: loader recovered");
    }
    if let Some(line) = case.expect
        && !text.contains(line)
    {
        missing.push(line);
    }
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("log lacks {missing:?}"))
    }
}

pub(crate) fn cmd_check(args: Args) -> Result<()> {
    args.finish()?;
    println!("== rustfmt");
    util::run(util::cargo().args(["fmt", "--all", "--check"]))?;
    println!("== clippy (host crates)");
    util::run(util::cargo().args([
        "clippy",
        "--quiet",
        "--workspace",
        "--exclude",
        "loader-efi",
        "--exclude",
        "kernel",
        "--exclude",
        "arch-x86_64",
        "--exclude",
        "arch-aarch64",
        "--all-targets",
        "--",
        "-D",
        "warnings",
    ]))?;
    for arch in Arch::ALL {
        let arch_crate = match arch {
            Arch::X86_64 => "arch-x86_64",
            Arch::Aarch64 => "arch-aarch64",
        };
        for (what, target, packages) in [
            ("loader", arch.uefi_target(), &["loader-efi"][..]),
            ("kernel", arch.kernel_target(), &["kernel", arch_crate][..]),
        ] {
            println!("== clippy ({arch} {what})");
            for features in [
                &[][..],
                &["--features", "ktest", "--target-dir", "target/ktest"][..],
            ] {
                let mut cargo = util::cargo();
                cargo.args(["clippy", "--quiet", "--target", target]);
                for p in packages {
                    cargo.args(["--package", p]);
                }
                util::run(cargo.args(features).args(["--", "-D", "warnings"]))?;
            }
        }
    }
    // DRIVER-MODEL.md §7.2: two drivers tied on a device with neither
    // yielding is a build error.
    println!("== driver table");
    util::run(util::cargo().args(["test", "--quiet", "--package", "driver-table"]))?;
    println!("== policy");
    policy::check()
}
