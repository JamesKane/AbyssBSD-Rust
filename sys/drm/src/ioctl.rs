// SPDX-License-Identifier: BSD-2-Clause
//! The Direct Rendering Manager ioctl dispatcher.
//!
//! A request carries its argument's size, but Direct Rendering Manager does not trust it:
//! each handler has the size of the structure it knows. A caller's
//! shorter structure is zero-extended to it; a longer one is cut to it,
//! its tail ignored (as `drm_ioctl` does; drivers' own requests that
//! demand a zero tail, panthor's, check it themselves). Only the bytes
//! the caller passed come back. Requests the core lacks are logged, so a
//! compositor or `modetest` says what it needs.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use abi::drm::*;
use abi::errno::Errno;
use abi::ioctl::IOC_OUT;
use addr::PAGE_SIZE;

use crate::card::{Open, client};
use crate::gem::Gem;
use crate::kms_ioctl as k;

type Handler = fn(&Open, &mut [u8]) -> Result<usize, Errno>;

/// One core request: its number, name (for the log), and the size of the
/// structure its handler reads.
struct Def {
    nr: u8,
    name: &'static str,
    size: usize,
    handler: Handler,
}

/// A request with no argument.
struct Empty;

impl Wire for Empty {
    const SIZE: usize = 0;
    fn decode(_: &[u8]) -> Self {
        Self
    }
    fn encode(&self, _: &mut [u8]) {}
}

/// A table entry for request `$req` whose argument is `$ty`, handled by
/// `$h(&Open, &mut $ty)`.
macro_rules! def {
    ($req:ident, $name:literal, $ty:ty, $h:path) => {
        Def {
            nr: ioctl_nr($req),
            name: $name,
            size: <$ty as Wire>::SIZE,
            handler: |f, b| {
                let mut a = <$ty as Wire>::decode(b);
                let r = $h(f, &mut a)?;
                a.encode(b);
                Ok(r)
            },
        }
    };
}

const TABLE: &[Def] = &[
    def!(DRM_IOCTL_VERSION, "VERSION", DrmVersion, version),
    def!(DRM_IOCTL_GEM_CLOSE, "GEM_CLOSE", DrmGemClose, gem_close),
    def!(DRM_IOCTL_GET_CAP, "GET_CAP", DrmGetCap, get_cap),
    def!(
        DRM_IOCTL_SET_CLIENT_CAP,
        "SET_CLIENT_CAP",
        DrmSetClientCap,
        set_client_cap
    ),
    def!(DRM_IOCTL_SET_MASTER, "SET_MASTER", Empty, set_master),
    def!(DRM_IOCTL_DROP_MASTER, "DROP_MASTER", Empty, drop_master),
    def!(
        DRM_IOCTL_SET_CLIENT_NAME,
        "SET_CLIENT_NAME",
        DrmSetClientName,
        set_client_name
    ),
    def!(
        DRM_IOCTL_MODE_CREATE_DUMB,
        "MODE_CREATE_DUMB",
        DrmModeCreateDumb,
        create_dumb
    ),
    def!(
        DRM_IOCTL_MODE_MAP_DUMB,
        "MODE_MAP_DUMB",
        DrmModeMapDumb,
        map_dumb
    ),
    def!(
        DRM_IOCTL_MODE_DESTROY_DUMB,
        "MODE_DESTROY_DUMB",
        DrmModeDestroyDumb,
        destroy_dumb
    ),
    def!(
        DRM_IOCTL_MODE_GETRESOURCES,
        "MODE_GETRESOURCES",
        DrmModeCardRes,
        k::get_resources
    ),
    def!(
        DRM_IOCTL_MODE_GETCRTC,
        "MODE_GETCRTC",
        DrmModeCrtc,
        k::get_crtc
    ),
    def!(
        DRM_IOCTL_MODE_GETENCODER,
        "MODE_GETENCODER",
        DrmModeGetEncoder,
        k::get_encoder
    ),
    def!(
        DRM_IOCTL_MODE_GETCONNECTOR,
        "MODE_GETCONNECTOR",
        DrmModeGetConnector,
        k::get_connector
    ),
    def!(
        DRM_IOCTL_MODE_GETPLANERESOURCES,
        "MODE_GETPLANERESOURCES",
        DrmModeGetPlaneRes,
        k::get_plane_resources
    ),
    def!(
        DRM_IOCTL_MODE_GETPLANE,
        "MODE_GETPLANE",
        DrmModeGetPlane,
        k::get_plane
    ),
    def!(
        DRM_IOCTL_MODE_GETPROPERTY,
        "MODE_GETPROPERTY",
        DrmModeGetProperty,
        k::get_property
    ),
    def!(
        DRM_IOCTL_MODE_GETPROPBLOB,
        "MODE_GETPROPBLOB",
        DrmModeGetBlob,
        k::get_blob
    ),
    def!(
        DRM_IOCTL_MODE_OBJ_GETPROPERTIES,
        "MODE_OBJ_GETPROPERTIES",
        DrmModeObjGetProperties,
        k::obj_get_properties
    ),
    def!(
        DRM_IOCTL_MODE_CREATEPROPBLOB,
        "MODE_CREATEPROPBLOB",
        DrmModeCreateBlob,
        k::create_blob
    ),
    def!(
        DRM_IOCTL_MODE_DESTROYPROPBLOB,
        "MODE_DESTROYPROPBLOB",
        DrmModeDestroyBlob,
        k::destroy_blob
    ),
    def!(
        DRM_IOCTL_MODE_ADDFB2,
        "MODE_ADDFB2",
        DrmModeFbCmd2,
        k::add_fb2
    ),
    def!(DRM_IOCTL_MODE_RMFB, "MODE_RMFB", k::Id, k::rm_fb),
    def!(
        DRM_IOCTL_MODE_CLOSEFB,
        "MODE_CLOSEFB",
        DrmModeClosefb,
        k::close_fb
    ),
];

/// Runs request `req` on `file`.
pub(crate) fn dispatch(file: &Open, req: u32, arg: &mut [u8]) -> Result<usize, Errno> {
    if !is_drm_ioctl(req) {
        return Err(Errno::ENOTTY);
    }
    if file.card.is_revoked() {
        return Err(Errno::ENODEV);
    }
    let n = ioctl_nr(req);
    if (DRM_COMMAND_BASE..DRM_COMMAND_END).contains(&n) {
        return file.card.driver().ioctl(n, arg);
    }
    let Some(def) = TABLE.iter().find(|d| d.nr == n) else {
        if let Some(env) = crate::env() {
            (env.log)(format_args!(
                "drm: card{}: request {n:#04x} ({req:#010x}) is not implemented\n",
                file.card.unit()
            ));
        }
        return Err(Errno::ENOTTY);
    };
    let _ = def.name;
    // The handler's own copy of the structure, sized as it expects.
    let mut k: Vec<u8> = Vec::new();
    k.try_reserve_exact(def.size).map_err(|_| Errno::ENOMEM)?;
    k.resize(def.size, 0);
    let common = arg.len().min(def.size);
    k[..common].copy_from_slice(&arg[..common]);
    let r = (def.handler)(file, &mut k)?;
    if req & IOC_OUT != 0 {
        arg[..common].copy_from_slice(&k[..common]);
    }
    Ok(r)
}

/// `drm_version`: names are copied out up to the lengths the caller
/// gave, and the lengths are set to the full ones (no NUL).
fn version(f: &Open, a: &mut DrmVersion) -> Result<usize, Errno> {
    let env = crate::env().ok_or(Errno::ENXIO)?;
    let info = f.card.driver().info();
    a.version_major = info.major;
    a.version_minor = info.minor;
    a.version_patchlevel = info.patch;
    for (len, ptr, text) in [
        (&mut a.name_len, a.name, info.name),
        (&mut a.date_len, a.date, info.date),
        (&mut a.desc_len, a.desc, info.desc),
    ] {
        let cap = usize::try_from(*len).unwrap_or(usize::MAX);
        let n = text.len().min(cap);
        if n != 0 && ptr != 0 {
            env.user.copy_out(ptr, &text.as_bytes()[..n])?;
        }
        *len = text.len() as u64;
    }
    Ok(0)
}

fn gem_close(f: &Open, a: &mut DrmGemClose) -> Result<usize, Errno> {
    // The buffer may be the last holder: free it outside the lock.
    let gone = f.handles.lock().remove(a.handle);
    gone.map(|_| 0).ok_or(Errno::EINVAL)
}

fn get_cap(f: &Open, a: &mut DrmGetCap) -> Result<usize, Errno> {
    let d = f.card.driver();
    let dumb = d.dumb();
    a.value = match a.capability {
        DRM_CAP_DUMB_BUFFER => u64::from(dumb.is_some()),
        DRM_CAP_VBLANK_HIGH_CRTC => 1,
        DRM_CAP_DUMB_PREFERRED_DEPTH => dumb.map_or(0, |l| u64::from(l.preferred_depth)),
        DRM_CAP_DUMB_PREFER_SHADOW => dumb.map_or(0, |l| u64::from(l.prefer_shadow)),
        // No dma-buf until M-G.2.
        DRM_CAP_PRIME => 0,
        DRM_CAP_TIMESTAMP_MONOTONIC => 1,
        DRM_CAP_ASYNC_PAGE_FLIP | DRM_CAP_PAGE_FLIP_TARGET => 0,
        DRM_CAP_CURSOR_WIDTH | DRM_CAP_CURSOR_HEIGHT => 64,
        DRM_CAP_ADDFB2_MODIFIERS | DRM_CAP_CRTC_IN_VBLANK_EVENT => u64::from(d.has_kms()),
        DRM_CAP_SYNCOBJ | DRM_CAP_SYNCOBJ_TIMELINE | DRM_CAP_ATOMIC_ASYNC_PAGE_FLIP => 0,
        other => d.cap(other).ok_or(Errno::EINVAL)?,
    };
    Ok(0)
}

fn set_client_cap(f: &Open, a: &mut DrmSetClientCap) -> Result<usize, Errno> {
    let (cap, value) = (a.capability, a.value);
    if value > 1 {
        return Err(Errno::EINVAL);
    }
    let bits = match cap {
        DRM_CLIENT_CAP_STEREO_3D => client::STEREO_3D,
        DRM_CLIENT_CAP_UNIVERSAL_PLANES => client::UNIVERSAL_PLANES,
        // Atomic turns on universal planes and aspect ratios with it.
        DRM_CLIENT_CAP_ATOMIC => client::ATOMIC | client::UNIVERSAL_PLANES | client::ASPECT_RATIO,
        DRM_CLIENT_CAP_ASPECT_RATIO => client::ASPECT_RATIO,
        DRM_CLIENT_CAP_WRITEBACK_CONNECTORS => {
            if !f.has_client_cap(client::ATOMIC) {
                return Err(Errno::EINVAL);
            }
            client::WRITEBACK
        }
        _ => return Err(Errno::EINVAL),
    };
    if !f.card.driver().has_kms() {
        return Err(Errno::EOPNOTSUPP);
    }
    if value == 1 {
        f.client_caps.fetch_or(bits, Ordering::Relaxed);
    } else if cap == DRM_CLIENT_CAP_ATOMIC {
        // Atomic can be turned on, not off.
        return Err(Errno::EINVAL);
    } else {
        f.client_caps.fetch_and(!bits, Ordering::Relaxed);
    }
    Ok(0)
}

fn set_master(f: &Open, _: &mut Empty) -> Result<usize, Errno> {
    f.card.set_master(f.id).map(|()| 0)
}

fn drop_master(f: &Open, _: &mut Empty) -> Result<usize, Errno> {
    f.card.drop_master(f.id).map(|()| 0)
}

/// `drm_set_client_name`: at most 63 printable bytes, set once.
fn set_client_name(f: &Open, a: &mut DrmSetClientName) -> Result<usize, Errno> {
    const MAX: u64 = 64;
    let env = crate::env().ok_or(Errno::ENXIO)?;
    if a.name_len > MAX {
        return Err(Errno::EINVAL);
    }
    let len = usize::try_from(a.name_len).unwrap_or(0);
    let mut buf = Vec::new();
    buf.try_reserve_exact(len).map_err(|_| Errno::ENOMEM)?;
    buf.resize(len, 0);
    env.user.copy_in(&mut buf, a.name)?;
    if buf.iter().any(|b| !(0x20..0x7f).contains(b)) {
        return Err(Errno::EINVAL);
    }
    let mut name = f.name.lock();
    if name.is_some() {
        return Err(Errno::EINVAL);
    }
    *name = Some(String::from_utf8(buf).map_err(|_| Errno::EINVAL)?);
    Ok(0)
}

/// `MODE_CREATE_DUMB`: width, height, bpp in; handle, pitch, size out.
fn create_dumb(f: &Open, a: &mut DrmModeCreateDumb) -> Result<usize, Errno> {
    const MAX_DIM: u32 = 16384;
    let env = crate::env().ok_or(Errno::ENXIO)?;
    let limits = f.card.driver().dumb().ok_or(Errno::ENOTTY)?;
    let (width, height, bpp) = (a.width, a.height, a.bpp);
    if width == 0 || height == 0 || bpp == 0 || width > MAX_DIM || height > MAX_DIM || bpp > 64 {
        return Err(Errno::EINVAL);
    }
    let row = u64::from(width) * u64::from(bpp.div_ceil(8));
    let align = u64::from(limits.pitch_align.max(1));
    let pitch = row.checked_next_multiple_of(align).ok_or(Errno::EINVAL)?;
    let size = pitch.checked_mul(u64::from(height)).ok_or(Errno::EINVAL)?;
    if size > limits.max_size {
        return Err(Errno::EINVAL);
    }
    let pages = size.div_ceil(PAGE_SIZE);
    let offset = f.card.alloc_offset(pages)?;
    let gem = Gem::new(env.pages, size, offset)?;
    a.size = gem.size();
    a.handle = f.handles.lock().insert(Arc::clone(&gem))?;
    a.pitch = u32::try_from(pitch).map_err(|_| Errno::EINVAL)?;
    Ok(0)
}

fn map_dumb(f: &Open, a: &mut DrmModeMapDumb) -> Result<usize, Errno> {
    f.card.driver().dumb().ok_or(Errno::ENOTTY)?;
    let gem = f.gem(a.handle).ok_or(Errno::ENOENT)?;
    a.offset = gem.mmap_offset();
    Ok(0)
}

fn destroy_dumb(f: &Open, a: &mut DrmModeDestroyDumb) -> Result<usize, Errno> {
    f.card.driver().dumb().ok_or(Errno::ENOTTY)?;
    let gone = f.handles.lock().remove(a.handle);
    gone.map(|_| 0).ok_or(Errno::EINVAL)
}
