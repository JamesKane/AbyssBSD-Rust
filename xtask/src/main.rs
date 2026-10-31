// SPDX-License-Identifier: BSD-2-Clause
//! `cargo xtask`: the single entry point for building, imaging, running and
//! testing AbyssBSD. See docs/BUILD-AND-TEST.md.

mod abicheck;
mod acpidump;
mod arch;
mod base;
mod build;
mod cli;
mod crash;
mod diff;
mod expect;
mod fattest;
mod fs;
mod image;
mod kyua;
mod multiuser;
mod net;
mod netdiff;
mod perf;
mod pjdfstest;
mod policy;
mod qemu;
mod qmp;
mod remote;
mod syscalls;
mod test;
mod ufsverify;
mod usertests;
mod util;

use std::process::ExitCode;

use cli::Args;
use util::Result;

const USAGE: &str = "\
usage: cargo xtask [--remote] <command> [options]

commands:
  build       --arch <a|all> [--release] [--ktest]       build the loader
  image       --arch <a> [--release] [--ktest]           build a GPT disk image with an ESP
  run         --arch <a> [--release] [--ktest] [--smp N] [--mem SIZE] [--gdb] [--display] [--usb]
              [--qmp SOCK]                               ... with a QMP socket for `qmp`
  qmp         --sock SOCK [--type TEXT] [--key KEYS] [--move DX,DY] [--screendump FILE] [--unplug ID]
                                                         type on, move the mouse of, capture the screen of, or unplug from, a running QEMU
  test        [--tier unit|boot|all] [--arch <a|all>] [--case NAME] [--no-usb] [--quick]
                                                         run tests (default: all tiers); --case runs one boot
                                                         case (x86_64-console), --no-usb drops the USB set,
                                                         --quick leaves the shell cases' test programs out
  perf        [--suite p2|p3|p4|p5|vt|diag] [--runs N] [--prepare] [--freebsd-only]
                                                         compare with FreeBSD in bhyve (server only)
  ubench      [--arch <a|all>] [--runs N] [--bhyve]      the P4 suite on AbyssBSD alone (QEMU, or bhyve on the server)
  freebsd-shell                                          run the boot-to-shell script against FreeBSD (server only)
  freebsd-multiuser [--arch <a|all>] [--vmm qemu|bhyve] run the multi-user login script against FreeBSD (server only)
  freebsd-net [--arch <a|all>] [--vmm qemu|bhyve]  run the network script against FreeBSD (server only)
  netdiff [--freebsd] [--scenario NAME]  differential captures on the test bridge (server only)
  freebsd-usertests [--arch <a|all>] [--test N,...]   run tests/user on FreeBSD 15.1 itself (server only)
  freebsd-diff [--arch <a|all>] [--scenario N,...]    record FreeBSD's output of tests/diff's scenarios (server only)
  kyua-baseline [--resume]                               record FreeBSD's kyua results per test case in bhyve (server only)
  kyua        [--resume] [--only DIR,...]                kyua's suite on AbyssBSD in bhyve, against FreeBSD's results (server only)
  fetch-base  [--arch <a|all>]                           FreeBSD base.txz: /rescue image, sysroot (server only)
  acpidump    --arch <a> [--vmm qemu|bhyve] [--machine M] --out DIR   capture a VM's ACPI tables
  freebsd-record [--out DIR] [--usb]                     record FreeBSD's device tree in bhyve; --usb: its USB
                                                         and input devices, in QEMU and bhyve (server only)
  fs-image    --fs ufs2|msdos --out IMG [--from DIR] [--size S] [--softdep]
                                                         make a file-system image with FreeBSD's tools (server only)
  fs-check    --fs ufs2|msdos IMG                        fsck an image read-only; fails unless clean (server only)
  crash-test  [--rounds N] [--arch A] [--seed S] [--debug]  kill QEMU mid-workload on a soft-updates disk; fsck_ffs -p must repair
  ufs-verify  [--keep]                                   lib/ufs against makefs and newfs images of base.txz (server only)
  check                                                  fmt, clippy (host and targets), policy
  policy                                                 dependency and license-header rules
  syscallgen  [--src DIR] [--check]                      regenerate lib/abi syscall tables
  abi-check   [--arch <a|all>]                           verify ABI layouts on the build server
  unsafe-report                                          count unsafe per crate

architectures: x86_64, aarch64
--remote runs the command on the build server ($ABYSS_BUILD_HOST, default ssh alias abyss-build)";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let mut args = Args::from_env();
    if args.take_flag("--help") || args.take_flag("-h") {
        println!("{USAGE}");
        return Ok(());
    }
    if args.take_flag("--remote") {
        return remote::run(&args.into_remaining());
    }
    let Some(cmd) = args.next_positional() else {
        println!("{USAGE}");
        return Err("no command given".into());
    };
    match cmd.as_str() {
        "build" => build::cmd(args),
        "image" => image::cmd(args),
        "run" => qemu::cmd_run(args),
        "qmp" => qmp::cmd(args),
        "test" => test::cmd(args),
        "perf" => perf::cmd(args),
        "ubench" => perf::cmd_ubench(args),
        "freebsd-record" => perf::cmd_record(args),
        "fs-image" => fs::cmd_image(args),
        "fs-check" => fs::cmd_check(args),
        "crash-test" => crash::cmd(args),
        "pjdfstest" => pjdfstest::cmd(args),
        "ufs-verify" => ufsverify::cmd(args),
        "fetch-base" => base::cmd(args),
        "freebsd-shell" => expect::cmd_freebsd_shell(args),
        "freebsd-multiuser" => multiuser::cmd_freebsd_multiuser(args),
        "freebsd-net" => net::cmd_freebsd_net(args),
        "netdiff" => netdiff::cmd_netdiff(args),
        "freebsd-usertests" => usertests::cmd(args),
        "freebsd-diff" => diff::cmd_freebsd_diff(args),
        "kyua-baseline" => kyua::cmd_kyua_baseline(args),
        "kyua" => kyua::cmd_kyua(args),
        "acpidump" => acpidump::cmd(args),
        "check" => test::cmd_check(args),
        "policy" => policy::cmd(args),
        "syscallgen" => syscalls::cmd(args),
        "abi-check" => abicheck::cmd(args),
        "unsafe-report" => policy::cmd_unsafe_report(args),
        other => Err(format!("unknown command `{other}`\n\n{USAGE}").into()),
    }
}
