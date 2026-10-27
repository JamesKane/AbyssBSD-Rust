// SPDX-License-Identifier: BSD-2-Clause
//! The filters that need nothing of the kernel: `EVFILT_USER`, and
//! `EVFILT_READ` on a kqueue.

use alloc::boxed::Box;
use alloc::sync::Arc;

use abi::errno::Errno;
use abi::event::{
    EV_CLEAR, Kevent, NOTE_FFAND, NOTE_FFCOPY, NOTE_FFCTRLMASK, NOTE_FFLAGSMASK, NOTE_FFNOP,
    NOTE_FFOR, NOTE_TRIGGER,
};

use crate::kqueue::{Filter, Knote, Kqueue, Note};

/// `EVFILT_USER`: an event user code triggers with `NOTE_TRIGGER` and
/// whose flags it sets with `NOTE_FFAND`, `NOTE_FFOR`, and `NOTE_FFCOPY`
/// (`user_filtops`).
#[derive(Debug, Default)]
pub struct UserFilter;

impl Filter for UserFilter {
    fn attach(&self, kn: &Arc<Knote>) -> Result<(), Errno> {
        // `filt_userattach` looks at `kn_fflags`, which registration has
        // just zeroed: a new event is never triggered.
        let _ = kn;
        Ok(())
    }

    fn event(&self, _kn: &Knote, note: &mut Note, _hint: u64) -> bool {
        note.hook != 0
    }

    fn touches(&self) -> bool {
        true
    }

    fn register(&self, _kn: &Knote, note: &mut Note, kev: &Kevent) {
        if kev.fflags & NOTE_TRIGGER != 0 {
            note.hook = 1;
        }
        let ffctrl = kev.fflags & NOTE_FFCTRLMASK;
        let ff = kev.fflags & NOTE_FFLAGSMASK;
        match ffctrl {
            NOTE_FFAND => note.sfflags &= ff,
            NOTE_FFOR => note.sfflags |= ff,
            NOTE_FFCOPY => note.sfflags = ff,
            // NOTE_FFNOP, and nothing else is possible in two bits.
            _ => debug_assert_eq!(ffctrl, NOTE_FFNOP),
        }
        note.sdata = kev.data;
        if kev.flags & EV_CLEAR != 0 {
            note.hook = 0;
            note.kev.data = 0;
            note.kev.fflags = 0;
        }
    }

    fn process(&self, _kn: &Knote, note: &mut Note) -> Kevent {
        let mut kev = note.kev;
        kev.fflags = note.sfflags;
        kev.data = note.sdata;
        if note.kev.flags & EV_CLEAR != 0 {
            note.hook = 0;
            note.kev.data = 0;
            note.kev.fflags = 0;
        }
        kev
    }

    fn fork(&self, _kn: &Knote) -> Option<Box<dyn Filter>> {
        Some(Box::new(Self))
    }
}

/// `EVFILT_READ` on a kqueue: active while it has events queued, `data`
/// their number (`kqread_filtops`).
#[derive(Debug)]
pub struct KqueueFilter(pub Arc<Kqueue>);

impl Filter for KqueueFilter {
    fn attach(&self, kn: &Arc<Knote>) -> Result<(), Errno> {
        self.0.sel.add(kn);
        Ok(())
    }

    fn detach(&self, kn: &Knote) {
        self.0.sel.remove(kn);
    }

    fn event(&self, _kn: &Knote, note: &mut Note, _hint: u64) -> bool {
        note.kev.data = i64::try_from(self.0.pending()).unwrap_or(i64::MAX);
        note.kev.data > 0
    }
}
