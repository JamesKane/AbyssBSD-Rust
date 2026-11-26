// SPDX-License-Identifier: BSD-2-Clause
//! The ioctls that change the display: the atomic one, and the older
//! calls (set a controller, flip a page, set a plane, set a property),
//! each of which builds a new display state and commits it the same way.
//!
//! All of them need the master: the file that holds the display.

use alloc::vec::Vec;

use abi::drm::*;
use abi::errno::Errno;
use kms_check::{DstRect, Mode, ObjId, SrcRect};
use sched::Sleepable;

use crate::card::{Open, client};
use crate::commit::Flags;
use crate::kms::{Kms, PlaneKind, PlaneSt, PropId, State};

/// The most objects, and properties in all, one atomic call may carry.
const MAX_OBJS: u32 = 1024;
const MAX_PROPS: u64 = 16384;
/// How long a commit waits for a plane's fence: a job that does not finish
/// in this time is a hung one (the driver resets its ring after 2 s).
const IN_FENCE_WAIT_NS: u64 = 10_000_000_000;

fn kms_of(f: &Open) -> Result<&Kms, Errno> {
    f.card.driver().kms().ok_or(Errno::ENOTTY)
}

/// The display is changed only by its master.
fn master_kms(f: &Open) -> Result<&Kms, Errno> {
    let k = kms_of(f)?;
    if f.is_master() {
        Ok(k)
    } else {
        Err(Errno::EACCES)
    }
}

/// Copies `count` elements of `size` bytes from user address `ptr`.
fn read_user(ptr: u64, count: usize, size: usize) -> Result<Vec<u8>, Errno> {
    let env = crate::env().ok_or(Errno::ENXIO)?;
    let len = count.checked_mul(size).ok_or(Errno::EINVAL)?;
    let mut v = Vec::new();
    v.try_reserve_exact(len).map_err(|_| Errno::ENOMEM)?;
    v.resize(len, 0);
    if len != 0 {
        env.user.copy_in(&mut v, ptr)?;
    }
    Ok(v)
}

fn le32(b: &[u8]) -> Vec<u32> {
    b.as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn le64(b: &[u8]) -> Vec<u64> {
    b.as_chunks::<8>()
        .0
        .iter()
        .map(|c| u64::from_ne_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]))
        .collect()
}

/// The atomic ioctl: a list of objects, each with a list of properties
/// and values, applied together or not at all.
pub(crate) fn atomic(f: &Open, s: &Sleepable, a: &mut DrmModeAtomic) -> Result<usize, Errno> {
    let k = master_kms(f)?;
    if !f.has_client_cap(client::ATOMIC) {
        return Err(Errno::EINVAL);
    }
    if a.flags & !DRM_MODE_ATOMIC_FLAGS != 0 || a.reserved != 0 {
        return Err(Errno::EINVAL);
    }
    // No asynchronous flips; a test makes no event.
    if a.flags & DRM_MODE_PAGE_FLIP_ASYNC != 0
        || (a.flags & DRM_MODE_ATOMIC_TEST_ONLY != 0 && a.flags & DRM_MODE_PAGE_FLIP_EVENT != 0)
    {
        return Err(Errno::EINVAL);
    }
    if a.count_objs > MAX_OBJS {
        return Err(Errno::EINVAL);
    }
    let n = usize::try_from(a.count_objs).map_err(|_| Errno::EINVAL)?;
    let objs = le32(&read_user(a.objs_ptr, n, 4)?);
    let counts = le32(&read_user(a.count_props_ptr, n, 4)?);
    let total: u64 = counts.iter().map(|c| u64::from(*c)).sum();
    if total > MAX_PROPS {
        return Err(Errno::EINVAL);
    }
    let total = usize::try_from(total).map_err(|_| Errno::EINVAL)?;
    let props = le32(&read_user(a.props_ptr, total, 4)?);
    let values = le64(&read_user(a.prop_values_ptr, total, 8)?);

    let mut new = k.state();
    let mut at = 0;
    // The fences the new buffers wait for (`IN_FENCE_FD` of the planes).
    let mut waits = Vec::new();
    for (obj, count) in objs.iter().zip(&counts) {
        let end = at + usize::try_from(*count).map_err(|_| Errno::EINVAL)?;
        for j in at..end {
            if PropId(props[j]) == k.std.in_fence_fd && k.plane_index(ObjId(*obj)).is_some() {
                let fd = i32::try_from(values[j].cast_signed()).map_err(|_| Errno::EINVAL)?;
                if fd >= 0 {
                    let env = crate::env().ok_or(Errno::ENXIO)?;
                    waits.push((env.import_syncfile)(s, fd)?.fence());
                }
                continue;
            }
            k.set_prop(&mut new, ObjId(*obj), PropId(props[j]), values[j])?;
        }
        at = end;
    }
    // The renderers' work on the buffers finishes before they are shown
    // (a test does not wait: it shows nothing).
    if a.flags & DRM_MODE_ATOMIC_TEST_ONLY == 0 {
        for fence in &waits {
            if !fence.wait(s, IN_FENCE_WAIT_NS) {
                return Err(Errno::ETIMEDOUT);
            }
        }
    }
    k.commit(
        f,
        s,
        &new,
        Flags {
            test_only: a.flags & DRM_MODE_ATOMIC_TEST_ONLY != 0,
            nonblock: a.flags & DRM_MODE_ATOMIC_NONBLOCK != 0,
            allow_modeset: a.flags & DRM_MODE_ATOMIC_ALLOW_MODESET != 0,
            event: a.flags & DRM_MODE_PAGE_FLIP_EVENT != 0,
            user_data: a.user_data,
            force: None,
        },
    )?;
    Ok(0)
}

/// The primary plane of display controller `crtc`.
fn primary_of(k: &Kms, crtc: usize) -> Result<usize, Errno> {
    k.planes
        .iter()
        .position(|p| p.kind == PlaneKind::Primary && p.possible_crtcs & (1 << crtc) != 0)
        .ok_or(Errno::EINVAL)
}

/// A plane state with nothing shown, keeping the driver's properties.
fn detached(p: &PlaneSt) -> PlaneSt {
    PlaneSt {
        extra: p.extra.clone(),
        ..PlaneSt::default()
    }
}

/// Turns display controller `crtc` off in `st`: its planes and
/// connectors let go of it.
fn switch_off(k: &Kms, st: &mut State, crtc: usize) {
    let id = Some(k.crtcs[crtc].id);
    let c = &mut st.crtcs[crtc];
    c.active = false;
    c.mode = None;
    c.mode_blob = None;
    for p in &mut st.planes {
        if p.crtc == id {
            *p = detached(p);
        }
    }
    for c in &mut st.conns {
        if c.crtc == id {
            c.crtc = None;
        }
    }
}

/// Set a controller: a framebuffer, a mode and connectors at once (the
/// older, pre-atomic way). A framebuffer of 0 turns the controller off.
pub(crate) fn set_crtc(f: &Open, s: &Sleepable, a: &mut DrmModeCrtc) -> Result<usize, Errno> {
    let count = usize::try_from(a.count_connectors).map_err(|_| Errno::EINVAL)?;
    if count > 32 {
        return Err(Errno::EINVAL);
    }
    // Connectors are named only when the controller is being turned on.
    let listed = if a.fb_id == 0 || a.mode_valid == 0 {
        Vec::new()
    } else {
        le32(&read_user(a.set_connectors_ptr, count, 4)?)
    };
    set_crtc_with(f, s, a, &listed)?;
    Ok(0)
}

/// [`set_crtc`] with the connectors given as numbers rather than a user
/// address: the same call for a client inside the kernel.
pub(crate) fn set_crtc_with(
    f: &Open,
    s: &Sleepable,
    a: &DrmModeCrtc,
    listed: &[u32],
) -> Result<(), Errno> {
    let k = master_kms(f)?;
    let ci = k.crtc_index(ObjId(a.crtc_id)).ok_or(Errno::ENOENT)?;
    let id = Some(k.crtcs[ci].id);
    let mut new = k.state();
    if a.fb_id == 0 {
        switch_off(k, &mut new, ci);
    } else {
        if a.mode_valid == 0 {
            return Err(Errno::EINVAL);
        }
        let fb = k.fb(ObjId(a.fb_id)).ok_or(Errno::ENOENT)?;
        let mode = Mode::from_info(&a.mode);
        let (x, y) = (a.x, a.y);
        if x > 0xffff || y > 0xffff {
            return Err(Errno::EINVAL);
        }
        let pi = primary_of(k, ci)?;
        let c = &mut new.crtcs[ci];
        c.active = true;
        c.mode = Some(mode);
        c.mode_blob = None;
        new.planes[pi] = PlaneSt {
            crtc: id,
            fb: Some(fb.id),
            src: SrcRect {
                x: x << 16,
                y: y << 16,
                w: u32::from(mode.hdisplay) << 16,
                h: u32::from(mode.vdisplay) << 16,
            },
            dst: DstRect {
                x: 0,
                y: 0,
                w: u32::from(mode.hdisplay),
                h: u32::from(mode.vdisplay),
            },
            extra: new.planes[pi].extra.clone(),
        };
        if !listed.is_empty() {
            let mut ks = Vec::new();
            for l in listed {
                ks.push(k.connector_index(ObjId(*l)).ok_or(Errno::ENOENT)?);
            }
            for (i, c) in new.conns.iter_mut().enumerate() {
                c.crtc = if ks.contains(&i) {
                    id
                } else if c.crtc == id {
                    None
                } else {
                    c.crtc
                };
            }
        }
    }
    k.commit(
        f,
        s,
        &new,
        Flags {
            allow_modeset: true,
            ..Flags::default()
        },
    )
}

/// Flip a controller's primary plane to another framebuffer at the
/// next frame, optionally with an event when it shows.
pub(crate) fn page_flip(
    f: &Open,
    s: &Sleepable,
    a: &mut DrmModeCrtcPageFlip,
) -> Result<usize, Errno> {
    let k = master_kms(f)?;
    if a.flags & !DRM_MODE_PAGE_FLIP_FLAGS != 0
        || a.flags & (DRM_MODE_PAGE_FLIP_ASYNC | DRM_MODE_PAGE_FLIP_TARGET) != 0
        || a.reserved != 0
    {
        return Err(Errno::EINVAL);
    }
    let ci = k.crtc_index(ObjId(a.crtc_id)).ok_or(Errno::ENOENT)?;
    let pi = primary_of(k, ci)?;
    let mut new = k.state();
    let cur = &new.planes[pi];
    if cur.crtc != Some(k.crtcs[ci].id) || !new.crtcs[ci].active {
        return Err(Errno::EINVAL);
    }
    let fb = k.fb(ObjId(a.fb_id)).ok_or(Errno::ENOENT)?;
    // A flip changes what is shown, not how: the new buffer is the old
    // one's size and format.
    let shown = cur
        .fb
        .and_then(|id| k.fb(id).or_else(|| k.shown.lock()[pi].clone()));
    match shown {
        Some(o) if o.width == fb.width && o.height == fb.height && o.format == fb.format => {}
        _ => return Err(Errno::EINVAL),
    }
    new.planes[pi].fb = Some(fb.id);
    k.commit(
        f,
        s,
        &new,
        Flags {
            nonblock: true,
            event: a.flags & DRM_MODE_PAGE_FLIP_EVENT != 0,
            user_data: a.user_data,
            force: Some(ci),
            ..Flags::default()
        },
    )?;
    Ok(0)
}

/// Set a plane: its framebuffer and where it shows. A framebuffer of 0
/// turns the plane off.
pub(crate) fn set_plane(f: &Open, s: &Sleepable, a: &mut DrmModeSetPlane) -> Result<usize, Errno> {
    let k = master_kms(f)?;
    let pi = k.plane_index(ObjId(a.plane_id)).ok_or(Errno::ENOENT)?;
    let mut new = k.state();
    if a.fb_id == 0 {
        new.planes[pi] = detached(&new.planes[pi]);
    } else {
        k.crtc_index(ObjId(a.crtc_id)).ok_or(Errno::ENOENT)?;
        let fb = k.fb(ObjId(a.fb_id)).ok_or(Errno::ENOENT)?;
        new.planes[pi] = PlaneSt {
            crtc: Some(ObjId(a.crtc_id)),
            fb: Some(fb.id),
            src: SrcRect {
                x: a.src_x,
                y: a.src_y,
                w: a.src_w,
                h: a.src_h,
            },
            dst: DstRect {
                x: a.crtc_x,
                y: a.crtc_y,
                w: a.crtc_w,
                h: a.crtc_h,
            },
            extra: new.planes[pi].extra.clone(),
        };
    }
    k.commit(f, s, &new, Flags::default())?;
    Ok(0)
}

/// The power state of a connector (the legacy `DPMS` property): off
/// turns its controller off, on turns it back on at its mode.
fn set_dpms(k: &Kms, st: &mut State, conn: usize, value: u64) -> Result<(), Errno> {
    if value > DRM_MODE_DPMS_OFF {
        return Err(Errno::EINVAL);
    }
    st.conns[conn].dpms = value;
    let Some(ci) = st.conns[conn].crtc.and_then(|id| k.crtc_index(id)) else {
        return Ok(());
    };
    if value == DRM_MODE_DPMS_ON {
        st.crtcs[ci].active = st.crtcs[ci].mode.is_some();
    } else {
        let id = st.conns[conn].crtc;
        st.crtcs[ci].active = false;
        for p in &mut st.planes {
            if p.crtc == id {
                *p = detached(p);
            }
        }
    }
    Ok(())
}

/// Sets one property the older way: it commits at once, and a mode may
/// change.
fn set_one(f: &Open, s: &Sleepable, obj: ObjId, prop: PropId, value: u64) -> Result<usize, Errno> {
    let k = master_kms(f)?;
    k.prop(prop).ok_or(Errno::ENOENT)?;
    let mut new = k.state();
    if let (Some(ci), true) = (k.connector_index(obj), prop == k.std.dpms) {
        set_dpms(k, &mut new, ci, value)?;
    } else {
        k.set_prop(&mut new, obj, prop, value)?;
    }
    k.commit(
        f,
        s,
        &new,
        Flags {
            allow_modeset: true,
            ..Flags::default()
        },
    )?;
    Ok(0)
}

pub(crate) fn obj_set_property(
    f: &Open,
    s: &Sleepable,
    a: &mut DrmModeObjSetProperty,
) -> Result<usize, Errno> {
    set_one(f, s, ObjId(a.obj_id), PropId(a.prop_id), a.value)
}

pub(crate) fn set_property(
    f: &Open,
    s: &Sleepable,
    a: &mut DrmModeConnectorSetProperty,
) -> Result<usize, Errno> {
    set_one(f, s, ObjId(a.connector_id), PropId(a.prop_id), a.value)
}
