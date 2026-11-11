// SPDX-License-Identifier: BSD-2-Clause
//! `/dev/tunN`, `/dev/tapN`, `/dev/vmnetN` (docs/NETWORKING.md §5.5): the
//! device side of [`net::tuntap`]. Each interface has its node, made when
//! the interface is (by `ifconfig tun create`, or by opening `/dev/tun`
//! or `/dev/tunN`, which makes one: devfs cloning), and an alias by its
//! name after a rename. One process opens it at a time; reads take a
//! packet the stack sent, sleeping for one unless non-blocking; writes
//! give it one; `select` and `kevent` see whether a read would wait;
//! `FIOASYNC` sends `SIGIO` as packets come.
//!
//! Behavior reference: sys/net/if_tuntap.c (`tunclone`, `tunopen`,
//! `tundtor`, `tunread`, `tunwrite`, `tunioctl`, `tunpoll`,
//! `tunkqfilter`, `tunrename`).

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use abi::errno::Errno;
use abi::event::{EVFILT_READ, EVFILT_WRITE, Kevent, POLLIN, POLLOUT, POLLRDNORM, POLLWRNORM};
use abi::ioctl::{FIOGETOWN, FIOSETOWN};
use event::{Filter, Knote, Note, Notify, Wake};
use mbuf::Chain;
use net::tuntap::{self, Kind, Tun, WriteLen};
use sched::{Sleepable, WaitQueue};
use sync::PreemptLock;
use vfs::{CharDevice, Uio};

use crate::cred::Priv;
use crate::fs::devfs;
use crate::signal::sigio::Sigio;

// SAFETY: `struct tuninfo` is integers with no padding.
unsafe impl crate::uaccess::Plain for abi::tun::Tuninfo {}

/// `TIOCSPGRP`, `TIOCGPGRP`.
const TIOCSPGRP: u32 = 0x8004_7476;
const TIOCGPGRP: u32 = 0x4004_7477;
/// `UID_UUCP`, `GID_DIALER`: the nodes' owner.
const UID_UUCP: u32 = 66;
const GID_DIALER: u32 = 68;

/// What a device wakes: readers, pollers and knotes, and `FIOASYNC`'s
/// owner.
struct Waker {
    wq: WaitQueue,
    notify: Notify,
    sigio: Sigio,
    /// Bumped by each wakeup, for sleepers to see it.
    wakes: AtomicU64,
    tun: Weak<Tun>,
}

impl tuntap::Wake for Waker {
    fn wake(&self) {
        self.wakes.fetch_add(1, Ordering::Release);
        self.wq.wake_all();
        self.notify.notify(0);
        if self.tun.upgrade().is_some_and(|t| t.async_io()) {
            self.sigio.signal();
        }
    }
}

/// A device's node: each open is a [`TunOpen`].
struct TunNode {
    tun: Arc<Tun>,
    w: Arc<Waker>,
}

fn pid() -> i32 {
    crate::proc::Process::current().map_or(0, |p| p.pid())
}

impl CharDevice for TunNode {
    /// `tunopen`: one open at a time.
    fn instance_open(
        &self,
        _s: &Sleepable,
        _nonblock: bool,
        _read: bool,
        _write: bool,
    ) -> Result<Option<Arc<dyn CharDevice>>, Errno> {
        if self.tun.dying() {
            return Err(Errno::ENXIO);
        }
        self.tun.open(pid())?;
        Ok(Some(Arc::new(TunOpen {
            tun: Arc::clone(&self.tun),
            w: Arc::clone(&self.w),
            closed: AtomicBool::new(false),
        })))
    }

    fn read(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENXIO)
    }

    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENXIO)
    }
}

/// An open of a device; its last close is `tundtor`.
struct TunOpen {
    tun: Arc<Tun>,
    w: Arc<Waker>,
    closed: AtomicBool,
}

impl TunOpen {
    fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let (pid, comm) =
            crate::proc::Process::current().map_or((0, alloc::string::String::new()), |p| {
                (
                    p.pid(),
                    p.with_comm(|c| alloc::string::String::from_utf8_lossy(c).into_owned()),
                )
            });
        self.w.sigio.clear();
        if self.tun.close(pid, &comm) {
            // A transient device goes with its last close
            // (`if_clone_destroyif`), from a thread of its own.
            queue(Job::Destroy(Arc::downgrade(&self.tun)));
        }
    }
}

impl Drop for TunOpen {
    fn drop(&mut self) {
        self.close();
    }
}

impl CharDevice for TunOpen {
    /// `tunread`: a packet at a time, as much of it as fits.
    fn read(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        _: u64,
        nonblock: bool,
    ) -> Result<usize, Errno> {
        let m = loop {
            let g = self.w.wakes.load(Ordering::Acquire);
            if let Some(m) = self.tun.dequeue()? {
                break m;
            }
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            let _w = sched::wait_message("tunread");
            let w = &self.w;
            crate::signal::sleep(s, &w.wq, || w.wakes.load(Ordering::Acquire) != g, None)?;
        };
        let mut done = 0;
        let vh = self.tun.vhdrlen().min(uio.resid());
        if vh > 0 {
            // No offload was asked of it: the header says none.
            let z = [0u8; abi::tun::VNET_HDR_MRG_LEN];
            done += uio.copy_out(&z[..vh])?;
        }
        for seg in m.data.segments() {
            if uio.resid() == 0 {
                break;
            }
            let n = seg.len().min(uio.resid());
            done += uio.copy_out(&seg[..n])?;
        }
        Ok(done)
    }

    /// `tunwrite`: a write is a packet.
    fn write(&self, _: &Sleepable, uio: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        let resid = uio.resid();
        let WriteLen::Packet { vhdrlen } = self.tun.write_len(resid)? else {
            return Ok(0);
        };
        let mut vh = [0u8; abi::tun::VNET_HDR_MRG_LEN];
        if vhdrlen > 0 && uio.copy_in(&mut vh[..vhdrlen])? != vhdrlen {
            return Err(Errno::EIO);
        }
        let len = resid - vhdrlen;
        let data = Chain::fill_leading(len, self.tun.write_lead(), |dst, _| uio.copy_in(dst))
            .map_err(|e: Errno| e)?;
        self.tun
            .write((vhdrlen > 0).then_some(&vh[..vhdrlen]), data)?;
        Ok(resid)
    }

    fn ioctl(&self, _: &Sleepable, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        let int = |arg: &[u8]| {
            arg.get(..4)
                .and_then(|b| b.try_into().ok())
                .map_or(0, i32::from_ne_bytes)
        };
        let put = |arg: &mut [u8], v: i32| {
            if let Some(a) = arg.get_mut(..4) {
                a.copy_from_slice(&v.to_ne_bytes());
            }
        };
        match req {
            abi::tun::TUNSIFINFO => {
                let mut info = abi::tun::Tuninfo::default();
                let b = crate::uaccess::bytes_of_mut(&mut info);
                let n = b.len().min(arg.len());
                b[..n].copy_from_slice(&arg[..n]);
                // Changing the MTU is `ifhwioctl`'s `SIOCSIFMTU`.
                if self.tun.mtu_changes(&info) {
                    crate::fs::priv_check(Priv::NetSetifmtu)?;
                }
                return self.tun.set_info(&info);
            }
            FIOSETOWN => return self.w.sigio.set(int(arg)),
            FIOGETOWN => {
                put(arg, self.w.sigio.get());
                return Ok(());
            }
            TIOCSPGRP => {
                return self
                    .w
                    .sigio
                    .set(int(arg).checked_neg().ok_or(Errno::EINVAL)?);
            }
            TIOCGPGRP => {
                put(arg, self.w.sigio.get().wrapping_neg());
                return Ok(());
            }
            _ => {}
        }
        if self.tun.ioctl(req, arg, pid())? {
            Ok(())
        } else {
            Err(Errno::ENOTTY)
        }
    }

    fn set_async(&self, on: bool) {
        self.tun.set_async(on);
    }

    /// `tunpoll`: readable while a packet waits; always writable.
    fn poll(&self, events: i16, w: Option<&Arc<dyn Wake>>) -> i16 {
        let mut revents = events & (POLLOUT | POLLWRNORM);
        if events & (POLLIN | POLLRDNORM) != 0 {
            if self.tun.queued() > 0 {
                revents |= events & (POLLIN | POLLRDNORM);
            } else if let Some(w) = w {
                self.w.notify.record(w);
            }
        }
        revents
    }

    /// `tunkqfilter`.
    fn kqfilter(&self, kev: &Kevent) -> Result<Box<dyn Filter>, Errno> {
        let write = match kev.filter {
            EVFILT_READ => false,
            EVFILT_WRITE => true,
            _ => return Err(Errno::EINVAL),
        };
        Ok(Box::new(TunFilter {
            tun: Arc::clone(&self.tun),
            w: Arc::clone(&self.w),
            write,
        }))
    }
}

/// `tun_read_filterops` and `tun_write_filterops`.
struct TunFilter {
    tun: Arc<Tun>,
    w: Arc<Waker>,
    write: bool,
}

impl Filter for TunFilter {
    fn attach(&self, kn: &Arc<Knote>) -> Result<(), Errno> {
        self.w.notify.add(kn);
        Ok(())
    }

    fn detach(&self, kn: &Knote) {
        self.w.notify.remove(kn);
    }

    /// `tunkqread`: the packets waiting; `tunkqwrite`: always, with the
    /// MTU.
    fn event(&self, _kn: &Knote, note: &mut Note, _hint: u64) -> bool {
        if self.write {
            note.kev.data = i64::from(self.tun.mtu());
            return true;
        }
        let q = self.tun.queued();
        note.kev.data = i64::try_from(q).unwrap_or(i64::MAX);
        q > 0
    }

    fn fork(&self, _kn: &Knote) -> Option<Box<dyn Filter>> {
        Some(Box::new(Self {
            tun: Arc::clone(&self.tun),
            w: Arc::clone(&self.w),
            write: self.write,
        }))
    }
}

/// What the device thread does where the caller cannot wait.
enum Job {
    /// A gone device's vnodes are doomed.
    Reclaim(u64),
    /// A renamed interface's alias: the old one goes, a new one comes.
    Alias {
        old: Option<Vec<u8>>,
        new: Option<(Vec<u8>, Vec<u8>)>,
    },
    /// A transient device closed: its interface is destroyed.
    Destroy(Weak<Tun>),
}

static JOBS: PreemptLock<VecDeque<Job>> = PreemptLock::new(VecDeque::new());
static JOBS_WQ: WaitQueue = WaitQueue::new();

fn queue(j: Job) {
    JOBS.lock().push_back(j);
    JOBS_WQ.wake_one();
}

/// The device thread.
fn worker(s: &Sleepable) {
    loop {
        JOBS_WQ.wait_until(s, || !JOBS.lock().is_empty());
        loop {
            let Some(j) = JOBS.lock().pop_front() else {
                break;
            };
            match j {
                Job::Reclaim(ino) => devfs::reclaim(s, ino),
                Job::Alias { old, new } => {
                    if let Some(o) = old {
                        devfs::remove(s, &o);
                    }
                    if let Some((alias, target)) = new {
                        devfs::link(&alias, &target);
                    }
                }
                Job::Destroy(t) => {
                    if let Some(t) = t.upgrade()
                        && let (Some(v), index) = t.home()
                    {
                        let name = v.net.lock().by_index(index).map(|i| i.name.clone());
                        if let Some(name) = name {
                            let _ = net::clone::destroy(&v, name.as_bytes());
                        }
                    }
                }
            }
        }
    }
}

/// The nodes made, by device name, so each can be unlinked by its own
/// device.
static NODES: PreemptLock<Vec<Node>> = PreemptLock::new(Vec::new());

/// A device and its node.
type Node = (Weak<Tun>, Arc<dyn CharDevice>);

/// `tun_create_device`: the node, `crw------- uucp:dialer`.
fn made(t: &Arc<Tun>) {
    let w = Arc::new(Waker {
        wq: WaitQueue::new(),
        notify: Notify::new(),
        sigio: Sigio::new(),
        wakes: AtomicU64::new(0),
        tun: Arc::downgrade(t),
    });
    t.set_wake(Arc::clone(&w) as Arc<dyn tuntap::Wake>);
    let node: Arc<dyn CharDevice> = Arc::new(TunNode {
        tun: Arc::clone(t),
        w,
    });
    let name = t.devname();
    devfs::add(
        name.as_bytes(),
        0o600,
        UID_UUCP,
        GID_DIALER,
        Arc::clone(&node),
    );
    let mut nodes = NODES.lock();
    nodes.retain(|(w, _)| w.strong_count() > 0);
    nodes.push((Arc::downgrade(t), node));
}

/// `destroy_dev`: the node goes now, its vnodes soon; its alias too.
fn gone(t: &Tun) {
    let node = {
        let mut nodes = NODES.lock();
        let at = nodes
            .iter()
            .position(|(w, _)| core::ptr::eq(w.as_ptr(), core::ptr::from_ref(t)));
        at.map(|at| nodes.remove(at).1)
    };
    if let Some(node) = node
        && let Some(ino) = devfs::unlink_dev(t.devname().as_bytes(), &node)
    {
        queue(Job::Reclaim(ino));
    }
    if let Some(a) = t.set_alias(None) {
        queue(Job::Alias {
            old: Some(a.into_bytes()),
            new: None,
        });
    }
}

/// `tunrename`: an alias by the interface's new name, unless that is the
/// device's own.
fn renamed(t: &Tun, name: &str) {
    let dev = t.devname();
    let new = (name != dev).then(|| alloc::string::String::from(name));
    let old = t.set_alias(new.clone());
    queue(Job::Alias {
        old: old.map(alloc::string::String::into_bytes),
        new: new.map(|n| (n.into_bytes(), dev.into_bytes())),
    });
}

/// `tunclone`: a name in `/dev` no node has. `tun`, `tap`, or `vmnet`
/// alone makes the lowest free unit; with a unit, that one. Only the
/// privileged may (`PRIV_NET_IFCREATE`), and only with
/// `net.link.*.devfs_cloning` (`tap` and `vmnet` for anyone with
/// `net.link.tap.user_open`).
fn clone(_s: &Sleepable, name: &[u8]) -> devfs::CloneResult {
    use devfs::CloneResult;
    let Some((kind, unit)) = Kind::parse(name) else {
        return CloneResult::NotMine;
    };
    if unit.is_some_and(|u| u > 0x7fff) {
        return CloneResult::NotMine;
    }
    let privileged = crate::fs::priv_check(Priv::NetIfcreate).is_ok();
    let on = |a: &core::sync::atomic::AtomicI32| a.load(Ordering::Relaxed) != 0;
    let may = if kind.l2() {
        (privileged || on(&tuntap::TAP_USER_OPEN)) && on(&tuntap::TAP_DEVFS_CLONING)
    } else {
        privileged && on(&tuntap::TUN_DEVFS_CLONING)
    };
    if !may {
        return CloneResult::Refused;
    }
    let vnet = super::curvnet();
    let ask = match unit {
        Some(u) => alloc::format!("{}{u}", kind.name()),
        None => alloc::string::String::from(kind.name()),
    };
    match net::clone::create(&vnet, ask.as_bytes(), &super::clone_ctx()) {
        Ok(ifname) => {
            // The device's name is the unit the interface got.
            let unit = &ifname[kind.name().len()..];
            CloneResult::Made(alloc::format!("{}{unit}", kind.name()).into_bytes())
        }
        Err(_) => CloneResult::Refused,
    }
}

/// Starts the device thread and registers the hooks and devfs cloning.
pub(crate) fn init() {
    tuntap::set_hooks(tuntap::Hooks {
        made,
        gone,
        renamed,
    });
    devfs::add_cloner(clone);
    let _ = sched::spawn("tuntap", sched::Priority::KERNEL, worker);
}
