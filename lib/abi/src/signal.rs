// SPDX-License-Identifier: BSD-2-Clause
//! Signals. Behavior reference: sys/sys/signal.h, sys/sys/_sigset.h.

use crate::layout::{Arches, ConstValue};

/// The highest signal number (`_SIG_MAXSIG`).
pub const SIG_MAXSIG: u32 = 128;

pub const SIGHUP: u32 = 1;
pub const SIGINT: u32 = 2;
pub const SIGQUIT: u32 = 3;
pub const SIGILL: u32 = 4;
pub const SIGTRAP: u32 = 5;
pub const SIGABRT: u32 = 6;
pub const SIGEMT: u32 = 7;
pub const SIGFPE: u32 = 8;
pub const SIGKILL: u32 = 9;
pub const SIGBUS: u32 = 10;
pub const SIGSEGV: u32 = 11;
pub const SIGSYS: u32 = 12;
pub const SIGPIPE: u32 = 13;
pub const SIGALRM: u32 = 14;
pub const SIGTERM: u32 = 15;
pub const SIGURG: u32 = 16;
pub const SIGSTOP: u32 = 17;
pub const SIGTSTP: u32 = 18;
pub const SIGCONT: u32 = 19;
pub const SIGCHLD: u32 = 20;
pub const SIGTTIN: u32 = 21;
pub const SIGTTOU: u32 = 22;
pub const SIGIO: u32 = 23;
pub const SIGXCPU: u32 = 24;
pub const SIGXFSZ: u32 = 25;
pub const SIGVTALRM: u32 = 26;
pub const SIGPROF: u32 = 27;
pub const SIGWINCH: u32 = 28;
pub const SIGINFO: u32 = 29;
pub const SIGUSR1: u32 = 30;
pub const SIGUSR2: u32 = 31;
pub const SIGTHR: u32 = 32;
pub const SIGLIBRT: u32 = 33;
pub const SIGRTMIN: u32 = 65;
pub const SIGRTMAX: u32 = 126;

/// `sa_flags`.
pub const SA_ONSTACK: i32 = 0x1;
pub const SA_RESTART: i32 = 0x2;
pub const SA_RESETHAND: i32 = 0x4;
pub const SA_NOCLDSTOP: i32 = 0x8;
pub const SA_NODEFER: i32 = 0x10;
pub const SA_NOCLDWAIT: i32 = 0x20;
pub const SA_SIGINFO: i32 = 0x40;

/// `ss_flags`.
pub const SS_ONSTACK: i32 = 0x1;
pub const SS_DISABLE: i32 = 0x4;
/// The smallest alternate stack (`MINSIGSTKSZ`) on amd64 and arm64.
pub const MINSIGSTKSZ_X86: u64 = 2048;
pub const MINSIGSTKSZ_ARM: u64 = 4096;
#[cfg(target_arch = "x86_64")]
pub const MINSIGSTKSZ: u64 = MINSIGSTKSZ_X86;
#[cfg(target_arch = "aarch64")]
pub const MINSIGSTKSZ: u64 = MINSIGSTKSZ_ARM;

/// `si_code`s: who sent the signal, or why the kernel did.
pub mod code {
    pub const SI_NOINFO: i32 = 0;
    pub const SI_USER: i32 = 0x10001;
    pub const SI_QUEUE: i32 = 0x10002;
    pub const SI_TIMER: i32 = 0x10003;
    pub const SI_ASYNCIO: i32 = 0x10004;
    pub const SI_MESGQ: i32 = 0x10005;
    pub const SI_KERNEL: i32 = 0x10006;
    pub const SI_LWP: i32 = 0x10007;
    pub const ILL_ILLOPC: i32 = 1;
    pub const ILL_ILLOPN: i32 = 2;
    pub const ILL_ILLADR: i32 = 3;
    pub const ILL_ILLTRP: i32 = 4;
    pub const ILL_PRVOPC: i32 = 5;
    pub const ILL_PRVREG: i32 = 6;
    pub const ILL_COPROC: i32 = 7;
    pub const ILL_BADSTK: i32 = 8;
    pub const BUS_ADRALN: i32 = 1;
    pub const BUS_ADRERR: i32 = 2;
    pub const BUS_OBJERR: i32 = 3;
    pub const BUS_OOMERR: i32 = 100;
    pub const SEGV_MAPERR: i32 = 1;
    pub const SEGV_ACCERR: i32 = 2;
    pub const FPE_INTOVF: i32 = 1;
    pub const FPE_INTDIV: i32 = 2;
    pub const FPE_FLTDIV: i32 = 3;
    pub const FPE_FLTOVF: i32 = 4;
    pub const FPE_FLTUND: i32 = 5;
    pub const FPE_FLTRES: i32 = 6;
    pub const FPE_FLTINV: i32 = 7;
    pub const FPE_FLTSUB: i32 = 8;
    pub const FPE_FLTIDO: i32 = 9;
    pub const TRAP_BRKPT: i32 = 1;
    pub const TRAP_TRACE: i32 = 2;
    pub const CLD_EXITED: i32 = 1;
    pub const CLD_KILLED: i32 = 2;
    pub const CLD_DUMPED: i32 = 3;
    pub const CLD_TRAPPED: i32 = 4;
    pub const CLD_STOPPED: i32 = 5;
    pub const CLD_CONTINUED: i32 = 6;
}

/// `sigprocmask` operations.
pub const SIG_BLOCK: u32 = 1;
pub const SIG_UNBLOCK: u32 = 2;
pub const SIG_SETMASK: u32 = 3;

/// Handlers that are not addresses.
pub const SIG_DFL: u64 = 0;
pub const SIG_IGN: u64 = 1;

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("_SIG_MAXSIG", SIG_MAXSIG as i64),
    c("SIGHUP", SIGHUP as i64),
    c("SIGINT", SIGINT as i64),
    c("SIGQUIT", SIGQUIT as i64),
    c("SIGILL", SIGILL as i64),
    c("SIGTRAP", SIGTRAP as i64),
    c("SIGABRT", SIGABRT as i64),
    c("SIGEMT", SIGEMT as i64),
    c("SIGFPE", SIGFPE as i64),
    c("SIGKILL", SIGKILL as i64),
    c("SIGBUS", SIGBUS as i64),
    c("SIGSEGV", SIGSEGV as i64),
    c("SIGSYS", SIGSYS as i64),
    c("SIGPIPE", SIGPIPE as i64),
    c("SIGALRM", SIGALRM as i64),
    c("SIGTERM", SIGTERM as i64),
    c("SIGURG", SIGURG as i64),
    c("SIGSTOP", SIGSTOP as i64),
    c("SIGTSTP", SIGTSTP as i64),
    c("SIGCONT", SIGCONT as i64),
    c("SIGCHLD", SIGCHLD as i64),
    c("SIGTTIN", SIGTTIN as i64),
    c("SIGTTOU", SIGTTOU as i64),
    c("SIGIO", SIGIO as i64),
    c("SIGXCPU", SIGXCPU as i64),
    c("SIGXFSZ", SIGXFSZ as i64),
    c("SIGVTALRM", SIGVTALRM as i64),
    c("SIGPROF", SIGPROF as i64),
    c("SIGWINCH", SIGWINCH as i64),
    c("SIGINFO", SIGINFO as i64),
    c("SIGUSR1", SIGUSR1 as i64),
    c("SIGUSR2", SIGUSR2 as i64),
    c("SIGTHR", SIGTHR as i64),
    c("SIGLIBRT", SIGLIBRT as i64),
    c("SIGRTMIN", SIGRTMIN as i64),
    c("SIGRTMAX", SIGRTMAX as i64),
    c("SIG_BLOCK", SIG_BLOCK as i64),
    c("SIG_UNBLOCK", SIG_UNBLOCK as i64),
    c("SIG_SETMASK", SIG_SETMASK as i64),
    c("SA_ONSTACK", SA_ONSTACK as i64),
    c("SA_RESTART", SA_RESTART as i64),
    c("SA_RESETHAND", SA_RESETHAND as i64),
    c("SA_NOCLDSTOP", SA_NOCLDSTOP as i64),
    c("SA_NODEFER", SA_NODEFER as i64),
    c("SA_NOCLDWAIT", SA_NOCLDWAIT as i64),
    c("SA_SIGINFO", SA_SIGINFO as i64),
    c("SS_ONSTACK", SS_ONSTACK as i64),
    c("SS_DISABLE", SS_DISABLE as i64),
    ConstValue {
        c_name: "MINSIGSTKSZ",
        header: "signal.h",
        value: MINSIGSTKSZ_X86 as i64,
        arches: Arches::Amd64,
    },
    ConstValue {
        c_name: "MINSIGSTKSZ",
        header: "signal.h",
        value: MINSIGSTKSZ_ARM as i64,
        arches: Arches::Arm64,
    },
    c("SI_NOINFO", code::SI_NOINFO as i64),
    c("SI_USER", code::SI_USER as i64),
    c("SI_QUEUE", code::SI_QUEUE as i64),
    c("SI_TIMER", code::SI_TIMER as i64),
    c("SI_ASYNCIO", code::SI_ASYNCIO as i64),
    c("SI_MESGQ", code::SI_MESGQ as i64),
    c("SI_KERNEL", code::SI_KERNEL as i64),
    c("SI_LWP", code::SI_LWP as i64),
    c("ILL_ILLOPC", code::ILL_ILLOPC as i64),
    c("ILL_ILLOPN", code::ILL_ILLOPN as i64),
    c("ILL_ILLADR", code::ILL_ILLADR as i64),
    c("ILL_ILLTRP", code::ILL_ILLTRP as i64),
    c("ILL_PRVOPC", code::ILL_PRVOPC as i64),
    c("ILL_PRVREG", code::ILL_PRVREG as i64),
    c("ILL_COPROC", code::ILL_COPROC as i64),
    c("ILL_BADSTK", code::ILL_BADSTK as i64),
    c("BUS_ADRALN", code::BUS_ADRALN as i64),
    c("BUS_ADRERR", code::BUS_ADRERR as i64),
    c("BUS_OBJERR", code::BUS_OBJERR as i64),
    c("BUS_OOMERR", code::BUS_OOMERR as i64),
    c("SEGV_MAPERR", code::SEGV_MAPERR as i64),
    c("SEGV_ACCERR", code::SEGV_ACCERR as i64),
    c("FPE_INTOVF", code::FPE_INTOVF as i64),
    c("FPE_INTDIV", code::FPE_INTDIV as i64),
    c("FPE_FLTDIV", code::FPE_FLTDIV as i64),
    c("FPE_FLTOVF", code::FPE_FLTOVF as i64),
    c("FPE_FLTUND", code::FPE_FLTUND as i64),
    c("FPE_FLTRES", code::FPE_FLTRES as i64),
    c("FPE_FLTINV", code::FPE_FLTINV as i64),
    c("FPE_FLTSUB", code::FPE_FLTSUB as i64),
    c("FPE_FLTIDO", code::FPE_FLTIDO as i64),
    c("TRAP_BRKPT", code::TRAP_BRKPT as i64),
    c("TRAP_TRACE", code::TRAP_TRACE as i64),
    c("CLD_EXITED", code::CLD_EXITED as i64),
    c("CLD_KILLED", code::CLD_KILLED as i64),
    c("CLD_DUMPED", code::CLD_DUMPED as i64),
    c("CLD_TRAPPED", code::CLD_TRAPPED as i64),
    c("CLD_STOPPED", code::CLD_STOPPED as i64),
    c("CLD_CONTINUED", code::CLD_CONTINUED as i64),
];

const fn c(c_name: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header: "signal.h",
        value,
        arches: Arches::All,
    }
}

/// Words in a `sigset_t` (`_SIG_WORDS`).
pub const SIG_WORDS: usize = 4;

/// `sigset_t`: one bit per signal, 128 signals.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sigset {
    pub bits: [u32; SIG_WORDS],
}

/// `struct sigaction`.
///
/// The handler is a union of two function pointers in C; here it is the raw
/// user-space address, which also encodes `SIG_DFL` (0) and `SIG_IGN` (1).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sigaction {
    pub sa_handler: u64,
    pub sa_flags: i32,
    pub sa_mask: Sigset,
    /// The C struct's tail padding, explicit so copies never carry stray
    /// kernel bytes.
    pub _pad: u32,
}

const _: () = assert!(size_of::<Sigaction>() == 32);

/// `siginfo_t`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Siginfo {
    pub si_signo: i32,
    pub si_errno: i32,
    pub si_code: i32,
    pub si_pid: i32,
    pub si_uid: u32,
    pub si_status: i32,
    /// The faulting address, for a fault.
    pub si_addr: u64,
    /// `union sigval`, as its pointer member.
    pub si_value: u64,
    /// The `_reason` union: `si_trapno` (a fault's machine trap number),
    /// `si_timerid`, and the rest.
    pub si_trapno: i32,
    pub _reason: [i32; 9],
}

/// `stack_t`: an alternate signal stack.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StackT {
    pub ss_sp: u64,
    pub ss_size: u64,
    pub ss_flags: i32,
    /// The C struct's tail padding.
    pub _pad: u32,
}

impl Sigset {
    /// The bit of signal `sig` (1-based): its word and mask.
    const fn bit(sig: u32) -> (usize, u32) {
        (((sig - 1) / 32) as usize, 1 << ((sig - 1) % 32))
    }

    /// Whether `sig` is in the set.
    #[must_use]
    pub const fn has(&self, sig: u32) -> bool {
        let (w, b) = Self::bit(sig);
        self.bits[w] & b != 0
    }

    pub const fn add(&mut self, sig: u32) {
        let (w, b) = Self::bit(sig);
        self.bits[w] |= b;
    }

    pub const fn remove(&mut self, sig: u32) {
        let (w, b) = Self::bit(sig);
        self.bits[w] &= !b;
    }

    #[must_use]
    pub fn union(self, other: Self) -> Self {
        let mut r = self;
        for (a, b) in r.bits.iter_mut().zip(other.bits) {
            *a |= b;
        }
        r
    }

    /// The signals in both.
    #[must_use]
    pub fn intersect(self, other: Self) -> Self {
        let mut r = self;
        for (a, b) in r.bits.iter_mut().zip(other.bits) {
            *a &= b;
        }
        r
    }

    /// The signals in `self` but not in `other`.
    #[must_use]
    pub fn minus(self, other: Self) -> Self {
        let mut r = self;
        for (a, b) in r.bits.iter_mut().zip(other.bits) {
            *a &= !b;
        }
        r
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bits.iter().all(|&w| w == 0)
    }

    /// The lowest-numbered signal in the set (FreeBSD's `sig_ffs`).
    #[must_use]
    pub fn first(&self) -> Option<u32> {
        self.bits
            .iter()
            .enumerate()
            .find(|(_, w)| **w != 0)
            .map(|(i, w)| u32::try_from(i).unwrap_or(0) * 32 + w.trailing_zeros() + 1)
    }
}
