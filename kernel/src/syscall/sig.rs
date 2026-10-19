// SPDX-License-Identifier: BSD-2-Clause
//! Signal state: `sigaction`, `sigprocmask`, `sigfastblock`. The state is
//! kept as FreeBSD keeps it; delivery comes with P4.4.
//!
//! Behavior reference: sys/kern/kern_sig.c.

use abi::errno::Errno;
use abi::signal::{
    SIG_BLOCK, SIG_DFL, SIG_MAXSIG, SIG_SETMASK, SIG_UNBLOCK, SIGKILL, SIGSTOP, Sigaction, Sigset,
};
use abi::thr::{SIGFASTBLOCK_SETPTR, SIGFASTBLOCK_UNBLOCK, SIGFASTBLOCK_UNSETPTR};

use super::{SysResult, process};
use crate::uaccess::{Plain, UserPtr};

// SAFETY: integers only, no padding (`abi-check` holds the C layout).
unsafe impl Plain for Sigset {}
// SAFETY: as above; the C struct's tail padding is an explicit field.
unsafe impl Plain for Sigaction {}

/// Signals that can be neither caught nor blocked.
fn unblockable(set: &mut Sigset) {
    for sig in [SIGKILL, SIGSTOP] {
        set.bits[((sig - 1) / 32) as usize] &= !(1 << ((sig - 1) % 32));
    }
}

/// `sigaction(sig, act, oact)`.
pub(super) fn sigaction(sig: u64, act: u64, oact: u64) -> SysResult {
    let sig = u32::try_from(sig)
        .ok()
        .filter(|s| (1..=SIG_MAXSIG).contains(s))
        .ok_or(Errno::EINVAL)?;
    let new = if act == 0 {
        None
    } else {
        Some(
            UserPtr::<Sigaction>::new(act)
                .read()
                .map_err(|_| Errno::EFAULT)?,
        )
    };
    if let Some(n) = &new
        && (sig == SIGKILL || sig == SIGSTOP)
        && n.sa_handler != SIG_DFL
    {
        return Err(Errno::EINVAL);
    }
    let p = process()?;
    let old = {
        let mut sigs = p.signals.lock();
        let slot = &mut sigs.actions[(sig - 1) as usize];
        let old = *slot;
        if let Some(mut n) = new {
            unblockable(&mut n.sa_mask);
            *slot = n;
        }
        old
    };
    if oact != 0 {
        UserPtr::<Sigaction>::new(oact)
            .write(&old)
            .map_err(|_| Errno::EFAULT)?;
    }
    Ok(0)
}

/// `sigprocmask(how, set, oset)`.
pub(super) fn sigprocmask(how: u64, set: u64, oset: u64) -> SysResult {
    let new = if set == 0 {
        None
    } else {
        Some(
            UserPtr::<Sigset>::new(set)
                .read()
                .map_err(|_| Errno::EFAULT)?,
        )
    };
    let p = process()?;
    let old = {
        let mut sigs = p.signals.lock();
        let old = sigs.mask;
        if let Some(n) = new {
            let how = u32::try_from(how).map_err(|_| Errno::EINVAL)?;
            let mut mask = old;
            for (m, b) in mask.bits.iter_mut().zip(n.bits) {
                *m = match how {
                    SIG_BLOCK => *m | b,
                    SIG_UNBLOCK => *m & !b,
                    SIG_SETMASK => b,
                    _ => return Err(Errno::EINVAL),
                };
            }
            unblockable(&mut mask);
            sigs.mask = mask;
        }
        old
    };
    if oset != 0 {
        UserPtr::<Sigset>::new(oset)
            .write(&old)
            .map_err(|_| Errno::EFAULT)?;
    }
    Ok(0)
}

/// `sigfastblock(cmd, ptr)`: the word libthr uses to block signals
/// without a system call.
pub(super) fn sigfastblock(cmd: u64, ptr: u64) -> SysResult {
    let p = process()?;
    let mut sigs = p.signals.lock();
    match u32::try_from(cmd).map_err(|_| Errno::EINVAL)? {
        SIGFASTBLOCK_SETPTR => {
            if sigs.fastblock != 0 {
                return Err(Errno::EBUSY);
            }
            if !ptr.is_multiple_of(4) || ptr == 0 {
                return Err(Errno::EINVAL);
            }
            sigs.fastblock = ptr;
        }
        SIGFASTBLOCK_UNBLOCK => {}
        SIGFASTBLOCK_UNSETPTR => {
            if sigs.fastblock == 0 {
                return Err(Errno::EINVAL);
            }
            sigs.fastblock = 0;
        }
        _ => return Err(Errno::EINVAL),
    }
    Ok(0)
}
