// SPDX-License-Identifier: BSD-2-Clause
//! What `execve` leaves for the new program: the ELF auxiliary vector
//! (`sys/elf_common.h`), `struct ps_strings` (`sys/exec.h`), and the ELF
//! branding a FreeBSD binary carries.

use crate::layout::{Arches, ConstValue};

/// Auxiliary vector entry types (`AT_*`).
pub mod at {
    pub const NULL: u64 = 0;
    pub const PHDR: u64 = 3;
    pub const PHENT: u64 = 4;
    pub const PHNUM: u64 = 5;
    pub const PAGESZ: u64 = 6;
    pub const BASE: u64 = 7;
    pub const FLAGS: u64 = 8;
    pub const ENTRY: u64 = 9;
    pub const EXECPATH: u64 = 15;
    pub const CANARY: u64 = 16;
    pub const CANARYLEN: u64 = 17;
    pub const OSRELDATE: u64 = 18;
    pub const NCPUS: u64 = 19;
    pub const PAGESIZES: u64 = 20;
    pub const PAGESIZESLEN: u64 = 21;
    pub const TIMEKEEP: u64 = 22;
    pub const STACKPROT: u64 = 23;
    pub const EHDRFLAGS: u64 = 24;
    pub const HWCAP: u64 = 25;
    pub const HWCAP2: u64 = 26;
    pub const BSDFLAGS: u64 = 27;
    pub const ARGC: u64 = 28;
    pub const ARGV: u64 = 29;
    pub const ENVC: u64 = 30;
    pub const ENVV: u64 = 31;
    pub const PS_STRINGS: u64 = 32;
    pub const FXRNG: u64 = 33;
    pub const KPRELOAD: u64 = 34;
    pub const USRSTACKBASE: u64 = 35;
    pub const USRSTACKLIM: u64 = 36;
    /// Entries the kernel reserves room for.
    pub const COUNT: u64 = 40;
}

/// `ELFOSABI_FREEBSD`.
pub const ELFOSABI_FREEBSD: u8 = 9;
/// `NT_FREEBSD_ABI_TAG`, in a note named `FreeBSD`.
pub const NT_FREEBSD_ABI_TAG: u32 = 1;
/// `NT_FREEBSD_FEATURE_CTL`: a binary's opt-outs, a word of `fctl::*`.
pub const NT_FREEBSD_FEATURE_CTL: u32 = 4;

/// `NT_FREEBSD_FEATURE_CTL`'s bits (`NT_FREEBSD_FCTL_*`).
pub mod fctl {
    pub const ASLR_DISABLE: u32 = 0x1;
    pub const PROTMAX_DISABLE: u32 = 0x2;
    pub const STKGAP_DISABLE: u32 = 0x4;
    pub const WXNEEDED: u32 = 0x8;
    pub const LA48: u32 = 0x10;
}

/// `AT_BSDFLAGS`' bits (`ELF_BSDF_*`).
pub mod bsdf {
    /// The kernel supports `sigfastblock`.
    pub const SIGFASTBLK: u64 = 0x1;
    pub const VMNOOVERCOMMIT: u64 = 0x2;
}

/// Page sizes in `AT_PAGESIZES` (`MAXPAGESIZES`).
#[cfg(target_arch = "x86_64")]
pub const MAXPAGESIZES: usize = 3;
#[cfg(target_arch = "aarch64")]
pub const MAXPAGESIZES: usize = 4;
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub const MAXPAGESIZES: usize = 3;

/// `struct ps_strings`: where `ps` finds a process's arguments.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PsStrings {
    pub ps_argvstr: u64,
    pub ps_nargvstr: u32,
    pub _pad0: u32,
    pub ps_envstr: u64,
    pub ps_nenvstr: u32,
    pub _pad1: u32,
}

const _: () = assert!(size_of::<PsStrings>() == 32);

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("AT_NULL", at::NULL),
    c("AT_PHDR", at::PHDR),
    c("AT_PHENT", at::PHENT),
    c("AT_PHNUM", at::PHNUM),
    c("AT_PAGESZ", at::PAGESZ),
    c("AT_BASE", at::BASE),
    c("AT_FLAGS", at::FLAGS),
    c("AT_ENTRY", at::ENTRY),
    c("AT_EXECPATH", at::EXECPATH),
    c("AT_CANARY", at::CANARY),
    c("AT_CANARYLEN", at::CANARYLEN),
    c("AT_OSRELDATE", at::OSRELDATE),
    c("AT_NCPUS", at::NCPUS),
    c("AT_PAGESIZES", at::PAGESIZES),
    c("AT_PAGESIZESLEN", at::PAGESIZESLEN),
    c("AT_TIMEKEEP", at::TIMEKEEP),
    c("AT_STACKPROT", at::STACKPROT),
    c("AT_EHDRFLAGS", at::EHDRFLAGS),
    c("AT_HWCAP", at::HWCAP),
    c("AT_HWCAP2", at::HWCAP2),
    c("AT_BSDFLAGS", at::BSDFLAGS),
    c("AT_ARGC", at::ARGC),
    c("AT_ARGV", at::ARGV),
    c("AT_ENVC", at::ENVC),
    c("AT_ENVV", at::ENVV),
    c("AT_PS_STRINGS", at::PS_STRINGS),
    c("AT_FXRNG", at::FXRNG),
    c("AT_KPRELOAD", at::KPRELOAD),
    c("AT_USRSTACKBASE", at::USRSTACKBASE),
    c("AT_USRSTACKLIM", at::USRSTACKLIM),
    c("AT_COUNT", at::COUNT),
    c("ELFOSABI_FREEBSD", ELFOSABI_FREEBSD as u64),
    c("NT_FREEBSD_ABI_TAG", NT_FREEBSD_ABI_TAG as u64),
    c("NT_FREEBSD_FEATURE_CTL", NT_FREEBSD_FEATURE_CTL as u64),
    c("NT_FREEBSD_FCTL_ASLR_DISABLE", fctl::ASLR_DISABLE as u64),
    c(
        "NT_FREEBSD_FCTL_PROTMAX_DISABLE",
        fctl::PROTMAX_DISABLE as u64,
    ),
    c(
        "NT_FREEBSD_FCTL_STKGAP_DISABLE",
        fctl::STKGAP_DISABLE as u64,
    ),
    c("NT_FREEBSD_FCTL_WXNEEDED", fctl::WXNEEDED as u64),
    c("NT_FREEBSD_FCTL_LA48", fctl::LA48 as u64),
    c("ELF_BSDF_SIGFASTBLK", bsdf::SIGFASTBLK),
    c("ELF_BSDF_VMNOOVERCOMMIT", bsdf::VMNOOVERCOMMIT),
    ConstValue {
        c_name: "MAXPAGESIZES",
        header: "machine/param.h",
        value: 3,
        arches: Arches::Amd64,
    },
    ConstValue {
        c_name: "MAXPAGESIZES",
        header: "machine/param.h",
        value: 4,
        arches: Arches::Arm64,
    },
];

#[allow(clippy::cast_possible_wrap)] // small values
const fn c(c_name: &'static str, value: u64) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/elf_common.h",
        value: value as i64,
        arches: Arches::All,
    }
}
