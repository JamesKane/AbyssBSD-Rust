// SPDX-License-Identifier: BSD-2-Clause
//! The DRM core on the host: a mock driver, heap pages, a user-memory
//! arena, and a devfs that records its nodes.

#![allow(unsafe_code, reason = "Sleepable::assume on a host thread")]

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use std::collections::HashMap;
use std::sync::Mutex;

use abi::drm::*;
use abi::errno::Errno;
use addr::PAGE_SIZE;
use sched::Sleepable;
use vfs::CharDevice;

use crate::gem::OFFSET_START;
use crate::{DriverInfo, DrmDriver, DumbLimits, Env, UserMemory};

fn s() -> Sleepable {
    // SAFETY: a host test thread may sleep.
    unsafe { Sleepable::assume() }
}

#[derive(Default)]
struct HeapPages {
    pages: Mutex<HashMap<u64, Box<[u8; 4096]>>>,
    next: core::sync::atomic::AtomicU64,
}

impl HeapPages {
    fn insert(&self, page: Box<[u8; 4096]>) -> vm::Pfn {
        let n = self
            .next
            .fetch_add(1, core::sync::atomic::Ordering::Relaxed)
            + 1;
        self.pages.lock().unwrap().insert(n, page);
        vm::Pfn(n)
    }
}

impl vm::object::PageOps for HeapPages {
    fn alloc_zeroed(&self) -> Option<vm::Pfn> {
        Some(self.insert(Box::new([0; 4096])))
    }
    fn alloc_copy(&self, from: vm::Pfn) -> Option<vm::Pfn> {
        let c = Box::new(*self.pages.lock().unwrap()[&from.0]);
        Some(self.insert(c))
    }
    fn alloc_filled(&self, fill: &mut dyn FnMut(&mut [u8])) -> Option<vm::Pfn> {
        let mut p = Box::new([0; 4096]);
        fill(&mut p[..]);
        Some(self.insert(p))
    }
    fn free(&self, pfn: vm::Pfn) {
        assert!(
            self.pages.lock().unwrap().remove(&pfn.0).is_some(),
            "double free"
        );
    }
    fn access(&self, pfn: vm::Pfn, f: &mut dyn FnMut(&mut [u8])) {
        f(&mut self
            .pages
            .lock()
            .unwrap()
            .get_mut(&pfn.0)
            .expect("freed page")[..]);
    }
}

/// "User memory": addresses are offsets into one arena, from `BASE`.
struct Arena(Mutex<Vec<u8>>);
const BASE: u64 = 0x1000;

impl UserMemory for Arena {
    fn copy_in(&self, dst: &mut [u8], src: u64) -> Result<(), Errno> {
        let m = self.0.lock().unwrap();
        let at = usize::try_from(src.checked_sub(BASE).ok_or(Errno::EFAULT)?).unwrap();
        dst.copy_from_slice(m.get(at..at + dst.len()).ok_or(Errno::EFAULT)?);
        Ok(())
    }
    fn copy_out(&self, dst: u64, src: &[u8]) -> Result<(), Errno> {
        let mut m = self.0.lock().unwrap();
        let at = usize::try_from(dst.checked_sub(BASE).ok_or(Errno::EFAULT)?).unwrap();
        m.get_mut(at..at + src.len())
            .ok_or(Errno::EFAULT)?
            .copy_from_slice(src);
        Ok(())
    }
}

static LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());
static NODES: Mutex<Vec<(String, u16, u32)>> = Mutex::new(Vec::new());

struct TestDevfs;

impl vfs::devices::Registry for TestDevfs {
    fn make(
        &self,
        path: &[u8],
        mode: u16,
        _: u32,
        gid: u32,
        _: Arc<dyn CharDevice>,
    ) -> Option<u64> {
        let p = String::from_utf8(path.to_vec()).unwrap();
        let mut n = NODES.lock().unwrap();
        if n.iter().any(|(q, ..)| *q == p) {
            return None;
        }
        n.push((p, mode, gid));
        Some(1)
    }
    fn alias(&self, _: &[u8], _: &[u8]) -> bool {
        false
    }
    fn destroy(&self, _: &Sleepable, path: &[u8]) {
        let p = String::from_utf8(path.to_vec()).unwrap();
        NODES.lock().unwrap().retain(|(q, ..)| *q != p);
    }
}

fn setup() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let arena: &'static Arena = Box::leak(Box::new(Arena(Mutex::new(std::vec![0; 1 << 16]))));
        let pages: &'static HeapPages = Box::leak(Box::default());
        crate::init(Env {
            user: arena,
            pages,
            log: |a| LOG.lock().unwrap().push(std::format!("{a}")),
            now_ns: || 5_000_000_000,
            ticker: || Box::new(TestTicker(Mutex::new(None))),
            export_dmabuf: |_, buf, cloexec, rdwr| {
                let mut t = DESCRIPTORS.lock().unwrap();
                t.push((buf, cloexec, rdwr));
                Ok(100 + i32::try_from(t.len()).unwrap())
            },
            import_dmabuf: |_, fd| {
                let t = DESCRIPTORS.lock().unwrap();
                let i = usize::try_from(fd - 101).map_err(|_| Errno::EBADF)?;
                t.get(i).map(|(b, ..)| b.clone()).ok_or(Errno::EBADF)
            },
        });
        vfs::devices::set_registry(Box::leak(Box::new(TestDevfs)));
        USER.set(arena).ok();
    });
}

static USER: std::sync::OnceLock<&'static Arena> = std::sync::OnceLock::new();

/// The descriptors the mock kernel has handed out for dma-bufs, from 101.
static DESCRIPTORS: Mutex<Vec<(Arc<crate::DmaBuf>, bool, bool)>> = Mutex::new(Vec::new());

type Timer = (usize, fn(&Sleepable, usize));
static TIMERS: Mutex<Vec<Timer>> = Mutex::new(Vec::new());

/// A timer that waits to be fired by hand ([`fire`]) or by [`pump`].
struct TestTicker(Mutex<Option<usize>>);

impl crate::Ticker for TestTicker {
    fn arm(&self, _delay_ns: u64, f: fn(&Sleepable, usize), arg: usize) {
        *self.0.lock().unwrap() = Some(arg);
        let mut t = TIMERS.lock().unwrap();
        t.retain(|(a, _)| *a != arg);
        t.push((arg, f));
    }
    fn cancel(&self) {
        if let Some(arg) = self.0.lock().unwrap().take() {
            TIMERS.lock().unwrap().retain(|(a, _)| *a != arg);
        }
    }
}

/// Fires the frame timers of card `unit`, as the softclock would.
fn fire(unit: u32) {
    let due: Vec<Timer> = {
        let mut t = TIMERS.lock().unwrap();
        let (due, keep) = t.drain(..).partition(|(a, _)| *a >> 8 == unit as usize);
        *t = keep;
        due
    };
    for (arg, f) in due {
        f(&s(), arg);
    }
}

/// A blocking commit waits for its frame: here it gets one.
pub(crate) fn idle(unit: u32) {
    fire(unit);
}

struct Mock {
    dumb: Option<DumbLimits>,
    kms: Option<crate::kms::Kms>,
    placement: crate::gem::Placement,
    /// The byte ranges the core asked to clean, by buffer size.
    cleaned: Arc<Mutex<Vec<u64>>>,
}

impl DrmDriver for Mock {
    fn info(&self) -> DriverInfo {
        DriverInfo {
            name: "mock",
            desc: "A mock",
            date: "20261122",
            major: 1,
            minor: 2,
            patch: 3,
        }
    }
    fn kms(&self) -> Option<&crate::kms::Kms> {
        self.kms.as_ref()
    }
    fn dumb(&self) -> Option<DumbLimits> {
        self.dumb
    }
    fn dumb_placement(&self) -> crate::gem::Placement {
        self.placement
    }
    fn sync_for_device(&self, _: &crate::gem::Gem, range: core::ops::Range<u64>) {
        self.cleaned.lock().unwrap().push(range.end - range.start);
    }
    fn cap(&self, cap: u64) -> Option<u64> {
        (cap == 0x1000).then_some(7)
    }
}

fn card(dumb: bool, kms: bool) -> Arc<crate::Card> {
    setup();
    crate::register(Arc::new(Mock {
        dumb: dumb.then(DumbLimits::default),
        kms: kms.then(|| mock_display(&Calls::default())),
        placement: crate::gem::Placement::SYSTEM_COHERENT,
        cleaned: Arc::default(),
    }))
    .unwrap()
}

/// A card with a display whose hardware calls are recorded.
fn card_logged() -> (Arc<crate::Card>, Calls) {
    setup();
    let calls = Calls::default();
    let c = crate::register(Arc::new(Mock {
        dumb: Some(DumbLimits::default()),
        kms: Some(mock_display(&calls)),
        placement: crate::gem::Placement::SYSTEM_COHERENT,
        cleaned: Arc::default(),
    }))
    .unwrap();
    (c, calls)
}

/// Opens the card through its node, as devfs does.
fn open(c: &Arc<crate::Card>) -> Arc<crate::Open> {
    c.open_file().unwrap()
}

fn call(f: &Arc<crate::Open>, req: u32, arg: &mut [u8]) -> Result<usize, Errno> {
    f.ioctl_file(&s(), req, arg, 3)
}

fn w(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

fn get_cap_arg(cap: u64, value: u64) -> Vec<u8> {
    w(&[&cap.to_ne_bytes(), &value.to_ne_bytes()])
}

fn create_dumb_arg(height: u32, width: u32, bpp: u32) -> Vec<u8> {
    w(&[
        &height.to_ne_bytes(),
        &width.to_ne_bytes(),
        &bpp.to_ne_bytes(),
        &[0u8; 20],
    ])
}

fn handle_arg(handle: u32, len: usize) -> Vec<u8> {
    let mut v = handle.to_ne_bytes().to_vec();
    v.resize(len, 0);
    v
}

fn u32_of(b: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes(b[at..at + 4].try_into().unwrap())
}

fn read_u64(b: &[u8], at: usize) -> u64 {
    u64::from_ne_bytes(b[at..at + 8].try_into().unwrap())
}

#[test]
fn registers_a_card_node_and_frees_the_unit() {
    let c = card(false, false);
    let name = std::format!("dri/card{}", c.unit());
    {
        let n = NODES.lock().unwrap();
        let (_, mode, gid) = n.iter().find(|(p, ..)| *p == name).unwrap();
        assert_eq!((*mode, *gid), (0o660, 44));
    }
    c.unregister(&s());
    assert!(NODES.lock().unwrap().iter().all(|(p, ..)| *p != name));
}

#[test]
fn the_first_open_is_master_and_masters_hand_over() {
    let c = card(false, false);
    let a = open(&c);
    let b = open(&c);
    assert_eq!(call(&b, DRM_IOCTL_SET_MASTER, &mut []), Err(Errno::EINVAL));
    assert_eq!(call(&b, DRM_IOCTL_DROP_MASTER, &mut []), Err(Errno::EINVAL));
    assert_eq!(call(&a, DRM_IOCTL_DROP_MASTER, &mut []), Ok(0));
    assert_eq!(call(&b, DRM_IOCTL_SET_MASTER, &mut []), Ok(0));
    drop(b);
    // Closing the master leaves none; the next to ask gets it.
    assert_eq!(call(&a, DRM_IOCTL_SET_MASTER, &mut []), Ok(0));
    c.unregister(&s());
}

#[test]
fn caps() {
    let c = card(true, true);
    let f = open(&c);
    let get = |cap: u64| {
        let mut a = get_cap_arg(cap, 99);
        call(&f, DRM_IOCTL_GET_CAP, &mut a).map(|_| read_u64(&a, 8))
    };
    assert_eq!(get(DRM_CAP_DUMB_BUFFER), Ok(1));
    assert_eq!(get(DRM_CAP_DUMB_PREFERRED_DEPTH), Ok(24));
    assert_eq!(get(DRM_CAP_TIMESTAMP_MONOTONIC), Ok(1));
    assert_eq!(get(DRM_CAP_CURSOR_WIDTH), Ok(64));
    assert_eq!(get(DRM_CAP_ADDFB2_MODIFIERS), Ok(1));
    assert_eq!(get(DRM_CAP_SYNCOBJ), Ok(0));
    assert_eq!(get(0x1000), Ok(7));
    assert_eq!(get(0x7777), Err(Errno::EINVAL));
    c.unregister(&s());
}

#[test]
fn client_caps_need_kms_and_atomic_turns_on_planes() {
    let nokms = card(false, false);
    let f = open(&nokms);
    let mut a = get_cap_arg(DRM_CLIENT_CAP_ATOMIC, 1);
    assert_eq!(
        call(&f, DRM_IOCTL_SET_CLIENT_CAP, &mut a),
        Err(Errno::EOPNOTSUPP)
    );
    nokms.unregister(&s());

    let c = card(false, true);
    let f = open(&c);
    let set = |cap, value| {
        let mut a = get_cap_arg(cap, value);
        call(&f, DRM_IOCTL_SET_CLIENT_CAP, &mut a)
    };
    assert_eq!(
        set(DRM_CLIENT_CAP_WRITEBACK_CONNECTORS, 1),
        Err(Errno::EINVAL)
    );
    assert_eq!(set(DRM_CLIENT_CAP_ATOMIC, 2), Err(Errno::EINVAL));
    assert_eq!(set(DRM_CLIENT_CAP_ATOMIC, 1), Ok(0));
    assert_eq!(set(DRM_CLIENT_CAP_WRITEBACK_CONNECTORS, 1), Ok(0));
    assert_eq!(set(DRM_CLIENT_CAP_ATOMIC, 0), Err(Errno::EINVAL));
    assert_eq!(set(0x99, 1), Err(Errno::EINVAL));
    c.unregister(&s());
}

#[test]
fn version_copies_names_to_the_caller() {
    let c = card(false, false);
    let f = open(&c);
    let user = USER.get().unwrap();
    let (name_at, desc_at) = (BASE + 0x100, BASE + 0x200);
    // name: room for 16; date: no buffer; desc: room for 3, so cut.
    let mut a = w(&[
        &[0u8; 12],
        &[0u8; 4],
        &16u64.to_ne_bytes(),
        &name_at.to_ne_bytes(),
        &0u64.to_ne_bytes(),
        &0u64.to_ne_bytes(),
        &3u64.to_ne_bytes(),
        &desc_at.to_ne_bytes(),
    ]);
    assert_eq!(call(&f, DRM_IOCTL_VERSION, &mut a), Ok(0));
    assert_eq!((u32_of(&a, 0), u32_of(&a, 4), u32_of(&a, 8)), (1, 2, 3));
    assert_eq!(read_u64(&a, 16), 4); // name_len
    // The caller gave no buffer for the date; its length is reported.
    assert_eq!(read_u64(&a, 32), 8);
    let mut name = [0u8; 4];
    user.copy_in(&mut name, name_at).unwrap();
    assert_eq!(&name, b"mock");
    let mut desc = [0u8; 3];
    user.copy_in(&mut desc, desc_at).unwrap();
    assert_eq!(&desc, b"A m");
    assert_eq!(read_u64(&a, 48), 6); // desc_len
    c.unregister(&s());
}

#[test]
fn dumb_buffers_have_handles_offsets_and_mappings() {
    let c = card(true, true);
    let f = open(&c);
    let mut a = create_dumb_arg(600, 799, 32);
    assert_eq!(call(&f, DRM_IOCTL_MODE_CREATE_DUMB, &mut a), Ok(0));
    let handle = u32_of(&a, 16);
    let pitch = u32_of(&a, 20);
    let size = read_u64(&a, 24);
    assert_eq!(handle, 1);
    // 799 * 4 = 3196, to a multiple of 64.
    assert_eq!(pitch, 3200);
    assert_eq!(size, (3200u64 * 600).next_multiple_of(PAGE_SIZE));

    let mut m = handle_arg(handle, 16);
    assert_eq!(call(&f, DRM_IOCTL_MODE_MAP_DUMB, &mut m), Ok(0));
    let off = read_u64(&m, 8);
    assert!(off >= OFFSET_START * PAGE_SIZE);
    let page = off / PAGE_SIZE;
    assert!(f.mmap(page, size / PAGE_SIZE).is_ok());
    assert_eq!(
        f.mmap(page, size / PAGE_SIZE + 1).err(),
        Some(Errno::EINVAL)
    );
    assert_eq!(f.mmap(page - 1, 1).err(), Some(Errno::EINVAL));
    assert_eq!(f.mmap(0, 1).err(), Some(Errno::EINVAL));

    // The kernel can fill it through the handle.
    let gem = f.gem(handle).unwrap();
    gem.with_page(1, &mut |p| p[0] = 0xab).unwrap();
    gem.with_page(1, &mut |p| assert_eq!(p[0], 0xab)).unwrap();
    assert_eq!(gem.with_page(gem.pages(), &mut |_| {}), Err(Errno::EINVAL));

    // A second buffer gets the next handle and a later offset.
    let mut a2 = create_dumb_arg(64, 64, 32);
    assert_eq!(call(&f, DRM_IOCTL_MODE_CREATE_DUMB, &mut a2), Ok(0));
    assert_eq!(u32_of(&a2, 16), 2);

    let mut d = handle_arg(handle, 4);
    assert_eq!(call(&f, DRM_IOCTL_MODE_DESTROY_DUMB, &mut d), Ok(0));
    assert_eq!(
        call(&f, DRM_IOCTL_MODE_DESTROY_DUMB, &mut d),
        Err(Errno::EINVAL)
    );
    assert_eq!(f.mmap(page, 1).err(), Some(Errno::EINVAL));
    // Handle 1 is free again; offsets are not reused.
    let mut a3 = create_dumb_arg(64, 64, 32);
    assert_eq!(call(&f, DRM_IOCTL_MODE_CREATE_DUMB, &mut a3), Ok(0));
    assert_eq!(u32_of(&a3, 16), 1);
    c.unregister(&s());
}

#[test]
fn dumb_validation() {
    let c = card(true, false);
    let f = open(&c);
    let mk = |w, h, bpp| {
        let mut a = create_dumb_arg(h, w, bpp);
        call(&f, DRM_IOCTL_MODE_CREATE_DUMB, &mut a)
    };
    assert_eq!(mk(0, 10, 32), Err(Errno::EINVAL));
    assert_eq!(mk(10, 0, 32), Err(Errno::EINVAL));
    assert_eq!(mk(10, 10, 0), Err(Errno::EINVAL));
    assert_eq!(mk(16385, 10, 32), Err(Errno::EINVAL));
    assert_eq!(mk(10, 10, 65), Err(Errno::EINVAL));
    // 16384 x 16384 x 4 bytes is 1 GiB + a bit over the limit's pitch.
    assert_eq!(mk(16384, 16384, 64), Err(Errno::EINVAL));
    assert_eq!(mk(16, 16, 8), Ok(0));
    c.unregister(&s());

    let nodumb = card(false, false);
    let g = open(&nodumb);
    let mut a = create_dumb_arg(1, 1, 32);
    assert_eq!(
        call(&g, DRM_IOCTL_MODE_CREATE_DUMB, &mut a),
        Err(Errno::ENOTTY)
    );
    nodumb.unregister(&s());
}

#[test]
fn arguments_are_resized_to_the_structure() {
    let c = card(false, false);
    let f = open(&c);
    // A caller built against an older, shorter `drm_get_cap` (8 bytes:
    // only the capability) is zero-extended; the reply is cut to what it
    // passed.
    let req8 = (DRM_IOCTL_GET_CAP & !(0x1fff << 16)) | (8 << 16);
    let mut a = (DRM_CAP_TIMESTAMP_MONOTONIC).to_ne_bytes().to_vec();
    assert_eq!(call(&f, req8, &mut a), Ok(0));
    assert_eq!(a.len(), 8);
    // A longer one is accepted, its tail left alone.
    let req24 = (DRM_IOCTL_GET_CAP & !(0x1fff << 16)) | (24 << 16);
    let mut b = get_cap_arg(DRM_CAP_TIMESTAMP_MONOTONIC, 0);
    b.extend_from_slice(&[0xee; 8]);
    assert_eq!(call(&f, req24, &mut b), Ok(0));
    assert_eq!(read_u64(&b, 8), 1);
    assert_eq!(&b[16..], &[0xee; 8]);
    c.unregister(&s());
}

#[test]
fn unknown_requests_are_logged_and_refused() {
    let c = card(false, false);
    let f = open(&c);
    LOG.lock().unwrap().clear();
    // _IOWR('d', 0xA3, ...) is CURSOR: not in this slice.
    assert_eq!(
        call(&f, DRM_IOCTL_MODE_CURSOR, &mut [0; 28]),
        Err(Errno::ENOTTY)
    );
    assert!(
        LOG.lock()
            .unwrap()
            .iter()
            .any(|l| l.contains("0xa3") && l.contains("not implemented"))
    );
    // Not a DRM request at all.
    assert_eq!(
        call(&f, abi::ioctl::FIONREAD, &mut [0; 4]),
        Err(Errno::ENOTTY)
    );
    // A driver's own range goes to the driver, which has none.
    let own = (DRM_IOCTL_GET_CAP & !0xff) | 0x40;
    assert_eq!(call(&f, own, &mut [0; 16]), Err(Errno::ENOTTY));
    c.unregister(&s());
}

#[test]
fn events_are_read_whole_and_poll() {
    use abi::event::{POLLIN, POLLRDNORM};
    let c = card(false, true);
    let f = open(&c);
    assert_eq!(f.poll(POLLIN | POLLRDNORM, None), 0);
    let mut buf = [0u8; 64];
    assert_eq!(
        f.read(&s(), &mut vfs::ReadBuf::new(&mut buf), 0, true),
        Err(Errno::EAGAIN)
    );

    let ev = |seq| {
        DrmEventVblank {
            base: DrmEvent {
                kind: DRM_EVENT_FLIP_COMPLETE,
                length: 32,
            },
            user_data: 9,
            tv_sec: 1,
            tv_usec: 2,
            sequence: seq,
            crtc_id: 3,
        }
        .to_bytes()
    };
    let file = &f;
    file.queue_event(&ev(1)).unwrap();
    file.queue_event(&ev(2)).unwrap();
    assert_eq!(f.poll(POLLIN | POLLRDNORM, None), POLLIN | POLLRDNORM);

    // 40 bytes of room: one whole event, never half of one.
    let mut b40 = [0u8; 40];
    let mut r = vfs::ReadBuf::new(&mut b40);
    assert_eq!(f.read(&s(), &mut r, 0, true), Ok(32));
    assert_eq!(&r.filled()[24..28], &1u32.to_ne_bytes());
    let mut b64 = [0u8; 64];
    let mut r = vfs::ReadBuf::new(&mut b64);
    assert_eq!(f.read(&s(), &mut r, 0, true), Ok(32));
    assert_eq!(&r.filled()[24..28], &2u32.to_ne_bytes());
    assert_eq!(f.poll(POLLIN | POLLRDNORM, None), 0);

    // The per-file event space is bounded.
    let mut n = 0;
    while file.queue_event(&ev(n)).is_ok() {
        n += 1;
    }
    assert_eq!(n, 4096 / 32);
    c.unregister(&s());
}

#[test]
fn unregistering_kills_open_files() {
    use abi::event::POLLHUP;
    let c = card(true, false);
    let f = open(&c);
    c.unregister(&s());
    assert_eq!(f.poll(abi::event::POLLIN, None), POLLHUP);
    let mut a = get_cap_arg(1, 0);
    assert_eq!(call(&f, DRM_IOCTL_GET_CAP, &mut a), Err(Errno::ENODEV));
    let mut buf = [0u8; 8];
    assert_eq!(
        f.read(&s(), &mut vfs::ReadBuf::new(&mut buf), 0, true),
        Err(Errno::ENODEV)
    );
}

// ---- the display ----

use crate::KmsBuilder;
use crate::kms::{
    Connection, ConnectorHw, CrtcHw, EncoderHw, KmsLimits, PlaneHw, PlaneKind, PropKind, PropSpec,
};
use abi::drm::Wire;
use drm_fourcc::{Fourcc, format};
use kms_check::Mode;

/// What the hardware was told, in order.
type Calls = Arc<Mutex<Vec<String>>>;

struct MockCrtc(Calls);
impl CrtcHw for MockCrtc {
    fn commit(&self, u: &crate::kms::CrtcUpdate<'_>) {
        self.0
            .lock()
            .unwrap()
            .push(std::format!("crtc{} active={}", u.index, u.new.active));
    }
}

const PLANE_FORMATS: &[Fourcc] = &[format::XRGB8888, format::ARGB8888];

struct MockPlane(Calls);
impl PlaneHw for MockPlane {
    fn commit(&self, u: &crate::kms::PlaneUpdate<'_>) {
        let fb = u.fb.map_or(0, |f| f.id.0);
        self.0.lock().unwrap().push(std::format!(
            "plane{} fb={fb} shown={}",
            u.index,
            u.visible.shown
        ));
    }
    fn formats(&self) -> &[Fourcc] {
        PLANE_FORMATS
    }
    fn properties(&self) -> &[PropSpec] {
        &[PropSpec {
            name: "zpos",
            kind: PropKind::Range { min: 0, max: 3 },
            default: 1,
            immutable: false,
        }]
    }
}

struct MockEncoder(Calls);
impl EncoderHw for MockEncoder {
    fn kind(&self) -> u32 {
        DRM_MODE_ENCODER_TMDS
    }
    fn enable(&self, crtc: usize, m: &Mode) {
        self.0.lock().unwrap().push(std::format!(
            "enable crtc{crtc} {}x{}",
            m.hdisplay,
            m.vdisplay
        ));
    }
    fn disable(&self) {
        self.0.lock().unwrap().push("disable".into());
    }
}

struct MockConnector;
impl ConnectorHw for MockConnector {
    fn kind(&self) -> u32 {
        DRM_MODE_CONNECTOR_EDP
    }
    fn detect(&self) -> Connection {
        Connection::Connected
    }
    fn modes(&self) -> Vec<Mode> {
        std::vec![Mode {
            clock_khz: 148_500,
            hdisplay: 1920,
            hsync_start: 2008,
            hsync_end: 2052,
            htotal: 2200,
            vdisplay: 1080,
            vsync_start: 1084,
            vsync_end: 1089,
            vtotal: 1125,
            kind: DRM_MODE_TYPE_PREFERRED | DRM_MODE_TYPE_DRIVER,
            ..Mode::default()
        }]
    }
    fn size_mm(&self) -> (u32, u32) {
        (344, 194)
    }
}

/// One display controller, a primary and an overlay plane, an encoder
/// and an embedded DisplayPort connector.
fn mock_display(calls: &Calls) -> crate::kms::Kms {
    let mut b = KmsBuilder::new(KmsLimits {
        min_width: 16,
        max_width: 4096,
        min_height: 16,
        max_height: 4096,
    });
    let c = b.crtc(Arc::new(MockCrtc(calls.clone())));
    b.plane(
        Arc::new(MockPlane(calls.clone())),
        PlaneKind::Primary,
        1 << c,
    );
    b.plane(
        Arc::new(MockPlane(calls.clone())),
        PlaneKind::Overlay,
        1 << c,
    );
    let e = b.encoder(Arc::new(MockEncoder(calls.clone())), 1 << c, 0);
    b.connector(Arc::new(MockConnector), &[e]);
    b.build().unwrap()
}

fn enc<T: Wire>(v: &T) -> Vec<u8> {
    let mut b = std::vec![0u8; T::SIZE];
    v.encode(&mut b);
    b
}

/// Calls a request with a typed argument; returns the argument as the
/// call left it.
fn ctl<T: Wire>(f: &Arc<crate::Open>, req: u32, v: &T) -> Result<T, Errno> {
    let mut b = enc(v);
    call(f, req, &mut b)?;
    Ok(T::decode(&b))
}

fn put(at: u64, bytes: &[u8]) {
    USER.get().unwrap().copy_out(BASE + at, bytes).unwrap();
}

fn get(at: u64, len: usize) -> Vec<u8> {
    let mut v = std::vec![0u8; len];
    USER.get().unwrap().copy_in(&mut v, BASE + at).unwrap();
    v
}

fn ids_at(at: u64, n: usize) -> Vec<u32> {
    get(at, n * 4)
        .chunks(4)
        .map(|c| u32::from_ne_bytes(c.try_into().unwrap()))
        .collect()
}

#[test]
fn resources_are_sized_then_fetched() {
    let c = card(true, true);
    let f = open(&c);
    // First call: no room, true counts.
    let r = ctl(&f, DRM_IOCTL_MODE_GETRESOURCES, &DrmModeCardRes::default()).unwrap();
    assert_eq!(
        (
            r.count_crtcs,
            r.count_connectors,
            r.count_encoders,
            r.count_fbs
        ),
        (1, 1, 1, 0)
    );
    assert_eq!(
        (r.min_width, r.max_width, r.min_height, r.max_height),
        (16, 4096, 16, 4096)
    );
    // Second: room at 0x4000.
    let r2 = ctl(
        &f,
        DRM_IOCTL_MODE_GETRESOURCES,
        &DrmModeCardRes {
            crtc_id_ptr: BASE + 0x4000,
            connector_id_ptr: BASE + 0x4100,
            encoder_id_ptr: BASE + 0x4200,
            count_crtcs: 1,
            count_connectors: 1,
            count_encoders: 1,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(r2.count_crtcs, 1);
    let crtc = ids_at(0x4000, 1)[0];
    let conn = ids_at(0x4100, 1)[0];
    let enc_id = ids_at(0x4200, 1)[0];
    // Distinct, nonzero object numbers.
    assert!(crtc != 0 && conn != 0 && enc_id != 0);
    assert!(crtc != conn && conn != enc_id && crtc != enc_id);
    // A list too small for the answer is left alone.
    put(0x4300, &[0xee; 8]);
    let small = DrmModeCardRes {
        crtc_id_ptr: BASE + 0x4300,
        count_crtcs: 0,
        ..Default::default()
    };
    let small = ctl(&f, DRM_IOCTL_MODE_GETRESOURCES, &small).unwrap();
    assert_eq!(small.count_crtcs, 1);
    assert_eq!(get(0x4300, 8), [0xee; 8]);
    c.unregister(&s());
}

#[test]
fn the_connector_reports_its_mode() {
    let c = card(true, true);
    let f = open(&c);
    put(0x5000, &[0; 8]);
    let res = ctl(
        &f,
        DRM_IOCTL_MODE_GETRESOURCES,
        &DrmModeCardRes {
            connector_id_ptr: BASE + 0x5000,
            count_connectors: 1,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(res.count_connectors, 1);
    let conn = ids_at(0x5000, 1)[0];

    let ask = |connector_id, modes_ptr, count_modes| DrmModeGetConnector {
        connector_id,
        modes_ptr,
        count_modes,
        ..Default::default()
    };
    let probe = ctl(&f, DRM_IOCTL_MODE_GETCONNECTOR, &ask(conn, 0, 0)).unwrap();
    assert_eq!(probe.count_modes, 1);
    assert_eq!(probe.count_encoders, 1);
    assert_eq!(probe.connection, DRM_MODE_CONNECTED);
    assert_eq!(probe.connector_type, DRM_MODE_CONNECTOR_EDP);
    assert_eq!(probe.connector_type_id, 1);
    assert_eq!((probe.mm_width, probe.mm_height), (344, 194));
    // Standard properties: DPMS, EDID, CRTC_ID.
    assert_eq!(probe.count_props, 3);

    let full = ctl(
        &f,
        DRM_IOCTL_MODE_GETCONNECTOR,
        &ask(conn, BASE + 0x6000, 1),
    )
    .unwrap();
    assert_eq!(full.count_modes, 1);
    let m = DrmModeModeinfo::decode(&get(0x6000, 68));
    assert_eq!((m.hdisplay, m.vdisplay, m.vrefresh), (1920, 1080, 60));
    assert_eq!(&m.name[..9], b"1920x1080");
    assert_eq!(m.kind & DRM_MODE_TYPE_PREFERRED, DRM_MODE_TYPE_PREFERRED);

    assert_eq!(
        ctl(&f, DRM_IOCTL_MODE_GETCONNECTOR, &ask(9999, 0, 0)).err(),
        Some(Errno::ENOENT)
    );
    c.unregister(&s());
}

#[test]
fn planes_are_hidden_until_universal_planes_is_asked_for() {
    let c = card(true, true);
    let f = open(&c);
    let n = ctl(
        &f,
        DRM_IOCTL_MODE_GETPLANERESOURCES,
        &DrmModeGetPlaneRes::default(),
    )
    .unwrap();
    // Only the overlay.
    assert_eq!(n.count_planes, 1);
    let set = DrmSetClientCap {
        capability: DRM_CLIENT_CAP_UNIVERSAL_PLANES,
        value: 1,
    };
    ctl(&f, DRM_IOCTL_SET_CLIENT_CAP, &set).unwrap();
    let n = ctl(
        &f,
        DRM_IOCTL_MODE_GETPLANERESOURCES,
        &DrmModeGetPlaneRes::default(),
    )
    .unwrap();
    assert_eq!(n.count_planes, 2);
    ctl(
        &f,
        DRM_IOCTL_MODE_GETPLANERESOURCES,
        &DrmModeGetPlaneRes {
            plane_id_ptr: BASE + 0x7000,
            count_planes: 2,
        },
    )
    .unwrap();
    let planes = ids_at(0x7000, 2);

    let q = |plane_id, ptr, count| DrmModeGetPlane {
        plane_id,
        format_type_ptr: ptr,
        count_format_types: count,
        ..Default::default()
    };
    let p = ctl(&f, DRM_IOCTL_MODE_GETPLANE, &q(planes[0], 0, 0)).unwrap();
    assert_eq!(p.possible_crtcs, 1);
    assert_eq!(p.count_format_types, 2);
    assert_eq!((p.crtc_id, p.fb_id), (0, 0));
    ctl(&f, DRM_IOCTL_MODE_GETPLANE, &q(planes[0], BASE + 0x7100, 2)).unwrap();
    assert_eq!(ids_at(0x7100, 2), [format::XRGB8888.0, format::ARGB8888.0]);
    c.unregister(&s());
}

/// The (id, value, name) of each of an object's properties.
fn props_of(f: &Arc<crate::Open>, obj: u32, kind: u32) -> Vec<(u32, u64, String)> {
    let n = ctl(
        f,
        DRM_IOCTL_MODE_OBJ_GETPROPERTIES,
        &DrmModeObjGetProperties {
            obj_id: obj,
            obj_type: kind,
            ..Default::default()
        },
    )
    .unwrap()
    .count_props as usize;
    ctl(
        f,
        DRM_IOCTL_MODE_OBJ_GETPROPERTIES,
        &DrmModeObjGetProperties {
            obj_id: obj,
            obj_type: kind,
            props_ptr: BASE + 0x8000,
            prop_values_ptr: BASE + 0x8400,
            count_props: u32::try_from(n).unwrap(),
        },
    )
    .unwrap();
    let ids = ids_at(0x8000, n);
    let vals: Vec<u64> = get(0x8400, n * 8)
        .chunks(8)
        .map(|c| u64::from_ne_bytes(c.try_into().unwrap()))
        .collect();
    ids.iter()
        .zip(vals)
        .map(|(id, v)| {
            let p = ctl(
                f,
                DRM_IOCTL_MODE_GETPROPERTY,
                &DrmModeGetProperty {
                    prop_id: *id,
                    ..Default::default()
                },
            )
            .unwrap();
            let name = p.name.iter().take_while(|b| **b != 0).copied().collect();
            (*id, v, String::from_utf8(name).unwrap())
        })
        .collect()
}

#[test]
fn plane_properties_and_in_formats() {
    let c = card(true, true);
    let f = open(&c);
    let set = DrmSetClientCap {
        capability: DRM_CLIENT_CAP_UNIVERSAL_PLANES,
        value: 1,
    };
    ctl(&f, DRM_IOCTL_SET_CLIENT_CAP, &set).unwrap();
    let list = DrmModeGetPlaneRes {
        plane_id_ptr: BASE + 0x7000,
        count_planes: 2,
    };
    ctl(&f, DRM_IOCTL_MODE_GETPLANERESOURCES, &list).unwrap();
    let planes = ids_at(0x7000, 2);
    // Planes are listed primary first, as built.
    let props = props_of(&f, planes[0], DRM_MODE_OBJECT_PLANE);
    let names: Vec<&str> = props.iter().map(|(_, _, n)| n.as_str()).collect();
    assert_eq!(
        names,
        [
            "type",
            "FB_ID",
            "CRTC_ID",
            "SRC_X",
            "SRC_Y",
            "SRC_W",
            "SRC_H",
            "CRTC_X",
            "CRTC_Y",
            "CRTC_W",
            "CRTC_H",
            "IN_FORMATS",
            "zpos"
        ]
    );
    let value = |n: &str| props.iter().find(|(_, _, x)| x == n).unwrap().1;
    assert_eq!(value("type"), 1, "the first plane is primary");
    assert_eq!(value("zpos"), 1, "a driver property starts at its default");
    let overlay = props_of(&f, planes[1], DRM_MODE_OBJECT_PLANE);
    assert_eq!(overlay.iter().find(|(_, _, n)| n == "type").unwrap().1, 0);

    // `type` is an immutable enum with the three names.
    let prop = |prop_id| {
        let q = DrmModeGetProperty {
            prop_id,
            ..Default::default()
        };
        ctl(&f, DRM_IOCTL_MODE_GETPROPERTY, &q).unwrap()
    };
    let enums = prop(props[0].0);
    assert_eq!(enums.count_enum_blobs, 3);
    assert_eq!(enums.flags & DRM_MODE_PROP_ENUM, DRM_MODE_PROP_ENUM);
    assert_eq!(
        enums.flags & DRM_MODE_PROP_IMMUTABLE,
        DRM_MODE_PROP_IMMUTABLE
    );
    // Atomic-only properties say so.
    let fbp = prop(props[1].0);
    assert_eq!(fbp.flags & DRM_MODE_PROP_ATOMIC, DRM_MODE_PROP_ATOMIC);
    assert_eq!(fbp.flags & DRM_MODE_PROP_OBJECT, DRM_MODE_PROP_OBJECT);

    // IN_FORMATS is a blob holding the formats and a linear modifier.
    let blob_id = u32::try_from(value("IN_FORMATS")).unwrap();
    let size = ctl(
        &f,
        DRM_IOCTL_MODE_GETPROPBLOB,
        &DrmModeGetBlob {
            blob_id,
            ..Default::default()
        },
    )
    .unwrap();
    let blob = ctl(
        &f,
        DRM_IOCTL_MODE_GETPROPBLOB,
        &DrmModeGetBlob {
            blob_id,
            length: size.length,
            data: BASE + 0x9000,
        },
    )
    .unwrap();
    let raw = get(0x9000, blob.length as usize);
    let h = DrmFormatModifierBlob::decode(&raw);
    assert_eq!((h.version, h.count_formats, h.count_modifiers), (1, 2, 1));
    let m = DrmFormatModifier::decode(&raw[h.modifiers_offset as usize..]);
    assert_eq!((m.formats, m.modifier), (0b11, 0));
    c.unregister(&s());
}

#[test]
fn framebuffers() {
    let c = card(true, true);
    let f = open(&c);
    let mut d = create_dumb_arg(64, 128, 32);
    call(&f, DRM_IOCTL_MODE_CREATE_DUMB, &mut d).unwrap();
    let handle = u32_of(&d, 16);
    let pitch = u32_of(&d, 20);
    assert_eq!(pitch, 512);

    let fb = |w: u32, h: u32, pitch: u32, handle: u32, fmt: Fourcc| {
        let mut a = DrmModeFbCmd2 {
            width: w,
            height: h,
            pixel_format: fmt.0,
            ..Default::default()
        };
        a.handles[0] = handle;
        a.pitches[0] = pitch;
        ctl(&f, DRM_IOCTL_MODE_ADDFB2, &a)
    };
    let x = format::XRGB8888;
    let good = fb(128, 64, 512, handle, x).unwrap();
    assert_ne!(good.fb_id, 0);
    assert_eq!(
        fb(128, 64, 400, handle, x).err(),
        Some(Errno::EINVAL),
        "pitch too small"
    );
    assert_eq!(
        fb(128, 64, 512, handle, Fourcc(0x1111_1111)).err(),
        Some(Errno::EINVAL),
        "unknown format"
    );
    assert_eq!(
        fb(128, 64, 512, 77, x).err(),
        Some(Errno::ENOENT),
        "no such buffer"
    );
    assert_eq!(
        fb(128, 200, 512, handle, x).err(),
        Some(Errno::EINVAL),
        "taller than the buffer"
    );
    assert_eq!(
        fb(8, 8, 512, handle, x).err(),
        Some(Errno::EINVAL),
        "below the device minimum"
    );

    // The file lists its framebuffers.
    let r = ctl(&f, DRM_IOCTL_MODE_GETRESOURCES, &DrmModeCardRes::default()).unwrap();
    assert_eq!(r.count_fbs, 1);
    // Another file cannot remove it; its owner can.
    let other = open(&c);
    let mut id = good.fb_id.to_ne_bytes().to_vec();
    assert_eq!(
        call(&other, DRM_IOCTL_MODE_RMFB, &mut id),
        Err(Errno::EINVAL)
    );
    assert_eq!(call(&f, DRM_IOCTL_MODE_RMFB, &mut id), Ok(0));
    assert_eq!(call(&f, DRM_IOCTL_MODE_RMFB, &mut id), Err(Errno::ENOENT));

    // Closing the file takes its framebuffers along.
    let g = fb(128, 64, 512, handle, x).unwrap();
    let k = c.driver().kms().unwrap();
    assert!(k.fb(kms_check::ObjId(g.fb_id)).is_some());
    drop(f);
    assert!(k.fb(kms_check::ObjId(g.fb_id)).is_none());
    c.unregister(&s());
}

#[test]
fn blobs_belong_to_their_file() {
    let c = card(false, true);
    let f = open(&c);
    put(0xa000, b"hello");
    let make = DrmModeCreateBlob {
        data: BASE + 0xa000,
        length: 5,
        blob_id: 0,
    };
    let b = ctl(&f, DRM_IOCTL_MODE_CREATEPROPBLOB, &make).unwrap();
    assert_ne!(b.blob_id, 0);
    let read = DrmModeGetBlob {
        blob_id: b.blob_id,
        length: 5,
        data: BASE + 0xa100,
    };
    let got = ctl(&f, DRM_IOCTL_MODE_GETPROPBLOB, &read).unwrap();
    assert_eq!(got.length, 5);
    assert_eq!(get(0xa100, 5), b"hello");
    let other = open(&c);
    let kill = DrmModeDestroyBlob { blob_id: b.blob_id };
    assert_eq!(
        ctl(&other, DRM_IOCTL_MODE_DESTROYPROPBLOB, &kill).err(),
        Some(Errno::EINVAL)
    );
    ctl(&f, DRM_IOCTL_MODE_DESTROYPROPBLOB, &kill).unwrap();
    assert_eq!(
        ctl(&f, DRM_IOCTL_MODE_GETPROPBLOB, &read).err(),
        Some(Errno::ENOENT)
    );
    let empty = DrmModeCreateBlob {
        data: BASE,
        length: 0,
        blob_id: 0,
    };
    assert_eq!(
        ctl(&f, DRM_IOCTL_MODE_CREATEPROPBLOB, &empty).err(),
        Some(Errno::EINVAL)
    );
    c.unregister(&s());
}

#[test]
fn a_card_without_a_display_refuses_display_requests() {
    let c = card(true, false);
    let f = open(&c);
    assert_eq!(
        ctl(&f, DRM_IOCTL_MODE_GETRESOURCES, &DrmModeCardRes::default()).err(),
        Some(Errno::ENOTTY)
    );
    c.unregister(&s());
}

// ---- commits ----

struct Ids {
    crtc: u32,
    conn: u32,
    primary: u32,
    overlay: u32,
}

/// Turns on universal planes and returns the display's objects.
fn ids(f: &Arc<crate::Open>) -> Ids {
    let set = DrmSetClientCap {
        capability: DRM_CLIENT_CAP_UNIVERSAL_PLANES,
        value: 1,
    };
    ctl(f, DRM_IOCTL_SET_CLIENT_CAP, &set).unwrap();
    let r = DrmModeCardRes {
        crtc_id_ptr: BASE + 0xc000,
        connector_id_ptr: BASE + 0xc100,
        count_crtcs: 1,
        count_connectors: 1,
        ..Default::default()
    };
    ctl(f, DRM_IOCTL_MODE_GETRESOURCES, &r).unwrap();
    let list = DrmModeGetPlaneRes {
        plane_id_ptr: BASE + 0xc200,
        count_planes: 2,
    };
    ctl(f, DRM_IOCTL_MODE_GETPLANERESOURCES, &list).unwrap();
    let planes = ids_at(0xc200, 2);
    Ids {
        crtc: ids_at(0xc000, 1)[0],
        conn: ids_at(0xc100, 1)[0],
        primary: planes[0],
        overlay: planes[1],
    }
}

/// A buffer and a framebuffer of `w` by `h`, as a dumb buffer in
/// `XRGB8888`.
fn make_fb(f: &Arc<crate::Open>, w: u32, h: u32) -> u32 {
    let mut d = create_dumb_arg(h, w, 32);
    call(f, DRM_IOCTL_MODE_CREATE_DUMB, &mut d).unwrap();
    let mut a = DrmModeFbCmd2 {
        width: w,
        height: h,
        pixel_format: format::XRGB8888.0,
        ..Default::default()
    };
    a.handles[0] = u32_of(&d, 16);
    a.pitches[0] = u32_of(&d, 20);
    ctl(f, DRM_IOCTL_MODE_ADDFB2, &a).unwrap().fb_id
}

fn mode1080() -> DrmModeModeinfo {
    MockConnector.modes()[0].to_info()
}

/// The legacy way to light the display: one framebuffer on the
/// controller, the connector named.
fn set_crtc(f: &Arc<crate::Open>, i: &Ids, fb: u32) -> Result<DrmModeCrtc, Errno> {
    put(0xd000, &i.conn.to_ne_bytes());
    let a = DrmModeCrtc {
        set_connectors_ptr: BASE + 0xd000,
        count_connectors: 1,
        crtc_id: i.crtc,
        fb_id: fb,
        mode_valid: 1,
        mode: mode1080(),
        ..Default::default()
    };
    ctl(f, DRM_IOCTL_MODE_SETCRTC, &a)
}

fn take(calls: &Calls) -> Vec<String> {
    core::mem::take(&mut *calls.lock().unwrap())
}

fn read_event(f: &Arc<crate::Open>) -> Option<DrmEventVblank> {
    let mut b = [0u8; 64];
    let mut r = vfs::ReadBuf::new(&mut b);
    match f.read(&s(), &mut r, 0, true) {
        Ok(32) => Some(DrmEventVblank {
            base: DrmEvent {
                kind: u32_of(r.filled(), 0),
                length: u32_of(r.filled(), 4),
            },
            user_data: read_u64(r.filled(), 8),
            tv_sec: u32_of(r.filled(), 16),
            tv_usec: u32_of(r.filled(), 20),
            sequence: u32_of(r.filled(), 24),
            crtc_id: u32_of(r.filled(), 28),
        }),
        _ => None,
    }
}

#[test]
fn set_crtc_programs_the_hardware_in_order_and_off_again() {
    let (c, calls) = card_logged();
    let f = open(&c);
    let i = ids(&f);
    let fb = make_fb(&f, 1920, 1080);

    let got = set_crtc(&f, &i, fb).unwrap();
    assert_eq!(got.crtc_id, i.crtc);
    // Planes, then the controller, then the encoder.
    assert_eq!(
        take(&calls),
        [
            std::format!("plane0 fb={fb} shown=true"),
            "crtc0 active=true".into(),
            "enable crtc0 1920x1080".into()
        ]
    );
    let now = ctl(
        &f,
        DRM_IOCTL_MODE_GETCRTC,
        &DrmModeCrtc {
            crtc_id: i.crtc,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!((now.mode_valid, now.fb_id), (1, fb));
    assert_eq!(now.mode.hdisplay, 1920);
    let plane = ctl(
        &f,
        DRM_IOCTL_MODE_GETPLANE,
        &DrmModeGetPlane {
            plane_id: i.primary,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!((plane.crtc_id, plane.fb_id), (i.crtc, fb));
    let conn = ctl(
        &f,
        DRM_IOCTL_MODE_GETCONNECTOR,
        &DrmModeGetConnector {
            connector_id: i.conn,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(conn.encoder_id != 0, "a connector in use names its encoder");

    // Off: the encoder stops first.
    set_crtc(&f, &i, 0).unwrap();
    assert_eq!(
        take(&calls),
        ["disable", "plane0 fb=0 shown=false", "crtc0 active=false"]
    );
    let now = ctl(
        &f,
        DRM_IOCTL_MODE_GETCRTC,
        &DrmModeCrtc {
            crtc_id: i.crtc,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(now.mode_valid, 0);
    c.unregister(&s());
}

#[test]
fn a_refused_commit_touches_nothing() {
    let (c, calls) = card_logged();
    let f = open(&c);
    let i = ids(&f);
    // A buffer smaller than the mode cannot fill it.
    let small = make_fb(&f, 1280, 720);
    assert_eq!(set_crtc(&f, &i, small).err(), Some(Errno::EINVAL));
    // Nor can a mode that is not a mode.
    let fb = make_fb(&f, 1920, 1080);
    let mut bad = DrmModeCrtc {
        crtc_id: i.crtc,
        fb_id: fb,
        mode_valid: 1,
        mode: mode1080(),
        ..Default::default()
    };
    bad.mode.hsync_start = 100;
    assert_eq!(
        ctl(&f, DRM_IOCTL_MODE_SETCRTC, &bad).err(),
        Some(Errno::EINVAL)
    );
    // An active controller with no connector.
    bad.mode = mode1080();
    assert_eq!(
        ctl(&f, DRM_IOCTL_MODE_SETCRTC, &bad).err(),
        Some(Errno::EINVAL)
    );
    assert!(take(&calls).is_empty());
    // Unknown objects.
    assert_eq!(
        set_crtc(
            &f,
            &Ids {
                crtc: 9999,
                ..ids_copy(&i)
            },
            fb
        )
        .err(),
        Some(Errno::ENOENT)
    );
    // Only the master changes the display.
    let other = open(&c);
    assert_eq!(set_crtc(&other, &i, fb).err(), Some(Errno::EACCES));
    c.unregister(&s());
}

fn ids_copy(i: &Ids) -> Ids {
    Ids {
        crtc: i.crtc,
        conn: i.conn,
        primary: i.primary,
        overlay: i.overlay,
    }
}

/// The property `name` of `obj`.
fn pid(f: &Arc<crate::Open>, obj: u32, kind: u32, name: &str) -> u32 {
    props_of(f, obj, kind)
        .iter()
        .find(|(_, _, n)| n == name)
        .unwrap()
        .0
}

/// Calls the atomic ioctl with these `(object, [(property, value)])`.
fn atomic(
    f: &Arc<crate::Open>,
    flags: u32,
    user_data: u64,
    changes: &[(u32, Vec<(u32, u64)>)],
) -> Result<DrmModeAtomic, Errno> {
    let objs: Vec<u8> = changes.iter().flat_map(|(o, _)| o.to_ne_bytes()).collect();
    let counts: Vec<u8> = changes
        .iter()
        .flat_map(|(_, p)| u32::try_from(p.len()).unwrap().to_ne_bytes())
        .collect();
    let props: Vec<u8> = changes
        .iter()
        .flat_map(|(_, p)| p.iter().flat_map(|(i, _)| i.to_ne_bytes()))
        .collect();
    let values: Vec<u8> = changes
        .iter()
        .flat_map(|(_, p)| p.iter().flat_map(|(_, v)| v.to_ne_bytes()))
        .collect();
    put(0xe000, &objs);
    put(0xe100, &counts);
    put(0xe200, &props);
    put(0xe400, &values);
    let a = DrmModeAtomic {
        flags,
        count_objs: u32::try_from(changes.len()).unwrap(),
        objs_ptr: BASE + 0xe000,
        count_props_ptr: BASE + 0xe100,
        props_ptr: BASE + 0xe200,
        prop_values_ptr: BASE + 0xe400,
        reserved: 0,
        user_data,
    };
    ctl(f, DRM_IOCTL_MODE_ATOMIC, &a)
}

/// The atomic changes that light the display with `fb`.
fn light(f: &Arc<crate::Open>, i: &Ids, fb: u32) -> Vec<(u32, Vec<(u32, u64)>)> {
    // The mode as a blob.
    put(0xf000, &enc(&mode1080()));
    let blob = ctl(
        f,
        DRM_IOCTL_MODE_CREATEPROPBLOB,
        &DrmModeCreateBlob {
            data: BASE + 0xf000,
            length: 68,
            blob_id: 0,
        },
    )
    .unwrap()
    .blob_id;
    let (c, p, k) = (
        DRM_MODE_OBJECT_CRTC,
        DRM_MODE_OBJECT_PLANE,
        DRM_MODE_OBJECT_CONNECTOR,
    );
    std::vec![
        (
            i.crtc,
            std::vec![
                (pid(f, i.crtc, c, "ACTIVE"), 1),
                (pid(f, i.crtc, c, "MODE_ID"), u64::from(blob))
            ]
        ),
        (
            i.conn,
            std::vec![(pid(f, i.conn, k, "CRTC_ID"), u64::from(i.crtc))]
        ),
        (
            i.primary,
            std::vec![
                (pid(f, i.primary, p, "FB_ID"), u64::from(fb)),
                (pid(f, i.primary, p, "CRTC_ID"), u64::from(i.crtc)),
                (pid(f, i.primary, p, "SRC_X"), 0),
                (pid(f, i.primary, p, "SRC_Y"), 0),
                (pid(f, i.primary, p, "SRC_W"), 1920 << 16),
                (pid(f, i.primary, p, "SRC_H"), 1080 << 16),
                (pid(f, i.primary, p, "CRTC_X"), 0),
                (pid(f, i.primary, p, "CRTC_Y"), 0),
                (pid(f, i.primary, p, "CRTC_W"), 1920),
                (pid(f, i.primary, p, "CRTC_H"), 1080),
            ]
        ),
    ]
}

#[test]
fn atomic_commits_complete_at_the_frame_with_their_event() {
    let (c, calls) = card_logged();
    let f = open(&c);
    let i = ids(&f);
    let fb = make_fb(&f, 1920, 1080);
    let changes = light(&f, &i, fb);
    let modeset = DRM_MODE_ATOMIC_ALLOW_MODESET;

    // Atomic is a client capability.
    assert_eq!(atomic(&f, modeset, 0, &changes).err(), Some(Errno::EINVAL));
    let on = DrmSetClientCap {
        capability: DRM_CLIENT_CAP_ATOMIC,
        value: 1,
    };
    ctl(&f, DRM_IOCTL_SET_CLIENT_CAP, &on).unwrap();

    // Turning the display on is a modeset, and must say so.
    assert_eq!(
        atomic(&f, DRM_MODE_ATOMIC_NONBLOCK, 0, &changes).err(),
        Some(Errno::EINVAL)
    );
    // A test commits nothing.
    atomic(&f, modeset | DRM_MODE_ATOMIC_TEST_ONLY, 0, &changes).unwrap();
    assert!(take(&calls).is_empty());
    assert_eq!(
        atomic(
            &f,
            modeset | DRM_MODE_ATOMIC_TEST_ONLY | DRM_MODE_PAGE_FLIP_EVENT,
            0,
            &changes
        )
        .err(),
        Some(Errno::EINVAL)
    );
    assert_eq!(
        atomic(&f, DRM_MODE_PAGE_FLIP_ASYNC, 0, &[]).err(),
        Some(Errno::EINVAL)
    );

    // The real one returns at once, and its event waits for the frame.
    atomic(
        &f,
        modeset | DRM_MODE_ATOMIC_NONBLOCK | DRM_MODE_PAGE_FLIP_EVENT,
        0xfeed,
        &changes,
    )
    .unwrap();
    assert_eq!(take(&calls).len(), 3);
    assert_eq!(f.poll(abi::event::POLLIN, None), 0);
    assert_eq!(c.driver().kms().unwrap().frames(0), 0);
    fire(c.unit());
    assert_eq!(c.driver().kms().unwrap().frames(0), 1);
    let ev = read_event(&f).expect("a flip event");
    assert_eq!(
        ev.base,
        DrmEvent {
            kind: DRM_EVENT_FLIP_COMPLETE,
            length: 32
        }
    );
    assert_eq!((ev.user_data, ev.crtc_id, ev.sequence), (0xfeed, i.crtc, 1));
    assert_eq!((ev.tv_sec, ev.tv_usec), (5, 0));
    assert!(read_event(&f).is_none());

    // Nothing waits now, so no further frames are timed.
    fire(c.unit());
    assert_eq!(c.driver().kms().unwrap().frames(0), 1);
    c.unregister(&s());
}

#[test]
fn page_flips_wait_their_turn_and_keep_their_size() {
    let (c, calls) = card_logged();
    let f = open(&c);
    let i = ids(&f);
    let fb = make_fb(&f, 1920, 1080);
    let next = make_fb(&f, 1920, 1080);
    let odd = make_fb(&f, 1280, 720);
    set_crtc(&f, &i, fb).unwrap();
    take(&calls);

    let flip = |fb_id, flags, user_data| {
        let a = DrmModeCrtcPageFlip {
            crtc_id: i.crtc,
            fb_id,
            flags,
            reserved: 0,
            user_data,
        };
        ctl(&f, DRM_IOCTL_MODE_PAGE_FLIP, &a)
    };
    assert_eq!(
        flip(odd, 0, 0).err(),
        Some(Errno::EINVAL),
        "a flip keeps the size"
    );
    assert_eq!(
        flip(next, DRM_MODE_PAGE_FLIP_ASYNC, 0).err(),
        Some(Errno::EINVAL)
    );
    assert_eq!(flip(7777, 0, 0).err(), Some(Errno::ENOENT));
    flip(next, DRM_MODE_PAGE_FLIP_EVENT, 7).unwrap();
    assert_eq!(
        take(&calls),
        [
            std::format!("plane0 fb={next} shown=true"),
            "crtc0 active=true".into()
        ]
    );
    // The first has not had its frame: a second must wait, and says so.
    assert_eq!(
        flip(fb, DRM_MODE_PAGE_FLIP_EVENT, 8).err(),
        Some(Errno::EBUSY)
    );
    fire(c.unit());
    let ev = read_event(&f).expect("the first flip's event");
    assert_eq!(ev.user_data, 7);
    // Now it can, and the event of a flip to what is already shown still
    // comes.
    flip(fb, DRM_MODE_PAGE_FLIP_EVENT, 9).unwrap();
    flip_again_is_busy(&f, &i, fb);
    fire(c.unit());
    assert_eq!(read_event(&f).unwrap().user_data, 9);
    // A flip with no event reserves none.
    flip(next, 0, 0).unwrap();
    fire(c.unit());
    assert!(read_event(&f).is_none());
    c.unregister(&s());
}

fn flip_again_is_busy(f: &Arc<crate::Open>, i: &Ids, fb: u32) {
    let a = DrmModeCrtcPageFlip {
        crtc_id: i.crtc,
        fb_id: fb,
        flags: 0,
        reserved: 0,
        user_data: 0,
    };
    assert_eq!(
        ctl(f, DRM_IOCTL_MODE_PAGE_FLIP, &a).err(),
        Some(Errno::EBUSY)
    );
}

#[test]
fn a_flip_whose_file_closed_is_dropped_quietly() {
    let (c, _calls) = card_logged();
    let f = open(&c);
    let i = ids(&f);
    let fb = make_fb(&f, 1920, 1080);
    let next = make_fb(&f, 1920, 1080);
    set_crtc(&f, &i, fb).unwrap();
    let a = DrmModeCrtcPageFlip {
        crtc_id: i.crtc,
        fb_id: next,
        flags: DRM_MODE_PAGE_FLIP_EVENT,
        reserved: 0,
        user_data: 1,
    };
    ctl(&f, DRM_IOCTL_MODE_PAGE_FLIP, &a).unwrap();
    drop(f);
    fire(c.unit());
    c.unregister(&s());
}

#[test]
fn planes_can_be_set_and_clipped() {
    let (c, calls) = card_logged();
    let f = open(&c);
    let i = ids(&f);
    let fb = make_fb(&f, 1920, 1080);
    set_crtc(&f, &i, fb).unwrap();
    take(&calls);

    // An overlay half off the right edge still shows.
    let small = make_fb(&f, 200, 100);
    let a = DrmModeSetPlane {
        plane_id: i.overlay,
        crtc_id: i.crtc,
        fb_id: small,
        flags: 0,
        crtc_x: 1820,
        crtc_y: 10,
        crtc_w: 200,
        crtc_h: 100,
        src_x: 0,
        src_y: 0,
        src_w: 200 << 16,
        src_h: 100 << 16,
    };
    ctl(&f, DRM_IOCTL_MODE_SETPLANE, &a).unwrap();
    assert_eq!(
        take(&calls),
        [
            std::format!("plane1 fb={small} shown=true"),
            "crtc0 active=true".into()
        ]
    );
    // Wholly off screen: no error, nothing shown.
    let off = DrmModeSetPlane { crtc_x: 5000, ..a };
    ctl(&f, DRM_IOCTL_MODE_SETPLANE, &off).unwrap();
    assert_eq!(
        take(&calls)[0],
        std::format!("plane1 fb={small} shown=false")
    );
    // A source outside its buffer is refused.
    let bad = DrmModeSetPlane {
        src_w: 300 << 16,
        ..a
    };
    assert_eq!(
        ctl(&f, DRM_IOCTL_MODE_SETPLANE, &bad).err(),
        Some(Errno::EINVAL)
    );
    // Switching it off.
    ctl(
        &f,
        DRM_IOCTL_MODE_SETPLANE,
        &DrmModeSetPlane {
            plane_id: i.overlay,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(take(&calls)[0], "plane1 fb=0 shown=false");
    c.unregister(&s());
}

#[test]
fn power_off_turns_the_controller_off_and_on_again() {
    let (c, calls) = card_logged();
    let f = open(&c);
    let i = ids(&f);
    let fb = make_fb(&f, 1920, 1080);
    set_crtc(&f, &i, fb).unwrap();
    take(&calls);
    let dpms = pid(&f, i.conn, DRM_MODE_OBJECT_CONNECTOR, "DPMS");
    let set = |v| DrmModeConnectorSetProperty {
        value: v,
        prop_id: dpms,
        connector_id: i.conn,
    };
    ctl(&f, DRM_IOCTL_MODE_SETPROPERTY, &set(DRM_MODE_DPMS_OFF)).unwrap();
    assert_eq!(take(&calls).last().unwrap(), "crtc0 active=false");
    assert_eq!(
        ctl(&f, DRM_IOCTL_MODE_SETPROPERTY, &set(9)).err(),
        Some(Errno::EINVAL)
    );
    c.unregister(&s());
}

#[test]
fn immutable_and_unknown_properties_are_refused() {
    let (c, _calls) = card_logged();
    let f = open(&c);
    let i = ids(&f);
    let p = DRM_MODE_OBJECT_PLANE;
    let ty = pid(&f, i.primary, p, "type");
    let set = |obj, kind, prop, value| {
        let a = DrmModeObjSetProperty {
            value,
            prop_id: prop,
            obj_id: obj,
            obj_type: kind,
        };
        ctl(&f, DRM_IOCTL_MODE_OBJ_SETPROPERTY, &a)
    };
    assert_eq!(set(i.primary, p, ty, 0).err(), Some(Errno::EINVAL));
    // A property of another object.
    let active = pid(&f, i.crtc, DRM_MODE_OBJECT_CRTC, "ACTIVE");
    assert_eq!(set(i.primary, p, active, 1).err(), Some(Errno::EINVAL));
    assert_eq!(set(i.primary, p, 9999, 1).err(), Some(Errno::ENOENT));
    // A driver property takes only its range.
    let zpos = pid(&f, i.primary, p, "zpos");
    assert_eq!(set(i.primary, p, zpos, 9).err(), Some(Errno::EINVAL));
    set(i.primary, p, zpos, 2).unwrap();
    assert_eq!(
        props_of(&f, i.primary, p)
            .iter()
            .find(|(_, _, n)| n == "zpos")
            .unwrap()
            .1,
        2
    );
    c.unregister(&s());
}

// ---- sharing buffers ----

fn prime(
    f: &Arc<crate::Open>,
    req: u32,
    handle: u32,
    flags: u32,
    fd: i32,
) -> Result<DrmPrimeHandle, Errno> {
    ctl(f, req, &DrmPrimeHandle { handle, flags, fd })
}

/// A dumb buffer of `w` by `h` in `f`: its handle.
fn dumb(f: &Arc<crate::Open>, w: u32, h: u32) -> u32 {
    let mut d = create_dumb_arg(h, w, 32);
    call(f, DRM_IOCTL_MODE_CREATE_DUMB, &mut d).unwrap();
    u32_of(&d, 16)
}

#[test]
fn a_buffer_crosses_to_another_file_through_a_descriptor() {
    let c = card(true, true);
    let a = open(&c);
    let b = open(&c);
    vouch(&a, &b);
    let handle = dumb(&a, 64, 64);
    a.gem(handle)
        .unwrap()
        .with_page(0, &mut |p| p[..4].copy_from_slice(b"abcd"))
        .unwrap();

    let got = prime(
        &a,
        DRM_IOCTL_PRIME_HANDLE_TO_FD,
        handle,
        DRM_CLOEXEC | DRM_RDWR,
        -1,
    )
    .unwrap();
    assert!(got.fd > 100);
    {
        let t = DESCRIPTORS.lock().unwrap();
        let (_, cloexec, rdwr) = &t[usize::try_from(got.fd - 101).unwrap()];
        assert!(*cloexec && *rdwr);
    }
    // Importing it where it came from gives the handle it already has.
    let back = prime(&a, DRM_IOCTL_PRIME_FD_TO_HANDLE, 0, 0, got.fd).unwrap();
    assert_eq!(back.handle, handle);
    // Another file gets a handle of its own for the same memory.
    let there = prime(&b, DRM_IOCTL_PRIME_FD_TO_HANDLE, 0, 0, got.fd).unwrap();
    assert_ne!(there.handle, 0);
    let g = b.gem(there.handle).unwrap();
    assert!(Arc::ptr_eq(&g, &a.gem(handle).unwrap()));
    g.with_page(0, &mut |p| assert_eq!(&p[..4], b"abcd"))
        .unwrap();
    // Importing twice does not make a second handle.
    assert_eq!(
        prime(&b, DRM_IOCTL_PRIME_FD_TO_HANDLE, 0, 0, got.fd)
            .unwrap()
            .handle,
        there.handle
    );

    // The buffer outlives the handle that made it.
    let mut close = handle.to_ne_bytes().to_vec();
    close.resize(8, 0);
    call(&a, DRM_IOCTL_GEM_CLOSE, &mut close).unwrap();
    g.with_page(0, &mut |p| assert_eq!(&p[..4], b"abcd"))
        .unwrap();
    c.unregister(&s());
}

#[test]
fn prime_refuses_what_it_cannot_do() {
    let c = card(true, true);
    let f = open(&c);
    let handle = dumb(&f, 16, 16);
    assert_eq!(
        prime(&f, DRM_IOCTL_PRIME_HANDLE_TO_FD, handle, 0x4000, -1).err(),
        Some(Errno::EINVAL)
    );
    assert_eq!(
        prime(&f, DRM_IOCTL_PRIME_HANDLE_TO_FD, 99, 0, -1).err(),
        Some(Errno::ENOENT)
    );
    assert_eq!(
        prime(&f, DRM_IOCTL_PRIME_FD_TO_HANDLE, 0, 1, 101).err(),
        Some(Errno::EINVAL)
    );
    assert_eq!(
        prime(&f, DRM_IOCTL_PRIME_FD_TO_HANDLE, 0, 0, 9999).err(),
        Some(Errno::EBADF)
    );
    // The capability says both directions work.
    let cap = ctl(
        &f,
        DRM_IOCTL_GET_CAP,
        &DrmGetCap {
            capability: DRM_CAP_PRIME,
            value: 0,
        },
    )
    .unwrap();
    assert_eq!(cap.value, DRM_PRIME_CAP_IMPORT | DRM_PRIME_CAP_EXPORT);
    c.unregister(&s());
}

#[test]
fn a_descriptor_maps_from_its_own_first_page() {
    let c = card(true, true);
    let f = open(&c);
    let handle = dumb(&f, 64, 128);
    let buf = crate::DmaBuf::new(f.gem(handle).unwrap());
    assert_eq!(buf.size(), 64 * 4 * 128);
    let pages = buf.size() / PAGE_SIZE;
    // Page 0 of the descriptor is the buffer's first page, wherever the
    // buffer sits in its card's file offsets.
    let (_, first) = buf.mapping(0, pages).unwrap();
    assert_eq!(first, f.gem(handle).unwrap().mmap_offset() / PAGE_SIZE);
    let (_, second) = buf.mapping(1, 1).unwrap();
    assert_eq!(second, first + 1);
    assert_eq!(buf.mapping(0, pages + 1).err(), Some(Errno::EINVAL));
    assert_eq!(buf.mapping(pages, 1).err(), Some(Errno::EINVAL));
    c.unregister(&s());
}

#[test]
fn a_framebuffer_drawn_in_place_is_reread_on_a_flip_to_itself_and_on_dirty() {
    let (c, calls) = card_logged();
    let f = open(&c);
    let i = ids(&f);
    let fb = make_fb(&f, 1920, 1080);
    set_crtc(&f, &i, fb).unwrap();
    take(&calls);

    // The same framebuffer again: nothing about the state changed, but
    // the layer must read it.
    let a = DrmModeCrtcPageFlip {
        crtc_id: i.crtc,
        fb_id: fb,
        flags: 0,
        reserved: 0,
        user_data: 0,
    };
    ctl(&f, DRM_IOCTL_MODE_PAGE_FLIP, &a).unwrap();
    assert_eq!(
        take(&calls),
        [
            std::format!("plane0 fb={fb} shown=true"),
            "crtc0 active=true".into()
        ]
    );
    fire(c.unit());

    // Dirty: the layers showing it read it, and nothing else is touched.
    let dirty = DrmModeFbDirtyCmd {
        fb_id: fb,
        ..Default::default()
    };
    ctl(&f, DRM_IOCTL_MODE_DIRTYFB, &dirty).unwrap();
    assert_eq!(take(&calls), [std::format!("plane0 fb={fb} shown=true")]);
    // A buffer no plane shows costs nothing; an unknown one is refused.
    let other = make_fb(&f, 64, 64);
    ctl(
        &f,
        DRM_IOCTL_MODE_DIRTYFB,
        &DrmModeFbDirtyCmd {
            fb_id: other,
            ..Default::default()
        },
    )
    .unwrap();
    assert!(take(&calls).is_empty());
    let nope = DrmModeFbDirtyCmd {
        fb_id: 9999,
        ..Default::default()
    };
    assert_eq!(
        ctl(&f, DRM_IOCTL_MODE_DIRTYFB, &nope).err(),
        Some(Errno::ENOENT)
    );
    c.unregister(&s());
}

// ---- where buffers live ----

use crate::gem::{Backing, CacheMode, Placement};

fn card_placed(placement: Placement) -> (Arc<crate::Card>, Arc<Mutex<Vec<u64>>>) {
    setup();
    let cleaned = Arc::<Mutex<Vec<u64>>>::default();
    let c = crate::register(Arc::new(Mock {
        dumb: Some(DumbLimits::default()),
        kms: Some(mock_display(&Calls::default())),
        placement,
        cleaned: cleaned.clone(),
    }))
    .unwrap();
    (c, cleaned)
}

#[test]
fn a_buffer_a_device_does_not_snoop_is_cleaned_before_it_is_shown() {
    // A chip whose display does not snoop the processor's caches.
    let (c, cleaned) = card_placed(Placement::SYSTEM_NONCOHERENT);
    let f = open(&c);
    let i = ids(&f);
    let fb = make_fb(&f, 1920, 1080);
    assert_eq!(f.gem(1).unwrap().placement(), Placement::SYSTEM_NONCOHERENT);
    assert!(
        cleaned.lock().unwrap().is_empty(),
        "making a buffer cleans nothing"
    );
    set_crtc(&f, &i, fb).unwrap();
    // Cleaned in full, once, before the layer reads it.
    let size = 1920u64 * 4 * 1080;
    assert_eq!(*cleaned.lock().unwrap(), [size.next_multiple_of(PAGE_SIZE)]);
    c.unregister(&s());
}

#[test]
fn a_coherent_buffer_needs_no_cleaning() {
    let (c, cleaned) = card_placed(Placement::SYSTEM_COHERENT);
    let f = open(&c);
    let i = ids(&f);
    let fb = make_fb(&f, 1920, 1080);
    set_crtc(&f, &i, fb).unwrap();
    assert!(cleaned.lock().unwrap().is_empty());
    c.unregister(&s());
}

#[test]
fn placements_not_built_yet_are_refused_with_a_clear_error() {
    // A discrete card's own memory, and uncached or write-combined pages,
    // need virtual memory support that is not there.
    for p in [
        Placement {
            backing: Backing::DeviceLocal,
            ..Placement::SYSTEM_COHERENT
        },
        Placement {
            backing: Backing::Contiguous,
            ..Placement::SYSTEM_COHERENT
        },
        Placement {
            cache: CacheMode::WriteCombine,
            ..Placement::SYSTEM_COHERENT
        },
        Placement {
            cache: CacheMode::Uncached,
            ..Placement::SYSTEM_NONCOHERENT
        },
    ] {
        let (c, _) = card_placed(p);
        let f = open(&c);
        let mut d = create_dumb_arg(16, 16, 32);
        assert_eq!(
            call(&f, DRM_IOCTL_MODE_CREATE_DUMB, &mut d),
            Err(Errno::EOPNOTSUPP),
            "{p:?}"
        );
        c.unregister(&s());
    }
}

// ---- authentication and leases ----

#[test]
fn the_master_vouches_for_a_file_by_its_magic() {
    let c = card(false, true);
    let master = open(&c);
    let guest = open(&c);
    assert!(master.is_authenticated(), "the master needs no vouching");
    assert!(!guest.is_authenticated());

    let magic = ctl(&guest, DRM_IOCTL_GET_MAGIC, &DrmAuth { magic: 0 })
        .unwrap()
        .magic;
    assert_ne!(magic, 0);
    // The same file gets the same number; another file a different one.
    assert_eq!(
        ctl(&guest, DRM_IOCTL_GET_MAGIC, &DrmAuth { magic: 0 })
            .unwrap()
            .magic,
        magic
    );
    let other = ctl(&master, DRM_IOCTL_GET_MAGIC, &DrmAuth { magic: 0 })
        .unwrap()
        .magic;
    assert_ne!(other, magic);

    // Only the master vouches, and only for a number that was given out.
    assert_eq!(
        ctl(&guest, DRM_IOCTL_AUTH_MAGIC, &DrmAuth { magic }).err(),
        Some(Errno::EACCES)
    );
    assert_eq!(
        ctl(
            &master,
            DRM_IOCTL_AUTH_MAGIC,
            &DrmAuth {
                magic: magic ^ 0x10
            }
        )
        .err(),
        Some(Errno::EINVAL)
    );
    ctl(&master, DRM_IOCTL_AUTH_MAGIC, &DrmAuth { magic }).unwrap();
    assert!(guest.is_authenticated());
    // A number is spent once it is used.
    assert_eq!(
        ctl(&master, DRM_IOCTL_AUTH_MAGIC, &DrmAuth { magic }).err(),
        Some(Errno::EINVAL)
    );
    c.unregister(&s());
}

#[test]
fn leases_are_refused_as_unsupported_so_callers_go_on_without() {
    let c = card(false, true);
    let f = open(&c);
    assert_eq!(
        ctl(
            &f,
            DRM_IOCTL_MODE_CREATE_LEASE,
            &DrmModeCreateLease::default()
        )
        .err(),
        Some(Errno::EOPNOTSUPP)
    );
    assert_eq!(
        ctl(
            &f,
            DRM_IOCTL_MODE_LIST_LESSEES,
            &DrmModeListLessees::default()
        )
        .err(),
        Some(Errno::EOPNOTSUPP)
    );
    assert_eq!(
        ctl(&f, DRM_IOCTL_MODE_GET_LEASE, &DrmModeGetLease::default()).err(),
        Some(Errno::EOPNOTSUPP)
    );
    assert_eq!(
        ctl(
            &f,
            DRM_IOCTL_MODE_REVOKE_LEASE,
            &DrmModeRevokeLease::default()
        )
        .err(),
        Some(Errno::EOPNOTSUPP)
    );
    c.unregister(&s());
}

// ---- frames ----

/// A lit display: the card, its file, and the objects.
fn lit() -> (Arc<crate::Card>, Arc<crate::Open>, Ids) {
    let (c, _calls) = card_logged();
    let f = open(&c);
    let i = ids(&f);
    let fb = make_fb(&f, 1920, 1080);
    set_crtc(&f, &i, fb).unwrap();
    (c, f, i)
}

fn wait(
    f: &Arc<crate::Open>,
    kind: u32,
    sequence: u32,
    signal: u64,
) -> Result<DrmWaitVblank, Errno> {
    ctl(
        f,
        DRM_IOCTL_WAIT_VBLANK,
        &DrmWaitVblank {
            kind,
            sequence,
            word0: signal,
            word1: 0,
        },
    )
}

#[test]
fn a_blocked_wait_ends_at_its_frame_and_says_when() {
    let (c, f, _) = lit();
    let k = c.driver().kms().unwrap();
    let before = k.frames(0);
    // Zero frames from now returns at once.
    let now = wait(&f, DRM_VBLANK_RELATIVE, 0, 0).unwrap();
    assert_eq!(u64::from(now.sequence), before);
    assert_eq!(k.frames(0), before);
    // Three frames from now: the frame timer is fired for each.
    let r = wait(&f, DRM_VBLANK_RELATIVE, 3, 0).unwrap();
    assert_eq!(u64::from(r.sequence), before + 3);
    assert_eq!(k.frames(0), before + 3);
    // The time is the frame's, in seconds and microseconds.
    assert_eq!((r.word0, r.word1), (5, 0));
    // Absolute: to a count in the future, and one already past, which
    // returns where it is.
    let abs = wait(
        &f,
        DRM_VBLANK_ABSOLUTE,
        u32::try_from(before + 5).unwrap(),
        0,
    )
    .unwrap();
    assert_eq!(u64::from(abs.sequence), before + 5);
    let past = wait(&f, DRM_VBLANK_ABSOLUTE, 1, 0).unwrap();
    assert_eq!(u64::from(past.sequence), before + 5);
    c.unregister(&s());
}

#[test]
fn a_wait_can_be_asked_for_as_an_event() {
    let (c, f, i) = lit();
    let k = c.driver().kms().unwrap();
    let before = k.frames(0);
    let r = wait(&f, DRM_VBLANK_RELATIVE | DRM_VBLANK_EVENT, 2, 0xbeef).unwrap();
    assert_eq!(
        u64::from(r.sequence),
        before + 2,
        "the reply names the frame it will come at"
    );
    assert!(read_event(&f).is_none());
    fire(c.unit());
    assert!(read_event(&f).is_none(), "one frame is not enough");
    fire(c.unit());
    let ev = read_event(&f).expect("the event at the second frame");
    assert_eq!(ev.base.kind, DRM_EVENT_VBLANK);
    assert_eq!((ev.user_data, ev.crtc_id), (0xbeef, i.crtc));
    assert_eq!(u64::from(ev.sequence), before + 2);
    assert_eq!((ev.tv_sec, ev.tv_usec), (5, 0));
    // Nothing waits now, so the timer stops.
    fire(c.unit());
    assert_eq!(k.frames(0), before + 2);
    // An event for a frame already past goes at once.
    wait(&f, DRM_VBLANK_ABSOLUTE | DRM_VBLANK_EVENT, 0, 7).unwrap();
    assert_eq!(read_event(&f).unwrap().user_data, 7);
    c.unregister(&s());
}

#[test]
fn what_a_wait_refuses() {
    let (c, f, _) = lit();
    assert_eq!(
        wait(&f, DRM_VBLANK_RELATIVE | DRM_VBLANK_SIGNAL, 1, 0).err(),
        Some(Errno::EINVAL)
    );
    // The second controller does not exist.
    assert_eq!(
        wait(&f, DRM_VBLANK_RELATIVE | DRM_VBLANK_SECONDARY, 1, 0).err(),
        Some(Errno::EINVAL)
    );
    c.unregister(&s());

    // A controller that is off has no frames to wait for.
    let (c, _calls) = card_logged();
    let g = open(&c);
    ids(&g);
    assert_eq!(
        wait(&g, DRM_VBLANK_RELATIVE, 1, 0).err(),
        Some(Errno::EINVAL)
    );
    c.unregister(&s());
}

fn read_sequence_event(f: &Arc<crate::Open>) -> Option<(u32, u64, i64, u64)> {
    let mut b = [0u8; 64];
    let mut r = vfs::ReadBuf::new(&mut b);
    match f.read(&s(), &mut r, 0, true) {
        Ok(32) => {
            let b = r.filled();
            Some((
                u32_of(b, 0),
                read_u64(b, 8),
                read_u64(b, 16).cast_signed(),
                read_u64(b, 24),
            ))
        }
        _ => None,
    }
}

#[test]
fn sequences_are_counted_and_queued() {
    let (c, f, i) = lit();
    let k = c.driver().kms().unwrap();
    let get = |f: &Arc<crate::Open>| {
        ctl(
            f,
            DRM_IOCTL_CRTC_GET_SEQUENCE,
            &DrmCrtcGetSequence {
                crtc_id: i.crtc,
                ..Default::default()
            },
        )
        .unwrap()
    };
    let start = get(&f);
    assert_eq!(start.active, 1);
    assert_eq!(start.sequence, k.frames(0));

    let queue = |flags, sequence, user_data| {
        let a = DrmCrtcQueueSequence {
            crtc_id: i.crtc,
            flags,
            sequence,
            user_data,
        };
        ctl(&f, DRM_IOCTL_CRTC_QUEUE_SEQUENCE, &a)
    };
    let q = queue(DRM_CRTC_SEQUENCE_RELATIVE, 2, 0x51).unwrap();
    assert_eq!(q.sequence, start.sequence + 2);
    assert_eq!(queue(0x8, 1, 0).err(), Some(Errno::EINVAL), "unknown flags");
    // A count already past is an error, or the next frame if asked.
    assert_eq!(queue(0, 1, 0).err(), Some(Errno::EINVAL));
    let next = queue(DRM_CRTC_SEQUENCE_NEXT_ON_MISS, 1, 0x52).unwrap();
    assert_eq!(next.sequence, start.sequence + 1);

    fire(c.unit());
    let first = read_sequence_event(&f).expect("the next-frame event");
    assert_eq!(
        (first.0, first.1, first.3),
        (DRM_EVENT_CRTC_SEQUENCE, 0x52, start.sequence + 1)
    );
    assert_eq!(first.2, 5_000_000_000, "its time is in nanoseconds");
    fire(c.unit());
    let second = read_sequence_event(&f).expect("the relative event");
    assert_eq!((second.1, second.3), (0x51, start.sequence + 2));
    let end = get(&f);
    assert_eq!(end.sequence, start.sequence + 2);
    assert_eq!(end.sequence_ns, 5_000_000_000);
    c.unregister(&s());
}

/// The master `m` vouches for file `g`.
fn vouch(m: &Arc<crate::Open>, g: &Arc<crate::Open>) {
    let magic = ctl(g, DRM_IOCTL_GET_MAGIC, &DrmAuth { magic: 0 })
        .unwrap()
        .magic;
    ctl(m, DRM_IOCTL_AUTH_MAGIC, &DrmAuth { magic }).unwrap();
}

#[test]
fn sharing_buffers_needs_the_masters_say_so() {
    let c = card(true, true);
    let master = open(&c);
    let guest = open(&c);
    let handle = dumb(&guest, 16, 16);
    // A file nobody vouched for cannot export or import.
    assert_eq!(
        prime(&guest, DRM_IOCTL_PRIME_HANDLE_TO_FD, handle, 0, -1).err(),
        Some(Errno::EACCES)
    );
    assert_eq!(
        prime(&guest, DRM_IOCTL_PRIME_FD_TO_HANDLE, 0, 0, 101).err(),
        Some(Errno::EACCES)
    );
    // The master needs no vouching; once vouched for, the guest can.
    let h = dumb(&master, 16, 16);
    assert!(prime(&master, DRM_IOCTL_PRIME_HANDLE_TO_FD, h, 0, -1).is_ok());
    vouch(&master, &guest);
    assert!(prime(&guest, DRM_IOCTL_PRIME_HANDLE_TO_FD, handle, 0, -1).is_ok());
    c.unregister(&s());
}

/// The firmware's framebuffer, as a card with nothing else.
struct FirmwareCard;

impl DrmDriver for FirmwareCard {
    fn info(&self) -> DriverInfo {
        DriverInfo {
            name: "simpledrm",
            desc: "Firmware framebuffer",
            date: "20261122",
            major: 1,
            minor: 0,
            patch: 0,
        }
    }
    fn firmware_framebuffer(&self) -> bool {
        true
    }
}

/// A native driver retires the firmware's framebuffer card, and only that
/// one: its node goes and the other card stays.
#[test]
fn retiring_firmware_cards_leaves_the_others() {
    setup();
    let native = card(true, false);
    let fw = crate::register(Arc::new(FirmwareCard)).unwrap();
    assert!(!fw.is_revoked() && !native.is_revoked());
    assert_eq!(crate::retire_firmware_cards(&s()), 1);
    assert!(fw.is_revoked());
    assert!(!native.is_revoked());
    // Nothing left to retire.
    assert_eq!(crate::retire_firmware_cards(&s()), 0);
    native.unregister(&s());
}

/// A card that counts the times a panic told it to show the console.
struct PanicCard(core::sync::atomic::AtomicU32);

impl DrmDriver for PanicCard {
    fn info(&self) -> DriverInfo {
        DriverInfo {
            name: "panic-test",
            desc: "Counts panics",
            date: "20261124",
            major: 0,
            minor: 0,
            patch: 0,
        }
    }
    fn panic_restore(&self) {
        self.0.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }
}

/// A panic reaches every registered card, and a card that is gone is not
/// asked.
#[test]
fn a_panic_tells_every_card_to_show_the_console() {
    setup();
    let a = Arc::new(PanicCard(core::sync::atomic::AtomicU32::new(0)));
    let b = Arc::new(PanicCard(core::sync::atomic::AtomicU32::new(0)));
    let ca = crate::register(a.clone()).unwrap();
    let cb = crate::register(b.clone()).unwrap();
    crate::panic_restore();
    assert_eq!(a.0.load(core::sync::atomic::Ordering::Relaxed), 1);
    assert_eq!(b.0.load(core::sync::atomic::Ordering::Relaxed), 1);
    cb.unregister(&s());
    drop(cb);
    crate::panic_restore();
    assert_eq!(a.0.load(core::sync::atomic::Ordering::Relaxed), 2);
    assert_eq!(b.0.load(core::sync::atomic::Ordering::Relaxed), 1);
    ca.unregister(&s());
}

/// A driver with a request of its own that uses the file's context: it makes
/// a buffer, copies a word to the caller, and notes when a file closes.
struct Render {
    closed: Arc<Mutex<Vec<u64>>>,
    seen_files: Arc<Mutex<Vec<u64>>>,
}

impl DrmDriver for Render {
    fn info(&self) -> DriverInfo {
        DriverInfo {
            name: "render",
            desc: "A render mock",
            date: "20261124",
            major: 3,
            minor: 64,
            patch: 0,
        }
    }
    fn ioctl_cx(&self, cx: &crate::IoctlCx<'_>, nr: u8, arg: &mut [u8]) -> Result<usize, Errno> {
        self.seen_files.lock().unwrap().push(cx.file_id());
        // The request number counts from the first driver request.
        match nr - abi::drm::DRM_COMMAND_BASE {
            // Make a buffer of the size in the first eight bytes.
            0 => {
                let size = u64::from_le_bytes(arg[..8].try_into().unwrap());
                let (h, g) = cx.new_gem(size, crate::gem::Placement::SYSTEM_COHERENT)?;
                arg[8..12].copy_from_slice(&h.to_le_bytes());
                arg[16..24].copy_from_slice(&g.size().to_le_bytes());
                Ok(0)
            }
            // Close the handle in the first four bytes.
            1 => {
                let h = u32::from_le_bytes(arg[..4].try_into().unwrap());
                cx.close_handle(h).ok_or(Errno::ENOENT).map(|_| 0)
            }
            // Report whether the handle in the first four bytes is the file's.
            2 => {
                let h = u32::from_le_bytes(arg[..4].try_into().unwrap());
                arg[4] = u8::from(cx.gem(h).is_some());
                Ok(0)
            }
            _ => Err(Errno::ENOTTY),
        }
    }
    fn file_closed(&self, file: u64) {
        self.closed.lock().unwrap().push(file);
    }
}

fn render_req(nr: u8, len: u32) -> u32 {
    0xc000_0000 | (len << 16) | 0x6400 | u32::from(abi::drm::DRM_COMMAND_BASE + nr)
}

#[test]
fn a_drivers_own_request_gets_the_file_and_its_handles_and_hears_when_the_file_closes() {
    setup();
    let closed = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let c = crate::register(Arc::new(Render {
        closed: Arc::clone(&closed),
        seen_files: Arc::clone(&seen),
    }))
    .unwrap();
    let a = open(&c);
    let b = open(&c);
    // Make a buffer in file a: the handle is a's.
    let mut arg = [0u8; 24];
    arg[..8].copy_from_slice(&5000u64.to_le_bytes());
    call(&a, render_req(0, 24), &mut arg).unwrap();
    let h = u32::from_le_bytes(arg[8..12].try_into().unwrap());
    assert!(h >= 1);
    // The size is rounded up to whole pages.
    assert_eq!(u64::from_le_bytes(arg[16..24].try_into().unwrap()), 8192);
    let mut q = [0u8; 8];
    q[..4].copy_from_slice(&h.to_le_bytes());
    call(&a, render_req(2, 8), &mut q).unwrap();
    assert_eq!(q[4], 1);
    // File b does not have it.
    let mut q = [0u8; 8];
    q[..4].copy_from_slice(&h.to_le_bytes());
    call(&b, render_req(2, 8), &mut q).unwrap();
    assert_eq!(q[4], 0);
    // The requests saw two different file ids.
    let ids = seen.lock().unwrap().clone();
    assert!(ids.contains(&a.id()) && ids.contains(&b.id()) && a.id() != b.id());
    // Close the handle, then the file: the driver is told.
    let mut x = [0u8; 8];
    x[..4].copy_from_slice(&h.to_le_bytes());
    call(&a, render_req(1, 8), &mut x).unwrap();
    assert_eq!(call(&a, render_req(1, 8), &mut x), Err(Errno::ENOENT));
    let a_id = a.id();
    drop(a);
    assert_eq!(*closed.lock().unwrap(), std::vec![a_id]);
    // A request the driver lacks is "no such ioctl".
    assert_eq!(
        call(&b, render_req(9, 8), &mut [0u8; 8]),
        Err(Errno::ENOTTY)
    );
}
