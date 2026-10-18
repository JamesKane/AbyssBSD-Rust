// SPDX-License-Identifier: BSD-2-Clause
//! Test tiers and the combined `check`. See docs/BUILD-AND-TEST.md §4.

use std::time::Duration;

use crate::arch::Arch;
use crate::build;
use crate::cli::Args;
use crate::image;
use crate::policy;
use crate::qemu::{Outcome, Vm};
use crate::util::{self, Result};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tier {
    Unit,
    Boot,
    All,
}

pub(crate) fn cmd(mut args: Args) -> Result<()> {
    let tier = match args.take_value("--tier")?.as_deref() {
        None | Some("all") => Tier::All,
        Some("unit") => Tier::Unit,
        Some("boot") => Tier::Boot,
        Some(t) => return Err(format!("unknown tier `{t}` (unit, boot, all)").into()),
    };
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
            println!("== boot tests ({}, QEMU)", case.name);
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
    Ok(())
}

/// One boot-test configuration.
#[derive(Debug)]
struct BootCase {
    name: &'static str,
    arch: Arch,
    /// aarch64 firmware at EL2, exercising the kernel's EL2 exit.
    el2: bool,
    /// QEMU's exit status when the kernel reports a pass.
    pass_status: i32,
}

const BOOT_CASES: &[BootCase] = &[
    BootCase {
        name: "x86_64",
        arch: Arch::X86_64,
        el2: false,
        // isa-debug-exit: (0x10 << 1) | 1.
        pass_status: 33,
    },
    BootCase {
        name: "aarch64",
        arch: Arch::Aarch64,
        el2: false,
        pass_status: 0,
    },
    BootCase {
        name: "aarch64-el2",
        arch: Arch::Aarch64,
        el2: true,
        pass_status: 0,
    },
];

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
    let disk = image::build(
        case.arch,
        build::Options {
            release,
            ktest: true,
        },
        "abyss.qemu_exit=1",
    )?;
    let log = disk.with_file_name(format!("boot-{}.log", case.name));
    let vm = Vm {
        arch: case.arch,
        disk,
        smp: 2,
        mem: "1G".into(),
        gdb: false,
        display: false,
        el2: case.el2,
        test_devices: true,
    };
    // Emulated aarch64 firmware is slow to start; leave generous headroom.
    let run = vm.run_captured(Duration::from_secs(240), &log, verbose)?;
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    let verdict = match run.outcome {
        Outcome::Passed => check_log(&text, case).and_then(|()| match run.exit_code {
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

fn check_log(text: &str, case: &BootCase) -> std::result::Result<(), String> {
    let mut missing: Vec<&str> = FAULT_REPORT
        .iter()
        .copied()
        .filter(|l| !text.contains(l))
        .collect();
    if case.el2 && !text.contains("entered at EL2") {
        missing.push("entered at EL2");
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
    println!("== policy");
    policy::check()
}
