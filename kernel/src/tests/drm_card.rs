// SPDX-License-Identifier: BSD-2-Clause
//! The graphics core on the real kernel: a card over a framebuffer in
//! memory, driven the way a compositor drives it, minus the pointers a
//! kernel thread cannot pass. What the host tests cannot show: buffer
//! pages from the real page pool at the fake offsets above 4 GiB, the
//! frame timer on the softclock thread, and a blocking commit waiting
//! for it.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::drm::*;
use abi::errno::Errno;
use gop_kms::{FrameMemory, Geometry, PixelLayout};
use ktest::ktest;
use sync::SpinLock;
use vfs::CharDevice;

use super::cx;

const W: usize = 64;
const H: usize = 48;
const PAGE: usize = 4096;

/// A screen in memory, `0xcc` until something is shown.
struct HeapScreen(SpinLock<Vec<u8>>);

impl FrameMemory for HeapScreen {
    fn write(&self, offset: usize, data: &[u8]) {
        let mut m = self.0.lock();
        let end = (offset + data.len()).min(m.len());
        if offset < end {
            m[offset..end].copy_from_slice(&data[..end - offset]);
        }
    }
}

/// The pixel at `(x, y)` of frame `n`: opaque, `n` in red, `y` in green,
/// `x` in blue.
fn pixel(n: u32, x: usize, y: usize) -> u32 {
    0xff00_0000 | (n << 16) | (u32::try_from(y).unwrap() << 8) | u32::try_from(x).unwrap()
}

fn ctl<T: Wire>(f: &Arc<drm::Open>, req: u32, v: &T) -> Result<T, Errno> {
    let mut b = alloc::vec![0u8; T::SIZE];
    v.encode(&mut b);
    f.ioctl_file(&cx(), req, &mut b, 3)?;
    Ok(T::decode(&b))
}

/// A framebuffer of frame `n`, drawn through the buffer's mapping (the
/// object a user `mmap` would reach), as `DrmModeFbCmd2`'s id and the
/// buffer's handle.
fn make_frame(f: &Arc<drm::Open>, n: u32) -> (u32, u32) {
    let dumb = ctl(
        f,
        DRM_IOCTL_MODE_CREATE_DUMB,
        &DrmModeCreateDumb {
            height: u32::try_from(H).unwrap(),
            width: u32::try_from(W).unwrap(),
            bpp: 32,
            ..Default::default()
        },
    )
    .expect("a dumb buffer");
    assert_eq!(dumb.pitch as usize, W * 4);
    assert_eq!(usize::try_from(dumb.size).unwrap(), W * 4 * H);
    let map = ctl(
        f,
        DRM_IOCTL_MODE_MAP_DUMB,
        &DrmModeMapDumb {
            handle: dumb.handle,
            ..Default::default()
        },
    )
    .expect("a map offset");
    assert!(
        map.offset >= 4 << 30,
        "fake offsets start above 4 GiB: {:#x}",
        map.offset
    );
    let first = map.offset / PAGE as u64;
    let pages = dumb.size / PAGE as u64;
    // What `mmap` of the card's node at that offset gets: the buffer's
    // object, whose pages are populated by the first touch.
    let object = f.mmap(first, pages).expect("the buffer maps");
    for k in 0..pages {
        let pfn = object.page_for_write(first + k).expect("a page");
        object.access(pfn, &mut |page| {
            for (i, px) in page.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let g = usize::try_from(k).unwrap() * PAGE / 4 + i;
                px.copy_from_slice(&pixel(n, g % W, g / W).to_le_bytes());
            }
        });
    }
    // The same pages through the handle.
    let gem = f.gem(dumb.handle).expect("the handle's buffer");
    gem.with_page(0, &mut |p| {
        assert_eq!(&p[..4], &pixel(n, 0, 0).to_le_bytes())
    })
    .unwrap();
    let mut a = DrmModeFbCmd2 {
        width: u32::try_from(W).unwrap(),
        height: u32::try_from(H).unwrap(),
        pixel_format: drm_fourcc::format::XRGB8888.0,
        ..Default::default()
    };
    a.handles[0] = dumb.handle;
    a.pitches[0] = dumb.pitch;
    let fb = ctl(f, DRM_IOCTL_MODE_ADDFB2, &a)
        .expect("a framebuffer")
        .fb_id;
    (fb, dumb.handle)
}

fn shows(screen: &HeapScreen, n: u32) -> bool {
    let m = screen.0.lock();
    (0..H).all(|y| {
        (0..W).all(|x| {
            let at = (y * W + x) * 4;
            m[at..at + 4] == pixel(n, x, y).to_le_bytes()
        })
    })
}

fn read_event(f: &Arc<drm::Open>) -> Option<(u32, u64, u32, u32)> {
    let mut b = [0u8; 64];
    let mut r = vfs::ReadBuf::new(&mut b);
    match f.read(&cx(), &mut r, 0, true) {
        Ok(32) => {
            let w = |at: usize| u32::from_ne_bytes(r.filled()[at..at + 4].try_into().unwrap());
            let ud = u64::from_ne_bytes(r.filled()[8..16].try_into().unwrap());
            Some((w(0), ud, w(28), w(24)))
        }
        _ => None,
    }
}

ktest! {
    /// A card over a framebuffer: a buffer drawn through its mapping is
    /// shown by a blocking set-controller (the frame timer on the
    /// softclock thread ends the wait), flipped to another with an event,
    /// and the card goes away cleanly.
    fn a_card_over_a_framebuffer_shows_buffers_and_flips() {
        let s = cx();
        let screen: &'static HeapScreen =
            Box::leak(Box::new(HeapScreen(SpinLock::new(alloc::vec![0xcc; W * 4 * H]))));
        let driver = gop_kms::create(
            Geometry {
                width: u32::try_from(W).unwrap(),
                height: u32::try_from(H).unwrap(),
                stride_bytes: u32::try_from(W * 4).unwrap(),
                layout: PixelLayout::Bgrx,
            },
            screen,
        )
        .expect("a card for the framebuffer");
        let card = drm::register(driver).expect("the card registers");
        let f = card.open_file().expect("an open file");
        assert!(f.is_master(), "the first open holds the display");
        let kms = card.driver().kms().expect("a display");
        let (crtc, conn) = (kms.crtc_ids()[0], kms.connector_ids()[0]);

        let (fb1, _) = make_frame(&f, 1);
        let (fb2, _) = make_frame(&f, 2);
        assert!(!shows(screen, 1), "nothing is shown before it is asked for");

        // The mode the card reports, in the call's own terms.
        let mode = DrmModeModeinfo {
            clock: u32::try_from(W * H * 60 / 1000).unwrap(),
            hdisplay: u16::try_from(W).unwrap(),
            hsync_start: u16::try_from(W).unwrap(),
            hsync_end: u16::try_from(W).unwrap(),
            htotal: u16::try_from(W).unwrap(),
            vdisplay: u16::try_from(H).unwrap(),
            vsync_start: u16::try_from(H).unwrap(),
            vsync_end: u16::try_from(H).unwrap(),
            vtotal: u16::try_from(H).unwrap(),
            vrefresh: 60,
            ..Default::default()
        };
        let set = DrmModeCrtc { crtc_id: crtc, fb_id: fb1, mode_valid: 1, mode, ..Default::default() };
        let before = kms.frames(0);
        f.set_crtc(&s, &set, &[conn]).expect("the controller is set, and its frame comes");
        assert!(shows(screen, 1), "frame 1 is on the screen");
        assert_eq!(kms.frames(0), before + 1, "one frame ended the wait");

        // A flip to another buffer with an event, which the frame brings.
        ctl(
            &f,
            DRM_IOCTL_MODE_PAGE_FLIP,
            &DrmModeCrtcPageFlip { crtc_id: crtc, fb_id: fb2, flags: DRM_MODE_PAGE_FLIP_EVENT, reserved: 0, user_data: 0x77 },
        )
        .expect("a flip");
        assert!(shows(screen, 2), "frame 2 is copied as the flip is committed");
        let mut event = None;
        for _ in 0..50 {
            event = read_event(&f);
            if event.is_some() {
                break;
            }
            crate::time::sleep_ns(&s, 10_000_000);
        }
        let (kind, user_data, crtc_id, sequence) = event.expect("the flip's event within half a second");
        assert_eq!((kind, user_data, crtc_id), (DRM_EVENT_FLIP_COMPLETE, 0x77, crtc));
        assert_eq!(u64::from(sequence), kms.frames(0) & 0xffff_ffff);

        // Off, and the card goes away: open files are dead.
        let off = DrmModeCrtc { crtc_id: crtc, fb_id: 0, ..Default::default() };
        f.set_crtc(&s, &off, &[]).expect("off");
        card.unregister(&s);
        let r = ctl(&f, DRM_IOCTL_GET_CAP, &DrmGetCap { capability: DRM_CAP_DUMB_BUFFER, value: 0 });
        assert_eq!(r.err(), Some(Errno::ENODEV));
    }
}

/// A display's video memory, sparse, and what it was told to show.
struct HeapDisplay {
    mem: SpinLock<alloc::collections::BTreeMap<usize, u8>>,
    shown: SpinLock<Vec<usize>>,
}

impl amd_kms::Display for HeapDisplay {
    fn write(&self, offset: usize, data: &[u8]) {
        let mut m = self.mem.lock();
        for (i, b) in data.iter().enumerate() {
            m.insert(offset + i, *b);
        }
    }
    fn flush(&self) {}
    fn show(&self, offset: usize) {
        self.shown.lock().push(offset);
    }
    fn pending(&self) -> bool {
        false
    }
    fn delay_us(&self, _: u32) {}
    fn usable(&self) -> usize {
        256 << 20
    }
}

fn shows_at(d: &HeapDisplay, base: usize, n: u32) -> bool {
    let m = d.mem.lock();
    (0..H).all(|y| {
        (0..W).all(|x| {
            let at = base + (y * W + x) * 4;
            (0..4).all(|k| m.get(&(at + k)) == Some(&pixel(n, x, y).to_le_bytes()[k]))
        })
    })
}

ktest! {
    /// The Radeon's card on the real kernel: a commit draws the buffer
    /// into the back surface and flips to it, the next into the other,
    /// and turning the controller off returns to the firmware's surface.
    fn the_radeon_card_copies_into_the_back_surface_and_flips() {
        let s = cx();
        let display = Arc::new(HeapDisplay {
            mem: SpinLock::new(alloc::collections::BTreeMap::new()),
            shown: SpinLock::new(Vec::new()),
        });
        let driver = amd_kms::create(
            amd_kms::Geometry { width: u32::try_from(W).unwrap(), height: u32::try_from(H).unwrap() },
            display.clone(),
        )
        .expect("a card for the display");
        let card = drm::register(driver).expect("the card registers");
        let f = card.open_file().expect("an open file");
        let kms = card.driver().kms().expect("a display");
        let (crtc, conn) = (kms.crtc_ids()[0], kms.connector_ids()[0]);
        let (fb1, _) = make_frame(&f, 1);
        let (fb2, _) = make_frame(&f, 2);
        assert!(display.shown.lock().is_empty(), "nothing is shown before it is asked for");

        let mode = DrmModeModeinfo {
            clock: u32::try_from(W * H * 60 / 1000).unwrap(),
            hdisplay: u16::try_from(W).unwrap(),
            hsync_start: u16::try_from(W).unwrap(),
            hsync_end: u16::try_from(W).unwrap(),
            htotal: u16::try_from(W).unwrap(),
            vdisplay: u16::try_from(H).unwrap(),
            vsync_start: u16::try_from(H).unwrap(),
            vsync_end: u16::try_from(H).unwrap(),
            vtotal: u16::try_from(H).unwrap(),
            vrefresh: 60,
            flags: DRM_MODE_FLAG_NHSYNC | DRM_MODE_FLAG_NVSYNC,
            ..Default::default()
        };
        let set = DrmModeCrtc { crtc_id: crtc, fb_id: fb1, mode_valid: 1, mode, ..Default::default() };
        f.set_crtc(&s, &set, &[conn]).expect("the controller is set");
        let (a, b) = (16 << 20, 32 << 20);
        assert_eq!(*display.shown.lock(), alloc::vec![a], "the first picture is drawn on surface A and shown");
        assert!(shows_at(&display, a, 1), "frame 1 is in surface A, opaque");

        ctl(
            &f,
            DRM_IOCTL_MODE_PAGE_FLIP,
            &DrmModeCrtcPageFlip { crtc_id: crtc, fb_id: fb2, flags: 0, reserved: 0, user_data: 0 },
        )
        .expect("a flip");
        assert_eq!(*display.shown.lock(), alloc::vec![a, b], "the next picture goes to the other surface");
        assert!(shows_at(&display, b, 2), "frame 2 is in surface B");
        assert!(shows_at(&display, a, 1), "surface A still holds frame 1, which was on show");

        drm::panic_restore();
        assert_eq!(display.shown.lock().last(), Some(&0), "a panic shows the console's surface");
        let off = DrmModeCrtc { crtc_id: crtc, fb_id: 0, ..Default::default() };
        f.set_crtc(&s, &off, &[]).expect("off");
        assert_eq!(display.shown.lock().last(), Some(&0), "off returns to the firmware's surface");
        card.unregister(&s);
    }
}
