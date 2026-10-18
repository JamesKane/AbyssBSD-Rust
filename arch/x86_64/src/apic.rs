// SPDX-License-Identifier: BSD-2-Clause
//! The local APIC (x2APIC through MSRs, or xAPIC through MMIO), its timer,
//! and the I/O APICs.
//!
//! Vector plan: 0x20–0x2f unused (legacy PIC range, masked), 0x30–0xef
//! device interrupts, 0xf0 local timer, 0xf1–0xfd IPIs, 0xfe APIC error,
//! 0xff spurious.
//!
//! Behavior reference: sys/x86/x86/local_apic.c, sys/x86/x86/io_apic.c
//! (Intel SDM Vol. 3A ch. 11 for the registers).

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::cpu::{self, outb, rdmsr, wrmsr};

/// The local timer's vector.
pub const TIMER_VECTOR: u8 = 0xf0;
/// The first IPI vector.
pub const IPI_BASE: u8 = 0xf1;
/// The APIC error vector.
pub const ERROR_VECTOR: u8 = 0xfe;
/// The spurious-interrupt vector.
pub const SPURIOUS_VECTOR: u8 = 0xff;

const MSR_APIC_BASE: u32 = 0x1b;
const MSR_TSC_DEADLINE: u32 = 0x6e0;
const APIC_BASE_ENABLE: u64 = 1 << 11;
const APIC_BASE_X2APIC: u64 = 1 << 10;

// Register offsets (xAPIC MMIO; x2APIC MSR = 0x800 + offset / 16).
const REG_ID: u32 = 0x20;
const REG_TPR: u32 = 0x80;
const REG_EOI: u32 = 0xb0;
const REG_SVR: u32 = 0xf0;
const REG_ESR: u32 = 0x280;
const REG_ICR_LOW: u32 = 0x300;
const REG_ICR_HIGH: u32 = 0x310;
const REG_LVT_TIMER: u32 = 0x320;
const REG_LVT_LINT0: u32 = 0x350;
const REG_LVT_LINT1: u32 = 0x360;
const REG_LVT_ERROR: u32 = 0x370;
const REG_TIMER_INITIAL: u32 = 0x380;
const REG_TIMER_CURRENT: u32 = 0x390;
const REG_TIMER_DIVIDE: u32 = 0x3e0;

const LVT_MASKED: u32 = 1 << 16;
const LVT_TSC_DEADLINE: u32 = 0b10 << 17;

/// How the local APIC is reached. Set once on the boot CPU.
static X2APIC: AtomicBool = AtomicBool::new(false);
/// The xAPIC register window (a kernel virtual address), when not x2APIC.
static XAPIC_BASE: AtomicU64 = AtomicU64::new(0);
static TSC_DEADLINE: AtomicBool = AtomicBool::new(false);
/// Local timer ticks per TSC tick, scaled by 2^32, for one-shot mode.
static TIMER_PER_TSC: AtomicU64 = AtomicU64::new(0);

/// Which register interface is in use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// MSR access, 32-bit IDs.
    X2Apic,
    /// MMIO access, 8-bit IDs.
    XApic,
}

/// Whether the CPU supports x2APIC mode.
#[must_use]
pub fn x2apic_supported() -> bool {
    core::arch::x86_64::__cpuid(1).ecx & (1 << 21) != 0
}

/// Whether the local timer supports TSC-deadline mode.
#[must_use]
pub fn tsc_deadline_supported() -> bool {
    core::arch::x86_64::__cpuid(1).ecx & (1 << 24) != 0
}

/// The physical address of the xAPIC window, from `IA32_APIC_BASE`.
#[must_use]
pub fn xapic_phys() -> u64 {
    // SAFETY: the MSR exists on every x86_64 CPU.
    unsafe { rdmsr(MSR_APIC_BASE) & 0x000f_ffff_ffff_f000 }
}

/// Chooses the register interface for every CPU. `xapic_va` maps the
/// xAPIC window and is used only if x2APIC is unavailable.
///
/// # Safety
///
/// Call once on the boot CPU before any other function here; `xapic_va`
/// must map [`xapic_phys`] uncached for the kernel's lifetime.
pub unsafe fn init(xapic_va: Option<u64>) -> Mode {
    if x2apic_supported() {
        X2APIC.store(true, Ordering::Relaxed);
        TSC_DEADLINE.store(tsc_deadline_supported(), Ordering::Relaxed);
        Mode::X2Apic
    } else {
        XAPIC_BASE.store(xapic_va.unwrap_or(0), Ordering::Relaxed);
        TSC_DEADLINE.store(tsc_deadline_supported(), Ordering::Relaxed);
        Mode::XApic
    }
}

fn x2apic() -> bool {
    // Relaxed: written once before other CPUs or interrupts exist.
    X2APIC.load(Ordering::Relaxed)
}

fn read(reg: u32) -> u32 {
    if x2apic() {
        // SAFETY: x2APIC mode is enabled on this CPU (`init_cpu`), so the
        // register MSRs exist; reads have no side effects here.
        let v = unsafe { rdmsr(0x800 + reg / 16) };
        // The registers are 32 bits wide (the ICR is not read here).
        u32::try_from(v & 0xffff_ffff).unwrap_or(0)
    } else {
        let p = (XAPIC_BASE.load(Ordering::Relaxed) + u64::from(reg)) as *const u32;
        // SAFETY: the window maps the xAPIC registers uncached (`init`).
        unsafe { p.read_volatile() }
    }
}

fn write(reg: u32, v: u32) {
    if x2apic() {
        // SAFETY: as in `read`.
        unsafe { wrmsr(0x800 + reg / 16, u64::from(v)) };
    } else {
        let p = (XAPIC_BASE.load(Ordering::Relaxed) + u64::from(reg)) as *mut u32;
        // SAFETY: as in `read`.
        unsafe { p.write_volatile(v) };
    }
}

/// Enables and configures this CPU's local APIC: software enable with the
/// spurious vector, everything masked except the error interrupt.
///
/// # Safety
///
/// Call once per CPU, after [`init`], with interrupts masked.
pub unsafe fn init_cpu() {
    // SAFETY: enabling the APIC (and x2APIC mode when chosen) is what the
    // caller asked for; the base address stays as firmware set it.
    unsafe {
        let mut base = rdmsr(MSR_APIC_BASE) | APIC_BASE_ENABLE;
        if x2apic() {
            base |= APIC_BASE_X2APIC;
        }
        wrmsr(MSR_APIC_BASE, base);
    }
    write(REG_TPR, 0);
    write(REG_LVT_TIMER, LVT_MASKED | u32::from(TIMER_VECTOR));
    write(REG_LVT_LINT0, LVT_MASKED);
    write(REG_LVT_LINT1, LVT_MASKED);
    write(REG_LVT_ERROR, u32::from(ERROR_VECTOR));
    // Clear stale errors (the ESR is write-then-read).
    write(REG_ESR, 0);
    write(REG_ESR, 0);
    write(REG_SVR, 0x100 | u32::from(SPURIOUS_VECTOR));
    eoi();
}

/// This CPU's APIC ID.
#[must_use]
pub fn id() -> u32 {
    let v = read(REG_ID);
    if x2apic() { v } else { v >> 24 }
}

/// Signals the end of the interrupt being serviced.
pub fn eoi() {
    write(REG_EOI, 0);
}

/// Reads the ICR low half (for measuring register access cost).
pub fn probe_read() -> u32 {
    read(REG_ICR_LOW)
}

/// Writes the task priority (0, its normal value) (for measuring register
/// access cost).
pub fn probe_write() {
    write(REG_TPR, 0);
}

/// Sends a fixed IPI with `vector` to the CPU with APIC ID `dest`.
pub fn send_ipi(dest: u32, vector: u8) {
    send_icr(dest, u32::from(vector));
}

/// Sends `vector` to every CPU but this one (the "all excluding self"
/// shorthand: one register write instead of one per CPU).
pub fn send_ipi_all_but_self(vector: u8) {
    send_icr(0, u32::from(vector) | (0b11 << 18));
}

/// Sends INIT to `dest` (SMP start-up).
pub fn send_init(dest: u32) {
    // INIT, level assert, edge.
    send_icr(dest, 0x0000_4500);
}

/// Sends a start-up IPI pointing at real-mode page `page` (SMP start-up).
pub fn send_startup(dest: u32, page: u8) {
    send_icr(dest, 0x0000_4600 | u32::from(page));
}

fn send_icr(dest: u32, low: u32) {
    if x2apic() {
        // SAFETY: x2APIC mode; the ICR is one 64-bit MSR.
        unsafe { wrmsr(0x830, (u64::from(dest) << 32) | u64::from(low)) };
    } else {
        // Wait for an earlier IPI to leave (bit 12, send pending) before
        // reusing the register, rather than after each send: under a
        // hypervisor every access is an exit, and delivery is usually done.
        while read(REG_ICR_LOW) & (1 << 12) != 0 {
            core::hint::spin_loop();
        }
        // A destination shorthand ignores the high half.
        if low & (0b11 << 18) == 0 {
            write(REG_ICR_HIGH, dest << 24);
        }
        write(REG_ICR_LOW, low);
    }
}

/// Calibrates the local timer against the TSC for one-shot mode: `wait`
/// must busy-wait a known interval and return the TSC ticks it took.
///
/// # Safety
///
/// Call on the boot CPU after [`init_cpu`], with interrupts masked.
pub unsafe fn calibrate_timer(wait: impl FnOnce() -> u64) {
    if TSC_DEADLINE.load(Ordering::Relaxed) {
        return;
    }
    write(REG_TIMER_DIVIDE, 0b1011); // divide by 1
    write(REG_LVT_TIMER, LVT_MASKED | u32::from(TIMER_VECTOR));
    write(REG_TIMER_INITIAL, u32::MAX);
    let tsc = wait();
    let elapsed = u64::from(u32::MAX - read(REG_TIMER_CURRENT));
    write(REG_TIMER_INITIAL, 0);
    if let Some(ratio) = (elapsed << 32).checked_div(tsc) {
        TIMER_PER_TSC.store(ratio, Ordering::Relaxed);
    }
}

/// Puts this CPU's timer in its working mode (TSC-deadline, or one-shot
/// counting at divide-by-1), so arming it is then a single register write.
/// Call once per CPU after [`calibrate_timer`] ran on the boot CPU.
pub fn configure_timer() {
    if TSC_DEADLINE.load(Ordering::Relaxed) {
        write(REG_LVT_TIMER, LVT_TSC_DEADLINE | u32::from(TIMER_VECTOR));
    } else {
        write(REG_TIMER_INITIAL, 0);
        write(REG_TIMER_DIVIDE, 0b1011);
        write(REG_LVT_TIMER, u32::from(TIMER_VECTOR));
    }
}

/// Whether the timer uses TSC-deadline mode.
#[must_use]
pub fn uses_tsc_deadline() -> bool {
    TSC_DEADLINE.load(Ordering::Relaxed)
}

/// Arms this CPU's timer to fire once at TSC value `deadline` (at once if
/// it has passed). One register write (each is a VM exit under a
/// hypervisor); [`configure_timer`] set the mode.
pub fn set_deadline(deadline: u64) {
    if TSC_DEADLINE.load(Ordering::Relaxed) {
        // SAFETY: TSC-deadline mode is supported and selected
        // (`configure_timer`); this MSR write arms the timer.
        unsafe { wrmsr(MSR_TSC_DEADLINE, deadline.max(1)) };
    } else {
        let now = cpu::timestamp();
        let delta = deadline.saturating_sub(now).max(1);
        let ticks = (u128::from(delta) * u128::from(TIMER_PER_TSC.load(Ordering::Relaxed))) >> 32;
        let ticks = u32::try_from(ticks).unwrap_or(u32::MAX).max(1);
        write(REG_TIMER_INITIAL, ticks);
    }
}

/// Disarms this CPU's timer.
pub fn cancel_deadline() {
    if TSC_DEADLINE.load(Ordering::Relaxed) {
        // SAFETY: writing 0 disarms the deadline.
        unsafe { wrmsr(MSR_TSC_DEADLINE, 0) };
    } else {
        write(REG_TIMER_INITIAL, 0);
    }
}

/// Masks the legacy 8259 PICs, which firmware may have left programmed.
pub fn mask_legacy_pics() {
    // SAFETY: writing the interrupt mask registers of both PICs only masks
    // their inputs.
    unsafe {
        outb(0x21, 0xff);
        outb(0xa1, 0xff);
    }
}

/// An I/O APIC.
#[derive(Debug)]
pub struct IoApic {
    base: u64,
    /// The first global system interrupt it handles.
    pub gsi_base: u32,
    /// How many inputs it has.
    pub inputs: u32,
}

impl IoApic {
    /// The I/O APIC whose registers are mapped at `va`.
    ///
    /// # Safety
    ///
    /// `va` must map an I/O APIC's register window uncached for the
    /// kernel's lifetime, and nothing else may use it.
    pub unsafe fn new(va: u64, gsi_base: u32) -> Self {
        let mut io = Self {
            base: va,
            gsi_base,
            inputs: 0,
        };
        io.inputs = ((io.read(1) >> 16) & 0xff) + 1;
        io
    }

    fn read(&self, reg: u32) -> u32 {
        // SAFETY: the window is ours (`new`): select, then read.
        unsafe {
            (self.base as *mut u32).write_volatile(reg);
            ((self.base + 0x10) as *const u32).read_volatile()
        }
    }

    fn write(&self, reg: u32, v: u32) {
        // SAFETY: as in `read`.
        unsafe {
            (self.base as *mut u32).write_volatile(reg);
            ((self.base + 0x10) as *mut u32).write_volatile(v);
        }
    }

    /// Masks every input.
    pub fn mask_all(&self) {
        for i in 0..self.inputs {
            self.write(0x10 + 2 * i, LVT_MASKED);
            self.write(0x11 + 2 * i, 0);
        }
    }

    /// Routes input `pin` to `vector` on APIC `dest`, masked or not.
    pub fn route(
        &self,
        pin: u32,
        vector: u8,
        dest: u32,
        level: bool,
        active_low: bool,
        masked: bool,
    ) {
        let mut low = u32::from(vector);
        if active_low {
            low |= 1 << 13;
        }
        if level {
            low |= 1 << 15;
        }
        if masked {
            low |= LVT_MASKED;
        }
        self.write(0x11 + 2 * pin, dest << 24);
        self.write(0x10 + 2 * pin, low);
    }

    /// Masks or unmasks input `pin`, keeping its routing.
    pub fn set_masked(&self, pin: u32, masked: bool) {
        let low = self.read(0x10 + 2 * pin);
        let low = if masked {
            low | LVT_MASKED
        } else {
            low & !LVT_MASKED
        };
        self.write(0x10 + 2 * pin, low);
    }
}
