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
    kms: bool,
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
    fn has_kms(&self) -> bool {
        self.kms
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
        kms,
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
    // _IOWR('d', 0xA7, ...) is GETCONNECTOR: not in this slice.
    assert_eq!(
        call(&f, DRM_IOCTL_MODE_GETCONNECTOR, &mut [0; 80]),
        Err(Errno::ENOTTY)
    );
    assert!(
        LOG.lock()
            .unwrap()
            .iter()
            .any(|l| l.contains("0xa7") && l.contains("not implemented"))
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
