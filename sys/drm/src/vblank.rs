// SPDX-License-Identifier: BSD-2-Clause
//! Waiting for frames: the wait-for-vertical-blank call and the
//! sequence calls (`CRTC_GET_SEQUENCE`, `CRTC_QUEUE_SEQUENCE`).
//!
//! A display controller counts its frames. A caller may sleep until a
//! count, or ask for an event at it; the event's room is reserved when
//! the call is made, so delivering it at the frame allocates nothing.
//! The counts are the ones commits complete on ([`Kms::vblank`]).

use alloc::sync::Weak;
use alloc::vec;
use alloc::vec::Vec;

use abi::drm::*;
use abi::errno::Errno;
use kms_check::ObjId;
use sched::Sleepable;

use crate::card::Open;
use crate::kms::{Kms, Waiter};

/// An event is 32 bytes either way.
const EVENT_LEN: usize = 32;

fn kms_of(f: &Open) -> Result<&Kms, Errno> {
    f.card.driver().kms().ok_or(Errno::ENOTTY)
}

impl Kms {
    /// Registers a wait for frame `target` of controller `crtc`, and
    /// makes sure frames come (the interrupt on, or the timer running).
    fn add_waiter(&self, crtc: usize, target: u64, event: Option<(Weak<Open>, Vec<u8>)>) {
        self.rt[crtc].lock().waiters.push(Waiter { target, event });
        if self.crtcs[crtc].hw.vblank_interrupt() {
            self.crtcs[crtc].hw.vblank_enable(true);
        } else {
            self.arm_timer(crtc);
        }
    }

    fn active(&self, crtc: usize) -> bool {
        self.state.lock().crtcs[crtc].active
    }
}

/// An event of `kind` for `user_data`, to be stamped at its frame.
fn event_for(kind: u32, user_data: u64) -> Vec<u8> {
    let mut ev = vec![0u8; EVENT_LEN];
    DrmEvent { kind, length: 32 }.encode(&mut ev);
    ev[8..16].copy_from_slice(&user_data.to_ne_bytes());
    ev
}

/// `WAIT_VBLANK`: sleep until a frame, or have an event sent at it.
pub(crate) fn wait_vblank(f: &Open, s: &Sleepable, a: &mut DrmWaitVblank) -> Result<usize, Errno> {
    let k = kms_of(f)?;
    let kind = a.kind;
    // Signals are not built.
    if kind & DRM_VBLANK_SIGNAL != 0 {
        return Err(Errno::EINVAL);
    }
    let high = (kind & DRM_VBLANK_HIGH_CRTC_MASK) >> DRM_VBLANK_HIGH_CRTC_SHIFT;
    let ci = if high != 0 {
        usize::try_from(high).map_err(|_| Errno::EINVAL)?
    } else {
        usize::from(kind & DRM_VBLANK_SECONDARY != 0)
    };
    if ci >= k.crtcs.len() || !k.active(ci) {
        return Err(Errno::EINVAL);
    }
    let relative = kind & DRM_VBLANK_RELATIVE != 0;
    let (cur, last) = {
        let r = k.rt[ci].lock();
        (r.seq, r.last_ns)
    };
    // The count the caller names, as the distance from the frame we are
    // at (counts are 32 bits and wrap).
    let delta = if relative {
        i64::from(a.sequence)
    } else {
        i64::from(
            a.sequence
                .wrapping_sub(u32::try_from(cur & 0xffff_ffff).unwrap_or(0))
                .cast_signed(),
        )
    };
    let missed = delta < 0 || (delta == 0 && !relative);
    let delta = if missed && kind & DRM_VBLANK_NEXTONMISS != 0 {
        1
    } else if missed {
        0
    } else {
        delta
    };
    let target = cur + u64::try_from(delta).unwrap_or(0);
    let reply = |seq: u64, ns: u64| {
        DrmWaitVblank::reply(
            kind,
            u32::try_from(seq & 0xffff_ffff).unwrap_or(0),
            i64::try_from(ns / 1_000_000_000).unwrap_or(i64::MAX),
            i64::try_from(ns % 1_000_000_000 / 1000).unwrap_or(0),
        )
    };
    if kind & DRM_VBLANK_EVENT != 0 {
        let user_data = a.word0;
        if target <= cur {
            // Already there: the event goes now.
            let mut ev = event_for(DRM_EVENT_VBLANK, user_data);
            DrmEventVblank {
                base: DrmEvent {
                    kind: DRM_EVENT_VBLANK,
                    length: 32,
                },
                user_data,
                tv_sec: u32::try_from(last / 1_000_000_000).unwrap_or(u32::MAX),
                tv_usec: u32::try_from(last % 1_000_000_000 / 1000).unwrap_or(0),
                sequence: u32::try_from(cur & 0xffff_ffff).unwrap_or(0),
                crtc_id: k.crtcs[ci].id.0,
            }
            .encode(&mut ev);
            f.queue_event(&ev)?;
        } else {
            f.reserve_event(EVENT_LEN)?;
            k.add_waiter(
                ci,
                target,
                Some((f.me.clone(), event_for(DRM_EVENT_VBLANK, user_data))),
            );
        }
        *a = reply(target, 0);
        return Ok(0);
    }
    if target > cur {
        k.add_waiter(ci, target, None);
        k.wait(s, || k.rt[ci].lock().seq >= target)?;
    }
    let (seq, ns) = {
        let r = k.rt[ci].lock();
        (r.seq, r.last_ns)
    };
    *a = reply(seq, ns);
    Ok(0)
}

/// `CRTC_GET_SEQUENCE`: the frame count and when the last frame began.
pub(crate) fn get_sequence(f: &Open, a: &mut DrmCrtcGetSequence) -> Result<usize, Errno> {
    let k = kms_of(f)?;
    let ci = k.crtc_index(ObjId(a.crtc_id)).ok_or(Errno::ENOENT)?;
    let r = k.rt[ci].lock();
    a.active = u32::from(k.active(ci));
    a.sequence = r.seq;
    a.sequence_ns = i64::try_from(r.last_ns).unwrap_or(i64::MAX);
    Ok(0)
}

/// `CRTC_QUEUE_SEQUENCE`: an event at a frame count; the call returns the
/// count it will be at.
pub(crate) fn queue_sequence(f: &Open, a: &mut DrmCrtcQueueSequence) -> Result<usize, Errno> {
    let k = kms_of(f)?;
    if a.flags & !(DRM_CRTC_SEQUENCE_RELATIVE | DRM_CRTC_SEQUENCE_NEXT_ON_MISS) != 0 {
        return Err(Errno::EINVAL);
    }
    let ci = k.crtc_index(ObjId(a.crtc_id)).ok_or(Errno::ENOENT)?;
    if !k.active(ci) {
        return Err(Errno::EINVAL);
    }
    let cur = k.rt[ci].lock().seq;
    let mut target = if a.flags & DRM_CRTC_SEQUENCE_RELATIVE != 0 {
        cur.checked_add(a.sequence).ok_or(Errno::EINVAL)?
    } else {
        a.sequence
    };
    if target <= cur {
        if a.flags & DRM_CRTC_SEQUENCE_NEXT_ON_MISS != 0 {
            target = cur + 1;
        } else {
            // A count already past is an error, as asked for.
            return Err(Errno::EINVAL);
        }
    }
    f.reserve_event(EVENT_LEN)?;
    let ev = event_for(DRM_EVENT_CRTC_SEQUENCE, a.user_data);
    k.add_waiter(ci, target, Some((f.me.clone(), ev)));
    a.sequence = target;
    Ok(0)
}
