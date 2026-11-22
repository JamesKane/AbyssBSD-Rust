// SPDX-License-Identifier: BSD-2-Clause
//! The DRM ioctl dispatcher.
//!
//! A request carries its argument's size, but DRM does not trust it:
//! each handler has the size of the structure it knows. A caller's
//! shorter structure is zero-extended to it; a longer one is cut to it.
//! Only the bytes the caller passed come back. Requests the core lacks
//! are logged, so a compositor or `modetest` says what it needs.

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

type Handler = fn(&Open, &mut [u8]) -> Result<usize, Errno>;

/// One core request: its number, name (for the log), and the size of the
/// structure its handler reads.
struct Def {
    nr: u8,
    name: &'static str,
    size: usize,
    handler: Handler,
}

const fn nr(req: u32) -> u8 {
    ioctl_nr(req)
}

const TABLE: &[Def] = &[
    Def {
        nr: nr(DRM_IOCTL_VERSION),
        name: "VERSION",
        size: 64,
        handler: version,
    },
    Def {
        nr: nr(DRM_IOCTL_GEM_CLOSE),
        name: "GEM_CLOSE",
        size: 8,
        handler: gem_close,
    },
    Def {
        nr: nr(DRM_IOCTL_GET_CAP),
        name: "GET_CAP",
        size: 16,
        handler: get_cap,
    },
    Def {
        nr: nr(DRM_IOCTL_SET_CLIENT_CAP),
        name: "SET_CLIENT_CAP",
        size: 16,
        handler: set_client_cap,
    },
    Def {
        nr: nr(DRM_IOCTL_SET_MASTER),
        name: "SET_MASTER",
        size: 0,
        handler: set_master,
    },
    Def {
        nr: nr(DRM_IOCTL_DROP_MASTER),
        name: "DROP_MASTER",
        size: 0,
        handler: drop_master,
    },
    Def {
        nr: nr(DRM_IOCTL_SET_CLIENT_NAME),
        name: "SET_CLIENT_NAME",
        size: 16,
        handler: set_client_name,
    },
    Def {
        nr: nr(DRM_IOCTL_MODE_CREATE_DUMB),
        name: "MODE_CREATE_DUMB",
        size: 32,
        handler: create_dumb,
    },
    Def {
        nr: nr(DRM_IOCTL_MODE_MAP_DUMB),
        name: "MODE_MAP_DUMB",
        size: 16,
        handler: map_dumb,
    },
    Def {
        nr: nr(DRM_IOCTL_MODE_DESTROY_DUMB),
        name: "MODE_DESTROY_DUMB",
        size: 4,
        handler: destroy_dumb,
    },
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

fn u32_at(b: &[u8], at: usize) -> u32 {
    let mut x = [0u8; 4];
    x.copy_from_slice(&b[at..at + 4]);
    u32::from_ne_bytes(x)
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut x = [0u8; 8];
    x.copy_from_slice(&b[at..at + 8]);
    u64::from_ne_bytes(x)
}

fn put_u32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_ne_bytes());
}

fn put_u64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_ne_bytes());
}

/// `drm_version`: names are copied out up to the lengths the caller
/// gave, and the lengths are set to the full ones (no NUL).
fn version(f: &Open, a: &mut [u8]) -> Result<usize, Errno> {
    let env = crate::env().ok_or(Errno::ENXIO)?;
    let info = f.card.driver().info();
    let put_i32 = |b: &mut [u8], at: usize, v: i32| b[at..at + 4].copy_from_slice(&v.to_ne_bytes());
    put_i32(a, 0, info.major);
    put_i32(a, 4, info.minor);
    put_i32(a, 8, info.patch);
    // (len at, pointer at, text)
    for (len_at, ptr_at, text) in [
        (16, 24, info.name),
        (32, 40, info.date),
        (48, 56, info.desc),
    ] {
        let cap = usize::try_from(u64_at(a, len_at)).unwrap_or(usize::MAX);
        let ptr = u64_at(a, ptr_at);
        let n = text.len().min(cap);
        if n != 0 && ptr != 0 {
            env.user.copy_out(ptr, &text.as_bytes()[..n])?;
        }
        put_u64(a, len_at, text.len() as u64);
    }
    Ok(0)
}

fn gem_close(f: &Open, a: &mut [u8]) -> Result<usize, Errno> {
    let handle = u32_at(a, 0);
    f.handles
        .lock()
        .remove(handle)
        .map(|_| 0)
        .ok_or(Errno::EINVAL)
}

fn get_cap(f: &Open, a: &mut [u8]) -> Result<usize, Errno> {
    let cap = u64_at(a, 0);
    let d = f.card.driver();
    let dumb = d.dumb();
    let v = match cap {
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
    put_u64(a, 8, v);
    Ok(0)
}

fn set_client_cap(f: &Open, a: &mut [u8]) -> Result<usize, Errno> {
    let cap = u64_at(a, 0);
    let value = u64_at(a, 8);
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

fn set_master(f: &Open, _: &mut [u8]) -> Result<usize, Errno> {
    f.card.set_master(f.id).map(|()| 0)
}

fn drop_master(f: &Open, _: &mut [u8]) -> Result<usize, Errno> {
    f.card.drop_master(f.id).map(|()| 0)
}

/// `drm_set_client_name`: at most 63 printable bytes, set once.
fn set_client_name(f: &Open, a: &mut [u8]) -> Result<usize, Errno> {
    const MAX: u64 = 64;
    let env = crate::env().ok_or(Errno::ENXIO)?;
    let len = u64_at(a, 0);
    let ptr = u64_at(a, 8);
    if len > MAX {
        return Err(Errno::EINVAL);
    }
    let mut buf = Vec::new();
    buf.try_reserve_exact(usize::try_from(len).unwrap_or(0))
        .map_err(|_| Errno::ENOMEM)?;
    buf.resize(usize::try_from(len).unwrap_or(0), 0);
    env.user.copy_in(&mut buf, ptr)?;
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
fn create_dumb(f: &Open, a: &mut [u8]) -> Result<usize, Errno> {
    const MAX_DIM: u32 = 16384;
    let env = crate::env().ok_or(Errno::ENXIO)?;
    let limits = f.card.driver().dumb().ok_or(Errno::ENOTTY)?;
    let height = u32_at(a, 0);
    let width = u32_at(a, 4);
    let bpp = u32_at(a, 8);
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
    let size = gem.size();
    let handle = f.handles.lock().insert(Arc::clone(&gem))?;
    put_u32(a, 16, handle);
    put_u32(a, 20, u32::try_from(pitch).map_err(|_| Errno::EINVAL)?);
    put_u64(a, 24, size);
    Ok(0)
}

fn map_dumb(f: &Open, a: &mut [u8]) -> Result<usize, Errno> {
    f.card.driver().dumb().ok_or(Errno::ENOTTY)?;
    let handle = u32_at(a, 0);
    let gem = f.gem(handle).ok_or(Errno::ENOENT)?;
    put_u64(a, 8, gem.mmap_offset());
    Ok(0)
}

fn destroy_dumb(f: &Open, a: &mut [u8]) -> Result<usize, Errno> {
    f.card.driver().dumb().ok_or(Errno::ENOTTY)?;
    gem_close(f, a)
}
