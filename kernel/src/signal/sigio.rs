// SPDX-License-Identifier: BSD-2-Clause
//! `SIGIO`'s owner for a device that signals when it has data: the process
//! or process group `FIOSETOWN` names (`fsetown`, `fgetown`, `funsetown`),
//! and the signal sent to it (`pgsigio`), with the credential the owner
//! was set with deciding whom it may reach (`CANSIGIO`).
//!
//! Behavior reference: sys/kern/kern_descrip.c (`fsetown`, `fgetown`,
//! `funsetown`), sys/kern/kern_sig.c (`pgsigio`, `CANSIGIO`).

use alloc::sync::Arc;

use abi::errno::Errno;
use abi::signal::{SIGIO, code};
use sync::SpinLock;

use crate::cred::Ucred;
use crate::proc::Process;

/// Whom the signal goes to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Owner {
    Process(i32),
    Group(i32),
}

/// A device's `SIGIO` owner (FreeBSD's `struct sigio *`).
pub(crate) struct Sigio(SpinLock<Option<(Owner, Arc<Ucred>)>>);

impl Sigio {
    pub(crate) const fn new() -> Self {
        Self(SpinLock::new(None))
    }

    /// `fsetown(pgid)`: a process (`pgid > 0`) or process group (`< 0`)
    /// in the caller's session; 0 clears the owner.
    pub(crate) fn set(&self, pgid: i32) -> Result<(), Errno> {
        if pgid == 0 {
            self.clear();
            return Ok(());
        }
        let me = Process::current().ok_or(Errno::ESRCH)?;
        let owner = if pgid > 0 {
            let p = crate::proc::find(pgid).ok_or(Errno::ESRCH)?;
            // No owner in another session.
            if p.sid() != me.sid() {
                return Err(Errno::EPERM);
            }
            Owner::Process(pgid)
        } else {
            let g = pgid.checked_neg().ok_or(Errno::ESRCH)?;
            let member = crate::proc::all()
                .into_iter()
                .find(|p| p.pgid() == g && !p.has_exited())
                .ok_or(Errno::ESRCH)?;
            if member.sid() != me.sid() {
                return Err(Errno::EPERM);
            }
            Owner::Group(g)
        };
        *self.0.lock() = Some((owner, me.cred()));
        Ok(())
    }

    /// `fgetown`: the owner's pid, its group's negated, or 0.
    pub(crate) fn get(&self) -> i32 {
        match self.0.lock().as_ref() {
            Some((Owner::Process(p), _)) => *p,
            Some((Owner::Group(g), _)) => -*g,
            None => 0,
        }
    }

    /// `funsetown`.
    pub(crate) fn clear(&self) {
        *self.0.lock() = None;
    }

    /// `pgsigio(SIGIO)`: to the owner, or each process of its group, that
    /// the owner's credential may signal.
    pub(crate) fn signal(&self) {
        self.signal_with(SIGIO);
    }

    /// `pgsigio(sig)`: [`Sigio::signal`] with another signal, as `bpf`'s
    /// `BIOCSRSIG` chooses; 0 sends none.
    pub(crate) fn signal_with(&self, sig: u32) {
        if sig == 0 {
            return;
        }
        let Some((owner, cred)) = self.0.lock().clone() else {
            return;
        };
        // `CANSIGIO`: the superuser, or a real or effective uid in common.
        let may = |p: &Process| {
            let c = p.cred();
            cred.uid == 0
                || cred.ruid == c.ruid
                || cred.uid == c.ruid
                || cred.ruid == c.uid
                || cred.uid == c.uid
        };
        let info = super::user_info(sig, 0, code::SI_KERNEL);
        match owner {
            Owner::Process(pid) => {
                if let Some(p) = crate::proc::find(pid).filter(|p| !p.has_exited() && may(p)) {
                    let _ = super::post(&p, info);
                }
            }
            Owner::Group(g) => {
                for p in crate::proc::all()
                    .iter()
                    .filter(|p| p.pgid() == g && !p.has_exited() && may(p))
                {
                    let _ = super::post(p, info);
                }
            }
        }
    }
}
