// SPDX-License-Identifier: BSD-2-Clause
//! Target architectures and their per-arch facts.

use std::fmt;

use crate::cli::Args;
use crate::util::Result;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Arch {
    X86_64,
    Aarch64,
}

impl Arch {
    pub(crate) const ALL: [Self; 2] = [Self::X86_64, Self::Aarch64];

    pub(crate) fn parse(s: &str) -> Result<Self> {
        match s {
            "x86_64" | "amd64" => Ok(Self::X86_64),
            "aarch64" | "arm64" => Ok(Self::Aarch64),
            _ => Err(format!("unknown architecture `{s}` (x86_64, aarch64)").into()),
        }
    }

    /// Reads `--arch`: one architecture, or every one for `all` (the default).
    pub(crate) fn from_args(args: &mut Args) -> Result<Vec<Self>> {
        match args.take_value("--arch")?.as_deref() {
            None | Some("all") => Ok(Self::ALL.to_vec()),
            Some(a) => Ok(vec![Self::parse(a)?]),
        }
    }

    /// Reads `--arch`, requiring exactly one (defaulting to the host's).
    pub(crate) fn one_from_args(args: &mut Args) -> Result<Self> {
        match args.take_value("--arch")? {
            Some(a) => Self::parse(&a),
            None if cfg!(target_arch = "aarch64") => Ok(Self::Aarch64),
            None => Ok(Self::X86_64),
        }
    }

    pub(crate) const fn uefi_target(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64-unknown-uefi",
            Self::Aarch64 => "aarch64-unknown-uefi",
        }
    }

    /// The removable-media boot path firmware looks for.
    pub(crate) const fn efi_boot_file(self) -> &'static str {
        match self {
            Self::X86_64 => "EFI/BOOT/BOOTX64.EFI",
            Self::Aarch64 => "EFI/BOOT/BOOTAA64.EFI",
        }
    }

    /// FreeBSD's name for the architecture (`uname -p`).
    pub(crate) const fn freebsd_name(self) -> &'static str {
        match self {
            Self::X86_64 => "amd64",
            Self::Aarch64 => "aarch64",
        }
    }

    pub(crate) const fn clang_target(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64-unknown-freebsd15.1",
            Self::Aarch64 => "aarch64-unknown-freebsd15.1",
        }
    }
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
        })
    }
}
