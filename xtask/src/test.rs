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
        for arch in arches {
            println!("== boot tests ({arch}, QEMU)");
            match boot(arch, release, verbose)? {
                Outcome::Passed => println!("{arch}: PASS"),
                other => {
                    println!("{arch}: {other:?}");
                    failed.push(arch);
                }
            }
        }
        if !failed.is_empty() {
            return Err(format!("boot tests failed on {failed:?}; logs in build/<arch>/").into());
        }
    }
    Ok(())
}

fn boot(arch: Arch, release: bool, verbose: bool) -> Result<Outcome> {
    let disk = image::build(
        arch,
        build::Options {
            release,
            ktest: true,
        },
    )?;
    let log = disk.with_extension("log");
    let vm = Vm {
        arch,
        disk,
        smp: 2,
        mem: "1G".into(),
        gdb: false,
        display: false,
    };
    // Emulated aarch64 firmware is slow to start; leave generous headroom.
    let outcome = vm.run_captured(Duration::from_secs(180), &log, verbose)?;
    if outcome != Outcome::Passed {
        println!("{arch}: serial log at {}", log.display());
    }
    Ok(outcome)
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
        "--all-targets",
        "--",
        "-D",
        "warnings",
    ]))?;
    for arch in Arch::ALL {
        println!("== clippy ({arch} loader)");
        for features in [
            &[][..],
            &["--features", "ktest", "--target-dir", "target/ktest"][..],
        ] {
            util::run(
                util::cargo()
                    .args(["clippy", "--quiet", "--package", "loader-efi"])
                    .args(["--target", arch.uefi_target()])
                    .args(features)
                    .args(["--", "-D", "warnings"]),
            )?;
        }
    }
    println!("== policy");
    policy::check()
}
