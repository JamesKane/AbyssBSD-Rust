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
        });
        vfs::devices::set_registry(Box::leak(Box::new(TestDevfs)));
        USER.set(arena).ok();
    });
}

static USER: std::sync::OnceLock<&'static Arena> = std::sync::OnceLock::new();

struct Mock {
    dumb: Option<DumbLimits>,
    kms: Option<crate::kms::Kms>,
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
    fn cap(&self, cap: u64) -> Option<u64> {
        (cap == 0x1000).then_some(7)
    }
}

fn card(dumb: bool, kms: bool) -> Arc<crate::Card> {
    setup();
    crate::register(Arc::new(Mock {
        dumb: dumb.then(DumbLimits::default),
        kms: kms.then(mock_display),
    }))
    .unwrap()
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
    // _IOWR('d', 0xA2, ...) is SETCRTC: not in this slice.
    assert_eq!(
        call(&f, DRM_IOCTL_MODE_SETCRTC, &mut [0; 104]),
        Err(Errno::ENOTTY)
    );
    assert!(
        LOG.lock()
            .unwrap()
            .iter()
            .any(|l| l.contains("0xa2") && l.contains("not implemented"))
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

struct MockCrtc;
impl CrtcHw for MockCrtc {}

const PLANE_FORMATS: &[Fourcc] = &[format::XRGB8888, format::ARGB8888];

struct MockPlane;
impl PlaneHw for MockPlane {
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

struct MockEncoder;
impl EncoderHw for MockEncoder {
    fn kind(&self) -> u32 {
        DRM_MODE_ENCODER_TMDS
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
fn mock_display() -> crate::kms::Kms {
    let mut b = KmsBuilder::new(KmsLimits {
        min_width: 16,
        max_width: 4096,
        min_height: 16,
        max_height: 4096,
    });
    let c = b.crtc(Arc::new(MockCrtc));
    b.plane(Arc::new(MockPlane), PlaneKind::Primary, 1 << c);
    b.plane(Arc::new(MockPlane), PlaneKind::Overlay, 1 << c);
    let e = b.encoder(Arc::new(MockEncoder), 1 << c, 0);
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
