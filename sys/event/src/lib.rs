// SPDX-License-Identifier: BSD-2-Clause
//! Events: how a thread waits for any of several objects (docs/MULTIUSER.md
//! §7).
//!
//! Every object that can be waited on owns a [`Notify`]: the threads
//! blocked in `poll` or `select` on it, and the knotes of kqueues watching
//! it (FreeBSD's `selinfo` with its `knlist`). When its state changes the
//! object calls [`Notify::notify`]; pollers wake and collect again, and
//! each knote's [`Filter`] decides from the object's state whether its
//! event is now active. Filters only read the object, so the event layer
//! is a leaf in every object's lock order.
//!
//! A [`Kqueue`] holds knotes, each one (ident, filter) pair registered by
//! `kevent`, through FreeBSD's states: attached or not, active, queued,
//! disabled. Active knotes wait in the kqueue's queue until a scan reports
//! them; level-triggered ones go back behind it while their condition
//! holds, `EV_CLEAR` ones reset, `EV_DISPATCH` ones disable, and
//! `EV_ONESHOT` ones go.
//!
//! Locks, outermost first: a [`Notify`]'s list, a knote's [`Note`], a
//! kqueue's state. No lock is held across a filter's attach or detach, or
//! across a wakeup of another kqueue's watchers (a kqueue may watch a
//! kqueue that watches it).
//!
//! Behavior reference: sys/kern/kern_event.c, sys/kern/sys_generic.c
//! (`selrecord`, `selwakeup`).

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;
#[cfg(test)]
extern crate std;

mod kqueue;
mod notify;
#[cfg(test)]
mod tests;
mod user;

pub use kqueue::{Filter, Ident, Knote, Kqueue, Note, Source};
pub use notify::{Notify, Wake};
pub use user::{KqueueFilter, UserFilter};

/// `KQ_NEVENTS`: how many events a scan collects between copies out.
pub const KQ_NEVENTS: usize = 8;
