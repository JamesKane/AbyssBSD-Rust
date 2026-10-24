// SPDX-License-Identifier: BSD-2-Clause
//! The frame buffer requests a virtual terminal answers (`sys/fbio.h`):
//! the frame buffer's type and size, its scan line, and its colors'
//! places, which X's `scfb` driver asks for.

use crate::ioctl::{IOC_IN, IOC_OUT, IOCPARM_MASK};
use crate::layout::{Arches, ConstValue};

/// `FBTYPE_EFIFB`: the UEFI Graphics Output Protocol's frame buffer.
pub const FBTYPE_EFIFB: i32 = 31;

const fn ioc(dir: u32, num: u8, len: u32) -> u32 {
    dir | ((len & IOCPARM_MASK) << 16) | (b'F' as u32) << 8 | num as u32
}

pub const FBIOGTYPE: u32 = ioc(IOC_OUT, 0, 24);
pub const FBIO_GETWINORG: u32 = ioc(IOC_OUT, 107, 4);
pub const FBIO_GETDISPSTART: u32 = ioc(IOC_OUT, 109, 8);
pub const FBIO_GETLINEWIDTH: u32 = ioc(IOC_OUT, 111, 4);
pub const FBIO_BLANK: u32 = ioc(IOC_IN, 115, 4);
pub const FBIO_GETRGBOFFS: u32 = ioc(IOC_OUT, 116, 12);

/// `struct fbtype` (`FBIOGTYPE`): the start of `struct fb_info`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fbtype {
    pub fb_type: i32,
    /// In pixels.
    pub fb_height: i32,
    pub fb_width: i32,
    /// Bits per pixel.
    pub fb_depth: i32,
    /// Color map entries.
    pub fb_cmsize: i32,
    /// In bytes.
    pub fb_size: i32,
}

impl Fbtype {
    pub const SIZE: usize = 24;

    /// Its bytes.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        let f = [
            self.fb_type,
            self.fb_height,
            self.fb_width,
            self.fb_depth,
            self.fb_cmsize,
            self.fb_size,
        ];
        for (d, v) in b.as_chunks_mut::<4>().0.iter_mut().zip(f) {
            *d = v.to_ne_bytes();
        }
        b
    }
}

const _: () = assert!(core::mem::size_of::<Fbtype>() == Fbtype::SIZE);

/// `struct fb_rgboffs` (`FBIO_GETRGBOFFS`): each color's lowest bit.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FbRgboffs {
    pub red: i32,
    pub green: i32,
    pub blue: i32,
}

impl FbRgboffs {
    pub const SIZE: usize = 12;

    /// Its bytes.
    #[must_use]
    pub fn to_bytes(&self) -> [u8; Self::SIZE] {
        let mut b = [0u8; Self::SIZE];
        for (d, v) in b
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip([self.red, self.green, self.blue])
        {
            *d = v.to_ne_bytes();
        }
        b
    }
}

const _: () = assert!(core::mem::size_of::<FbRgboffs>() == FbRgboffs::SIZE);

/// `video_display_start_t` (`FBIO_GETDISPSTART`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VideoDisplayStart {
    pub x: i32,
    pub y: i32,
}

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("FBIOGTYPE", FBIOGTYPE),
    c("FBIO_GETWINORG", FBIO_GETWINORG),
    c("FBIO_GETDISPSTART", FBIO_GETDISPSTART),
    c("FBIO_GETLINEWIDTH", FBIO_GETLINEWIDTH),
    c("FBIO_BLANK", FBIO_BLANK),
    c("FBIO_GETRGBOFFS", FBIO_GETRGBOFFS),
    ConstValue {
        c_name: "FBTYPE_EFIFB",
        header: "sys/fbio.h",
        value: FBTYPE_EFIFB as i64,
        arches: Arches::All,
    },
];

const fn c(c_name: &'static str, value: u32) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/fbio.h",
        value: value as i64,
        arches: Arches::All,
    }
}
