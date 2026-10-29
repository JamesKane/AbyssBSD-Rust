// SPDX-License-Identifier: BSD-2-Clause
//! Kernel tracing (`sys/ktrace.h`): `ktrace(2)`'s operations and
//! facilities, and the records a trace file holds, each a
//! `struct ktr_header` followed by its type's part and payload.
//!
//! Also the `sv_flags` of FreeBSD's native ELF64 ABI (`sys/sysent.h`),
//! which `KTR_PROCCTOR` and `KTR_CAPFAIL` records carry.

use crate::capsicum::CapRights;
use crate::fcntl::PATH_MAX;
use crate::kinfo::MAXCOMLEN;
use crate::layout::{Arches, ConstValue};
use crate::signal::Sigset;
use crate::time::Timespec;

/// `ktrace(2)` operations (`KTROP(ops)`).
pub const KTROP_SET: u32 = 0;
pub const KTROP_CLEAR: u32 = 1;
pub const KTROP_CLEARFILE: u32 = 2;
/// The operation's bits.
pub const KTROP_MASK: u32 = 3;
/// Also every descendant of the process (or group).
pub const KTRFLAG_DESCEND: u32 = 4;

/// Record types.
pub const KTR_SYSCALL: u16 = 1;
pub const KTR_SYSRET: u16 = 2;
pub const KTR_NAMEI: u16 = 3;
pub const KTR_GENIO: u16 = 4;
pub const KTR_PSIG: u16 = 5;
pub const KTR_CSW: u16 = 6;
pub const KTR_USER: u16 = 7;
pub const KTR_STRUCT: u16 = 8;
pub const KTR_SYSCTL: u16 = 9;
pub const KTR_PROCCTOR: u16 = 10;
pub const KTR_PROCDTOR: u16 = 11;
pub const KTR_CAPFAIL: u16 = 12;
pub const KTR_FAULT: u16 = 13;
pub const KTR_FAULTEND: u16 = 14;
pub const KTR_STRUCT_ARRAY: u16 = 15;
pub const KTR_ARGS: u16 = 16;
pub const KTR_ENVS: u16 = 17;
pub const KTR_EXTERR: u16 = 18;
/// In `ktr_type`: a record before this one was dropped.
pub const KTR_DROP: u16 = 0x8000;
/// In `ktr_type`: the header is a `struct ktr_header` (version 1), not
/// the old `ktr_header_v0`.
pub const KTR_VERSIONED: u16 = 0x4000;
/// `ktr_version` of [`KtrHeader`].
pub const KTR_VERSION1: i16 = 1;
/// The longest `utrace` record.
pub const KTR_USER_MAXLEN: usize = 2048;

/// Facilities (`p_traceflag` bits), one per record type.
pub const KTRFAC_MASK: u32 = 0x00ff_ffff;
pub const KTRFAC_SYSCALL: u32 = 1 << KTR_SYSCALL;
pub const KTRFAC_SYSRET: u32 = 1 << KTR_SYSRET;
pub const KTRFAC_NAMEI: u32 = 1 << KTR_NAMEI;
pub const KTRFAC_GENIO: u32 = 1 << KTR_GENIO;
pub const KTRFAC_PSIG: u32 = 1 << KTR_PSIG;
pub const KTRFAC_CSW: u32 = 1 << KTR_CSW;
pub const KTRFAC_USER: u32 = 1 << KTR_USER;
pub const KTRFAC_STRUCT: u32 = 1 << KTR_STRUCT;
pub const KTRFAC_SYSCTL: u32 = 1 << KTR_SYSCTL;
pub const KTRFAC_PROCCTOR: u32 = 1 << KTR_PROCCTOR;
pub const KTRFAC_PROCDTOR: u32 = 1 << KTR_PROCDTOR;
pub const KTRFAC_CAPFAIL: u32 = 1 << KTR_CAPFAIL;
pub const KTRFAC_FAULT: u32 = 1 << KTR_FAULT;
pub const KTRFAC_FAULTEND: u32 = 1 << KTR_FAULTEND;
pub const KTRFAC_STRUCT_ARRAY: u32 = 1 << KTR_STRUCT_ARRAY;
pub const KTRFAC_ARGS: u32 = 1 << KTR_ARGS;
pub const KTRFAC_ENVS: u32 = 1 << KTR_ENVS;
pub const KTRFAC_EXTERR: u32 = 1 << KTR_EXTERR;
/// Root set the trace: only root may change it.
pub const KTRFAC_ROOT: u32 = 0x8000_0000;
/// Children inherit the trace.
pub const KTRFAC_INHERIT: u32 = 0x4000_0000;
/// The last record was dropped.
pub const KTRFAC_DROP: u32 = 0x2000_0000;

/// `enum ktr_cap_violation`.
pub const CAPFAIL_NOTCAPABLE: u32 = 0;
pub const CAPFAIL_INCREASE: u32 = 1;
pub const CAPFAIL_SYSCALL: u32 = 2;
pub const CAPFAIL_SIGNAL: u32 = 3;
pub const CAPFAIL_PROTO: u32 = 4;
pub const CAPFAIL_SOCKADDR: u32 = 5;
pub const CAPFAIL_NAMEI: u32 = 6;
pub const CAPFAIL_CPUSET: u32 = 7;

/// `enum uio_rw`, as `KTR_GENIO` records it.
pub const UIO_READ: i32 = 0;
pub const UIO_WRITE: i32 = 1;

/// `sv_flags` bits (`SV_*`).
pub const SV_LP64: u32 = 0x00_0200;
pub const SV_SHP: u32 = 0x01_0000;
pub const SV_SIGSYS: u32 = 0x02_0000;
pub const SV_TIMEKEEP: u32 = 0x04_0000;
pub const SV_ASLR: u32 = 0x08_0000;
pub const SV_RNG_SEED_VER: u32 = 0x10_0000;
pub const SV_DSO_SIG: u32 = 0x80_0000;
pub const SV_ABI_FREEBSD: u32 = 9;
/// The native ABI's flags (`elf64_freebsd_sysvec`'s): amd64's has its
/// signal trampoline in the vDSO, arm64's does not.
#[cfg(target_arch = "x86_64")]
pub const SV_FLAGS: u32 = SV_ABI_FREEBSD
    | SV_ASLR
    | SV_LP64
    | SV_SHP
    | SV_TIMEKEEP
    | SV_RNG_SEED_VER
    | SV_DSO_SIG
    | SV_SIGSYS;
#[cfg(not(target_arch = "x86_64"))]
pub const SV_FLAGS: u32 =
    SV_SHP | SV_TIMEKEEP | SV_ABI_FREEBSD | SV_LP64 | SV_ASLR | SV_RNG_SEED_VER | SV_SIGSYS;

/// `struct ktr_header`: every record's start.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KtrHeader {
    /// The length of what follows the header.
    pub ktr_len: i32,
    /// The type, with [`KTR_VERSIONED`] and maybe [`KTR_DROP`].
    pub ktr_type: i16,
    pub ktr_version: i16,
    pub ktr_pid: i32,
    /// The thread's name.
    pub ktr_comm: [u8; MAXCOMLEN + 1],
    pub ktr_time: Timespec,
    pub ktr_tid: i64,
    pub ktr_cpu: i32,
    pub _pad: u32,
}

/// `struct ktr_syscall`: the number and the argument count; the
/// arguments (`register_t` each) follow at `ktr_args`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KtrSyscall {
    pub ktr_code: i16,
    pub ktr_narg: i16,
    pub _pad: u32,
    pub ktr_args: [u64; 1],
}

/// What a `KTR_SYSCALL` record has before its arguments
/// (`offsetof(struct ktr_syscall, ktr_args)`).
pub const KTR_SYSCALL_HEAD: usize = 8;

/// `struct ktr_sysret`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KtrSysret {
    pub ktr_code: i16,
    pub ktr_eosys: i16,
    pub ktr_error: i32,
    pub ktr_retval: i64,
}

/// `struct ktr_genio`; the data follows.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KtrGenio {
    pub ktr_fd: i32,
    pub ktr_rw: i32,
}

/// `struct ktr_psig`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KtrPsig {
    pub signo: i32,
    pub _pad0: u32,
    /// The handler (`sig_t`): `SIG_DFL` 0, `SIG_IGN` 1, or an address.
    pub action: u64,
    pub code: i32,
    pub mask: Sigset,
    pub _pad1: u32,
}

/// `struct ktr_csw`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KtrCsw {
    /// 1 switching out, 0 in.
    pub out: i32,
    /// 1 preempted in user mode, 0 sleeping in the kernel.
    pub user: i32,
    pub wmesg: [u8; 8],
}

/// `struct ktr_proc_ctor`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KtrProcCtor {
    pub sv_flags: u32,
}

/// `struct ktr_cap_fail`. `cap_data` is `union ktr_cap_data`: two
/// `cap_rights_t` (needed, held), an `int`, a `struct sockaddr`'s
/// bytes, or a path.
#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KtrCapFail {
    pub cap_type: u32,
    pub cap_code: i16,
    pub _pad0: u16,
    pub cap_svflags: u32,
    pub _pad1: u32,
    pub cap_data: [u8; PATH_MAX],
}

impl Default for KtrCapFail {
    fn default() -> Self {
        Self {
            cap_type: 0,
            cap_code: 0,
            _pad0: 0,
            cap_svflags: 0,
            _pad1: 0,
            cap_data: [0; PATH_MAX],
        }
    }
}

const _: () = assert!(core::mem::size_of::<[CapRights; 2]>() <= PATH_MAX);

/// `struct ktr_fault`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KtrFault {
    pub vaddr: u64,
    pub r#type: i32,
    pub _pad: u32,
}

/// `struct ktr_faultend`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KtrFaultend {
    pub result: i32,
}

/// `struct ktr_struct_array`; the name (NUL-terminated) and the
/// elements follow.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KtrStructArray {
    pub struct_size: u64,
}

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("KTROP_SET", KTROP_SET as i64),
    c("KTROP_CLEAR", KTROP_CLEAR as i64),
    c("KTROP_CLEARFILE", KTROP_CLEARFILE as i64),
    c("KTRFLAG_DESCEND", KTRFLAG_DESCEND as i64),
    c("KTR_SYSCALL", KTR_SYSCALL as i64),
    c("KTR_SYSRET", KTR_SYSRET as i64),
    c("KTR_NAMEI", KTR_NAMEI as i64),
    c("KTR_GENIO", KTR_GENIO as i64),
    c("KTR_PSIG", KTR_PSIG as i64),
    c("KTR_CSW", KTR_CSW as i64),
    c("KTR_USER", KTR_USER as i64),
    c("KTR_STRUCT", KTR_STRUCT as i64),
    c("KTR_SYSCTL", KTR_SYSCTL as i64),
    c("KTR_PROCCTOR", KTR_PROCCTOR as i64),
    c("KTR_PROCDTOR", KTR_PROCDTOR as i64),
    c("KTR_CAPFAIL", KTR_CAPFAIL as i64),
    c("KTR_FAULT", KTR_FAULT as i64),
    c("KTR_FAULTEND", KTR_FAULTEND as i64),
    c("KTR_STRUCT_ARRAY", KTR_STRUCT_ARRAY as i64),
    c("KTR_ARGS", KTR_ARGS as i64),
    c("KTR_ENVS", KTR_ENVS as i64),
    c("KTR_EXTERR", KTR_EXTERR as i64),
    c("KTR_DROP", KTR_DROP as i64),
    c("KTR_VERSIONED", KTR_VERSIONED as i64),
    c("KTR_VERSION1", KTR_VERSION1 as i64),
    c("KTR_USER_MAXLEN", KTR_USER_MAXLEN as i64),
    c("KTRFAC_MASK", KTRFAC_MASK as i64),
    c("KTRFAC_ROOT", KTRFAC_ROOT as i64),
    c("KTRFAC_INHERIT", KTRFAC_INHERIT as i64),
    c("KTRFAC_DROP", KTRFAC_DROP as i64),
    c("CAPFAIL_NOTCAPABLE", CAPFAIL_NOTCAPABLE as i64),
    c("CAPFAIL_INCREASE", CAPFAIL_INCREASE as i64),
    c("CAPFAIL_SYSCALL", CAPFAIL_SYSCALL as i64),
    c("CAPFAIL_SIGNAL", CAPFAIL_SIGNAL as i64),
    c("CAPFAIL_PROTO", CAPFAIL_PROTO as i64),
    c("CAPFAIL_SOCKADDR", CAPFAIL_SOCKADDR as i64),
    c("CAPFAIL_NAMEI", CAPFAIL_NAMEI as i64),
    c("CAPFAIL_CPUSET", CAPFAIL_CPUSET as i64),
    ConstValue {
        c_name: "SV_LP64",
        header: "sys/sysent.h",
        value: SV_LP64 as i64,
        arches: Arches::All,
    },
    ConstValue {
        c_name: "SV_ABI_FREEBSD",
        header: "sys/sysent.h",
        value: SV_ABI_FREEBSD as i64,
        arches: Arches::All,
    },
    ConstValue {
        c_name: "SV_DSO_SIG",
        header: "sys/sysent.h",
        value: SV_DSO_SIG as i64,
        arches: Arches::All,
    },
];

const fn c(c_name: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/ktrace.h",
        value,
        arches: Arches::All,
    }
}
