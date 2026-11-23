// SPDX-License-Identifier: BSD-2-Clause
//! The firmware framebuffer as a graphics card (`drivers/gop-kms`): the
//! loader's framebuffer memory handed to the driver, and the card
//! registered so `/dev/dri/card0` exists on any machine that booted with
//! one. `vt` keeps drawing the console on it until a client puts its
//! window in graphics mode.

use bootproto::PixelFormat;
use gop_kms::{FrameMemory, Geometry, PixelLayout};

/// The framebuffer the loader mapped write-combining, for writing.
struct Memory {
    base: usize,
    len: usize,
}

impl FrameMemory for Memory {
    fn write(&self, offset: usize, data: &[u8]) {
        if offset >= self.len {
            return;
        }
        let n = data.len().min(self.len - offset);
        // SAFETY: the loader mapped `len` bytes at `base`, write
        // combining, for the kernel's lifetime; `offset + n` is inside
        // them, `data` is a kernel slice that cannot overlap device
        // memory, and `vt`, the only other writer, does not draw while a
        // client holds the screen in graphics mode.
        unsafe {
            core::ptr::copy_nonoverlapping(data.as_ptr(), (self.base + offset) as *mut u8, n);
        }
    }
}

/// Registers the framebuffer as a graphics card, if the loader found one.
pub(crate) fn start() {
    let info = crate::boot::info();
    let Some(f) = info.framebuffer.get() else {
        return;
    };
    let Ok(base) = usize::try_from(f.virt.get()) else {
        return;
    };
    let Ok(len) = usize::try_from(f.size) else {
        return;
    };
    let geometry = Geometry {
        width: f.width,
        height: f.height,
        stride_bytes: f.stride.saturating_mul(4),
        layout: match f.format {
            PixelFormat::Rgbx => PixelLayout::Rgbx,
            PixelFormat::Bgrx => PixelLayout::Bgrx,
            PixelFormat::Masks { red, green, blue } => PixelLayout::Masks { red, green, blue },
        },
    };
    let mem: &'static Memory =
        alloc::boxed::Box::leak(alloc::boxed::Box::new(Memory { base, len }));
    match gop_kms::create(geometry, mem) {
        Ok(card) => {
            if let Err(e) = drm::register(card) {
                kprintln!("gop-kms: not registered: {e:?}");
            }
        }
        Err(e) => kprintln!("gop-kms: no card for this framebuffer: {e:?}"),
    }
}
