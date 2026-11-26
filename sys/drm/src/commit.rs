// SPDX-License-Identifier: BSD-2-Clause
//! Commits: applying property changes to a copy of the display state,
//! checking it, programming the hardware, and completing at the frame.
//!
//! Every way of changing the display (the atomic ioctl, and the older
//! set-controller, page-flip, set-plane and set-property calls) builds a
//! new [`State`] and comes here. A commit is checked whole before any
//! hardware is touched; a failed check changes nothing.
//!
//! A commit completes at the next frame: the controller's vertical
//! blanking interrupt calls [`Kms::vblank`], or, for a controller with no
//! such interrupt, a timer at the mode's refresh rate does. Completion
//! sends the flip event the requester reserved when the commit began, so
//! the completion path allocates nothing and cannot lose it.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use abi::drm::*;
use abi::errno::Errno;
use kms_check::{
    CrtcState, FbInfo, Mode, ObjId, PlaneCaps, PlaneState, Visible, check_crtc, check_plane,
    check_routing,
};
use sched::Sleepable;

use crate::card::Open;
use crate::kms::{CrtcUpdate, Kms, Pending, PlaneUpdate, PropId, PropKind, State};

/// How a commit is to be carried out.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Flags {
    /// Check only: change nothing.
    pub test_only: bool,
    /// Return at once; the frame completes it later.
    pub nonblock: bool,
    /// Turning controllers on or off, or changing modes, is allowed.
    pub allow_modeset: bool,
    /// Send a flip-complete event to the file when the frame comes.
    pub event: bool,
    pub user_data: u64,
    /// A controller a commit reaches even if no state of it changes (a
    /// page flip to the framebuffer already shown still owes its event).
    pub force: Option<usize>,
}

/// An event is one `drm_event_vblank`.
const EVENT_LEN: usize = 32;

/// The refresh interval of a controller with no mode: 60 hertz.
const DEFAULT_FRAME_NS: u64 = 16_666_667;

impl Kms {
    /// Records the card's unit, for timer callbacks.
    pub(crate) fn attach(&self, unit: u32) {
        self.unit.store(unit, Ordering::Relaxed);
    }

    /// Applies one property change to `st`.
    ///
    /// # Errors
    ///
    /// `ENOENT` for an object that does not exist; `EINVAL` for a
    /// property the object lacks, a value out of its range, or a
    /// property the caller may not set.
    pub(crate) fn set_prop(
        &self,
        st: &mut State,
        obj: ObjId,
        prop: PropId,
        value: u64,
    ) -> Result<(), Errno> {
        let s = &self.std;
        let object = |o: u64| -> Result<Option<ObjId>, Errno> {
            Ok((o != 0).then_some(ObjId(u32::try_from(o).map_err(|_| Errno::ENOENT)?)))
        };
        let crtc_ref = |o: u64| -> Result<Option<ObjId>, Errno> {
            let id = object(o)?;
            if let Some(i) = id {
                self.crtc_index(i).ok_or(Errno::ENOENT)?;
            }
            Ok(id)
        };
        let small = |v: u64| u32::try_from(v).map_err(|_| Errno::EINVAL);
        let signed32 = |v: u64| i32::try_from(v.cast_signed()).map_err(|_| Errno::EINVAL);

        if let Some(i) = self.plane_index(obj) {
            let p = &mut st.planes[i];
            match prop {
                x if x == s.fb_id => {
                    let id = object(value)?;
                    if let Some(i) = id {
                        self.fb(i).ok_or(Errno::ENOENT)?;
                    }
                    p.fb = id;
                }
                x if x == s.plane_crtc_id => p.crtc = crtc_ref(value)?,
                x if x == s.src[0] => p.src.x = small(value)?,
                x if x == s.src[1] => p.src.y = small(value)?,
                x if x == s.src[2] => p.src.w = small(value)?,
                x if x == s.src[3] => p.src.h = small(value)?,
                x if x == s.dst[0] => p.dst.x = signed32(value)?,
                x if x == s.dst[1] => p.dst.y = signed32(value)?,
                x if x == s.dst[2] => {
                    p.dst.w = small(value)
                        .and_then(|v| i32::try_from(v).map_err(|_| Errno::EINVAL).map(|_| v))?
                }
                x if x == s.dst[3] => {
                    p.dst.h = small(value)
                        .and_then(|v| i32::try_from(v).map_err(|_| Errno::EINVAL).map(|_| v))?
                }
                x if x == s.plane_type || x == s.in_formats => return Err(Errno::EINVAL),
                // Taken by the atomic call, which waits for it (a state
                // does not keep it); set any other way it does nothing.
                x if x == s.in_fence_fd => {}
                _ => {
                    let at = self.extra_slot(&self.planes[i].props, p.extra.len(), prop)?;
                    p.extra[at] = self.check_extra(prop, value)?;
                }
            }
            return Ok(());
        }
        if let Some(i) = self.crtc_index(obj) {
            let c = &mut st.crtcs[i];
            match prop {
                x if x == s.active => {
                    if value > 1 {
                        return Err(Errno::EINVAL);
                    }
                    c.active = value == 1;
                }
                x if x == s.mode_id => {
                    if value == 0 {
                        c.mode = None;
                        c.mode_blob = None;
                    } else {
                        let id = object(value)?.ok_or(Errno::ENOENT)?;
                        let blob = self.blob(id).ok_or(Errno::ENOENT)?;
                        if blob.data.len() != <DrmModeModeinfo as Wire>::SIZE {
                            return Err(Errno::EINVAL);
                        }
                        c.mode = Some(Mode::from_info(&DrmModeModeinfo::decode(&blob.data)));
                        c.mode_blob = Some(id);
                    }
                }
                _ => {
                    let at = self.extra_slot(&self.crtcs[i].props, c.extra.len(), prop)?;
                    c.extra[at] = self.check_extra(prop, value)?;
                }
            }
            return Ok(());
        }
        if let Some(i) = self.connector_index(obj) {
            let c = &mut st.conns[i];
            match prop {
                x if x == s.conn_crtc_id => c.crtc = crtc_ref(value)?,
                // The legacy power property and the display identification
                // data are not set through here.
                x if x == s.dpms || x == s.edid => return Err(Errno::EINVAL),
                _ => {
                    let at = self.extra_slot(&self.connectors[i].props, c.extra.len(), prop)?;
                    c.extra[at] = self.check_extra(prop, value)?;
                }
            }
            return Ok(());
        }
        Err(Errno::ENOENT)
    }

    /// The place of driver property `prop` among an object's driver
    /// values (the standard properties come first in `props`).
    fn extra_slot(&self, props: &[PropId], extras: usize, prop: PropId) -> Result<usize, Errno> {
        let pos = props.iter().position(|p| *p == prop).ok_or(Errno::EINVAL)?;
        pos.checked_sub(props.len() - extras).ok_or(Errno::EINVAL)
    }

    /// Whether `value` is one `prop` can hold; a driver property that
    /// is immutable cannot be set.
    fn check_extra(&self, prop: PropId, value: u64) -> Result<u64, Errno> {
        let p = self.prop(prop).ok_or(Errno::EINVAL)?;
        if p.immutable {
            return Err(Errno::EINVAL);
        }
        let ok = match p.kind {
            PropKind::Range { min, max } => (min..=max).contains(&value),
            PropKind::SignedRange { min, max } => (min..=max).contains(&value.cast_signed()),
            PropKind::Enum(e) => e.iter().any(|(_, v)| *v == value),
            PropKind::Bitmask(e) => value & !e.iter().fold(0, |m, (_, v)| m | (1u64 << *v)) == 0,
            PropKind::Blob => {
                value == 0
                    || self
                        .blob(ObjId(u32::try_from(value).unwrap_or(0)))
                        .is_some()
            }
            PropKind::Object(_) => true,
        };
        if ok { Ok(value) } else { Err(Errno::EINVAL) }
    }

    /// The framebuffer a plane state names: the device's, or the one the
    /// plane already shows if it has since been removed.
    fn plane_fb(
        &self,
        old: &State,
        new: &State,
        i: usize,
    ) -> Result<Option<Arc<crate::kms::Fb>>, Errno> {
        let Some(id) = new.planes[i].fb else {
            return Ok(None);
        };
        if let Some(f) = self.fb(id) {
            return Ok(Some(f));
        }
        if old.planes[i].fb == Some(id)
            && let Some(f) = self.shown.lock()[i].clone()
        {
            return Ok(Some(f));
        }
        Err(Errno::ENOENT)
    }

    /// Checks `new` as a change from `old`. Returns, for each plane, its
    /// clipped geometry and framebuffer, and for each controller whether
    /// it needs a modeset.
    #[allow(clippy::type_complexity, reason = "one internal return")]
    fn validate(
        &self,
        old: &State,
        new: &State,
        flags: &Flags,
    ) -> Result<(Vec<(Visible, Option<Arc<crate::kms::Fb>>)>, Vec<bool>), Errno> {
        let invalid = |_| Errno::EINVAL;
        // Controllers.
        let mut kc = Vec::with_capacity(new.crtcs.len());
        for c in &new.crtcs {
            let st = CrtcState {
                active: c.active,
                mode: c.mode,
            };
            check_crtc(&st).map_err(invalid)?;
            kc.push(st);
        }
        // Connectors: reachable controllers, no sharing, and an active
        // controller needs one.
        let mut routes = Vec::with_capacity(new.conns.len());
        for (k, c) in new.conns.iter().enumerate() {
            let mask = self.connectors[k]
                .encoders
                .iter()
                .fold(0, |m, e| m | self.encoders[*e].possible_crtcs);
            let idx = c
                .crtc
                .and_then(|id| self.crtc_index(id))
                .map(|i| u32::try_from(i).unwrap_or(32));
            routes.push((mask, idx));
        }
        check_routing(&routes).map_err(invalid)?;
        for (i, c) in new.crtcs.iter().enumerate() {
            if c.active && !new.conns.iter().any(|k| k.crtc == Some(self.crtcs[i].id)) {
                return Err(Errno::EINVAL);
            }
        }
        // Planes.
        let mut planes = Vec::with_capacity(new.planes.len());
        for (i, p) in new.planes.iter().enumerate() {
            let fb = self.plane_fb(old, new, i)?;
            let hw = &self.planes[i].hw;
            let caps = PlaneCaps {
                possible_crtcs: self.planes[i].possible_crtcs,
                formats: hw.formats(),
                scale: hw.scale(),
            };
            let crtc_idx = p.crtc.and_then(|id| self.crtc_index(id));
            let st = PlaneState {
                crtc: p.crtc,
                fb: fb.as_ref().map(|f| FbInfo {
                    id: f.id,
                    format: f.format,
                    modifier: f.modifier,
                    width: f.width,
                    height: f.height,
                }),
                src: p.src,
                dst: p.dst,
            };
            let crtc = crtc_idx.map(|i| (u32::try_from(i).unwrap_or(32), &kc[i]));
            let vis = check_plane(&caps, &st, crtc).map_err(invalid)?;
            if let Some(f) = &fb
                && vis.shown
                && !hw.modifiers().contains(&f.modifier)
            {
                return Err(Errno::EINVAL);
            }
            planes.push((vis, fb));
        }
        // Which controllers change mode, power, or route.
        let mut modeset = Vec::with_capacity(new.crtcs.len());
        for (i, c) in new.crtcs.iter().enumerate() {
            let o = &old.crtcs[i];
            let id = Some(self.crtcs[i].id);
            let route_changed = old
                .conns
                .iter()
                .zip(&new.conns)
                .any(|(a, b)| a.crtc != b.crtc && (a.crtc == id || b.crtc == id));
            let m = o.active != c.active
                || (c.active && o.mode != c.mode)
                || (c.active && route_changed);
            if m && !flags.allow_modeset {
                return Err(Errno::EINVAL);
            }
            modeset.push(m);
        }
        // The hardware's own limits.
        for (i, c) in new.crtcs.iter().enumerate() {
            self.crtcs[i].hw.check(&CrtcUpdate {
                index: i,
                old: &old.crtcs[i],
                new: c,
                modeset: modeset[i],
                s: None,
            })?;
        }
        for (i, p) in new.planes.iter().enumerate() {
            let (vis, fb) = &planes[i];
            self.planes[i].hw.check(&PlaneUpdate {
                index: i,
                crtc: p.crtc.and_then(|id| self.crtc_index(id)),
                fb: fb.as_ref(),
                visible: *vis,
                old: &old.planes[i],
                new: p,
            })?;
        }
        Ok((planes, modeset))
    }

    /// Sleeps until `done` holds (a frame completed something).
    ///
    /// # Errors
    ///
    /// `EINTR` if the thread is interrupted first.
    pub(crate) fn wait(&self, s: &Sleepable, done: impl FnMut() -> bool) -> Result<(), Errno> {
        #[cfg(not(test))]
        {
            self.done
                .wait_until_interruptible(s, done)
                .map_err(|_| Errno::EINTR)
        }
        // A host thread has no scheduler to sleep on: the test's frames
        // come when it asks for them.
        #[cfg(test)]
        {
            let _ = s;
            let mut done = done;
            let mut spins = 0;
            while !done() {
                crate::tests::idle(self.unit.load(Ordering::Relaxed));
                spins += 1;
                assert!(spins < 100, "a wait that no frame ends");
            }
            Ok(())
        }
    }

    /// Whether commit `id` still waits for a frame on any of `crtcs`.
    fn is_pending(&self, id: u64, crtcs: &[usize]) -> bool {
        crtcs
            .iter()
            .any(|i| self.rt[*i].lock().pending.iter().any(|p| p.id == id))
    }

    /// Carries out a change from the committed state to `new`.
    ///
    /// # Errors
    ///
    /// `EINVAL` for a state the checks refuse (nothing changed);
    /// `EBUSY` for a nonblocking commit while one is waiting for its
    /// frame; `ENOMEM` if the requester has no room for the event; `EINTR`
    /// if a wait for the frame is interrupted (the commit stands).
    pub(crate) fn commit(
        &self,
        file: &Open,
        s: &Sleepable,
        new: &State,
        flags: Flags,
    ) -> Result<(), Errno> {
        let id = self.commit_id.fetch_add(1, Ordering::Relaxed);
        let touched = loop {
            let guard = self.commit_lock.lock();
            let old = self.state.lock().clone();
            let (planes, modeset) = self.validate(&old, new, &flags)?;
            if flags.test_only {
                return Ok(());
            }
            let touched = self.touched(&old, new, &modeset, flags.force);
            if touched
                .iter()
                .any(|i| !self.rt[*i].lock().pending.is_empty())
            {
                drop(guard);
                if flags.nonblock {
                    return Err(Errno::EBUSY);
                }
                // The frame of the commit before ours.
                self.wait(s, || {
                    touched
                        .iter()
                        .all(|i| self.rt[*i].lock().pending.is_empty())
                })?;
                continue;
            }
            // Room for each event first: a refusal leaves everything as
            // it was.
            let mut events: Vec<Option<(alloc::sync::Weak<Open>, Vec<u8>)>> = Vec::new();
            for i in &touched {
                if flags.event && new.crtcs[*i].active {
                    if let Err(e) = file.reserve_event(EVENT_LEN) {
                        for _ in events.iter().flatten() {
                            file.cancel_event(EVENT_LEN);
                        }
                        return Err(e);
                    }
                    let mut ev = alloc::vec![0u8; EVENT_LEN];
                    DrmEventVblank {
                        base: DrmEvent {
                            kind: DRM_EVENT_FLIP_COMPLETE,
                            length: 32,
                        },
                        user_data: flags.user_data,
                        tv_sec: 0,
                        tv_usec: 0,
                        sequence: 0,
                        crtc_id: self.crtcs[*i].id.0,
                    }
                    .encode(&mut ev);
                    events.push(Some((file.me.clone(), ev)));
                } else {
                    events.push(None);
                }
            }
            // Buffers a device reads without snooping the processor's
            // caches are cleaned before it looks (the producer's part of
            // the hand-off, GRAPHICS.md §4.4).
            for (_, fb) in &planes {
                for g in fb.iter().flat_map(|f| f.gems.iter().flatten()) {
                    if !g.placement().coherent {
                        file.card.driver().sync_for_device(g, 0..g.size());
                    }
                }
            }
            self.program(&old, new, &planes, &modeset, flags.force, Some(s));
            *self.state.lock() = new.clone();
            {
                let mut shown = self.shown.lock();
                for (i, (_, fb)) in planes.iter().enumerate() {
                    shown[i].clone_from(fb);
                }
            }
            for (i, event) in touched.iter().zip(events) {
                if new.crtcs[*i].active {
                    self.rt[*i].lock().pending.push(Pending { id, event });
                }
            }
            drop(guard);
            break touched;
        };
        for i in &touched {
            if self.crtcs[*i].hw.vblank_interrupt() {
                self.crtcs[*i].hw.vblank_enable(true);
            } else {
                self.arm_timer(*i);
            }
        }
        if !flags.nonblock {
            self.wait(s, || !self.is_pending(id, &touched))?;
        }
        Ok(())
    }

    /// The controllers a change reaches: its own, and those whose planes
    /// or connectors it moves.
    fn touched(
        &self,
        old: &State,
        new: &State,
        modeset: &[bool],
        force: Option<usize>,
    ) -> Vec<usize> {
        let mut t = alloc::vec![false; new.crtcs.len()];
        for (i, c) in new.crtcs.iter().enumerate() {
            t[i] = modeset[i] || *c != old.crtcs[i];
        }
        let mut mark = |id: Option<ObjId>| {
            if let Some(i) = id.and_then(|id| self.crtc_index(id)) {
                t[i] = true;
            }
        };
        for (a, b) in old.planes.iter().zip(&new.planes) {
            if a != b {
                mark(a.crtc);
                mark(b.crtc);
            }
        }
        for (a, b) in old.conns.iter().zip(&new.conns) {
            if a != b {
                mark(a.crtc);
                mark(b.crtc);
            }
        }
        if let Some(c) = force {
            t[c] = true;
        }
        t.iter()
            .enumerate()
            .filter(|(_, x)| **x)
            .map(|(i, _)| i)
            .collect()
    }

    /// Programs the hardware: encoders that stop, then planes, then
    /// controllers (which latch), then encoders that start.
    fn program(
        &self,
        old: &State,
        new: &State,
        planes: &[(Visible, Option<Arc<crate::kms::Fb>>)],
        modeset: &[bool],
        force: Option<usize>,
        s: Option<&Sleepable>,
    ) {
        let crtc_of = |id: Option<ObjId>| id.and_then(|id| self.crtc_index(id));
        let first_encoder = |k: usize| {
            self.connectors[k]
                .encoders
                .first()
                .map(|e| &self.encoders[*e])
        };
        for k in 0..new.conns.len() {
            let (a, b) = (&old.conns[k], &new.conns[k]);
            if let (Some(c), Some(e)) = (crtc_of(a.crtc), first_encoder(k)) {
                let stops = b.crtc != a.crtc || !new.crtcs[c].active || modeset[c];
                if old.crtcs[c].active && stops {
                    e.hw.disable();
                }
            }
        }
        for (i, p) in new.planes.iter().enumerate() {
            let touches_modeset = [crtc_of(old.planes[i].crtc), crtc_of(p.crtc)]
                .into_iter()
                .flatten()
                .any(|c| modeset[c]);
            // A flip to the buffer already shown still reaches a layer
            // that must be told to read it again.
            let forced = force.is_some() && force == crtc_of(p.crtc);
            if *p != old.planes[i] || touches_modeset || forced {
                self.planes[i].hw.commit(&PlaneUpdate {
                    index: i,
                    crtc: crtc_of(p.crtc),
                    fb: planes[i].1.as_ref(),
                    visible: planes[i].0,
                    old: &old.planes[i],
                    new: p,
                });
            }
        }
        for (i, c) in new.crtcs.iter().enumerate() {
            if *c != old.crtcs[i]
                || modeset[i]
                || force == Some(i)
                || self.touched_by_planes(old, new, i)
            {
                self.crtcs[i].hw.commit(&CrtcUpdate {
                    index: i,
                    old: &old.crtcs[i],
                    new: c,
                    modeset: modeset[i],
                    s,
                });
            }
        }
        for k in 0..new.conns.len() {
            let (a, b) = (&old.conns[k], &new.conns[k]);
            if let (Some(c), Some(e)) = (crtc_of(b.crtc), first_encoder(k))
                && new.crtcs[c].active
                && (a.crtc != b.crtc || modeset[c])
                && let Some(m) = &new.crtcs[c].mode
            {
                e.hw.enable(c, m);
            }
        }
    }

    fn touched_by_planes(&self, old: &State, new: &State, crtc: usize) -> bool {
        let id = Some(self.crtcs[crtc].id);
        old.planes
            .iter()
            .zip(&new.planes)
            .any(|(a, b)| a != b && (a.crtc == id || b.crtc == id))
    }

    /// A frame of display controller `crtc` began at `now_ns`: completes
    /// what waited for it, sending each requester its event. A controller
    /// with a vertical blanking interrupt calls this from the handler.
    pub fn vblank(&self, crtc: usize, now_ns: u64) {
        let Some(rt) = self.rt.get(crtc) else { return };
        let (seq, pending) = {
            let mut r = rt.lock();
            r.seq += 1;
            r.last_ns = now_ns;
            // Events waiting on a frame count, those due now: sent in
            // place, each reserved, so nothing here allocates.
            let seq = r.seq;
            let mut i = 0;
            while i < r.waiters.len() {
                if r.waiters[i].target <= seq {
                    let w = r.waiters.swap_remove(i);
                    if let Some((file, mut ev)) = w.event {
                        stamp_wait_event(&mut ev, now_ns, seq, self.crtcs[crtc].id.0);
                        if let Some(f) = file.upgrade() {
                            f.send_reserved(ev);
                        }
                    }
                } else {
                    i += 1;
                }
            }
            (seq, core::mem::take(&mut r.pending))
        };
        let id = self.crtcs[crtc].id.0;
        for p in pending {
            if let Some((file, mut ev)) = p.event {
                let v = DrmEventVblank {
                    base: DrmEvent {
                        kind: DRM_EVENT_FLIP_COMPLETE,
                        length: 32,
                    },
                    user_data: u64_at(&ev, 8),
                    tv_sec: u32::try_from(now_ns / 1_000_000_000).unwrap_or(u32::MAX),
                    tv_usec: u32::try_from(now_ns % 1_000_000_000 / 1000).unwrap_or(0),
                    sequence: u32::try_from(seq & 0xffff_ffff).unwrap_or(0),
                    crtc_id: id,
                };
                v.encode(&mut ev);
                if let Some(f) = file.upgrade() {
                    f.send_reserved(ev);
                }
            }
        }
        if self.crtcs[crtc].hw.vblank_interrupt() {
            let r = rt.lock();
            if r.pending.is_empty() && r.waiters.is_empty() {
                self.crtcs[crtc].hw.vblank_enable(false);
            }
        }
        self.done.wake_all();
    }

    /// Tells the layers that show framebuffer `fb` that its contents
    /// changed (the dirty-framebuffer call: for hardware that does not
    /// scan the buffer by itself, this is when it reads it).
    pub(crate) fn refresh(&self, fb: ObjId) {
        let _guard = self.commit_lock.lock();
        let st = self.state.lock().clone();
        let shown = self.shown.lock().clone();
        for (i, p) in st.planes.iter().enumerate() {
            if p.fb != Some(fb) {
                continue;
            }
            let Some(buf) = &shown[i] else { continue };
            let Some(c) = p.crtc.and_then(|id| self.crtc_index(id)) else {
                continue;
            };
            let crtc_state = kms_check::CrtcState {
                active: st.crtcs[c].active,
                mode: st.crtcs[c].mode,
            };
            let caps = PlaneCaps {
                possible_crtcs: self.planes[i].possible_crtcs,
                formats: self.planes[i].hw.formats(),
                scale: self.planes[i].hw.scale(),
            };
            let info = FbInfo {
                id: buf.id,
                format: buf.format,
                modifier: buf.modifier,
                width: buf.width,
                height: buf.height,
            };
            let ps = PlaneState {
                crtc: p.crtc,
                fb: Some(info),
                src: p.src,
                dst: p.dst,
            };
            let Ok(visible) = check_plane(
                &caps,
                &ps,
                Some((u32::try_from(c).unwrap_or(32), &crtc_state)),
            ) else {
                continue;
            };
            self.planes[i].hw.commit(&PlaneUpdate {
                index: i,
                crtc: Some(c),
                fb: Some(buf),
                visible,
                old: p,
                new: p,
            });
        }
    }

    /// The number of frames display controller `crtc` has had.
    #[must_use]
    pub fn frames(&self, crtc: usize) -> u64 {
        self.rt.get(crtc).map_or(0, |r| r.lock().seq)
    }

    /// Starts the frame timer of `crtc` if it is not running.
    pub(crate) fn arm_timer(&self, crtc: usize) {
        let Some(env) = crate::env() else { return };
        let frame = {
            let st = self.state.lock();
            st.crtcs[crtc].mode.map_or(0, |m| m.frame_ns())
        };
        let frame = if frame == 0 { DEFAULT_FRAME_NS } else { frame };
        let mut r = self.rt[crtc].lock();
        if (r.pending.is_empty() && r.waiters.is_empty()) || r.armed {
            return;
        }
        r.armed = true;
        let unit = usize::try_from(self.unit.load(Ordering::Relaxed)).unwrap_or(0);
        let ticker = r.ticker.get_or_insert_with(env.ticker);
        ticker.arm(frame, tick, (unit << 8) | crtc);
    }

    /// Stops the frame timers and ends every wait: the device is going.
    pub(crate) fn shutdown(&self) {
        for rt in &self.rt {
            let mut r = rt.lock();
            if let Some(t) = &r.ticker {
                t.cancel();
            }
            r.armed = false;
            // Reserved events go unsent; their files are closing.
            r.pending.clear();
        }
        self.done.wake_all();
    }
}

/// Fills in the time, count and controller of a wait event built in
/// advance: a vertical blanking event (`WAIT_VBLANK`, whose time is in
/// seconds and microseconds) or a sequence event (`CRTC_QUEUE_SEQUENCE`,
/// in nanoseconds), by its type.
fn stamp_wait_event(ev: &mut [u8], now_ns: u64, seq: u64, crtc: u32) {
    let kind = u32::from_ne_bytes([ev[0], ev[1], ev[2], ev[3]]);
    if kind == DRM_EVENT_CRTC_SEQUENCE {
        let user_data = u64_at(ev, 8);
        DrmEventCrtcSequence {
            base: DrmEvent { kind, length: 32 },
            user_data,
            time_ns: i64::try_from(now_ns).unwrap_or(i64::MAX),
            sequence: seq,
        }
        .encode(ev);
    } else {
        let user_data = u64_at(ev, 8);
        DrmEventVblank {
            base: DrmEvent { kind, length: 32 },
            user_data,
            tv_sec: u32::try_from(now_ns / 1_000_000_000).unwrap_or(u32::MAX),
            tv_usec: u32::try_from(now_ns % 1_000_000_000 / 1000).unwrap_or(0),
            sequence: u32::try_from(seq & 0xffff_ffff).unwrap_or(0),
            crtc_id: crtc,
        }
        .encode(ev);
    }
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut x = [0u8; 8];
    x.copy_from_slice(&b[at..at + 8]);
    u64::from_ne_bytes(x)
}

/// The frame timer's handler.
fn tick(_: &Sleepable, arg: usize) {
    let (unit, crtc) = (u32::try_from(arg >> 8).unwrap_or(0), arg & 0xff);
    let Some(card) = crate::card::card_by_unit(unit) else {
        return;
    };
    let Some(k) = card.driver().kms() else { return };
    let Some(env) = crate::env() else { return };
    if let Some(r) = k.rt.get(crtc) {
        r.lock().armed = false;
    }
    k.vblank(crtc, (env.now_ns)());
    // More waiting: another frame.
    k.arm_timer(crtc);
}
