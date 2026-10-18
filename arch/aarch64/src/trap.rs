// SPDX-License-Identifier: BSD-2-Clause
//! EL1 exception vectors and exception decoding (Arm ARM D1.10, ESR_EL1
//! encodings in D17.2.37).
//!
//! Each of the 16 vector slots saves `x0`/`x1`, loads its slot number, and
//! branches to a common path that saves the rest of the [`TrapFrame`] and
//! calls [`dispatch`]. A data abort at an address listed in the fixup table
//! resumes at its fixup; everything else is reported to the kernel as
//! fatal.

use core::arch::{asm, global_asm};
use core::fmt;

/// Registers saved on exception entry. The layout is shared with the
/// assembly below.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct TrapFrame {
    /// `x0`–`x30`.
    pub x: [u64; 31],
    /// The interrupted stack pointer.
    pub sp: u64,
    /// `ELR_EL1`: where to resume.
    pub elr: u64,
    /// `SPSR_EL1`: the interrupted PSTATE.
    pub spsr: u64,
    /// `ESR_EL1`: the syndrome.
    pub esr: u64,
    /// `FAR_EL1`: the faulting address, for aborts.
    pub far: u64,
    /// The vector slot (0–15).
    pub slot: u64,
    _pad: u64,
}

const FRAME_SIZE: usize = size_of::<TrapFrame>();
const _: () = assert!(FRAME_SIZE == 304 && FRAME_SIZE.is_multiple_of(16));

global_asm!(
    r#"
    .section .text.trap, "ax"
    .balign 2048
    .global abyss_el1_vectors
abyss_el1_vectors:
    .set slot, 0
    .rept 16
    .balign 128
    sub sp, sp, #{frame}
    stp x0, x1, [sp, #0]
    mov x0, #slot
    b abyss_el1_common
    .set slot, slot + 1
    .endr

abyss_el1_common:
    stp x2, x3, [sp, #16]
    stp x4, x5, [sp, #32]
    stp x6, x7, [sp, #48]
    stp x8, x9, [sp, #64]
    stp x10, x11, [sp, #80]
    stp x12, x13, [sp, #96]
    stp x14, x15, [sp, #112]
    stp x16, x17, [sp, #128]
    stp x18, x19, [sp, #144]
    stp x20, x21, [sp, #160]
    stp x22, x23, [sp, #176]
    stp x24, x25, [sp, #192]
    stp x26, x27, [sp, #208]
    stp x28, x29, [sp, #224]
    add x1, sp, #{frame}
    stp x30, x1, [sp, #240]
    mrs x1, elr_el1
    mrs x2, spsr_el1
    stp x1, x2, [sp, #256]
    mrs x1, esr_el1
    mrs x2, far_el1
    stp x1, x2, [sp, #272]
    str x0, [sp, #288]
    mov x0, sp
    bl {dispatch}
    ldp x1, x2, [sp, #256]
    msr elr_el1, x1
    msr spsr_el1, x2
    ldp x2, x3, [sp, #16]
    ldp x4, x5, [sp, #32]
    ldp x6, x7, [sp, #48]
    ldp x8, x9, [sp, #64]
    ldp x10, x11, [sp, #80]
    ldp x12, x13, [sp, #96]
    ldp x14, x15, [sp, #112]
    ldp x16, x17, [sp, #128]
    ldp x18, x19, [sp, #144]
    ldp x20, x21, [sp, #160]
    ldp x22, x23, [sp, #176]
    ldp x24, x25, [sp, #192]
    ldp x26, x27, [sp, #208]
    ldp x28, x29, [sp, #224]
    ldr x30, [sp, #240]
    ldp x0, x1, [sp, #0]
    add sp, sp, #{frame}
    eret
    "#,
    frame = const FRAME_SIZE,
    dispatch = sym dispatch,
);

unsafe extern "C" {
    static abyss_el1_vectors: [u8; 2048];
}

/// Installs the EL1 vectors.
///
/// # Safety
///
/// Call once per CPU, before exceptions can occur.
pub(crate) unsafe fn init_boot_cpu() {
    let base = &raw const abyss_el1_vectors;
    // SAFETY: the table is 2 KiB aligned and lives for the life of the
    // kernel.
    unsafe { asm!("msr vbar_el1, {}", "isb", in(reg) base, options(nostack)) };
}

/// Called from the common path with interrupts masked.
extern "C" fn dispatch(frame: &mut TrapFrame) {
    let trap = Trap::decode(frame);
    let same_el = frame.slot < 8;
    if same_el
        && matches!(trap, Trap::DataAbort { .. })
        && let Some(fixup) = crate::fixup::lookup(frame.elr)
    {
        frame.elr = fixup;
        return;
    }
    let report = TrapReport {
        frame: *frame,
        trap,
        ttbr0: crate::cpu::read_sysreg!("ttbr0_el1"),
        ttbr1: crate::cpu::read_sysreg!("ttbr1_el1"),
    };
    // SAFETY: defined by the kernel through `kernel_hooks!`.
    unsafe { crate::abyss_fatal_trap(&report) }
}

/// The fault status of an abort (`DFSC`/`IFSC`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultStatus {
    /// No translation at this level.
    Translation(u8),
    /// The access flag was clear at this level.
    AccessFlag(u8),
    /// Permission denied at this level.
    Permission(u8),
    /// An alignment fault.
    Alignment,
    /// Anything else (raw code).
    Other(u8),
}

impl FaultStatus {
    fn decode(iss: u64) -> Self {
        let code = u8::try_from(iss & 0x3f).unwrap_or(0);
        let level = code & 3;
        match code >> 2 {
            0b0001 => Self::Translation(level),
            0b0010 => Self::AccessFlag(level),
            0b0011 => Self::Permission(level),
            _ if code == 0b10_0001 => Self::Alignment,
            _ => Self::Other(code),
        }
    }
}

/// A decoded exception.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trap {
    /// A data abort.
    DataAbort {
        /// The faulting address.
        address: u64,
        /// Whether it was a write.
        write: bool,
        /// From EL0.
        user: bool,
        /// Why.
        status: FaultStatus,
    },
    /// An instruction abort.
    InstructionAbort {
        /// The faulting address.
        address: u64,
        /// From EL0.
        user: bool,
        /// Why.
        status: FaultStatus,
    },
    /// `SVC` from AArch64.
    Svc(u16),
    /// `BRK`.
    Breakpoint(u16),
    /// PC alignment fault.
    PcAlignment,
    /// SP alignment fault.
    SpAlignment,
    /// An undefined or trapped instruction.
    Undefined,
    /// Illegal execution state.
    IllegalState,
    /// An asynchronous SError.
    SError,
    /// An interrupt (IRQ or FIQ).
    Interrupt,
    /// Any other synchronous exception class.
    Other(u8),
}

impl Trap {
    fn decode(f: &TrapFrame) -> Self {
        match f.slot % 4 {
            1 | 2 => return Self::Interrupt,
            3 => return Self::SError,
            _ => {}
        }
        let ec = u8::try_from((f.esr >> 26) & 0x3f).unwrap_or(0);
        let iss = f.esr & 0x01ff_ffff;
        let imm16 = u16::try_from(iss & 0xffff).unwrap_or(0);
        match ec {
            0x00 => Self::Undefined,
            0x0e => Self::IllegalState,
            0x15 => Self::Svc(imm16),
            0x20 | 0x21 => Self::InstructionAbort {
                address: f.far,
                user: ec == 0x20,
                status: FaultStatus::decode(iss),
            },
            0x22 => Self::PcAlignment,
            0x24 | 0x25 => Self::DataAbort {
                address: f.far,
                write: iss & (1 << 6) != 0,
                user: ec == 0x24,
                status: FaultStatus::decode(iss),
            },
            0x26 => Self::SpAlignment,
            0x3c => Self::Breakpoint(imm16),
            ec => Self::Other(ec),
        }
    }
}

impl fmt::Display for Trap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::DataAbort {
                address,
                write,
                user,
                status,
            } => write!(
                f,
                "page fault at {address:#018x}: {} {} in {} mode ({status:?})",
                match status {
                    FaultStatus::Translation(_) => "unmapped",
                    FaultStatus::Permission(_) => "protection violation on",
                    _ => "faulting",
                },
                if write { "write" } else { "read" },
                if user { "user" } else { "kernel" },
            ),
            Self::InstructionAbort {
                address,
                user,
                status,
            } => write!(
                f,
                "page fault at {address:#018x}: instruction fetch in {} mode ({status:?})",
                if user { "user" } else { "kernel" },
            ),
            other => write!(f, "{other:?}"),
        }
    }
}

/// Everything known about a fatal exception, for the kernel to report.
#[derive(Clone, Copy, Debug)]
pub struct TrapReport {
    /// The saved registers.
    pub frame: TrapFrame,
    /// What happened.
    pub trap: Trap,
    /// `TTBR0_EL1` at the time.
    pub ttbr0: u64,
    /// `TTBR1_EL1` at the time.
    pub ttbr1: u64,
}

impl TrapReport {
    /// The interrupted instruction.
    pub fn pc(&self) -> u64 {
        self.frame.elr
    }

    /// The interrupted code's frame pointer, for a backtrace.
    pub fn frame_pointer(&self) -> u64 {
        self.frame.x[29]
    }
}

impl fmt::Display for TrapReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let r = &self.frame;
        const KINDS: [&str; 4] = ["sync", "irq", "fiq", "serror"];
        const ORIGINS: [&str; 4] = ["EL1t", "EL1h", "EL0/64", "EL0/32"];
        let slot = usize::try_from(r.slot).unwrap_or(0) % 16;
        writeln!(
            f,
            "exception ({} from {}): {} (esr {:#x}) on cpu {}",
            KINDS[slot % 4],
            ORIGINS[slot / 4],
            self.trap,
            r.esr,
            crate::cpu::cpu_id()
        )?;
        for (i, row) in r.x.chunks(3).enumerate() {
            for (j, v) in row.iter().enumerate() {
                let n = i * 3 + j;
                let pad = if n < 10 { "   " } else { "  " };
                write!(f, "{pad}x{n} {v:#018x}")?;
            }
            writeln!(f)?;
        }
        writeln!(
            f,
            "    sp {:#018x}   elr {:#018x}  spsr {:#018x}",
            r.sp, r.elr, r.spsr
        )?;
        writeln!(
            f,
            "   far {:#018x} ttbr0 {:#018x} ttbr1 {:#018x}",
            r.far, self.ttbr0, self.ttbr1
        )
    }
}
