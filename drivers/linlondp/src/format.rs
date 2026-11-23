// SPDX-License-Identifier: BSD-2-Clause
//! The pixel formats the layers fetch (specification `linlon-dpu` 04
//! §4.1.1): each standard format, the hardware's identifier for it, which
//! kinds of layer take it, and what rotation it allows.
//!
//! Linear buffers only. The compressed (AFBC) formats of §4.1.2 and the
//! formats `drm-fourcc` does not yet name (RGBA1010102, BGRA1010102,
//! RGBA5551, ABGR1555, X0L2) are not built.

use alloc::vec::Vec;

use drm_fourcc::{Fourcc, format};

/// Which layers take a format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayerKind {
    /// A read layer with chroma upsampling.
    Rich,
    /// A read layer for RGB only.
    Simple,
    /// The write-back layer.
    WriteBack,
}

/// What rotation a format allows when linear.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rotation {
    /// Mirroring only.
    Flip,
    /// Neither rotation nor mirroring.
    None,
}

const R: u8 = 1;
const S: u8 = 2;
const W: u8 = 4;

/// A format as the hardware knows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HwFormat {
    pub fourcc: Fourcc,
    /// The 6-bit `L_FORMAT` value: `[5:3]` group, `[2:0]` index.
    pub id: u32,
    kinds: u8,
    pub rotation: Rotation,
}

impl HwFormat {
    /// Whether a layer of `kind` takes it.
    #[must_use]
    pub fn takes(&self, kind: LayerKind) -> bool {
        self.kinds
            & match kind {
                LayerKind::Rich => R,
                LayerKind::Simple => S,
                LayerKind::WriteBack => W,
            }
            != 0
    }
}

const fn f(fourcc: Fourcc, id: u32, kinds: u8, rotation: Rotation) -> HwFormat {
    HwFormat {
        fourcc,
        id,
        kinds,
        rotation,
    }
}

use Rotation::{Flip, None as NoRot};

/// Every linear format the layers take.
pub const FORMATS: &[HwFormat] = &[
    f(format::ARGB2101010, 0x00, R | S | W, Flip),
    f(format::ABGR2101010, 0x01, R | S | W, Flip),
    f(format::ARGB8888, 0x08, R | S | W, Flip),
    f(format::ABGR8888, 0x09, R | S | W, Flip),
    f(format::RGBA8888, 0x0A, R | S | W, Flip),
    f(format::BGRA8888, 0x0B, R | S | W, Flip),
    f(format::XRGB8888, 0x10, R | S | W, Flip),
    f(format::XBGR8888, 0x11, R | S | W, Flip),
    f(format::RGBX8888, 0x12, R | S | W, Flip),
    f(format::BGRX8888, 0x13, R | S | W, Flip),
    f(format::RGB888, 0x18, R | S | W, NoRot),
    f(format::BGR888, 0x19, R | S | W, NoRot),
    f(format::RGB565, 0x22, R | S, Flip),
    f(format::BGR565, 0x23, R | S, Flip),
    f(format::R8, 0x24, S, NoRot),
    f(format::YUYV, 0x2A, R, Flip),
    f(format::UYVY, 0x2B, R, Flip),
    f(format::NV12, 0x2E, R | W, Flip),
    f(format::YUV420, 0x2F, R, Flip),
    f(format::P010, 0x37, R, Flip),
];

/// The hardware's description of `fourcc`, if the layers take it.
#[must_use]
pub fn lookup(fourcc: Fourcc) -> Option<&'static HwFormat> {
    FORMATS.iter().find(|h| h.fourcc == fourcc)
}

/// The formats a layer of `kind` offers userland, each once.
#[must_use]
pub fn offered(kind: LayerKind) -> Vec<Fourcc> {
    FORMATS
        .iter()
        .filter(|h| h.takes(kind))
        .map(|h| h.fourcc)
        .collect()
}
