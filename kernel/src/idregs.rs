// SPDX-License-Identifier: BSD-2-Clause
//! arm64: the ID registers user mode sees, one view for the whole system
//! merged over every CPU ([`idregs::UserView`]), the `AT_HWCAP` and
//! `AT_HWCAP2` words exec gives programs, and the emulation of the `mrs`
//! that reads the view (FreeBSD's `identcpu.c`, `user_idreg_handler`).
//!
//! Each CPU records its registers as it comes up; once all are up, the
//! view is made in CPU order and pointer authentication, which the kernel
//! does not enable for user mode, is hidden.

use hal::{CpuId, MAX_CPUS};
use idregs::{IdRegs, MrsRead, OwnReg, Reg, UserView};
use sync::{Once, SpinLock};

use crate::arch::trap::TrapFrame;

static CPUS: SpinLock<[Option<IdRegs>; MAX_CPUS]> = SpinLock::new([None; MAX_CPUS]);

/// The view, and its hwcaps.
static VIEW: Once<(UserView, u64, u64)> = Once::new();

/// Records this CPU's ID registers. Runs on each CPU as it comes up.
pub(crate) fn record_this_cpu(cpu: CpuId) {
    let mut regs = IdRegs::default();
    for r in Reg::ALL {
        let (crm, op2) = r.encoding();
        regs.set(r, crate::arch::cpu::id_register(crm, op2));
    }
    CPUS.lock()[cpu.index()] = Some(regs);
}

/// Makes the view from every CPU that came up (FreeBSD's
/// `identify_hwcaps_sysinit`). Call once they all have.
pub(crate) fn finish() {
    let cpus = *CPUS.lock();
    let mut all = cpus.iter().flatten();
    let Some(boot) = all.next() else {
        return;
    };
    let mut view = UserView::new(boot);
    for cpu in all {
        view.add_cpu(cpu);
    }
    view.hide_pointer_auth();
    let (hwcap, hwcap2) = view.hwcaps();
    let _ = VIEW.set((view, hwcap, hwcap2));
}

/// `AT_HWCAP` and `AT_HWCAP2`.
pub(crate) fn hwcaps() -> (u64, u64) {
    VIEW.get().map_or((0, 0), |&(_, h, h2)| (h, h2))
}

/// Emulates `read`, a user-mode `mrs` of the ID register space that
/// trapped (`EC` 0x18) or was undefined (`EC` 0, on CPUs without
/// FEAT_IDST): writes the destination and steps past it. `false` if it is
/// not one the view answers.
pub(crate) fn emulate_mrs(frame: &mut TrapFrame, read: Option<MrsRead>) -> bool {
    let Some(m) = read else {
        return false;
    };
    let Some(&(view, _, _)) = VIEW.get() else {
        return false;
    };
    let own = |r| match r {
        OwnReg::Midr => crate::arch::cpu::midr(),
        OwnReg::Mpidr => crate::arch::cpu::mpidr_el1(),
        OwnReg::Revidr => crate::arch::cpu::revidr(),
    };
    let Some(value) = view.read(m.crm, m.op2, own) else {
        return false;
    };
    frame.elr += 4;
    // Register 31 is xzr: the value goes nowhere.
    if let Some(x) = frame.x.get_mut(usize::from(m.rt)) {
        *x = value;
    }
    true
}
