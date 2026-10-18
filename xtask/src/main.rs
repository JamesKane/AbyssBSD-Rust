// SPDX-License-Identifier: BSD-2-Clause
//! `cargo xtask`: the single entry point for building, imaging, running and
//! testing AbyssBSD. See docs/BUILD-AND-TEST.md.

mod abicheck;
mod arch;
mod build;
mod cli;
mod image;
mod perf;
mod policy;
mod qemu;
mod remote;
mod syscalls;
mod test;
mod util;

use std::process::ExitCode;

use cli::Args;
use util::Result;

const USAGE: &str = "\
usage: cargo xtask [--remote] <command> [options]

commands:
  build       --arch <a|all> [--release] [--ktest]       build the loader
  image       --arch <a> [--release] [--ktest]           build a GPT disk image with an ESP
  run         --arch <a> [--release] [--ktest] [--smp N] [--mem SIZE] [--gdb] [--display]
  test        [--tier unit|boot|all] [--arch <a|all>]    run tests (default: all tiers)
  perf        [--suite p2|diag] [--runs N] [--prepare]   compare with FreeBSD in bhyve (server only)
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
        "test" => test::cmd(args),
        "perf" => perf::cmd(args),
        "check" => test::cmd_check(args),
        "policy" => policy::cmd(args),
        "syscallgen" => syscalls::cmd(args),
        "abi-check" => abicheck::cmd(args),
        "unsafe-report" => policy::cmd_unsafe_report(args),
        other => Err(format!("unknown command `{other}`\n\n{USAGE}").into()),
    }
}
