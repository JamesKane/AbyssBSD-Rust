// SPDX-License-Identifier: BSD-2-Clause
//! Building the loader and kernel images.

use std::path::PathBuf;

use crate::arch::Arch;
use crate::cli::Args;
use crate::util::{self, Result};

/// What to build.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Options {
    pub(crate) release: bool,
    /// Build the in-image test runners instead of the normal boot path.
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

    /// Test and normal builds use separate target directories so they do
    /// not rebuild each other.
    fn target_dir(self) -> &'static str {
        if self.ktest { "target/ktest" } else { "target" }
    }
}

/// The built images for one architecture.
#[derive(Debug)]
pub(crate) struct Images {
    pub(crate) loader: PathBuf,
    pub(crate) kernel: PathBuf,
}

pub(crate) fn cmd(mut args: Args) -> Result<()> {
    let arches = Arch::from_args(&mut args)?;
    let opts = Options::from_args(&mut args);
    args.finish()?;
    for arch in arches {
        let images = all(arch, opts)?;
        println!("{arch}: {}", images.loader.display());
        println!("{arch}: {}", images.kernel.display());
    }
    Ok(())
}

/// Builds the loader and the kernel.
pub(crate) fn all(arch: Arch, opts: Options) -> Result<Images> {
    Ok(Images {
        loader: package(
            arch,
            opts,
            "loader-efi",
            arch.uefi_target(),
            "loader-efi.efi",
        )?,
        kernel: package(arch, opts, "kernel", arch.kernel_target(), "kernel")?,
    })
}

/// Where [`all`] leaves the kernel ELF (with full symbols, for debuggers).
pub(crate) fn kernel_path(arch: Arch, opts: Options) -> PathBuf {
    util::root()
        .join(opts.target_dir())
        .join(arch.kernel_target())
        .join(opts.profile_dir())
        .join("kernel")
}

fn package(arch: Arch, opts: Options, pkg: &str, target: &str, file: &str) -> Result<PathBuf> {
    let _ = arch;
    let mut cargo = util::cargo();
    cargo.args(["build", "--package", pkg, "--target", target]);
    cargo.args(["--target-dir", opts.target_dir()]);
    if opts.release {
        cargo.arg("--release");
    }
    if opts.ktest {
        cargo.args(["--features", "ktest"]);
    }
    util::run(&mut cargo)?;
    Ok(util::root()
        .join(opts.target_dir())
        .join(target)
        .join(opts.profile_dir())
        .join(file))
}
