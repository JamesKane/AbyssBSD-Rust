// SPDX-License-Identifier: BSD-2-Clause
//! The kernel mode setting ioctls that read the display and manage framebuffers and
//! blobs. (Commits are in `commit.rs`.)
//!
//! Arrays go out the way libdrm expects: the caller gives a count and a
//! pointer per list; the call always returns the true counts, and fills a
//! list only if the caller's count was big enough. So a first call with
//! zero counts sizes the lists, and a second fetches them.

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::drm::*;
use abi::errno::Errno;
use drm_fourcc::{FbError, FbRequest, Fourcc, MOD_LINEAR, Modifier, check_fb};
use kms_check::{Mode, ObjId};

use crate::UserMemory;
use crate::card::{Open, client};
use crate::kms::{Connection, Fb, Kms, PlaneKind, Prop, PropId, PropKind};

/// An argument that is one `unsigned int` (`RMFB`).
pub(crate) struct Id(pub u32);

impl Wire for Id {
    const SIZE: usize = 4;
    fn decode(b: &[u8]) -> Self {
        Self(u32::get(b))
    }
    fn encode(&self, b: &mut [u8]) {
        self.0.put(b);
    }
}

fn kms_of(f: &Open) -> Result<&Kms, Errno> {
    f.card.driver().kms().ok_or(Errno::ENOTTY)
}

fn user() -> Result<&'static dyn UserMemory, Errno> {
    crate::env().map(|e| e.user).ok_or(Errno::ENXIO)
}

/// Copies `data` (`total` elements of `elem` bytes) to `ptr` if the
/// caller made room for them (`cap` elements).
fn put_list(ptr: u64, cap: u32, total: usize, data: &[u8]) -> Result<(), Errno> {
    if ptr != 0 && usize::try_from(cap).unwrap_or(0) >= total && total != 0 {
        user()?.copy_out(ptr, data)?;
    }
    Ok(())
}

fn u32s(v: &[u32]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_ne_bytes()).collect()
}

fn u64s(v: &[u64]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_ne_bytes()).collect()
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// An object's `(property ids, values)`.
fn obj_props(k: &Kms, obj: ObjId, ids: &[PropId]) -> (Vec<u32>, Vec<u64>) {
    let mut p = Vec::with_capacity(ids.len());
    let mut v = Vec::with_capacity(ids.len());
    for id in ids {
        p.push(id.0);
        v.push(k.value(obj, *id).unwrap_or(0));
    }
    (p, v)
}

pub(crate) fn get_resources(f: &Open, a: &mut DrmModeCardRes) -> Result<usize, Errno> {
    let k = kms_of(f)?;
    let fbs: Vec<u32> = k
        .fbs
        .lock()
        .values()
        .filter(|fb| fb.owner == f.id)
        .map(|fb| fb.id.0)
        .collect();
    let crtcs: Vec<u32> = k.crtcs.iter().map(|c| c.id.0).collect();
    let conns: Vec<u32> = k.connectors.iter().map(|c| c.id.0).collect();
    let encs: Vec<u32> = k.encoders.iter().map(|c| c.id.0).collect();
    put_list(a.fb_id_ptr, a.count_fbs, fbs.len(), &u32s(&fbs))?;
    put_list(a.crtc_id_ptr, a.count_crtcs, crtcs.len(), &u32s(&crtcs))?;
    put_list(
        a.connector_id_ptr,
        a.count_connectors,
        conns.len(),
        &u32s(&conns),
    )?;
    put_list(a.encoder_id_ptr, a.count_encoders, encs.len(), &u32s(&encs))?;
    a.count_fbs = count(fbs.len());
    a.count_crtcs = count(crtcs.len());
    a.count_connectors = count(conns.len());
    a.count_encoders = count(encs.len());
    a.min_width = k.limits.min_width;
    a.max_width = k.limits.max_width;
    a.min_height = k.limits.min_height;
    a.max_height = k.limits.max_height;
    Ok(0)
}

pub(crate) fn get_crtc(f: &Open, a: &mut DrmModeCrtc) -> Result<usize, Errno> {
    let k = kms_of(f)?;
    let i = k.crtc_index(ObjId(a.crtc_id)).ok_or(Errno::ENOENT)?;
    let st = k.state.lock();
    // The legacy view of a CRTC is what its primary plane shows.
    let primary = k
        .planes
        .iter()
        .zip(st.planes.iter())
        .find(|(p, s)| p.kind == PlaneKind::Primary && s.crtc == Some(k.crtcs[i].id));
    a.fb_id = 0;
    a.x = 0;
    a.y = 0;
    if let Some((_, s)) = primary {
        a.fb_id = s.fb.map_or(0, |o| o.0);
        a.x = s.src.x >> 16;
        a.y = s.src.y >> 16;
    }
    a.gamma_size = k.crtcs[i].hw.gamma_size();
    match st.crtcs[i].mode {
        Some(m) if st.crtcs[i].active => {
            a.mode_valid = 1;
            a.mode = m.to_info();
        }
        _ => {
            a.mode_valid = 0;
            a.mode = DrmModeModeinfo::default();
        }
    }
    a.count_connectors = 0;
    Ok(0)
}

/// The encoder a connector bound to a CRTC uses: its first.
fn bound_encoder(k: &Kms, conn: usize) -> Option<usize> {
    let st = k.state.lock();
    st.conns[conn].crtc?;
    k.connectors[conn].encoders.first().copied()
}

pub(crate) fn get_encoder(f: &Open, a: &mut DrmModeGetEncoder) -> Result<usize, Errno> {
    let k = kms_of(f)?;
    let i = k.encoder_index(ObjId(a.encoder_id)).ok_or(Errno::ENOENT)?;
    let e = &k.encoders[i];
    a.encoder_type = e.hw.kind();
    a.possible_crtcs = e.possible_crtcs;
    a.possible_clones = e.possible_clones;
    a.crtc_id = 0;
    let st = k.state.lock();
    for (c, s) in st.conns.iter().enumerate() {
        if let (Some(crtc), true) = (s.crtc, k.connectors[c].encoders.first() == Some(&i)) {
            a.crtc_id = crtc.0;
        }
    }
    Ok(0)
}

pub(crate) fn get_connector(f: &Open, a: &mut DrmModeGetConnector) -> Result<usize, Errno> {
    let k = kms_of(f)?;
    let i = k
        .connector_index(ObjId(a.connector_id))
        .ok_or(Errno::ENOENT)?;
    let c = &k.connectors[i];
    let connection = c.hw.detect();
    let modes: Vec<Mode> = if connection == Connection::Connected {
        c.hw.modes()
    } else {
        Vec::new()
    };
    a.connection = connection.wire_value();
    a.connector_type = c.hw.kind();
    a.connector_type_id = count(
        1 + k.connectors[..i]
            .iter()
            .filter(|o| o.hw.kind() == c.hw.kind())
            .count(),
    );
    let (w, h) = c.hw.size_mm();
    a.mm_width = w;
    a.mm_height = h;
    // Unknown subpixel order.
    a.subpixel = 1;
    a.pad = 0;
    a.encoder_id = bound_encoder(k, i).map_or(0, |e| k.encoders[e].id.0);

    let encs: Vec<u32> = c.encoders.iter().map(|e| k.encoders[*e].id.0).collect();
    put_list(a.encoders_ptr, a.count_encoders, encs.len(), &u32s(&encs))?;
    a.count_encoders = count(encs.len());

    let mut bytes = alloc::vec![0u8; modes.len() * <DrmModeModeinfo as Wire>::SIZE];
    for (n, m) in modes.iter().enumerate() {
        m.to_info()
            .encode(&mut bytes[n * <DrmModeModeinfo as Wire>::SIZE..]);
    }
    put_list(a.modes_ptr, a.count_modes, modes.len(), &bytes)?;
    a.count_modes = count(modes.len());

    // The legacy call lists every property, as the atomic one does.
    let (ids, vals) = obj_props(k, c.id, &c.props);
    put_list(a.props_ptr, a.count_props, ids.len(), &u32s(&ids))?;
    put_list(a.prop_values_ptr, a.count_props, ids.len(), &u64s(&vals))?;
    a.count_props = count(ids.len());
    Ok(0)
}

pub(crate) fn get_plane_resources(f: &Open, a: &mut DrmModeGetPlaneRes) -> Result<usize, Errno> {
    let k = kms_of(f)?;
    // Without universal planes a client sees overlays only; primary and
    // cursor planes are reached through the CRTC calls.
    let universal = f.has_client_cap(client::UNIVERSAL_PLANES);
    let ids: Vec<u32> = k
        .planes
        .iter()
        .filter(|p| universal || p.kind == PlaneKind::Overlay)
        .map(|p| p.id.0)
        .collect();
    put_list(a.plane_id_ptr, a.count_planes, ids.len(), &u32s(&ids))?;
    a.count_planes = count(ids.len());
    Ok(0)
}

pub(crate) fn get_plane(f: &Open, a: &mut DrmModeGetPlane) -> Result<usize, Errno> {
    let k = kms_of(f)?;
    let i = k.plane_index(ObjId(a.plane_id)).ok_or(Errno::ENOENT)?;
    let p = &k.planes[i];
    let st = k.state.lock();
    a.crtc_id = st.planes[i].crtc.map_or(0, |o| o.0);
    a.fb_id = st.planes[i].fb.map_or(0, |o| o.0);
    a.possible_crtcs = p.possible_crtcs;
    a.gamma_size = 0;
    let formats: Vec<u32> = p.hw.formats().iter().map(|f| f.0).collect();
    put_list(
        a.format_type_ptr,
        a.count_format_types,
        formats.len(),
        &u32s(&formats),
    )?;
    a.count_format_types = count(formats.len());
    Ok(0)
}

/// What a property's `GETPROPERTY` answer lists: its values array, and
/// its enum entries.
fn prop_values(p: &Prop) -> (Vec<u64>, Vec<DrmModePropertyEnum>) {
    let enum_entry = |name: &str, value: u64| {
        let mut e = DrmModePropertyEnum {
            value,
            name: [0; DRM_PROP_NAME_LEN],
        };
        let n = name.len().min(DRM_PROP_NAME_LEN - 1);
        e.name[..n].copy_from_slice(&name.as_bytes()[..n]);
        e
    };
    match p.kind {
        PropKind::Range { min, max } => (alloc::vec![min, max], Vec::new()),
        PropKind::SignedRange { min, max } => (
            alloc::vec![min.cast_unsigned(), max.cast_unsigned()],
            Vec::new(),
        ),
        PropKind::Enum(e) | PropKind::Bitmask(e) => (
            e.iter().map(|(_, v)| *v).collect(),
            e.iter().map(|(n, v)| enum_entry(n, *v)).collect(),
        ),
        PropKind::Blob => (Vec::new(), Vec::new()),
        PropKind::Object(t) => (alloc::vec![u64::from(t)], Vec::new()),
    }
}

pub(crate) fn get_property(f: &Open, a: &mut DrmModeGetProperty) -> Result<usize, Errno> {
    let k = kms_of(f)?;
    let p = k.prop(PropId(a.prop_id)).ok_or(Errno::ENOENT)?;
    a.flags = p.flags();
    a.name = [0; DRM_PROP_NAME_LEN];
    let n = p.name.len().min(DRM_PROP_NAME_LEN - 1);
    a.name[..n].copy_from_slice(&p.name.as_bytes()[..n]);
    let (values, enums) = prop_values(p);
    put_list(a.values_ptr, a.count_values, values.len(), &u64s(&values))?;
    let mut eb = alloc::vec![0u8; enums.len() * <DrmModePropertyEnum as Wire>::SIZE];
    for (i, e) in enums.iter().enumerate() {
        e.encode(&mut eb[i * <DrmModePropertyEnum as Wire>::SIZE..]);
    }
    put_list(a.enum_blob_ptr, a.count_enum_blobs, enums.len(), &eb)?;
    a.count_values = count(values.len());
    a.count_enum_blobs = count(enums.len());
    Ok(0)
}

pub(crate) fn get_blob(f: &Open, a: &mut DrmModeGetBlob) -> Result<usize, Errno> {
    let k = kms_of(f)?;
    let b = k.blob(ObjId(a.blob_id)).ok_or(Errno::ENOENT)?;
    let len = b.data.len();
    if a.data != 0 && usize::try_from(a.length).unwrap_or(0) >= len {
        user()?.copy_out(a.data, &b.data)?;
    }
    a.length = count(len);
    Ok(0)
}

pub(crate) fn create_blob(f: &Open, a: &mut DrmModeCreateBlob) -> Result<usize, Errno> {
    let k = kms_of(f)?;
    let len = usize::try_from(a.length).map_err(|_| Errno::EINVAL)?;
    if len == 0 || len > crate::kms::MAX_BLOB {
        return Err(Errno::EINVAL);
    }
    let mut data = Vec::new();
    data.try_reserve_exact(len).map_err(|_| Errno::ENOMEM)?;
    data.resize(len, 0);
    user()?.copy_in(&mut data, a.data)?;
    a.blob_id = k.create_blob(&data, f.id)?.0;
    Ok(0)
}

pub(crate) fn destroy_blob(f: &Open, a: &mut DrmModeDestroyBlob) -> Result<usize, Errno> {
    kms_of(f)?.destroy_blob(ObjId(a.blob_id), f.id).map(|()| 0)
}

pub(crate) fn obj_get_properties(
    f: &Open,
    a: &mut DrmModeObjGetProperties,
) -> Result<usize, Errno> {
    let k = kms_of(f)?;
    let id = ObjId(a.obj_id);
    let ids: &[PropId] = match a.obj_type {
        DRM_MODE_OBJECT_CRTC => &k.crtcs[k.crtc_index(id).ok_or(Errno::ENOENT)?].props,
        DRM_MODE_OBJECT_PLANE => &k.planes[k.plane_index(id).ok_or(Errno::ENOENT)?].props,
        DRM_MODE_OBJECT_CONNECTOR => {
            &k.connectors[k.connector_index(id).ok_or(Errno::ENOENT)?].props
        }
        DRM_MODE_OBJECT_ANY => {
            if let Some(i) = k.crtc_index(id) {
                &k.crtcs[i].props
            } else if let Some(i) = k.plane_index(id) {
                &k.planes[i].props
            } else if let Some(i) = k.connector_index(id) {
                &k.connectors[i].props
            } else {
                return Err(Errno::ENOENT);
            }
        }
        DRM_MODE_OBJECT_ENCODER | DRM_MODE_OBJECT_FB | DRM_MODE_OBJECT_BLOB => &[],
        _ => return Err(Errno::EINVAL),
    };
    let (p, v) = obj_props(k, id, ids);
    put_list(a.props_ptr, a.count_props, p.len(), &u32s(&p))?;
    put_list(a.prop_values_ptr, a.count_props, p.len(), &u64s(&v))?;
    a.count_props = count(p.len());
    Ok(0)
}

/// `ADDFB2`: a framebuffer over the buffers `handles` name in this file.
pub(crate) fn add_fb2(f: &Open, a: &mut DrmModeFbCmd2) -> Result<usize, Errno> {
    let k = kms_of(f)?;
    if a.flags & !(DRM_MODE_FB_INTERLACED | DRM_MODE_FB_MODIFIERS) != 0
        || a.flags & DRM_MODE_FB_INTERLACED != 0
    {
        return Err(Errno::EINVAL);
    }
    let modifier = if a.flags & DRM_MODE_FB_MODIFIERS != 0 {
        // One modifier for the whole buffer: every used plane's agrees.
        let m = a.modifier[0];
        if a.modifier
            .iter()
            .zip(a.handles)
            .any(|(x, h)| h != 0 && *x != m)
            || m == drm_fourcc::MOD_INVALID.0
        {
            return Err(Errno::EINVAL);
        }
        Modifier(m)
    } else {
        if a.modifier.iter().any(|m| *m != 0) {
            return Err(Errno::EINVAL);
        }
        MOD_LINEAR
    };
    let format = Fourcc(a.pixel_format);
    // Pitch and offset rules, for the buffers' format.
    let sizes = check_fb(
        &FbRequest {
            format,
            width: a.width,
            height: a.height,
            pitches: a.pitches,
            offsets: a.offsets,
            handles: a.handles,
            modifier,
        },
        k.limits.max_width,
        k.limits.max_height,
    )
    .map_err(|_: FbError| Errno::EINVAL)?;
    if a.width < k.limits.min_width || a.height < k.limits.min_height {
        return Err(Errno::EINVAL);
    }
    let mut gems: [Option<Arc<crate::gem::Gem>>; 4] = [None, None, None, None];
    for (i, h) in a.handles.iter().enumerate() {
        if *h == 0 {
            continue;
        }
        let g = f.gem(*h).ok_or(Errno::ENOENT)?;
        // The buffer holds the plane's lines past its offset.
        if sizes.get(i).copied().unwrap_or(0) > g.size() {
            return Err(Errno::EINVAL);
        }
        gems[i] = Some(g);
    }
    let id = k.add_fb(Fb {
        id: ObjId(0),
        width: a.width,
        height: a.height,
        format,
        modifier,
        flags: a.flags,
        gems,
        pitches: a.pitches,
        offsets: a.offsets,
        owner: f.id,
    })?;
    a.fb_id = id.0;
    Ok(0)
}

pub(crate) fn rm_fb(f: &Open, a: &mut Id) -> Result<usize, Errno> {
    kms_of(f)?.remove_fb(ObjId(a.0), f.id).map(|()| 0)
}

pub(crate) fn close_fb(f: &Open, a: &mut DrmModeClosefb) -> Result<usize, Errno> {
    if a.pad != 0 {
        return Err(Errno::EINVAL);
    }
    kms_of(f)?.remove_fb(ObjId(a.fb_id), f.id).map(|()| 0)
}
