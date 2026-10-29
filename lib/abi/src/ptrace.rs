// SPDX-License-Identifier: BSD-2-Clause
//! Process tracing (`sys/ptrace.h`, `machine/reg.h`, `machine/ptrace.h`):
//! `ptrace(2)`'s requests and the structures they pass, and the register
//! sets they read and write.

use crate::layout::{Arches, ConstValue};
use crate::signal::{Siginfo, Sigset};

/// Requests (`PT_*`).
pub const PT_TRACE_ME: i32 = 0;
pub const PT_READ_I: i32 = 1;
pub const PT_READ_D: i32 = 2;
pub const PT_WRITE_I: i32 = 4;
pub const PT_WRITE_D: i32 = 5;
pub const PT_CONTINUE: i32 = 7;
pub const PT_KILL: i32 = 8;
pub const PT_STEP: i32 = 9;
pub const PT_ATTACH: i32 = 10;
pub const PT_DETACH: i32 = 11;
pub const PT_IO: i32 = 12;
pub const PT_LWPINFO: i32 = 13;
pub const PT_GETNUMLWPS: i32 = 14;
pub const PT_GETLWPLIST: i32 = 15;
pub const PT_CLEARSTEP: i32 = 16;
pub const PT_SETSTEP: i32 = 17;
pub const PT_SUSPEND: i32 = 18;
pub const PT_RESUME: i32 = 19;
pub const PT_TO_SCE: i32 = 20;
pub const PT_TO_SCX: i32 = 21;
pub const PT_SYSCALL: i32 = 22;
pub const PT_FOLLOW_FORK: i32 = 23;
pub const PT_LWP_EVENTS: i32 = 24;
pub const PT_GET_EVENT_MASK: i32 = 25;
pub const PT_SET_EVENT_MASK: i32 = 26;
pub const PT_GET_SC_ARGS: i32 = 27;
pub const PT_GET_SC_RET: i32 = 28;
pub const PT_COREDUMP: i32 = 29;
pub const PT_GETREGS: i32 = 33;
pub const PT_SETREGS: i32 = 34;
pub const PT_GETFPREGS: i32 = 35;
pub const PT_SETFPREGS: i32 = 36;
pub const PT_GETDBREGS: i32 = 37;
pub const PT_SETDBREGS: i32 = 38;
pub const PT_VM_TIMESTAMP: i32 = 40;
pub const PT_VM_ENTRY: i32 = 41;
pub const PT_GETREGSET: i32 = 42;
pub const PT_SETREGSET: i32 = 43;
pub const PT_SC_REMOTE: i32 = 44;
pub const PT_FIRSTMACH: i32 = 64;
/// The kernel's own requests, refused from user mode
/// (`PTINTERNAL_FIRST`..`PTINTERNAL_LAST`).
pub const PTINTERNAL_FIRST: i32 = 128;
pub const PTINTERNAL_LAST: i32 = 191;

/// amd64's own requests (`machine/ptrace.h`).
pub mod amd64 {
    use super::PT_FIRSTMACH;
    pub const PT_GETXMMREGS: i32 = PT_FIRSTMACH;
    pub const PT_SETXMMREGS: i32 = PT_FIRSTMACH + 1;
    pub const PT_GETXSTATE_OLD: i32 = PT_FIRSTMACH + 2;
    pub const PT_SETXSTATE_OLD: i32 = PT_FIRSTMACH + 3;
    pub const PT_GETXSTATE_INFO: i32 = PT_FIRSTMACH + 4;
    pub const PT_GETXSTATE: i32 = PT_FIRSTMACH + 5;
    pub const PT_SETXSTATE: i32 = PT_FIRSTMACH + 6;
    pub const PT_GETFSBASE: i32 = PT_FIRSTMACH + 7;
    pub const PT_SETFSBASE: i32 = PT_FIRSTMACH + 8;
    pub const PT_GETGSBASE: i32 = PT_FIRSTMACH + 9;
    pub const PT_SETGSBASE: i32 = PT_FIRSTMACH + 10;
    pub const PT_GETTLSBASE: i32 = PT_FIRSTMACH + 11;
    pub const PT_SETTLSBASE: i32 = PT_FIRSTMACH + 12;
}

/// Events a debugger may ask to stop for (`p_ptevents`).
pub const PTRACE_EXEC: u32 = 0x0001;
pub const PTRACE_SCE: u32 = 0x0002;
pub const PTRACE_SCX: u32 = 0x0004;
pub const PTRACE_SYSCALL: u32 = PTRACE_SCE | PTRACE_SCX;
pub const PTRACE_FORK: u32 = 0x0008;
pub const PTRACE_LWP: u32 = 0x0010;
pub const PTRACE_VFORK: u32 = 0x0020;
pub const PTRACE_DEFAULT: u32 = PTRACE_EXEC;

/// `PT_IO` operations.
pub const PIOD_READ_D: i32 = 1;
pub const PIOD_WRITE_D: i32 = 2;
pub const PIOD_READ_I: i32 = 3;
pub const PIOD_WRITE_I: i32 = 4;

/// `pl_event`.
pub const PL_EVENT_NONE: i32 = 0;
pub const PL_EVENT_SIGNAL: i32 = 1;
/// `pl_flags`.
pub const PL_FLAG_SA: i32 = 0x01;
pub const PL_FLAG_BOUND: i32 = 0x02;
pub const PL_FLAG_SCE: i32 = 0x04;
pub const PL_FLAG_SCX: i32 = 0x08;
pub const PL_FLAG_EXEC: i32 = 0x10;
pub const PL_FLAG_SI: i32 = 0x20;
pub const PL_FLAG_FORKED: i32 = 0x40;
pub const PL_FLAG_CHILD: i32 = 0x80;
pub const PL_FLAG_BORN: i32 = 0x100;
pub const PL_FLAG_EXITED: i32 = 0x200;
pub const PL_FLAG_VFORKED: i32 = 0x400;
pub const PL_FLAG_VFORK_DONE: i32 = 0x800;

/// `PT_COREDUMP` flags.
pub const PC_COMPRESS: u32 = 0x0000_0001;
pub const PC_ALL: u32 = 0x0000_0002;

/// Register sets `PT_GETREGSET` names (`NT_*`, `sys/elf_common.h`).
pub const NT_PRSTATUS: i32 = 1;
pub const NT_FPREGSET: i32 = 2;
pub const NT_X86_SEGBASES: i32 = 0x200;
pub const NT_X86_XSTATE: i32 = 0x202;
pub const NT_ARM_VFP: i32 = 0x400;
pub const NT_ARM_TLS: i32 = 0x401;

/// `struct ptrace_io_desc`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PtraceIoDesc {
    pub piod_op: i32,
    pub _pad: u32,
    pub piod_offs: u64,
    pub piod_addr: u64,
    pub piod_len: u64,
}

/// `struct ptrace_lwpinfo`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PtraceLwpinfo {
    pub pl_lwpid: i32,
    pub pl_event: i32,
    pub pl_flags: i32,
    pub pl_sigmask: Sigset,
    pub pl_siglist: Sigset,
    pub _pad0: u32,
    pub pl_siginfo: Siginfo,
    pub pl_tdname: [u8; crate::kinfo::MAXCOMLEN + 1],
    pub pl_child_pid: i32,
    pub pl_syscall_code: u32,
    pub pl_syscall_narg: u32,
}

/// `struct ptrace_sc_ret`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PtraceScRet {
    pub sr_retval: [u64; 2],
    pub sr_error: i32,
    pub _pad: u32,
}

/// `struct ptrace_vm_entry`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PtraceVmEntry {
    pub pve_entry: i32,
    pub pve_timestamp: i32,
    pub pve_start: u64,
    pub pve_end: u64,
    pub pve_offset: u64,
    pub pve_prot: u32,
    pub pve_pathlen: u32,
    pub pve_fileid: i64,
    pub pve_fsid: u32,
    pub _pad: u32,
    pub pve_path: u64,
}

/// `struct ptrace_coredump`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PtraceCoredump {
    pub pc_fd: i32,
    pub pc_flags: u32,
    pub pc_limit: i64,
}

/// `struct ptrace_sc_remote`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PtraceScRemote {
    pub pscr_ret: PtraceScRet,
    pub pscr_syscall: u32,
    pub pscr_nargs: u32,
    pub pscr_args: u64,
}

/// amd64's register sets (`machine/reg.h`).
pub mod regs_amd64 {
    /// `struct reg` (`struct __reg64`).
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Reg {
        pub r_r15: i64,
        pub r_r14: i64,
        pub r_r13: i64,
        pub r_r12: i64,
        pub r_r11: i64,
        pub r_r10: i64,
        pub r_r9: i64,
        pub r_r8: i64,
        pub r_rdi: i64,
        pub r_rsi: i64,
        pub r_rbp: i64,
        pub r_rbx: i64,
        pub r_rdx: i64,
        pub r_rcx: i64,
        pub r_rax: i64,
        pub r_trapno: u32,
        pub r_fs: u16,
        pub r_gs: u16,
        pub r_err: u32,
        pub r_es: u16,
        pub r_ds: u16,
        pub r_rip: i64,
        pub r_cs: i64,
        pub r_rflags: i64,
        pub r_rsp: i64,
        pub r_ss: i64,
    }

    /// `struct fpreg` (`struct __fpreg64`): the FXSAVE area.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Fpreg {
        pub fpr_env: [u64; 4],
        pub fpr_acc: [[u8; 16]; 8],
        pub fpr_xacc: [[u8; 16]; 16],
        pub fpr_spare: [u64; 12],
    }

    /// `struct dbreg` (`struct __dbreg64`).
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Dbreg {
        pub dr: [u64; 16],
    }
}

/// arm64's register sets (`machine/reg.h`).
pub mod regs_arm64 {
    /// `struct reg`.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Reg {
        pub x: [u64; 30],
        pub lr: u64,
        pub sp: u64,
        pub elr: u64,
        pub spsr: u64,
    }

    /// `struct fpreg`.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Fpreg {
        pub fp_q: [u128; 32],
        pub fp_sr: u32,
        pub fp_cr: u32,
        pub _pad: u64,
    }

    /// One breakpoint or watchpoint of [`Dbreg`].
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct DbregPair {
        pub addr: u64,
        pub ctrl: u32,
        pub pad: u32,
    }

    /// `struct dbreg`.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Dbreg {
        pub db_debug_ver: u8,
        pub db_nbkpts: u8,
        pub db_nwtpts: u8,
        pub db_pad: [u8; 5],
        pub db_breakregs: [DbregPair; 16],
        pub db_watchregs: [DbregPair; 16],
    }
}

/// This architecture's register sets.
#[cfg(target_arch = "x86_64")]
pub use regs_amd64::{Dbreg, Fpreg, Reg};
#[cfg(target_arch = "aarch64")]
pub use regs_arm64::{Dbreg, Fpreg, Reg};

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("PT_TRACE_ME", PT_TRACE_ME as i64),
    c("PT_READ_I", PT_READ_I as i64),
    c("PT_READ_D", PT_READ_D as i64),
    c("PT_WRITE_I", PT_WRITE_I as i64),
    c("PT_WRITE_D", PT_WRITE_D as i64),
    c("PT_CONTINUE", PT_CONTINUE as i64),
    c("PT_KILL", PT_KILL as i64),
    c("PT_STEP", PT_STEP as i64),
    c("PT_ATTACH", PT_ATTACH as i64),
    c("PT_DETACH", PT_DETACH as i64),
    c("PT_IO", PT_IO as i64),
    c("PT_LWPINFO", PT_LWPINFO as i64),
    c("PT_GETNUMLWPS", PT_GETNUMLWPS as i64),
    c("PT_GETLWPLIST", PT_GETLWPLIST as i64),
    c("PT_CLEARSTEP", PT_CLEARSTEP as i64),
    c("PT_SETSTEP", PT_SETSTEP as i64),
    c("PT_SUSPEND", PT_SUSPEND as i64),
    c("PT_RESUME", PT_RESUME as i64),
    c("PT_TO_SCE", PT_TO_SCE as i64),
    c("PT_TO_SCX", PT_TO_SCX as i64),
    c("PT_SYSCALL", PT_SYSCALL as i64),
    c("PT_FOLLOW_FORK", PT_FOLLOW_FORK as i64),
    c("PT_LWP_EVENTS", PT_LWP_EVENTS as i64),
    c("PT_GET_EVENT_MASK", PT_GET_EVENT_MASK as i64),
    c("PT_SET_EVENT_MASK", PT_SET_EVENT_MASK as i64),
    c("PT_GET_SC_ARGS", PT_GET_SC_ARGS as i64),
    c("PT_GET_SC_RET", PT_GET_SC_RET as i64),
    c("PT_COREDUMP", PT_COREDUMP as i64),
    c("PT_GETREGS", PT_GETREGS as i64),
    c("PT_SETREGS", PT_SETREGS as i64),
    c("PT_GETFPREGS", PT_GETFPREGS as i64),
    c("PT_SETFPREGS", PT_SETFPREGS as i64),
    c("PT_GETDBREGS", PT_GETDBREGS as i64),
    c("PT_SETDBREGS", PT_SETDBREGS as i64),
    c("PT_VM_TIMESTAMP", PT_VM_TIMESTAMP as i64),
    c("PT_VM_ENTRY", PT_VM_ENTRY as i64),
    c("PT_GETREGSET", PT_GETREGSET as i64),
    c("PT_SETREGSET", PT_SETREGSET as i64),
    c("PT_SC_REMOTE", PT_SC_REMOTE as i64),
    c("PT_FIRSTMACH", PT_FIRSTMACH as i64),
    c("PTRACE_EXEC", PTRACE_EXEC as i64),
    c("PTRACE_SCE", PTRACE_SCE as i64),
    c("PTRACE_SCX", PTRACE_SCX as i64),
    c("PTRACE_FORK", PTRACE_FORK as i64),
    c("PTRACE_LWP", PTRACE_LWP as i64),
    c("PTRACE_VFORK", PTRACE_VFORK as i64),
    c("PTRACE_DEFAULT", PTRACE_DEFAULT as i64),
    c("PIOD_READ_D", PIOD_READ_D as i64),
    c("PIOD_WRITE_D", PIOD_WRITE_D as i64),
    c("PIOD_READ_I", PIOD_READ_I as i64),
    c("PIOD_WRITE_I", PIOD_WRITE_I as i64),
    c("PL_EVENT_NONE", PL_EVENT_NONE as i64),
    c("PL_EVENT_SIGNAL", PL_EVENT_SIGNAL as i64),
    c("PL_FLAG_SCE", PL_FLAG_SCE as i64),
    c("PL_FLAG_SCX", PL_FLAG_SCX as i64),
    c("PL_FLAG_EXEC", PL_FLAG_EXEC as i64),
    c("PL_FLAG_SI", PL_FLAG_SI as i64),
    c("PL_FLAG_FORKED", PL_FLAG_FORKED as i64),
    c("PL_FLAG_CHILD", PL_FLAG_CHILD as i64),
    c("PL_FLAG_BORN", PL_FLAG_BORN as i64),
    c("PL_FLAG_EXITED", PL_FLAG_EXITED as i64),
    c("PL_FLAG_VFORKED", PL_FLAG_VFORKED as i64),
    c("PL_FLAG_VFORK_DONE", PL_FLAG_VFORK_DONE as i64),
    c("PC_COMPRESS", PC_COMPRESS as i64),
    c("PC_ALL", PC_ALL as i64),
    ConstValue {
        c_name: "NT_PRSTATUS",
        header: "sys/elf_common.h",
        value: NT_PRSTATUS as i64,
        arches: Arches::All,
    },
    ConstValue {
        c_name: "NT_FPREGSET",
        header: "sys/elf_common.h",
        value: NT_FPREGSET as i64,
        arches: Arches::All,
    },
    ConstValue {
        c_name: "NT_X86_XSTATE",
        header: "sys/elf_common.h",
        value: NT_X86_XSTATE as i64,
        arches: Arches::All,
    },
    ConstValue {
        c_name: "NT_ARM_VFP",
        header: "sys/elf_common.h",
        value: NT_ARM_VFP as i64,
        arches: Arches::All,
    },
];

const fn c(c_name: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/ptrace.h",
        value,
        arches: Arches::All,
    }
}
