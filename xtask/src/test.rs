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
    args.finish()?;

    if matches!(tier, Tier::Unit | Tier::All) {
        println!("== unit tests (host)");
        util::run(util::cargo().args(["test", "--quiet"]))?;
    }
    if matches!(tier, Tier::Boot | Tier::All) {
        let mut failed = Vec::new();
        for case in BOOT_CASES.iter().filter(|c| arches.contains(&c.arch)) {
            if !case.vmm.available(case.arch) {
                println!(
                    "== boot tests ({}): skipped, needs a FreeBSD host",
                    case.name
                );
                continue;
            }
            println!("== boot tests ({}, {:?})", case.name, case.vmm);
            match boot(case, release, verbose)? {
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
                if !vmm.available(arch) {
                    if vmm == Vmm::Bhyve && arch == Arch::X86_64 {
                        println!("== boot tests ({name}): skipped, needs a FreeBSD host");
                    }
                    continue;
                }
                println!("== boot tests ({name}, {vmm:?})");
                match shell(arch, vmm, release)? {
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
        warm_reset: false,
        // One attach worker, INTx instead of MSI-X (through _PRT and the
        // root port's swizzle), and a real power-off at the end.
        cmdline: "abyss.bus.serial=1 hw.pci.enable_msix=0 abyss.test.poweroff=1",
        pass_status: 0,
        expect: Some("power: off (PSCI SYSTEM_OFF)"),
        expect_saved: None,
    },
    BootCase {
        name: "aarch64-warm-reset",
        vmm: Vmm::Qemu,
        arch: Arch::Aarch64,
        el2: false,
        warm_reset: true,
        cmdline: "",
        pass_status: 0,
        expect: None,
        expect_saved: None,
    },
    BootCase {
        name: "x86_64-warm-reset",
        vmm: Vmm::Qemu,
        arch: Arch::X86_64,
        el2: false,
        warm_reset: true,
        // A real power-off at the end: QEMU exits 0 on ACPI S5.
        cmdline: "abyss.test.poweroff=1",
        pass_status: 0,
        expect: Some("power: off (ACPI S5)"),
        expect_saved: None,
    },
    BootCase {
        name: "x86_64-watchdog",
        vmm: Vmm::Qemu,
        arch: Arch::X86_64,
        el2: false,
        warm_reset: true,
        // The first boot arms ichwd and hangs; its expiry must reset the
        // machine, and the second boot runs the tests.
        cmdline: "abyss.test.watchdog_reset=1",
        pass_status: 33,
        expect: None,
        expect_saved: Some("watchdog: armed for"),
    },
    BootCase {
        name: "x86_64-bhyve",
        vmm: Vmm::Bhyve,
        arch: Arch::X86_64,
        el2: false,
        warm_reset: false,
        // bhyve's status when the guest powers off.
        cmdline: "",
        pass_status: 1,
        expect: Some("power: off (ACPI S5)"),
        expect_saved: None,
    },
];

/// The line the first boot of a warm-reset case leaves in the log ring.
const WARM_RESET_MARKER: &str = "logring: first boot marker";

/// The boot-to-shell test (ROADMAP P4.7): a normal boot ends in
/// `/rescue/sh` on the serial console. The script `freebsd-shell` proves
/// against FreeBSD runs against it, and `exit` must power the machine off.
///
/// With `ufs_root`, the loader must find the kernel and the memory disk on
/// a UFS2 partition (the ESP holds neither).
fn shell(arch: Arch, vmm: Vmm, release: bool) -> Result<std::result::Result<(), String>> {
    let opts = build::Options {
        release,
        ktest: false,
    };
    // On FreeBSD (which has makefs) the root is UFS2 on an NVMe disk, all
    // of base.txz; elsewhere the memory disk's /rescue.
    let ufs_root = cfg!(target_os = "freebsd");
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
    let log = disk.with_file_name(format!("shell{suffix}.log"));
    let vm = Vm {
        smp: 4,
        nvme: ufs_root,
        ..Vm::new(vmm, arch, disk.clone())
    };
    let mut session = crate::expect::Session::start(vm, &log)?;
    let result = (|| -> Result<()> {
        session.expect("# ", Duration::from_secs(240))?;
        session.script(crate::expect::SHELL, Duration::from_secs(30))?;
        if ufs_root {
            session.script(crate::expect::UFS_ROOT, Duration::from_secs(600))?;
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
        Ok(()) if ufs_root => root_left_clean(&disk),
        Ok(()) => Ok(()),
    };
    if verdict.is_err() {
        println!("{arch}-shell{suffix}: serial log at {}", log.display());
    }
    Ok(verdict)
}

/// After a shell session on a UFS2 root and a power-off: the root was
/// unmounted cleanly (its superblock says so, which `dumpfs` shows) and
/// `fsck_ffs -f -n` finds nothing.
pub(crate) fn root_left_clean(disk: &std::path::Path) -> std::result::Result<(), String> {
    let part = image::ufs_partition(disk).map_err(|e| e.to_string())?;
    let root = disk.with_file_name("shell-root.img");
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

fn boot(case: &BootCase, release: bool, verbose: bool) -> Result<std::result::Result<(), String>> {
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
    let disk = image::build(
        case.arch,
        build::Options {
            release,
            ktest: true,
        },
        cmdline,
    )?;
    let disk = if case.warm_reset {
        // Its own copy: the guest writes to it.
        let copy = disk.with_file_name(format!("disk-{}.img", case.name));
        std::fs::copy(&disk, &copy)?;
        copy
    } else {
        disk
    };
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
        test_devices: true,
        allow_reset: case.warm_reset,
        extra_disk: Some(scratch),
        devices,
        ..Vm::new(case.vmm, case.arch, disk)
    };
    // Emulated aarch64 is slow to start and slow through the file-system
    // tests (soft updates and msdosfs alone take minutes under TCG); leave
    // generous headroom.
    let run = vm.run_captured(Duration::from_secs(600), &log, verbose)?;
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
