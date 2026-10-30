// SPDX-License-Identifier: BSD-2-Clause
//! An inotify descriptor (`__specialfd(SPECIALFD_INOTIFY)`): the file
//! around `vfs::inotify`'s queue. Reading waits for an event and returns
//! whole records; polling and `EVFILT_READ` say whether one is pending.
//!
//! Behavior reference: sys/kern/vfs_inotify.c (`inotifyfdops`).

use alloc::boxed::Box;
use alloc::sync::Arc;

use abi::errno::Errno;
use abi::event::{EVFILT_READ, Kevent, POLLIN, POLLRDNORM};
use abi::inotify::{InotifyEvent, name_size};
use abi::stat::{S_IFREG, S_IRUSR, Stat};
use event::{Filter, Knote, Note, Wake};
use sched::Sleepable;
use vfs::Uio;
use vfs::inotify::Inotify;

/// `NAME_MAX`, for `st_blksize`.
const NAME_MAX: usize = 255;

/// An inotify descriptor's open file: closing it ends its watches.
pub(crate) struct InotifyFile(pub(crate) Arc<Inotify>);

impl Drop for InotifyFile {
    fn drop(&mut self) {
        self.0.close();
    }
}

impl InotifyFile {
    pub(crate) fn new(uid: u32, gid: u32) -> Self {
        Self(Inotify::new(uid, gid))
    }

    /// `inotify_read`: waits while nothing is pending, then as many whole
    /// records as fit.
    pub(crate) fn read(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        nonblock: bool,
    ) -> Result<usize, Errno> {
        let room = uio.resid();
        let bytes = loop {
            match self.0.read(room) {
                Err(Errno::EAGAIN) if !nonblock => {
                    let _w = sched::wait_message("inotify");
                    crate::signal::sleep(s, &self.0.wait, || self.0.pending_records() > 0, None)?;
                }
                r => break r?,
            }
        };
        if crate::ktrace::point(abi::ktrace::KTR_STRUCT) {
            crate::ktrace::structure(s, "inotify", &bytes);
        }
        uio.copy_out(&bytes)
    }

    /// `inotify_poll`.
    pub(crate) fn poll(&self, events: i16, w: Option<&Arc<dyn Wake>>) -> i16 {
        if let Some(w) = w {
            self.0.notify.record(w);
        }
        if events & (POLLIN | POLLRDNORM) != 0 && self.0.pending_records() > 0 {
            events & (POLLIN | POLLRDNORM)
        } else {
            0
        }
    }

    /// `inotify_kqfilter`: `EVFILT_READ` only.
    pub(crate) fn kqfilter(&self, kev: &Kevent) -> Result<Box<dyn Filter>, Errno> {
        if kev.filter != EVFILT_READ {
            return Err(Errno::EINVAL);
        }
        Ok(Box::new(InotifyFilter(Arc::clone(&self.0))))
    }

    /// `FIONREAD`: the bytes a read would return.
    pub(crate) fn nread(&self) -> usize {
        self.0.pending_bytes()
    }

    /// `inotify_stat`: a regular file its creator may read, sized by what
    /// is pending.
    pub(crate) fn stat(&self) -> Stat {
        let st = self.0.stat();
        Stat {
            st_mode: S_IFREG | S_IRUSR,
            st_blksize: i32::try_from(size_of::<InotifyEvent>() + name_size(NAME_MAX))
                .unwrap_or(i32::MAX),
            st_size: i64::try_from(st.bytes).unwrap_or(i64::MAX),
            st_blocks: i64::try_from(st.records).unwrap_or(i64::MAX),
            st_uid: st.uid,
            st_gid: st.gid,
            st_ino: st.ino,
            ..Stat::default()
        }
    }
}

/// `inotify_rfiltops`.
struct InotifyFilter(Arc<Inotify>);

impl Filter for InotifyFilter {
    fn attach(&self, kn: &Arc<Knote>) -> Result<(), Errno> {
        self.0.notify.add(kn);
        Ok(())
    }

    fn detach(&self, kn: &Knote) {
        self.0.notify.remove(kn);
    }

    fn event(&self, _kn: &Knote, note: &mut Note, _hint: u64) -> bool {
        let n = self.0.pending_bytes();
        note.kev.data = i64::try_from(n).unwrap_or(i64::MAX);
        n > 0
    }

    fn fork(&self, _kn: &Knote) -> Option<Box<dyn Filter>> {
        Some(Box::new(Self(Arc::clone(&self.0))))
    }
}

/// Adds `vfs.inotify`, its watch limits a third and a half of
/// `kern.maxvnodes` (`inotify_init`).
pub(crate) fn sysctl_init(t: &mut sysctl::Tree, vfs: sysctl::NodeId) {
    use core::sync::atomic::{AtomicI32, Ordering::Relaxed};
    use sysctl::{Number, Value};
    use vfs::inotify as ino;
    let vnodes = i32::try_from(super::max_vnodes()).unwrap_or(i32::MAX);
    ino::MAX_USER_WATCHES.store(vnodes / 3, Relaxed);
    ino::MAX_WATCHES.store(vnodes / 2, Relaxed);
    let node = t
        .add_node(vfs, Number::Auto, "inotify", "inotify configuration")
        .expect("vfs.inotify");
    let writable: [(&str, &str, &'static AtomicI32); 5] = [
        (
            "max_queued_events",
            "Maximum number of events to queue on an inotify descriptor",
            &ino::MAX_QUEUED_EVENTS,
        ),
        (
            "max_user_instances",
            "Maximum number of inotify descriptors per user",
            &ino::MAX_USER_INSTANCES,
        ),
        (
            "max_user_watches",
            "Maximum number of inotify watches per user",
            &ino::MAX_USER_WATCHES,
        ),
        (
            "max_watches",
            "Maximum number of inotify watches system-wide",
            &ino::MAX_WATCHES,
        ),
        (
            "coalesce",
            "Coalesce inotify events when possible",
            &ino::COALESCE,
        ),
    ];
    for (name, descr, v) in writable {
        t.add_writable(
            node,
            Number::Auto,
            name,
            descr,
            0,
            Value::Int(Box::new(|| v.load(Relaxed))),
            sysctl::int_writer(|n| {
                v.store(n, Relaxed);
                Ok(())
            }),
        )
        .expect(name);
    }
    t.add_value(
        node,
        Number::Auto,
        "watches",
        "Total number of inotify watches currently in use",
        Value::Int(Box::new(|| ino::WATCHES.load(Relaxed))),
    )
    .expect("vfs.inotify.watches");
    t.add_value(
        node,
        Number::Auto,
        "event_drops",
        "Number of inotify events dropped due to limits or allocation failures",
        Value::U64(Box::new(|| ino::EVENT_DROPS.load(Relaxed))),
    )
    .expect("vfs.inotify.event_drops");
}
