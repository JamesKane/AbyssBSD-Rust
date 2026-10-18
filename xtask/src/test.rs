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
    let cmdline = format!("abyss.qemu_exit=1 abyss.stress={duration}");
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
    let base = match (case.vmm, case.warm_reset) {
        (Vmm::Bhyve, _) => "",
        // QEMU runs can press the power button (QMP).
        (Vmm::Qemu, true) => "abyss.qemu_exit=1 abyss.test.warm_reset=1 abyss.test.power_button=1",
        (Vmm::Qemu, false) => "abyss.qemu_exit=1 abyss.test.power_button=1",
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
    let nvme = blank("nvme")?;
    let sata = blank("sata")?;
    let devices = match case.vmm {
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
    let vm = Vm {
        el2: case.el2,
        test_devices: true,
        allow_reset: case.warm_reset,
        extra_disk: Some(scratch),
        devices,
        ..Vm::new(case.vmm, case.arch, disk)
    };
    // Emulated aarch64 firmware is slow to start; leave generous headroom.
    let run = vm.run_captured(Duration::from_secs(240), &log, verbose)?;
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
