// SPDX-License-Identifier: BSD-2-Clause
//! The IDT, trap entry stubs, and exception decoding.
//!
//! Every vector has a 16-byte stub that pushes a dummy error code (unless
//! the CPU pushed one), then the vector number, and jumps to a common path
//! that saves the general registers into a [`TrapFrame`] and calls
//! [`dispatch`]. A fault at an address listed in the fixup table resumes at
//! its fixup; everything else is reported to the kernel as fatal.

use core::arch::{asm, global_asm};
use core::fmt;

use crate::Racy;
use crate::gdt::{IST_DOUBLE_FAULT, IST_MACHINE_CHECK, IST_NMI, KERNEL_CS};

/// Registers saved on trap entry, in stack order. The layout is shared with
/// the assembly below.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
#[allow(missing_docs)]
pub struct TrapFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rbp: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    pub vector: u64,
    pub error: u64,
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    pub rsp: u64,
    pub ss: u64,
}

global_asm!(
    r#"
    .section .text.trap, "ax"
    .balign 16
    .global abyss_trap_vectors
abyss_trap_vectors:
    .set vector, 0
    .rept 256
    .balign 16
    .if vector == 8 || (vector >= 10 && vector <= 14) || vector == 17 || vector == 21 || vector == 29 || vector == 30
    .else
    push 0
    .endif
    push vector
    jmp abyss_trap_common
    .set vector, vector + 1
    .endr

abyss_trap_common:
    push rax
    push rbx
    push rcx
    push rdx
    push rsi
    push rdi
    push rbp
    push r8
    push r9
    push r10
    push r11
    push r12
    push r13
    push r14
    push r15
    mov rdi, rsp
    cld
    call {dispatch}
    pop r15
    pop r14
    pop r13
    pop r12
    pop r11
    pop r10
    pop r9
    pop r8
    pop rbp
    pop rdi
    pop rsi
    pop rdx
    pop rcx
    pop rbx
    pop rax
    add rsp, 16
    iretq
    "#,
    dispatch = sym dispatch,
);

unsafe extern "C" {
    static abyss_trap_vectors: [[u8; 16]; 256];
}

/// One interrupt gate.
#[repr(C)]
#[derive(Clone, Copy)]
struct Gate {
    offset_low: u16,
    selector: u16,
    ist: u8,
    attributes: u8,
    offset_mid: u16,
    offset_high: u32,
    reserved: u32,
}

const EMPTY_GATE: Gate = Gate {
    offset_low: 0,
    selector: 0,
    ist: 0,
    attributes: 0,
    offset_mid: 0,
    offset_high: 0,
    reserved: 0,
};

// Written once by `init_boot_cpu` on the boot CPU before interrupts; read
// only by the CPU afterwards.
static IDT: Racy<[Gate; 256]> = Racy::new([EMPTY_GATE; 256]);

/// Fills and loads the IDT.
///
/// # Safety
///
/// Call once, on the boot CPU, after the GDT and TSS are loaded.
pub(crate) unsafe fn init_boot_cpu() {
    let idt = IDT.get();
    // SAFETY: single-threaded early boot; the stubs are in our image.
    unsafe {
        for (v, gate) in (*idt).iter_mut().enumerate() {
            let target = (&raw const abyss_trap_vectors[v]) as u64;
            let ist = match v {
                2 => IST_NMI,
                8 => IST_DOUBLE_FAULT,
                18 => IST_MACHINE_CHECK,
                _ => 0,
            };
            let [b0, b1, b2, b3, b4, b5, b6, b7] = target.to_le_bytes();
            *gate = Gate {
                offset_low: u16::from_le_bytes([b0, b1]),
                selector: KERNEL_CS,
                ist,
                // Present, DPL 0, 64-bit interrupt gate (interrupts off).
                attributes: 0x8e,
                offset_mid: u16::from_le_bytes([b2, b3]),
                offset_high: u32::from_le_bytes([b4, b5, b6, b7]),
                reserved: 0,
            };
        }
        #[repr(C, packed)]
        struct Pointer {
            limit: u16,
            base: u64,
        }
        let ptr = Pointer {
            limit: crate::to_u16(size_of::<[Gate; 256]>() - 1),
            base: idt as u64,
        };
        asm!("lidt [{}]", in(reg) &raw const ptr, options(nostack));
    }
}

/// Called from the common stub with interrupts disabled.
extern "C" fn dispatch(frame: &mut TrapFrame) {
    let trap = Trap::decode(frame);
    if matches!(
        trap,
        Trap::PageFault { .. } | Trap::GeneralProtection { .. }
    ) && frame.cs & 3 == 0
        && let Some(fixup) = crate::fixup::lookup(frame.rip)
    {
        frame.rip = fixup;
        return;
    }
    let report = TrapReport {
        frame: *frame,
        trap,
        cr3: crate::cpu::cr3(),
    };
    // SAFETY: defined by the kernel through `kernel_hooks!`.
    unsafe { crate::abyss_fatal_trap(&report) }
}

/// How a page fault happened (the error code).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageFaultCause {
    /// The page was present (a protection violation).
    pub present: bool,
    /// A write.
    pub write: bool,
    /// From user mode.
    pub user: bool,
    /// An instruction fetch.
    pub fetch: bool,
    /// A reserved bit was set in a table entry.
    pub reserved_bit: bool,
}

/// A decoded trap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum Trap {
    DivideError,
    Debug,
    Nmi,
    Breakpoint,
    Overflow,
    BoundRange,
    InvalidOpcode,
    DeviceNotAvailable,
    DoubleFault,
    InvalidTss { selector: u64 },
    SegmentNotPresent { selector: u64 },
    StackFault { selector: u64 },
    GeneralProtection { selector: u64 },
    PageFault { address: u64, cause: PageFaultCause },
    X87,
    AlignmentCheck,
    MachineCheck,
    Simd,
    Virtualization,
    ControlProtection { code: u64 },
    Reserved(u8),
    Interrupt(u8),
}

impl Trap {
    fn decode(f: &TrapFrame) -> Self {
        let e = f.error;
        match f.vector {
            0 => Self::DivideError,
            1 => Self::Debug,
            2 => Self::Nmi,
            3 => Self::Breakpoint,
            4 => Self::Overflow,
            5 => Self::BoundRange,
            6 => Self::InvalidOpcode,
            7 => Self::DeviceNotAvailable,
            8 => Self::DoubleFault,
            10 => Self::InvalidTss { selector: e },
            11 => Self::SegmentNotPresent { selector: e },
            12 => Self::StackFault { selector: e },
            13 => Self::GeneralProtection { selector: e },
            14 => Self::PageFault {
                address: crate::cpu::cr2(),
                cause: PageFaultCause {
                    present: e & 1 != 0,
                    write: e & 2 != 0,
                    user: e & 4 != 0,
                    reserved_bit: e & 8 != 0,
                    fetch: e & 16 != 0,
                },
            },
            16 => Self::X87,
            17 => Self::AlignmentCheck,
            18 => Self::MachineCheck,
            19 => Self::Simd,
            20 => Self::Virtualization,
            21 => Self::ControlProtection { code: e },
            v => {
                let v = u8::try_from(v).unwrap_or(u8::MAX);
                if v < 32 {
                    Self::Reserved(v)
                } else {
                    Self::Interrupt(v)
                }
            }
        }
    }
}

impl fmt::Display for Trap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::PageFault { address, cause } => write!(
                f,
                "page fault at {address:#018x}: {} {} in {} mode{}",
                if cause.present {
                    "protection violation on"
                } else {
                    "unmapped"
                },
                if cause.fetch {
                    "instruction fetch"
                } else if cause.write {
                    "write"
                } else {
                    "read"
                },
                if cause.user { "user" } else { "kernel" },
                if cause.reserved_bit {
                    " (reserved bit set)"
                } else {
                    ""
                },
            ),
            Self::GeneralProtection { selector } => {
                write!(f, "general protection fault (selector {selector:#x})")
            }
            other => write!(f, "{other:?}"),
        }
    }
}

/// Everything known about a fatal trap, for the kernel to report.
#[derive(Clone, Copy, Debug)]
pub struct TrapReport {
    /// The saved registers.
    pub frame: TrapFrame,
    /// What happened.
    pub trap: Trap,
    /// The page-table root at the time.
    pub cr3: u64,
}

impl TrapReport {
    /// The interrupted instruction.
    pub fn pc(&self) -> u64 {
        self.frame.rip
    }

    /// The interrupted code's frame pointer, for a backtrace.
    pub fn frame_pointer(&self) -> u64 {
        self.frame.rbp
    }
}

impl fmt::Display for TrapReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let r = &self.frame;
        writeln!(
            f,
            "trap {}: {} (error {:#x}) on cpu {}",
            r.vector,
            self.trap,
            r.error,
            crate::gdt::cpu_id()
        )?;
        let regs = [
            ("rax", r.rax),
            ("rbx", r.rbx),
            ("rcx", r.rcx),
            ("rdx", r.rdx),
            ("rsi", r.rsi),
            ("rdi", r.rdi),
            ("rbp", r.rbp),
            ("rsp", r.rsp),
            ("r8", r.r8),
            ("r9", r.r9),
            ("r10", r.r10),
            ("r11", r.r11),
            ("r12", r.r12),
            ("r13", r.r13),
            ("r14", r.r14),
            ("r15", r.r15),
            ("rip", r.rip),
            ("rflags", r.rflags),
            ("cs", r.cs),
            ("ss", r.ss),
            ("cr3", self.cr3),
        ];
        for row in regs.chunks(3) {
            for (name, v) in row {
                write!(f, "  {name:>6} {v:#018x}")?;
            }
            writeln!(f)?;
        }
        Ok(())
    }
}
