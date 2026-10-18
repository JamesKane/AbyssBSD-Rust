// SPDX-License-Identifier: BSD-2-Clause
//! Building the firmware and kernel images.

use std::path::PathBuf;

use crate::arch::Arch;
use crate::cli::Args;
use crate::util::{self, Result};

/// What to build.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Options {
    pub(crate) release: bool,
    /// Build the in-image test runner instead of the normal boot path.
    pub(crate) ktest: bool,
}

impl Options {
    pub(crate) fn from_args(args: &mut Args) -> Self {
        Self {
            release: args.take_flag("--release"),
            ktest: args.take_flag("--ktest"),
        }
    }

    fn profile_dir(self) -> &'static str {
        if self.release { "release" } else { "debug" }
    }
}

pub(crate) fn cmd(mut args: Args) -> Result<()> {
    let arches = Arch::from_args(&mut args)?;
    let opts = Options::from_args(&mut args);
    args.finish()?;
    for arch in arches {
        let path = loader(arch, opts)?;
        println!("{arch}: {}", path.display());
    }
    Ok(())
}

/// Builds the UEFI loader and returns the path of the `.efi` image.
pub(crate) fn loader(arch: Arch, opts: Options) -> Result<PathBuf> {
    let mut cargo = util::cargo();
    cargo.args([
        "build",
        "--package",
        "loader-efi",
        "--target",
        arch.uefi_target(),
    ]);
    if opts.release {
        cargo.arg("--release");
    }
    if opts.ktest {
        // A separate target directory keeps test and normal builds from
        // rebuilding each other.
        cargo.args(["--features", "ktest", "--target-dir", "target/ktest"]);
    }
    util::run(&mut cargo)?;
    let target_dir = if opts.ktest { "target/ktest" } else { "target" };
    Ok(util::root()
        .join(target_dir)
        .join(arch.uefi_target())
        .join(opts.profile_dir())
        .join("loader-efi.efi"))
}
