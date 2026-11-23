// SPDX-License-Identifier: BSD-2-Clause
//! The firmware framebuffer card on the host: pixel copies against
//! vectors, and what the card is made of.

use alloc::vec::Vec;
use std::boxed::Box;
use std::sync::Mutex;

use super::*;

/// A screen in memory.
struct Screen8(Mutex<Vec<u8>>);

impl FrameMemory for Screen8 {
    fn write(&self, offset: usize, data: &[u8]) {
        let mut m = self.0.lock().unwrap();
        let end = (offset + data.len()).min(m.len());
        if offset < end {
            m[offset..end].copy_from_slice(&data[..end - offset]);
        }
    }
}

/// A buffer in memory, handed out a 4096-byte page at a time as the real
/// buffers are.
struct Buf(Vec<u8>);

impl Pixels for Buf {
    fn chunk(&self, offset: u64, f: &mut dyn FnMut(&[u8])) -> bool {
        let at = usize::try_from(offset).unwrap();
        if at >= self.0.len() {
            return false;
        }
        let end = (at / 4096 + 1) * 4096;
        f(&self.0[at..end.min(self.0.len())]);
        true
    }
}

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| u8::try_from(i % 251).unwrap()).collect()
}

#[test]
fn a_region_is_copied_line_by_line_across_page_edges() {
    // A 300-pixel-wide buffer: 1200 bytes a line, so lines cross pages.
    let src = Buf(pattern(1200 * 40));
    let screen = Screen8(Mutex::new(std::vec![0xee; 1024 * 4 * 30]));
    let ok = copy_region(
        &src,
        0,
        1200,
        Rect {
            x: 10,
            y: 5,
            w: 100,
            h: 20,
        },
        (7, 3),
        1024 * 4,
        &screen,
    );
    assert!(ok);
    let s = screen.0.lock().unwrap();
    for row in 0..20usize {
        for col in 0..100usize {
            for b in 0..4usize {
                let from = (5 + row) * 1200 + (10 + col) * 4 + b;
                let to = (3 + row) * 4096 + (7 + col) * 4 + b;
                assert_eq!(s[to], src.0[from], "row {row} col {col} byte {b}");
            }
        }
    }
    // Nothing outside the rectangle changed.
    assert_eq!(s[3 * 4096 + 6 * 4 + 3], 0xee);
    assert_eq!(s[3 * 4096 + 107 * 4], 0xee);
    assert_eq!(s[2 * 4096 + 10 * 4], 0xee);
    assert_eq!(s[23 * 4096 + 10 * 4], 0xee);
}

#[test]
fn a_short_source_reports_it_and_copies_what_it_has() {
    let src = Buf(pattern(1200 * 4));
    let screen = Screen8(Mutex::new(std::vec![0; 1024 * 4 * 8]));
    let ok = copy_region(
        &src,
        0,
        1200,
        Rect {
            x: 0,
            y: 0,
            w: 10,
            h: 6,
        },
        (0, 0),
        4096,
        &screen,
    );
    assert!(!ok);
    let s = screen.0.lock().unwrap();
    assert_eq!(&s[..40], &src.0[..40]);
    assert_eq!(&s[3 * 4096..3 * 4096 + 40], &src.0[3 * 1200..3 * 1200 + 40]);
}

#[test]
fn pixel_layouts_have_a_standard_format() {
    assert_eq!(native_format(PixelLayout::Bgrx), Some(format::XRGB8888));
    assert_eq!(native_format(PixelLayout::Rgbx), Some(format::XBGR8888));
    let m = |red, green, blue| PixelLayout::Masks { red, green, blue };
    assert_eq!(
        native_format(m(0xff0000, 0xff00, 0xff)),
        Some(format::XRGB8888)
    );
    assert_eq!(
        native_format(m(0xff, 0xff00, 0xff0000)),
        Some(format::XBGR8888)
    );
    assert_eq!(native_format(m(0xf800, 0x7e0, 0x1f)), None);
}

fn geometry(w: u32, h: u32, stride: u32, layout: PixelLayout) -> Geometry {
    Geometry {
        width: w,
        height: h,
        stride_bytes: stride,
        layout,
    }
}

fn screen() -> &'static Screen8 {
    Box::leak(Box::new(Screen8(Mutex::new(Vec::new()))))
}

#[test]
fn the_card_has_one_of_each_object_and_the_firmwares_mode() {
    let card = create(geometry(1280, 800, 1280 * 4, PixelLayout::Bgrx), screen()).unwrap();
    let k = card.kms().unwrap();
    let st = k.state();
    assert_eq!((st.crtcs.len(), st.planes.len(), st.conns.len()), (1, 1, 1));
    assert_eq!(card.info().name, "simpledrm");
    assert!(card.dumb().is_some());

    let m = mode_of(&geometry(1920, 1080, 1920 * 4, PixelLayout::Bgrx));
    assert_eq!(m.validate(), Ok(()));
    assert_eq!(m.refresh_hz(), 60);
    assert_eq!((m.hdisplay, m.vdisplay), (1920, 1080));
    assert_eq!(m.kind & DRM_MODE_TYPE_PREFERRED, DRM_MODE_TYPE_PREFERRED);
}

#[test]
fn bad_framebuffers_make_no_card() {
    let ok = geometry(640, 480, 640 * 4, PixelLayout::Rgbx);
    assert!(create(ok, screen()).is_ok());
    assert_eq!(
        create(
            geometry(
                640,
                480,
                640 * 4,
                PixelLayout::Masks {
                    red: 1,
                    green: 2,
                    blue: 4
                }
            ),
            screen()
        )
        .err(),
        Some(Error::Layout)
    );
    assert_eq!(
        create(geometry(0, 480, 2560, PixelLayout::Bgrx), screen()).err(),
        Some(Error::Size)
    );
    assert_eq!(
        create(geometry(640, 480, 2000, PixelLayout::Bgrx), screen()).err(),
        Some(Error::Size)
    );
    assert_eq!(
        create(geometry(640, 480, 2562, PixelLayout::Bgrx), screen()).err(),
        Some(Error::Size)
    );
}
