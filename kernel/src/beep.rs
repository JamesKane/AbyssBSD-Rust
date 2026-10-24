// SPDX-License-Identifier: BSD-2-Clause
//! `sysbeep`: a tone of a pitch for a time on the PC speaker, which
//! the i8254's counter 2 drives on amd64; arm64 has no speaker.
//!
//! Behavior reference: sys/kern/kern_cons.c (`sysbeep`,
//! `sysbeepstop`), sys/x86/isa/clock.c (`timer_spkr_*`).

use abi::errno::Errno;

/// Sounds `pitch` Hz for `duration_ns` (`sysbeep`). A tone already
/// sounding changes pitch and keeps its end.
///
/// # Errors
///
/// `EBUSY` if something else holds the speaker; `ENODEV` without one.
#[cfg(target_arch = "x86_64")]
pub(crate) fn sysbeep(pitch: u32, duration_ns: u64) -> Result<(), Errno> {
    if pitch == 0 {
        return Err(Errno::EINVAL);
    }
    let Some(timer) = x86::STOP.get() else {
        return Err(Errno::ENODEV);
    };
    let mut beeping = x86::BEEPING.lock();
    if !speaker::acquire() && !*beeping {
        // Something else owns it.
        return Err(Errno::EBUSY);
    }
    speaker::set_freq(pitch);
    if !*beeping {
        *beeping = true;
        timer.as_ref().reset(duration_ns, x86::stop, 0);
    }
    Ok(())
}

/// Makes the callout that ends a tone (`sysbeep_init`).
pub(crate) fn init() {
    #[cfg(target_arch = "x86_64")]
    {
        use alloc::boxed::Box;
        use core::pin::Pin;
        let t = Pin::static_ref(Box::leak(Box::new(crate::time::Timer::new())));
        let _ = x86::STOP.set(t);
    }
}

#[cfg(target_arch = "x86_64")]
mod x86 {
    use core::pin::Pin;

    use sched::IrqCx;
    use sync::{Once, SpinLock};

    use crate::time::Timer;

    /// The callout that ends the tone (`beeping_timer`).
    pub(super) static STOP: Once<Pin<&'static Timer>> = Once::new();
    /// Whether a tone sounds (`beeping`).
    pub(super) static BEEPING: SpinLock<bool> = SpinLock::new(false);

    /// `sysbeepstop`.
    pub(super) fn stop(_: usize, _: &IrqCx) {
        let mut beeping = BEEPING.lock();
        super::speaker::release();
        *beeping = false;
    }
}

/// No hardware, no sound.
#[cfg(target_arch = "aarch64")]
pub(crate) fn sysbeep(_pitch: u32, _duration_ns: u64) -> Result<(), Errno> {
    Err(Errno::ENODEV)
}

/// The i8254's counter 2 and the gate to the speaker (port B, 0x61).
#[cfg(target_arch = "x86_64")]
mod speaker {
    use core::sync::atomic::{AtomicBool, Ordering};

    use crate::arch::cpu::{inb, outb};

    const TIMER_CNTR2: u16 = 0x42;
    const TIMER_MODE: u16 = 0x43;
    const TIMER_SEL2: u8 = 0x80;
    const TIMER_SQWAVE: u8 = 0x06;
    const TIMER_16BIT: u8 = 0x30;
    /// `IO_PPI`: the counter 2 gate and the speaker data bit.
    const PPI: u16 = 0x61;
    const PIT_ENABLETMR2: u8 = 0x01;
    const PIT_SPKRDATA: u8 = 0x02;
    /// `i8254_freq`.
    const I8254_FREQ: u32 = 1_193_182;

    /// Counter 2 is the speaker's (`timer2_state`).
    static ACQUIRED: AtomicBool = AtomicBool::new(false);

    /// `timer_spkr_acquire`: square waves, the speaker on; `false` if
    /// it was already acquired.
    pub(super) fn acquire() -> bool {
        if ACQUIRED.swap(true, Ordering::AcqRel) {
            return false;
        }
        // SAFETY: counter 2 and port B are the speaker's alone; the
        // kernel times nothing with counter 2.
        unsafe {
            outb(TIMER_MODE, TIMER_SEL2 | TIMER_SQWAVE | TIMER_16BIT);
            outb(PPI, inb(PPI) | PIT_ENABLETMR2 | PIT_SPKRDATA);
        }
        true
    }

    /// `timer_spkr_release`: the speaker off.
    pub(super) fn release() {
        if !ACQUIRED.swap(false, Ordering::AcqRel) {
            return;
        }
        // SAFETY: as in `acquire`.
        unsafe {
            outb(TIMER_MODE, TIMER_SEL2 | TIMER_SQWAVE | TIMER_16BIT);
            outb(PPI, inb(PPI) & !(PIT_ENABLETMR2 | PIT_SPKRDATA));
        }
    }

    /// `timer_spkr_setfreq`: counter 2's divisor for `freq` Hz.
    pub(super) fn set_freq(freq: u32) {
        let [lo, hi, ..] = (I8254_FREQ / freq).to_le_bytes();
        // SAFETY: as in `acquire`; the caller holds the beep lock, so
        // the two bytes go out together.
        unsafe {
            outb(TIMER_CNTR2, lo);
            outb(TIMER_CNTR2, hi);
        }
    }
}
