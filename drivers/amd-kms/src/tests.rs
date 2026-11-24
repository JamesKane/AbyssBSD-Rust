// SPDX-License-Identifier: BSD-2-Clause
//! The card's parts against a display in memory.

use alloc::vec::Vec;
use std::sync::Mutex;

use super::*;

/// Video memory in a vector and a display that records what it is told.
struct Fake {
    mem: Mutex<Vec<u8>>,
    shown: Mutex<Vec<usize>>,
    pending: Mutex<u32>,
    flushes: Mutex<u32>,
}

impl Fake {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            mem: Mutex::new(std::vec![0; 256 << 20]),
            shown: Mutex::new(Vec::new()),
            pending: Mutex::new(0),
            flushes: Mutex::new(0),
        })
    }
}

impl Display for Fake {
    fn write(&self, offset: usize, data: &[u8]) {
        let mut m = self.mem.lock().unwrap();
        m[offset..offset + data.len()].copy_from_slice(data);
    }
    fn flush(&self) {
        *self.flushes.lock().unwrap() += 1;
    }
    fn show(&self, offset: usize) {
        self.shown.lock().unwrap().push(offset);
        // The flip waits for two polls of the pending bit.
        *self.pending.lock().unwrap() = 2;
    }
    fn pending(&self) -> bool {
        let mut p = self.pending.lock().unwrap();
        if *p > 0 {
            *p -= 1;
            true
        } else {
            false
        }
    }
    fn delay_us(&self, _: u32) {}
    fn usable(&self) -> usize {
        254 << 20
    }
}

fn card(w: u32, h: u32, d: &Arc<Fake>) -> Result<Arc<AmdKms>, Error> {
    create(
        Geometry {
            width: w,
            height: h,
        },
        d.clone(),
    )
}

#[test]
fn the_card_has_one_of_each_object_and_the_firmwares_mode() {
    let d = Fake::new();
    let c = card(1920, 1080, &d).unwrap();
    let k = c.kms().unwrap();
    let st = k.state();
    assert_eq!((st.crtcs.len(), st.planes.len(), st.conns.len()), (1, 1, 1));
    assert_eq!(c.info().name, "amd-display");
    assert!(c.dumb().is_some());
    assert!(!c.firmware_framebuffer());

    let m = mode_of(&Geometry {
        width: 1920,
        height: 1080,
    });
    assert_eq!(m.validate(), Ok(()));
    assert_eq!(m.refresh_hz(), 60);
    assert_eq!((m.htotal, m.vtotal, m.clock_khz), (2200, 1125, 148_500));
    // Another size has no blanking and is valid as well.
    let m = mode_of(&Geometry {
        width: 1280,
        height: 800,
    });
    assert_eq!(m.validate(), Ok(()));
}

#[test]
fn a_size_that_does_not_fit_makes_no_card() {
    let d = Fake::new();
    assert_eq!(card(0, 1080, &d).err(), Some(Error::Size));
    assert_eq!(card(1920, 0, &d).err(), Some(Error::Size));
    // 134 MB of pixels a surface do not fit in 254 MiB three times.
    assert_eq!(card(8192, 4096, &d).err(), Some(Error::Size));
    assert!(card(3840, 2160, &d).is_ok());
}

#[test]
fn the_back_surface_alternates_and_never_is_the_one_on_show() {
    assert_eq!(Front::Console.back(), Front::A);
    assert_eq!(Front::A.back(), Front::B);
    assert_eq!(Front::B.back(), Front::A);
    let at = surfaces(1920 * 1080 * 4);
    assert_eq!(at, (16 << 20, 32 << 20));
    // A 4K picture's first surface starts past the firmware's 33 MB (two steps).
    let at4k = surfaces(3840 * 2160 * 4);
    assert_eq!(at4k, (32 << 20, 64 << 20));
    assert_eq!(Front::Console.offset(at), 0);
    assert_eq!(Front::A.offset(at), at.0);
    assert_eq!(Front::B.offset(at), at.1);
}

#[test]
fn pixels_are_written_opaque_whatever_the_client_left_in_the_unused_byte() {
    let d = Fake::new();
    let mem = Opaque {
        display: &*d,
        base: 16 << 20,
    };
    // Two pixels with alpha 0 and 0x12, written at pixel 3 of a line.
    mem.write(12, &[1, 2, 3, 0x00, 4, 5, 6, 0x12]);
    let m = d.mem.lock().unwrap();
    assert_eq!(
        &m[(16 << 20) + 12..(16 << 20) + 20],
        &[1, 2, 3, 0xff, 4, 5, 6, 0xff]
    );
    assert_eq!(m[(16 << 20) + 11], 0, "the neighbours are untouched");
    assert_eq!(m[(16 << 20) + 20], 0);
}

#[test]
fn a_long_write_is_cut_into_pieces_with_the_pixels_kept_whole() {
    let d = Fake::new();
    let mem = Opaque {
        display: &*d,
        base: 0,
    };
    let src: Vec<u8> = (0..4096u32).map(|i| (i % 253) as u8).collect();
    mem.write(0, &src);
    let m = d.mem.lock().unwrap();
    for (i, px) in src.as_chunks::<4>().0.iter().enumerate() {
        assert_eq!(&m[i * 4..i * 4 + 3], &px[..3]);
        assert_eq!(m[i * 4 + 3], 0xff);
    }
}

#[test]
fn restoring_shows_the_firmwares_surface_once_and_waits_for_a_pending_flip() {
    let d = Fake::new();
    let c = card(1920, 1080, &d).unwrap();
    // Nothing shown yet: nothing to restore.
    c.restore();
    assert!(d.shown.lock().unwrap().is_empty());
    // Pretend a commit put surface A on show, with its flip still pending.
    *c.screen.front.lock() = Front::A;
    *d.pending.lock().unwrap() = 2;
    c.restore();
    assert_eq!(*d.shown.lock().unwrap(), std::vec![0]);
    assert_eq!(*c.screen.front.lock(), Front::Console);
    // The wait drained the earlier flip before showing, so it is the
    // restoring flip that is pending now.
    assert!(d.pending());
    // Restoring again does nothing.
    let before = d.shown.lock().unwrap().len();
    c.restore();
    assert_eq!(d.shown.lock().unwrap().len(), before);
}

#[test]
fn a_panic_shows_the_console_at_once_without_a_lock_or_a_wait() {
    let d = Fake::new();
    let c = card(1920, 1080, &d).unwrap();
    // Surface B is on show, its flip pending, and the lock that says so is
    // held (by a processor that has been stopped).
    *c.screen.front.lock() = Front::B;
    *d.pending.lock().unwrap() = 1_000_000;
    let _held = c.screen.front.lock();
    c.panic_restore();
    assert_eq!(*d.shown.lock().unwrap(), std::vec![0]);
}

#[test]
fn the_controller_has_a_vblank_interrupt_only_if_the_display_does() {
    let d = Fake::new();
    let c = card(1920, 1080, &d).unwrap();
    // The fake has none: the core times frames.
    assert!(!Crtc(c.screen.clone()).vblank_interrupt());
}
