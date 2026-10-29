// SPDX-License-Identifier: BSD-2-Clause
//! `/dev/devctl`, the device events `devd(8)` reads, and `/dev/devctl2`,
//! the requests `devctl(8)` makes of the device tree.
//!
//! - `/dev/devctl` has one reader at a time (`EBUSY` otherwise). Each read
//!   takes one event line from the bus manager's queue
//!   ([`bus::Manager::take_event`]), as much of it as the buffer holds:
//!   the rest is lost, as FreeBSD's `devread` loses it. A read waits for
//!   an event, or fails with `EAGAIN` without blocking; `poll`, `select`,
//!   and `EVFILT_READ` see the queue, and `FIOASYNC` asks for `SIGIO` when
//!   an event comes. Events queue whether or not the device is open.
//! - `/dev/devctl2` answers `DEV_*` with `struct devreq`, through the
//!   manager's devctl operations; all but `DEV_GET_PATH` need
//!   `PRIV_DRIVER`, checked first.
//!
//! Behavior reference: sys/kern/kern_devctl.c (`devopen`, `devclose`,
//! `devread`, `devioctl`, `devpoll`, `devkqfilter`), sys/kern/subr_bus.c
//! (`devctl2_ioctl`).

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::mem::{offset_of, size_of};
use core::sync::atomic::{AtomicBool, Ordering};

use abi::bus::*;
use abi::errno::Errno;
use abi::event::{EVFILT_READ, Kevent, POLLIN, POLLRDNORM};
use abi::ioctl::{FIOASYNC, FIOGETOWN, FIONBIO, FIOSETOWN};
use event::{Filter, Knote, Note, Notify, Wake};
use sched::{Sleepable, WaitQueue};
use vfs::{CharDevice, Uio};

use crate::signal::sigio::Sigio;

/// `/dev/devctl` is open (`devsoftc.inuse`).
static OPEN: AtomicBool = AtomicBool::new(false);
/// `FIOASYNC` is on (`devsoftc.async`).
static ASYNC: AtomicBool = AtomicBool::new(false);
static SIGIO: Sigio = Sigio::new();
/// Readers (`devsoftc.cv`).
static WAIT: WaitQueue = WaitQueue::new();
/// Pollers and knotes (`devsoftc.sel`).
static NOTIFY: Notify = Notify::new();

/// An event was queued (`devctl_queue`'s wakeups).
fn queued() {
    WAIT.wake_all();
    NOTIFY.notify(0);
    if ASYNC.load(Ordering::Acquire) {
        SIGIO.signal();
    }
}

fn queue_len() -> usize {
    bus::manager().events_queued()
}

/// `/dev/devctl`'s node: each open is a [`DevctlOpen`].
struct Devctl;

impl CharDevice for Devctl {
    /// `devopen`.
    fn instance(&self, _s: &Sleepable) -> Result<Option<Arc<dyn CharDevice>>, Errno> {
        if OPEN.swap(true, Ordering::AcqRel) {
            return Err(Errno::EBUSY);
        }
        Ok(Some(Arc::new(DevctlOpen)))
    }

    fn read(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENXIO)
    }

    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }
}

/// An open of `/dev/devctl`; its last close is `devclose`.
struct DevctlOpen;

impl Drop for DevctlOpen {
    fn drop(&mut self) {
        ASYNC.store(false, Ordering::Release);
        SIGIO.clear();
        OPEN.store(false, Ordering::Release);
        WAIT.wake_all();
    }
}

fn int(arg: &[u8]) -> i32 {
    arg.get(..4)
        .and_then(|b| b.try_into().ok())
        .map_or(0, i32::from_ne_bytes)
}

impl CharDevice for DevctlOpen {
    /// `devread`: one event per read.
    fn read(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        _: u64,
        nonblock: bool,
    ) -> Result<usize, Errno> {
        let m = bus::manager();
        loop {
            if let Some(line) = m.take_event() {
                return uio.copy_out(line.as_bytes());
            }
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            let _w = sched::wait_message("dev cv");
            crate::signal::sleep(s, &WAIT, || m.events_queued() > 0, None)?;
        }
    }

    /// No `d_write` (`enodev`).
    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }

    /// `devioctl`: the rest (`FIONREAD`, `TIOCSPGRP`, ...) is `ENOTTY`.
    fn ioctl(&self, _: &Sleepable, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        match req {
            // The file's flag is the non-blocking state reads see.
            FIONBIO => Ok(()),
            FIOASYNC => {
                ASYNC.store(int(arg) != 0, Ordering::Release);
                Ok(())
            }
            FIOSETOWN => SIGIO.set(int(arg)),
            FIOGETOWN => {
                if let Some(a) = arg.get_mut(..4) {
                    a.copy_from_slice(&SIGIO.get().to_ne_bytes());
                }
                Ok(())
            }
            _ => Err(Errno::ENOTTY),
        }
    }

    fn set_async(&self, on: bool) {
        ASYNC.store(on, Ordering::Release);
    }

    /// `devpoll`: readable with an event queued.
    fn poll(&self, events: i16, w: Option<&Arc<dyn Wake>>) -> i16 {
        if events & (POLLIN | POLLRDNORM) == 0 {
            return 0;
        }
        if let Some(w) = w {
            NOTIFY.record(w);
        }
        if queue_len() > 0 {
            events & (POLLIN | POLLRDNORM)
        } else {
            0
        }
    }

    /// `devkqfilter`: `EVFILT_READ` only.
    fn kqfilter(&self, kev: &Kevent) -> Result<Box<dyn Filter>, Errno> {
        if kev.filter != EVFILT_READ {
            return Err(Errno::EINVAL);
        }
        Ok(Box::new(DevctlFilter))
    }
}

/// `devctl_rfiltops`: active with events queued, their number as data.
struct DevctlFilter;

impl Filter for DevctlFilter {
    fn attach(&self, kn: &Arc<Knote>) -> Result<(), Errno> {
        NOTIFY.add(kn);
        Ok(())
    }

    fn detach(&self, kn: &Knote) {
        NOTIFY.remove(kn);
    }

    fn event(&self, _kn: &Knote, note: &mut Note, _hint: u64) -> bool {
        let n = queue_len();
        note.kev.data = i64::try_from(n).unwrap_or(i64::MAX);
        n != 0
    }

    fn fork(&self, _kn: &Knote) -> Option<Box<dyn Filter>> {
        Some(Box::new(Self))
    }
}

/// `/dev/devctl2`: requests only.
struct Devctl2;

/// A `struct devreq` field at `at`.
fn get_usize(arg: &[u8], at: usize) -> usize {
    arg.get(at..at + size_of::<usize>())
        .and_then(|b| b.try_into().ok())
        .map_or(0, usize::from_ne_bytes)
}

fn str_errno(e: crate::uaccess::StrError) -> Errno {
    match e {
        crate::uaccess::StrError::Fault => Errno::EFAULT,
        crate::uaccess::StrError::TooLong => Errno::ENAMETOOLONG,
    }
}

/// A string the caller passes by address, of at most `max` bytes with its
/// NUL (`copyinstr`). One that is not UTF-8 names nothing here.
fn user_str(addr: usize, max: usize) -> Result<alloc::string::String, Errno> {
    let b = crate::uaccess::copy_in_str_vec(addr as u64, max).map_err(str_errno)?;
    alloc::string::String::from_utf8(b).map_err(|_| Errno::ENOENT)
}

impl Devctl2 {
    /// `devctl2_ioctl`.
    fn request(s: &Sleepable, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        // Which requests name a device, and which need privilege.
        let (find, privileged) = match req {
            DEV_ATTACH | DEV_DETACH | DEV_ENABLE | DEV_DISABLE | DEV_SUSPEND | DEV_RESUME
            | DEV_SET_DRIVER | DEV_CLEAR_DRIVER | DEV_RESCAN | DEV_DELETE | DEV_RESET => {
                (true, true)
            }
            DEV_FREEZE | DEV_THAW => (false, true),
            DEV_GET_PATH => (true, false),
            _ => return Err(Errno::ENOTTY),
        };
        if arg.len() != size_of::<Devreq>() {
            return Err(Errno::EINVAL);
        }
        if privileged {
            crate::fs::priv_check(crate::cred::Priv::Driver)?;
        }
        let m = bus::manager();
        let flags = int(&arg[offset_of!(Devreq, dr_flags)..]);
        let buffer = get_usize(
            arg,
            offset_of!(Devreq, dr_buffer) + offset_of!(DevreqBuffer, buffer),
        );
        let id = if find {
            // `find_device`: the name must end within its field.
            let name = &arg[offset_of!(Devreq, dr_name)..][..128];
            let len = name.iter().position(|&b| b == 0).ok_or(Errno::EINVAL)?;
            let name = core::str::from_utf8(&name[..len]).map_err(|_| Errno::ENOENT)?;
            Some(m.lookup(name, s).ok_or(Errno::ENOENT)?)
        } else {
            None
        };
        let Some(id) = id else {
            return if req == DEV_FREEZE {
                m.devctl_freeze()
            } else {
                m.devctl_thaw()
            };
        };
        match req {
            DEV_ATTACH => m.devctl_attach(id, s),
            DEV_DETACH => m.devctl_detach(id, flags & DEVF_FORCE_DETACH != 0, s),
            DEV_ENABLE => m.devctl_enable(id, s),
            DEV_DISABLE => m.devctl_disable(id, flags & DEVF_FORCE_DETACH != 0, s),
            DEV_SUSPEND => m.devctl_suspend(id, s),
            DEV_RESUME => m.devctl_resume(id, s),
            DEV_SET_DRIVER => {
                let driver = user_str(buffer, 128)?;
                m.devctl_set_driver(id, &driver, flags & DEVF_SET_DRIVER_DETACH != 0, s)
            }
            DEV_CLEAR_DRIVER => m.devctl_clear_driver(id, flags & DEVF_CLEAR_DRIVER_DETACH != 0, s),
            DEV_RESCAN => m.devctl_rescan(id, s),
            DEV_DELETE => m.devctl_delete(id, flags & DEVF_FORCE_DELETE != 0, s),
            DEV_RESET => m.devctl_reset(id, flags, s),
            _ => {
                let locator = user_str(buffer, 64)?;
                let mut path = m.device_path(id, &locator, s)?.into_bytes();
                path.push(0);
                let at = offset_of!(Devreq, dr_buffer) + offset_of!(DevreqBuffer, length);
                let room = get_usize(arg, at);
                arg[at..at + size_of::<usize>()].copy_from_slice(&path.len().to_ne_bytes());
                if room < path.len() {
                    return Err(Errno::ENAMETOOLONG);
                }
                crate::uaccess::copy_out(buffer as u64, &path).map_err(|_| Errno::EFAULT)
            }
        }
    }
}

impl CharDevice for Devctl2 {
    /// No `d_read` (`enodev`).
    fn read(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }

    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }

    fn ioctl(&self, s: &Sleepable, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        Self::request(s, req, arg)
    }
}

/// Adds `/dev/devctl` and `/dev/devctl2` (FreeBSD's modes, owned by
/// `root:wheel`), and has the manager wake `/dev/devctl`'s readers.
pub(crate) fn init() {
    bus::manager().set_event_hook(queued);
    crate::fs::devfs::add(b"devctl", 0o600, 0, 0, Arc::new(Devctl));
    crate::fs::devfs::add(b"devctl2", 0o644, 0, 0, Arc::new(Devctl2));
}

#[cfg(feature = "ktest")]
pub(crate) mod ktests {
    use alloc::format;
    use alloc::string::String;

    use abi::event::{EV_ADD, EVFILT_WRITE};
    use abi::ioctl::{FIONREAD, TIOCSPGRP};
    use ktest::ktest;
    use vfs::ReadBuf;

    use super::*;

    pub(crate) fn cx() -> Sleepable {
        // SAFETY: tests run in threads, holding no spin locks.
        unsafe { Sleepable::assume() }
    }

    /// Opens `/dev/<name>` as `open(2)` would: its per-open device, or the
    /// node's own.
    pub(crate) fn open(s: &Sleepable, name: &[u8]) -> Result<Arc<dyn CharDevice>, Errno> {
        let v = crate::fs::devfs::vnode_of(s, name).ok_or(Errno::ENOENT)?;
        let d = v.device(s)?.ops;
        Ok(d.instance(s)?.unwrap_or(d))
    }

    /// One descriptor, `dev`, for a kqueue to watch.
    pub(crate) struct One(pub(crate) Arc<dyn CharDevice>);

    impl event::Source for One {
        fn ident(&self, _: &Kevent) -> Result<event::Ident, Errno> {
            Ok(event::Ident::Fd)
        }

        fn filter(&self, kev: &Kevent) -> Result<Box<dyn Filter>, Errno> {
            self.0.kqfilter(kev)
        }
    }

    pub(crate) struct NoWake;

    impl Wake for NoWake {
        fn wake(&self) {}
    }

    pub(crate) fn kev(filter: i16) -> Kevent {
        Kevent {
            ident: 3,
            filter,
            flags: EV_ADD,
            ..Kevent::default()
        }
    }

    fn read(
        d: &Arc<dyn CharDevice>,
        s: &Sleepable,
        len: usize,
        nonblock: bool,
    ) -> Result<String, Errno> {
        let mut buf = alloc::vec![0u8; len];
        let n = d.read(s, &mut ReadBuf::new(&mut buf), 0, nonblock)?;
        Ok(String::from_utf8_lossy(&buf[..n]).into_owned())
    }

    ktest! {
        fn devctl_queues_events_for_one_reader() {
            let s = cx();
            let m = bus::manager();
            let _ = m.take_events();
            // What was queued before the open is there for the reader.
            m.notify("KTEST", "devctl", "before", None);
            let d = open(&s, b"devctl").unwrap();
            assert_eq!(open(&s, b"devctl").err(), Some(Errno::EBUSY));
            assert_eq!(read(&d, &s, 256, true).unwrap(), "!system=KTEST subsystem=devctl type=before\n");
            assert_eq!(read(&d, &s, 256, true), Err(Errno::EAGAIN));
            assert_eq!(d.poll(POLLIN | POLLRDNORM, None), 0);
            // A knote, for reads only.
            let kq = event::Kqueue::new(Box::new(NoWake));
            kq.register(&kev(EVFILT_READ), &One(Arc::clone(&d))).unwrap();
            assert_eq!(kq.register(&kev(EVFILT_WRITE), &One(Arc::clone(&d))), Err(Errno::EINVAL));
            assert_eq!(kq.pending(), 0);
            m.notify("KTEST", "devctl", "two", Some("a=1"));
            m.notify("KTEST", "devctl", "three", None);
            assert_eq!(d.poll(POLLIN | POLLRDNORM, None), POLLIN | POLLRDNORM);
            assert_eq!(d.poll(abi::event::POLLOUT, None), 0);
            assert_eq!(kq.pending(), 1);
            let mut out = [Kevent::default(); 2];
            assert_eq!(kq.collect(kq.begin_scan(), &mut out), 1);
            assert_eq!((out[0].filter, out[0].data), (EVFILT_READ, 2));
            // A short read loses the rest of its line.
            assert_eq!(read(&d, &s, 8, true).unwrap(), "!system=");
            assert_eq!(read(&d, &s, 256, true).unwrap(), "!system=KTEST subsystem=devctl type=three\n");
            // A blocked reader wakes for the next event.
            let t = sched::spawn("devctl-ev", sched::Priority::KERNEL, |s| {
                crate::time::sleep_ns(s, 20_000_000);
                bus::manager().notify("KTEST", "devctl", "later", None);
            })
            .unwrap();
            assert_eq!(read(&d, &s, 256, false).unwrap(), "!system=KTEST subsystem=devctl type=later\n");
            t.join(&s);
            // devctl's requests; the rest are not its.
            let mut arg = [0u8; 4];
            assert_eq!(d.ioctl(&s, FIONREAD, &mut arg), Err(Errno::ENOTTY));
            assert_eq!(d.ioctl(&s, TIOCSPGRP, &mut arg), Err(Errno::ENOTTY));
            assert_eq!(d.ioctl(&s, FIOGETOWN, &mut arg), Ok(()));
            assert_eq!(i32::from_ne_bytes(arg), 0);
            assert_eq!(d.ioctl(&s, FIONBIO, &mut 1i32.to_ne_bytes()), Ok(()));
            kq.close();
            drop(d);
            // Closed, it opens again.
            drop(open(&s, b"devctl").unwrap());
            // Only turning the queue off may be asked once up.
            assert_eq!(m.set_devctl_queue(5), Err(Errno::EINVAL));
            assert_eq!(m.devctl_queue(), bus::DEVCTL_DEFAULT_QUEUE);
            let _ = m.take_events();
        }
    }

    /// A `struct devreq` naming `name`.
    fn devreq(name: &str, flags: i32) -> [u8; size_of::<Devreq>()] {
        let mut r = [0u8; size_of::<Devreq>()];
        r[..name.len()].copy_from_slice(name.as_bytes());
        r[offset_of!(Devreq, dr_flags)..][..4].copy_from_slice(&flags.to_ne_bytes());
        r
    }

    ktest! {
        fn devctl2_detaches_and_attaches_a_device() {
            let s = cx();
            let m = bus::manager();
            let d2 = open(&s, b"devctl2").unwrap();
            let ask = |req: u32, name: &str, flags: i32| d2.ioctl(&s, req, &mut devreq(name, flags));
            let (transport, rnd) =
                crate::devices::find_named(m, &s, crate::devices::nexus(), "vtrnd0").expect("vtrnd0");
            let tname = m.nameunit(transport, &s).unwrap();
            let _ = m.take_events();
            // Names, and what is not a request.
            assert_eq!(ask(DEV_ATTACH, "nosuch0", 0), Err(Errno::ENOENT));
            let mut long = [b'x'; size_of::<Devreq>()];
            assert_eq!(d2.ioctl(&s, DEV_ATTACH, &mut long), Err(Errno::EINVAL));
            assert_eq!(d2.ioctl(&s, abi::ioctl::FIONREAD, &mut [0; 4]), Err(Errno::ENOTTY));
            assert_eq!(m.lookup("vtrnd0", &s), Some(rnd));
            // The path from the root down.
            let path = m.device_path(rnd, BUS_LOCATOR_FREEBSD, &s).unwrap();
            assert!(path.starts_with("/nexus0/") && path.ends_with(&format!("/{tname}/vtrnd0")), "{path}");
            assert_eq!(m.device_path(m.root(), BUS_LOCATOR_UEFI, &s).as_deref(), Ok("/"));
            assert_eq!(m.device_path(rnd, BUS_LOCATOR_UEFI, &s), Err(Errno::EIO));
            // What the driver model cannot do, as a bus without it answers.
            assert_eq!(ask(DEV_ATTACH, "vtrnd0", 0), Err(Errno::EBUSY));
            assert_eq!(ask(DEV_SUSPEND, "vtrnd0", 0), Err(Errno::ENXIO));
            assert_eq!(ask(DEV_RESUME, "vtrnd0", 0), Err(Errno::EINVAL));
            assert_eq!(ask(DEV_RESCAN, "vtrnd0", 0), Err(Errno::ENODEV));
            assert_eq!(ask(DEV_RESET, "vtrnd0", 2), Err(Errno::EINVAL));
            assert_eq!(ask(DEV_RESET, "vtrnd0", 0), Err(Errno::ENXIO));
            assert_eq!(ask(DEV_DELETE, "vtrnd0", 0), Err(Errno::EBUSY));
            assert_eq!(ask(DEV_DELETE, "root0", DEVF_FORCE_DELETE), Err(Errno::EINVAL));
            assert_eq!(ask(DEV_DETACH, "nexus0", DEVF_FORCE_DETACH), Err(Errno::ENXIO));
            assert_eq!(ask(DEV_FREEZE, "", 0), Ok(()));
            assert_eq!(ask(DEV_FREEZE, "", 0), Err(Errno::EBUSY));
            assert_eq!(ask(DEV_THAW, "", 0), Ok(()));
            assert_eq!(ask(DEV_THAW, "", 0), Err(Errno::EBUSY));
            // The bus's own name for the transport outlives its driver's.
            let selector = match m.ident(transport, &s) {
                Some(bus::Ident::Pci(_, a, _)) => format!("pci{}:{}:{}:{}", a.domain, a.bus, a.slot, a.function),
                other => panic!("vtrnd0's transport is {other:?}"),
            };
            assert_eq!(m.lookup(&selector, &s), Some(transport));
            // Detach, and the events devd sees.
            assert_eq!(ask(DEV_DETACH, &tname, 0), Ok(()));
            assert_eq!(m.state(transport, &s), Some(bus::NodeState::NotPresent));
            let events = m.take_events();
            assert!(events.iter().any(|e| e.starts_with("-vtrnd0 at ")), "{events:?}");
            assert!(events.iter().any(|e| e.starts_with(&format!("-{tname} at "))), "{events:?}");
            assert_eq!(ask(DEV_DETACH, &selector, 0), Err(Errno::ENXIO));
            assert_eq!(ask(DEV_ATTACH, &tname, 0), Err(Errno::ENOENT));
            // Attach by the selector: the same transport, a working vtrnd0.
            assert_eq!(ask(DEV_ATTACH, &selector, 0), Ok(()));
            m.wait_idle(&s);
            assert_eq!(m.state(transport, &s), Some(bus::NodeState::Attached));
            let events = m.take_events();
            assert!(events.iter().any(|e| e.starts_with("+vtrnd0 at ")), "{events:?}");
            let mut buf = [0u8; 32];
            assert_eq!(virtio_random::read(&s, &mut buf), Some(32));
            // Disabled, it keeps its name and does not attach.
            let tname = m.nameunit(transport, &s).unwrap();
            assert_eq!(ask(DEV_DISABLE, &tname, 0), Ok(()));
            assert!(m.is_disabled(transport, &s));
            assert_eq!(m.nameunit(transport, &s).as_deref(), Some(tname.as_str()));
            assert_eq!(ask(DEV_DISABLE, &tname, 0), Err(Errno::ENXIO));
            assert_eq!(ask(DEV_ATTACH, &tname, 0), Err(Errno::ENXIO));
            assert_eq!(ask(DEV_ENABLE, &tname, 0), Ok(()));
            m.wait_idle(&s);
            assert_eq!(ask(DEV_ENABLE, &selector, 0), Err(Errno::EBUSY));
            assert_eq!(m.state(transport, &s), Some(bus::NodeState::Attached));
            assert!(crate::devices::find_named(m, &s, crate::devices::nexus(), "vtrnd0").is_some());
            assert_eq!(virtio_random::read(&s, &mut buf), Some(32));
            let _ = m.take_events();
        }
    }
}
