// SPDX-License-Identifier: BSD-2-Clause
//! The FIFO transfers against a fake device and fake user memory.

extern crate std;

use alloc::vec;
use alloc::vec::Vec;

use abi::usb::{USB_FS_FLAG_FORCE_SHORT, USB_FS_FLAG_SINGLE_SHORT_OK, UsbFsOpen, err};
use usb::descriptor::{Companion, Endpoint};

use super::*;

const BASE: u64 = 0x1000_0000;
/// Where the test puts the `usb_fs_endpoint` array, the frames'
/// pointers, their lengths, and their data.
const EPS: u64 = BASE;
const PTRS: u64 = BASE + 0x1000;
const LENS: u64 = BASE + 0x2000;
const DATA: u64 = BASE + 0x3000;

struct Mem {
    bytes: SpinLock<Vec<u8>>,
    privileged: bool,
}

impl Mem {
    fn new(privileged: bool) -> Self {
        Self {
            bytes: SpinLock::new(vec![0; 0x10_000]),
            privileged,
        }
    }

    fn range(at: u64, len: usize) -> Result<core::ops::Range<usize>, Errno> {
        let start = usize::try_from(at.checked_sub(BASE).ok_or(Errno::EFAULT)?)
            .map_err(|_| Errno::EFAULT)?;
        let end = start + len;
        if end > 0x10_000 {
            return Err(Errno::EFAULT);
        }
        Ok(start..end)
    }

    fn put(&self, at: u64, b: &[u8]) {
        let r = Self::range(at, b.len()).unwrap();
        self.bytes.lock()[r].copy_from_slice(b);
    }

    fn get(&self, at: u64, len: usize) -> Vec<u8> {
        let r = Self::range(at, len).unwrap();
        self.bytes.lock()[r].to_vec()
    }

    fn u32_at(&self, at: u64) -> u32 {
        u32::from_ne_bytes(self.get(at, 4).try_into().unwrap())
    }

    fn i32_at(&self, at: u64) -> i32 {
        i32::from_ne_bytes(self.get(at, 4).try_into().unwrap())
    }

    /// Describes transfer `index`: `frames` (each a length, its data at
    /// `DATA + 0x400 * n`), with `flags`.
    fn describe(&self, index: u8, frames: &[u32], flags: u16) {
        let base = u64::from(index) * 0x100;
        for (n, &len) in frames.iter().enumerate() {
            let n64 = u64::try_from(n).unwrap();
            self.put(PTRS + base + 8 * n64, &(DATA + 0x400 * n64).to_ne_bytes());
            self.put(LENS + base + 4 * n64, &len.to_ne_bytes());
        }
        let mut e = [0u8; UsbFsEndpoint::LEN];
        e[0..8].copy_from_slice(&(PTRS + base).to_ne_bytes());
        e[8..16].copy_from_slice(&(LENS + base).to_ne_bytes());
        e[16..20].copy_from_slice(&u32::try_from(frames.len()).unwrap().to_ne_bytes());
        e[24..26].copy_from_slice(&flags.to_ne_bytes());
        e[26..28].copy_from_slice(&1000u16.to_ne_bytes());
        self.put(
            EPS + u64::from(index) * u64::try_from(UsbFsEndpoint::LEN).unwrap(),
            &e,
        );
    }

    fn status(&self, index: u8) -> (i32, u32) {
        let at = EPS + u64::from(index) * u64::try_from(UsbFsEndpoint::LEN).unwrap();
        (self.i32_at(at + 32), self.u32_at(at + 20))
    }
}

impl Kernel for Mem {
    fn copy_in(&self, dst: &mut [u8], src: u64) -> Result<(), Errno> {
        let r = Self::range(src, dst.len())?;
        dst.copy_from_slice(&self.bytes.lock()[r]);
        Ok(())
    }

    fn copy_out(&self, dst: u64, src: &[u8]) -> Result<(), Errno> {
        let r = Self::range(dst, src.len())?;
        self.bytes.lock()[r].copy_from_slice(src);
        Ok(())
    }

    fn priv_driver(&self) -> Result<(), Errno> {
        if self.privileged {
            Ok(())
        } else {
            Err(Errno::EPERM)
        }
    }
}

/// A high-speed device with a bulk IN (0x81), a bulk OUT (0x02, 512
/// bytes), and an interrupt IN (0x83); what is submitted waits until the
/// test completes it.
struct Dev {
    queued: SpinLock<Vec<(u8, Option<Setup>, Transfer)>>,
    aborted: SpinLock<Vec<u8>>,
    reset: SpinLock<Vec<u8>>,
}

impl Default for Dev {
    fn default() -> Self {
        Self {
            queued: SpinLock::new(Vec::new()),
            aborted: SpinLock::new(Vec::new()),
            reset: SpinLock::new(Vec::new()),
        }
    }
}

impl Dev {
    fn take(&self) -> (u8, Option<Setup>, Transfer) {
        let mut q = self.queued.lock();
        assert!(!q.is_empty(), "nothing submitted");
        q.remove(0)
    }

    fn pending(&self) -> usize {
        self.queued.lock().len()
    }
}

fn ep(address: u8, attributes: u8, mps: u16) -> Endpoint {
    Endpoint {
        address,
        attributes,
        max_packet_raw: mps,
        interval: 1,
        companion: None::<Companion>,
        extra: Vec::new(),
    }
}

impl Pipe for Dev {
    fn submit(&self, endpoint: u8, setup: Option<Setup>, t: Transfer) -> Result<(), Rejected> {
        self.queued.lock().push((endpoint, setup, t));
        Ok(())
    }

    fn abort(&self, _s: &Sleepable, endpoint: u8) {
        self.aborted.lock().push(endpoint);
        let mine: Vec<Transfer> = {
            let mut q = self.queued.lock();
            let (mine, rest): (Vec<_>, Vec<_>) = q.drain(..).partition(|(e, _, _)| *e == endpoint);
            *q = rest;
            mine.into_iter().map(|(_, _, t)| t).collect()
        };
        for t in mine {
            t.complete(Err(UsbError::Cancelled));
        }
    }

    fn clear_halt(&self, _s: &Sleepable, _endpoint: u8) -> Result<(), UsbError> {
        Ok(())
    }

    fn reset_endpoint(&self, _s: &Sleepable, endpoint: u8) {
        self.reset.lock().push(endpoint);
    }

    fn speed(&self) -> Speed {
        Speed::High
    }

    fn endpoint(&self, address: u8) -> Option<Endpoint> {
        match address {
            0 | 0x80 => Some(ep(0, 0, 64)),
            0x81 => Some(ep(0x81, 2, 512)),
            0x02 => Some(ep(0x02, 2, 512)),
            0x83 => Some(ep(0x83, 3, 8)),
            _ => None,
        }
    }
}

fn s() -> Sleepable {
    // SAFETY: host tests run on threads that may block.
    #[allow(unsafe_code)]
    unsafe {
        Sleepable::assume()
    }
}

fn opened(ep_no: u8, index: u8, read: bool, write: bool) -> (Arc<Fs>, Arc<Dev>) {
    let fs = Fs::new();
    fs.init(0, EPS, 8).unwrap();
    let dev = Arc::new(Dev::default());
    let mut o = UsbFsOpen {
        max_bufsize: 4096,
        max_frames: 4,
        ep_index: index,
        ep_no,
        ..UsbFsOpen::default()
    };
    let pipe: Arc<dyn Pipe> = dev.clone();
    fs.open(pipe, &mut o, 0, read, write).unwrap();
    (fs, dev)
}

/// Completes the next submitted transfer with `data` (a read's) or
/// `Ok(moved)`.
fn finish(
    dev: &Dev,
    data: Option<&[u8]>,
    r: Result<usize, UsbError>,
) -> (u8, Option<Setup>, usize) {
    let (e, setup, mut t) = dev.take();
    let len = t.len();
    if let Some(d) = data {
        let mut v = vec![0; len];
        v[..d.len()].copy_from_slice(d);
        t.buf = IoBuf::Owned(v);
    }
    t.complete(r);
    (e, setup, len)
}

#[test]
fn init_and_open_check_their_arguments() {
    let fs = Fs::new();
    assert_eq!(fs.init(0, 0, 4), Err(Errno::EINVAL));
    assert_eq!(fs.init(0, EPS, 127), Err(Errno::EINVAL));
    assert_eq!(fs.init(1, EPS, 4), Err(Errno::EINVAL));
    assert_eq!(fs.init(0, EPS, 0), Err(Errno::EINVAL));
    fs.init(0, EPS, 4).unwrap();
    assert!(fs.is_active());
    assert_eq!(fs.init(0, EPS, 4), Err(Errno::EBUSY));
    let dev: Arc<dyn Pipe> = Arc::new(Dev::default());
    let mut o = UsbFsOpen {
        max_bufsize: 100_000,
        max_frames: 1,
        ep_index: 0,
        ep_no: 0x81,
        ..UsbFsOpen::default()
    };
    // Reading needs the open to read.
    assert_eq!(
        fs.open(dev.clone(), &mut o, 0, false, true),
        Err(Errno::EPERM)
    );
    fs.open(dev.clone(), &mut o, 0, true, false).unwrap();
    // High-speed bulk: 512-byte frames, the buffer whole frames.
    assert_eq!(
        (o.max_packet_length, o.max_bufsize, o.max_frames),
        (512, 100_352, 1)
    );
    assert_eq!(
        fs.open(dev.clone(), &mut o, 0, true, false),
        Err(Errno::EBUSY)
    );
    o.ep_index = 4;
    assert_eq!(
        fs.open(dev.clone(), &mut o, 0, true, false),
        Err(Errno::EINVAL)
    );
    o.ep_index = 1;
    o.ep_no = 0x05;
    assert_eq!(
        fs.open(dev.clone(), &mut o, 0, true, true),
        Err(Errno::EINVAL)
    );
    o.ep_no = 0x02;
    o.max_frames = 0;
    assert_eq!(fs.open(dev, &mut o, 0, true, true), Err(Errno::EINVAL));
    assert_eq!(fs.uninit(None).map(|a| a.len()), Ok(0));
    assert!(!fs.is_active());
    assert_eq!(fs.uninit(None).map(|a| a.len()), Err(Errno::EINVAL));
}

#[test]
fn a_bulk_read_returns_its_data_and_length() {
    let mem = Mem::new(false);
    let (fs, dev) = opened(0x81, 2, true, false);
    mem.describe(2, &[64], USB_FS_FLAG_SINGLE_SHORT_OK);
    fs.start(&s(), &mem, 2).unwrap();
    // Running: a second start is refused, and nothing is complete yet.
    assert_eq!(fs.start(&s(), &mem, 2), Err(Errno::EBUSY));
    assert!(!fs.has_completion());
    assert_eq!(fs.complete(&mem), Err(Errno::EBUSY));
    let (e, setup, len) = finish(&dev, Some(b"hello"), Ok(5));
    assert_eq!((e, setup, len), (0x81, None, 64));
    assert!(fs.has_completion());
    assert_eq!(fs.complete(&mem), Ok(2));
    assert_eq!(mem.get(DATA, 5), b"hello");
    assert_eq!(mem.u32_at(LENS + 0x200), 5);
    assert_eq!(mem.status(2), (err::NORMAL_COMPLETION, 1));
    assert!(!fs.has_completion());
    // It can go again.
    fs.start(&s(), &mem, 2).unwrap();
    assert_eq!(dev.pending(), 1);
}

#[test]
fn a_short_read_without_a_short_flag_is_an_error() {
    let mem = Mem::new(false);
    let (fs, dev) = opened(0x81, 0, true, false);
    mem.describe(0, &[64, 64], 0);
    fs.start(&s(), &mem, 0).unwrap();
    finish(&dev, Some(b"ab"), Ok(2));
    // The short frame ended it: the second never went.
    assert_eq!(dev.pending(), 0);
    assert_eq!(fs.complete(&mem), Ok(0));
    assert_eq!(mem.status(0), (err::SHORT_XFER, 1));
}

#[test]
fn a_write_goes_frame_by_frame_and_ends_short_when_asked() {
    let mem = Mem::new(false);
    let (fs, dev) = opened(0x02, 1, false, true);
    mem.describe(1, &[512, 3], 0);
    mem.put(DATA, &[0xaa; 512]);
    mem.put(DATA + 0x400, b"xyz");
    fs.start(&s(), &mem, 1).unwrap();
    let (e, _, len) = dev.take_peek();
    assert_eq!((e, len), (0x02, 512));
    let (_, _, t) = dev.take();
    let IoBuf::Owned(v) = &t.buf else { panic!() };
    assert!(v.iter().all(|&b| b == 0xaa));
    t.complete(Ok(512));
    let (_, _, t) = dev.take();
    let IoBuf::Owned(v) = &t.buf else { panic!() };
    assert_eq!(v, b"xyz");
    t.complete(Ok(3));
    assert_eq!(fs.complete(&mem), Ok(1));
    assert_eq!(mem.status(1), (err::NORMAL_COMPLETION, 2));
    // A whole last packet with FORCE_SHORT: a zero-length packet follows.
    mem.describe(1, &[512], USB_FS_FLAG_FORCE_SHORT);
    fs.start(&s(), &mem, 1).unwrap();
    finish(&dev, None, Ok(512));
    let (_, _, len) = finish(&dev, None, Ok(0));
    assert_eq!(len, 0);
    assert_eq!(fs.complete(&mem), Ok(1));
    assert_eq!(mem.status(1), (err::NORMAL_COMPLETION, 1));
}

impl Dev {
    fn take_peek(&self) -> (u8, Option<Setup>, usize) {
        let q = self.queued.lock();
        let (e, s, t) = &q[0];
        (*e, *s, t.len())
    }
}

#[test]
fn a_control_transfer_is_its_setup_and_data_frames() {
    let mem = Mem::new(false);
    let (fs, dev) = opened(0, 0, true, true);
    // GET_DESCRIPTOR(device), 18 bytes.
    mem.describe(0, &[8, 18], USB_FS_FLAG_SINGLE_SHORT_OK);
    mem.put(DATA, &[0x80, 6, 0, 1, 0, 0, 18, 0]);
    fs.start(&s(), &mem, 0).unwrap();
    let (e, setup, len) = finish(&dev, Some(&[18, 1, 0, 2]), Ok(18));
    assert_eq!((e, len), (0, 18));
    assert_eq!(setup, Some(Setup::get_descriptor(1, 0, 0, 18)));
    assert_eq!(fs.complete(&mem), Ok(0));
    assert_eq!(mem.status(0), (err::NORMAL_COMPLETION, 2));
    assert_eq!(mem.get(DATA + 0x400, 4), [18, 1, 0, 2]);
    assert_eq!(mem.u32_at(LENS + 4), 18);
    // The setup frame must be eight bytes.
    mem.describe(0, &[7, 18], 0);
    fs.start(&s(), &mem, 0).unwrap();
    assert_eq!(dev.pending(), 0);
    assert_eq!(fs.complete(&mem), Ok(0));
    assert_eq!(mem.status(0).0, err::INVAL);
}

#[test]
fn control_requests_are_checked_as_do_request_checks_them() {
    let mem = Mem::new(false);
    let (fs, dev) = opened(0, 0, true, true);
    // SET_CONFIGURATION needs privilege.
    mem.describe(0, &[8], 0);
    mem.put(DATA, &[0, 9, 1, 0, 0, 0, 0, 0]);
    fs.start(&s(), &mem, 0).unwrap();
    assert_eq!(dev.pending(), 0);
    assert_eq!(fs.complete(&mem), Ok(0));
    assert_eq!(mem.status(0).0, err::INVAL);
    // A request to an endpoint the device lacks.
    mem.put(DATA, &[2, 1, 0, 0, 0x85, 0, 0, 0]);
    fs.start(&s(), &mem, 0).unwrap();
    assert_eq!(dev.pending(), 0);
    assert_eq!(fs.complete(&mem), Ok(0));
    assert_eq!(mem.status(0).0, err::INVAL);
    // Clearing a real endpoint's halt resets its toggle too.
    mem.put(DATA, &[2, 1, 0, 0, 0x81, 0, 0, 0]);
    fs.start(&s(), &mem, 0).unwrap();
    assert_eq!(*dev.reset.lock(), [0x81]);
    finish(&dev, None, Ok(0));
    assert_eq!(fs.complete(&mem), Ok(0));
    assert_eq!(mem.status(0), (err::NORMAL_COMPLETION, 1));
    // As root, SET_CONFIGURATION goes out.
    let root = Mem::new(true);
    root.describe(0, &[8], 0);
    root.put(DATA, &[0, 9, 1, 0, 0, 0, 0, 0]);
    fs.start(&s(), &root, 0).unwrap();
    assert_eq!(dev.pending(), 1);
}

#[test]
fn stop_cancels_and_close_forgets() {
    let mem = Mem::new(false);
    let (fs, dev) = opened(0x83, 3, true, false);
    mem.describe(3, &[8], USB_FS_FLAG_SINGLE_SHORT_OK);
    fs.start(&s(), &mem, 3).unwrap();
    fs.stop(&s(), 3).unwrap();
    assert_eq!(*dev.aborted.lock(), [0x83]);
    assert_eq!(fs.complete(&mem), Ok(3));
    assert_eq!(mem.status(3), (err::CANCELLED, 0));
    // Stopping what is not running does nothing; out of range is EINVAL.
    fs.stop(&s(), 3).unwrap();
    assert_eq!(fs.stop(&s(), 8), Err(Errno::EINVAL));
    // A transfer closed while running completes into nothing.
    fs.start(&s(), &mem, 3).unwrap();
    fs.close(&s(), 3).unwrap();
    assert!(!fs.has_completion());
    assert_eq!(fs.close(&s(), 3), Err(Errno::EINVAL));
    // Uninit hands back what still runs, for the caller to cancel.
    let mut o = UsbFsOpen {
        max_bufsize: 8,
        max_frames: 1,
        ep_index: 5,
        ep_no: 0x83,
        ..UsbFsOpen::default()
    };
    let pipe: Arc<dyn Pipe> = dev.clone();
    fs.open(pipe, &mut o, 0, true, false).unwrap();
    mem.describe(5, &[8], 0);
    fs.start(&s(), &mem, 5).unwrap();
    let running = fs.uninit(None).unwrap();
    assert_eq!(running.len(), 1);
    assert_eq!(running[0].1, 0x83);
}

#[test]
fn bad_descriptions_complete_with_inval() {
    let mem = Mem::new(false);
    let (fs, dev) = opened(0x81, 0, true, false);
    // No frames, and more frames than opened.
    mem.describe(0, &[], 0);
    fs.start(&s(), &mem, 0).unwrap();
    assert_eq!(fs.complete(&mem), Ok(0));
    assert_eq!(mem.status(0).0, err::INVAL);
    mem.describe(0, &[1, 1, 1, 1, 1], 0);
    fs.start(&s(), &mem, 0).unwrap();
    assert_eq!(fs.complete(&mem), Ok(0));
    assert_eq!(mem.status(0).0, err::INVAL);
    // More data than the buffer.
    mem.describe(0, &[4096, 4096], 0);
    fs.start(&s(), &mem, 0).unwrap();
    assert_eq!(fs.complete(&mem), Ok(0));
    assert_eq!(mem.status(0).0, err::INVAL);
    assert_eq!(dev.pending(), 0);
    // An endpoint array pointer that faults.
    let fs = Fs::new();
    fs.init(0, BASE + 0x20_000, 1).unwrap();
    let mut o = UsbFsOpen {
        max_bufsize: 64,
        max_frames: 1,
        ep_no: 0x81,
        ..UsbFsOpen::default()
    };
    fs.open(dev, &mut o, 0, true, false).unwrap();
    assert_eq!(fs.start(&s(), &mem, 0), Err(Errno::EFAULT));
}

#[test]
fn errors_map_to_usb_errors() {
    assert_eq!(status_of(UsbError::Stalled), err::STALLED);
    assert_eq!(status_of(UsbError::TimedOut), err::TIMEOUT);
    assert_eq!(status_of(UsbError::Gone), err::CANCELLED);
    assert_eq!(status_of(UsbError::Transaction), err::IOERROR);
}

#[test]
fn a_stalled_read_reports_the_stall() {
    let mem = Mem::new(false);
    let (fs, dev) = opened(0x81, 0, true, false);
    mem.describe(0, &[64], 0);
    fs.start(&s(), &mem, 0).unwrap();
    finish(&dev, None, Err(UsbError::Stalled));
    assert_eq!(fs.complete(&mem), Ok(0));
    assert_eq!(mem.status(0), (err::STALLED, 0));
    // Nothing was written back to the length.
    assert_eq!(mem.u32_at(LENS), 64);
}
