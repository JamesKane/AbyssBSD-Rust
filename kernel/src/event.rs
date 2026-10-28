// SPDX-License-Identifier: BSD-2-Clause
//! The kernel's half of events (docs/MULTIUSER.md §7): the threads that
//! wait in `poll`, `select`, and `kevent`, kqueue descriptors, and the
//! filters for what is not a descriptor's own (processes, signals, file
//! systems) or that every vnode shares.
//!
//! Behavior reference: sys/kern/kern_event.c, sys/kern/sys_generic.c,
//! sys/kern/vfs_subr.c (`vfs_kqfilter`).

use alloc::boxed::Box;
use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, Ordering};

use abi::errno::Errno;
use abi::event::{
    EV_ADD, EV_CLEAR, EV_DROP, EV_ENABLE, EV_EOF, EV_FLAG1, EV_FLAG2, EV_ONESHOT, EVFILT_EMPTY,
    EVFILT_FS, EVFILT_PROC, EVFILT_PROCDESC, EVFILT_READ, EVFILT_SIGNAL, EVFILT_TIMER, EVFILT_USER,
    EVFILT_VNODE, EVFILT_WRITE, Kevent, NOTE_CHILD, NOTE_EXEC, NOTE_EXIT, NOTE_FILE_POLL,
    NOTE_FORK, NOTE_PCTRLMASK, NOTE_PDSIGCHLD, NOTE_REAP, NOTE_REVOKE, NOTE_SIGNAL, NOTE_TRACK,
    NOTE_TRACKERR,
};
use event::{Filter, Ident, Knote, Kqueue, Note, Notify, Source, Wake};
use sched::{Sleepable, WaitQueue};

use crate::fs::file::File;
use crate::proc::Process;

/// A thread in `poll` or `select` (FreeBSD's `seltd`): registered on the
/// objects it polls, and woken by the first that changes.
pub(crate) struct Poller {
    fired: AtomicBool,
    queue: WaitQueue,
}

impl Wake for Poller {
    fn wake(&self) {
        self.fired.store(true, Ordering::SeqCst);
        self.queue.wake_all();
    }
}

impl Poller {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            fired: AtomicBool::new(false),
            queue: WaitQueue::new(),
        })
    }

    /// Before a scan: what wakes us from now on counts.
    pub(crate) fn arm(&self) {
        self.fired.store(false, Ordering::SeqCst);
    }

    /// Sleeps until an object it polls changes, `ns` pass, or a signal
    /// must be handled (`seltdwait`).
    pub(crate) fn wait(
        &self,
        s: &Sleepable,
        ns: Option<u64>,
    ) -> Result<crate::signal::Woke, Errno> {
        crate::signal::sleep(s, &self.queue, || self.fired.load(Ordering::SeqCst), ns)
    }
}

/// A kqueue's scanners.
struct Scanners(Arc<WaitQueue>);

impl Wake for Scanners {
    fn wake(&self) {
        self.0.wake_all();
    }
}

/// An open kqueue (`DTYPE_KQUEUE`): closing its last descriptor drops its
/// knotes.
pub(crate) struct KqueueFile {
    pub(crate) kq: Arc<Kqueue>,
    pub(crate) wait: Arc<WaitQueue>,
    /// `KQUEUE_CPONFORK`: a forked child gets a copy.
    pub(crate) cponfork: bool,
    /// Its count against its maker's `RLIMIT_KQUEUES`.
    _charge: Option<crate::uidinfo::Charge>,
}

impl KqueueFile {
    pub(crate) fn new(cponfork: bool) -> Self {
        let wait = Arc::new(WaitQueue::new());
        Self {
            kq: Kqueue::new(Box::new(Scanners(Arc::clone(&wait)))),
            wait,
            cponfork,
            _charge: None,
        }
    }

    /// A new kqueue counted against the caller's `RLIMIT_KQUEUES`
    /// (`kern_kqueue`'s `chgkqcnt`): `ENOMEM` past it.
    pub(crate) fn charged(cponfork: bool) -> Result<Self, abi::errno::Errno> {
        let charge =
            crate::uidinfo::Charge::take(crate::uidinfo::Kind::Kqueues, abi::errno::Errno::ENOMEM)?;
        let mut k = Self::new(cponfork);
        k._charge = Some(charge);
        Ok(k)
    }

    /// `kqueue_poll`.
    pub(crate) fn poll(&self, events: i16, w: Option<&Arc<dyn Wake>>) -> i16 {
        use abi::event::{POLLIN, POLLRDNORM};
        if events & (POLLIN | POLLRDNORM) == 0 {
            return 0;
        }
        if let Some(w) = w {
            self.kq.sel.record(w);
        }
        if self.kq.pending() > 0 {
            events & (POLLIN | POLLRDNORM)
        } else {
            0
        }
    }
}

impl Drop for KqueueFile {
    fn drop(&mut self) {
        self.kq.close();
    }
}

/// What a process's `kevent` changes reach: its descriptors, and the rest
/// of the system.
pub(crate) struct Changes<'a> {
    pub(crate) s: &'a Sleepable,
    pub(crate) p: &'a Process,
    /// The kqueue changed, which cannot watch itself.
    pub(crate) kq: &'a Arc<Kqueue>,
}

impl Changes<'_> {
    /// The file a descriptor filter watches, which must have `CAP_EVENT`.
    fn file(&self, kev: &Kevent) -> Result<Arc<File>, Errno> {
        let fd = i32::try_from(kev.ident).map_err(|_| Errno::EBADF)?;
        self.p
            .files
            .lock(self.s)
            .get_checked(fd, &crate::capsicum::rights::EVENT)
    }
}

impl Source for Changes<'_> {
    fn ident(&self, kev: &Kevent) -> Result<Ident, Errno> {
        match kev.filter {
            EVFILT_READ | EVFILT_WRITE | EVFILT_EMPTY | EVFILT_VNODE | EVFILT_PROCDESC => {
                self.file(kev)?;
                Ok(Ident::Fd)
            }
            EVFILT_PROC | EVFILT_SIGNAL | EVFILT_TIMER | EVFILT_USER | EVFILT_FS => {
                Ok(Ident::Other)
            }
            _ => Err(Errno::EINVAL),
        }
    }

    fn filter(&self, kev: &Kevent) -> Result<Box<dyn Filter>, Errno> {
        match kev.filter {
            EVFILT_PROC => proc_filter(kev),
            EVFILT_SIGNAL => {
                let sig = u32::try_from(kev.ident).map_err(|_| Errno::EINVAL)?;
                if !(1..=abi::signal::SIG_MAXSIG).contains(&sig) {
                    return Err(Errno::EINVAL);
                }
                let p = crate::proc::find(self.p.pid()).ok_or(Errno::ESRCH)?;
                Ok(Box::new(SignalFilter(Arc::downgrade(&p))))
            }
            EVFILT_TIMER => crate::timers::kevent_timer(kev),
            EVFILT_USER => Ok(Box::new(event::UserFilter)),
            EVFILT_FS => Ok(Box::new(FsFilter)),
            _ => {
                let f = self.file(kev)?;
                if let crate::fs::file::FileKind::Kqueue(k) = f.kind()
                    && Arc::ptr_eq(&k.kq, self.kq)
                {
                    return Err(Errno::EINVAL);
                }
                File::kqfilter(&f, self.s, kev)
            }
        }
    }
}

/// A filter that never fires, for a filter a descriptor is not open for
/// (`pipe_nfiltops`).
pub(crate) struct Never;

impl Filter for Never {
    fn event(&self, _kn: &Knote, _note: &mut Note, _hint: u64) -> bool {
        false
    }

    fn fork(&self, _kn: &Knote) -> Option<Box<dyn Filter>> {
        Some(Box::new(Self))
    }
}

/// `EVFILT_READ` on a vnode (`filt_vfsread`): the bytes from the file's
/// offset to its end; with `NOTE_FILE_POLL`, always, as `poll` would be.
pub(crate) struct VnodeRead(pub(crate) Arc<File>);

/// `EVFILT_WRITE` on a vnode (`filt_vfswrite`): always.
pub(crate) struct VnodeWrite(pub(crate) Arc<File>);

/// `EVFILT_VNODE` (`filt_vfsvnode`): the `NOTE_*` asked for, as the VFS
/// raises them.
pub(crate) struct VnodeNotes(pub(crate) Arc<File>);

fn vnode_of(f: &File) -> &Arc<vfs::Vnode> {
    f.vnode()
        .unwrap_or_else(|| unreachable!("a vnode filter on a vnode file"))
}

macro_rules! vnode_attach {
    () => {
        fn attach(&self, kn: &Arc<Knote>) -> Result<(), Errno> {
            vnode_of(&self.0).notify().add(kn);
            Ok(())
        }

        fn detach(&self, kn: &Knote) {
            vnode_of(&self.0).notify().remove(kn);
        }

        fn fork(&self, _kn: &Knote) -> Option<Box<dyn Filter>> {
            Some(Box::new(Self(Arc::clone(&self.0))))
        }
    };
}

impl Filter for VnodeRead {
    vnode_attach!();

    fn event(&self, _kn: &Knote, note: &mut Note, hint: u64) -> bool {
        if hint == u64::from(NOTE_REVOKE) {
            note.kev.flags |= EV_EOF | EV_ONESHOT;
            return true;
        }
        // The size as last set; the offset as the file last left it
        // (both may move meanwhile, as FreeBSD's unlocked reads may).
        let size = vnode_of(&self.0).cached_size();
        let off = self.0.offset_hint();
        note.kev.data =
            i64::try_from(size).unwrap_or(i64::MAX) - i64::try_from(off).unwrap_or(i64::MAX);
        note.sfflags & NOTE_FILE_POLL != 0 || note.kev.data != 0
    }
}

impl Filter for VnodeWrite {
    vnode_attach!();

    fn event(&self, _kn: &Knote, note: &mut Note, hint: u64) -> bool {
        if hint == u64::from(NOTE_REVOKE) {
            note.kev.flags |= EV_EOF | EV_ONESHOT;
        }
        note.kev.data = 0;
        true
    }
}

impl Filter for VnodeNotes {
    vnode_attach!();

    fn event(&self, _kn: &Knote, note: &mut Note, hint: u64) -> bool {
        let hint = u32::try_from(hint).unwrap_or(0);
        if note.sfflags & hint != 0 {
            note.kev.fflags |= hint;
        }
        if hint == NOTE_REVOKE {
            note.kev.flags |= EV_EOF;
            return true;
        }
        note.kev.fflags != 0
    }
}

/// The filter a vnode file gives (`vfs_kqfilter`).
pub(crate) fn vnode_filter(
    f: &Arc<File>,
    s: &Sleepable,
    kev: &Kevent,
) -> Result<Box<dyn Filter>, Errno> {
    match kev.filter {
        EVFILT_READ => {
            vnode_of(f).watch_size(s);
            Ok(Box::new(VnodeRead(Arc::clone(f))))
        }
        EVFILT_WRITE => Ok(Box::new(VnodeWrite(Arc::clone(f)))),
        EVFILT_VNODE => Ok(Box::new(VnodeNotes(Arc::clone(f)))),
        _ => Err(Errno::EINVAL),
    }
}

/// What a process event does to a note (`filt_proc` and
/// `procdesc_kqops_event`): the events asked for are recorded, an exit
/// reports the wait status, and an exit (or, if asked for, the reaping)
/// ends the note, dropped unreported if it saw nothing it asked for.
/// Returns whether the note is active and whether the process is done.
fn process_event(note: &mut Note, event: u32, p: Option<&Process>) -> (bool, bool) {
    if note.sfflags & event != 0 {
        note.kev.fflags |= note.sfflags & event;
    }
    if note.kev.fflags & NOTE_EXIT != 0
        && let Some(p) = p
    {
        note.kev.data = i64::from(p.wait_status());
    }
    if event & NOTE_REAP != 0 || (event & NOTE_EXIT != 0 && note.sfflags & NOTE_REAP == 0) {
        note.kev.flags |= EV_EOF | EV_ONESHOT;
        if note.kev.fflags == 0 {
            note.kev.flags |= EV_DROP;
        }
        return (true, true);
    }
    (note.kev.fflags != 0, false)
}

/// `EVFILT_PROC` (`proc_filtops`): a process's exit (`data` its status),
/// fork, exec, and reaping; with `NOTE_TRACK`, its children are watched
/// too. Always `EV_CLEAR`.
pub(crate) struct ProcFilter {
    p: Weak<Process>,
    /// It exited or was reaped: nothing more comes (`kn_ptr.p_proc`
    /// cleared).
    gone: AtomicBool,
}

/// `filt_procattach`'s lookup: a zombie only for a note that asks for its
/// exit or reaping.
fn proc_filter(kev: &Kevent) -> Result<Box<dyn Filter>, Errno> {
    let pid = i32::try_from(kev.ident).map_err(|_| Errno::ESRCH)?;
    let p = crate::proc::find(pid).ok_or(Errno::ESRCH)?;
    if p.is_zombie() && kev.fflags & (NOTE_EXIT | NOTE_REAP) == 0 {
        return Err(Errno::ESRCH);
    }
    Ok(Box::new(ProcFilter {
        p: Arc::downgrade(&p),
        gone: AtomicBool::new(false),
    }))
}

impl Filter for ProcFilter {
    fn attach(&self, kn: &Arc<Knote>) -> Result<(), Errno> {
        let p = self.p.upgrade().ok_or(Errno::ESRCH)?;
        kn.trigger(|n| {
            n.kev.flags |= EV_CLEAR;
            // A child a tracking note follows: reported at once.
            let child = n.kev.flags & EV_FLAG2 != 0;
            if child {
                n.kev.flags &= !EV_FLAG2;
                n.kev.data = n.sdata;
                n.kev.fflags = NOTE_CHILD;
                n.sfflags &= !NOTE_PCTRLMASK;
            }
            n.kev.flags &= !EV_FLAG1;
            child
        });
        p.klist.add(kn);
        // Already exiting: its exit is reported now.
        if p.has_exited() {
            kn.trigger(|n| {
                let (active, done) = process_event(n, NOTE_EXIT, Some(&p));
                if done {
                    self.gone.store(true, Ordering::Relaxed);
                }
                active
            });
        }
        Ok(())
    }

    fn detach(&self, kn: &Knote) {
        if let Some(p) = self.p.upgrade() {
            p.klist.remove(kn);
        }
    }

    fn event(&self, _kn: &Knote, note: &mut Note, hint: u64) -> bool {
        if self.gone.load(Ordering::Relaxed) {
            return false;
        }
        let event = u32::try_from(hint).unwrap_or(0) & NOTE_PCTRLMASK;
        let p = self.p.upgrade();
        let (active, done) = process_event(note, event, p.as_deref());
        if done {
            self.gone.store(true, Ordering::Relaxed);
        }
        active
    }

    fn fork(&self, _kn: &Knote) -> Option<Box<dyn Filter>> {
        Some(Box::new(Self {
            p: Weak::clone(&self.p),
            gone: AtomicBool::new(self.gone.load(Ordering::Relaxed)),
        }))
    }
}

/// `knote_fork`: process `parent` forked `child`. Its notes see
/// `NOTE_FORK`; those with `NOTE_TRACK` also register two notes on the
/// child: one reporting `NOTE_CHILD` (with the parent's pid) at once, and
/// one for what the child does next.
pub(crate) fn proc_forked(parent: &Process, child: i32) {
    for kn in parent.klist.knotes_where(|n| n.sfflags & NOTE_TRACK != 0) {
        let Some(kq) = kn.kqueue() else {
            continue;
        };
        let note = kn.note();
        let mut kev = Kevent {
            ident: u64::from(child.cast_unsigned()),
            filter: EVFILT_PROC,
            flags: note.kev.flags | EV_ADD | EV_ENABLE | EV_ONESHOT | EV_FLAG2,
            fflags: note.sfflags,
            data: i64::try_from(kn.ident).unwrap_or(0),
            udata: note.kev.udata,
            ext: [0; 4],
        };
        let first = kq.register(&kev, &Tracking);
        kev.flags = note.kev.flags | EV_ADD | EV_ENABLE | EV_FLAG1;
        let second = kq.register(&kev, &Tracking);
        if first.is_err() || second.is_err() {
            kn.trigger(|n| {
                n.kev.fflags |= NOTE_TRACKERR;
                true
            });
        }
    }
    parent.klist.knote(u64::from(NOTE_FORK));
    parent.pd_sel.knote(u64::from(NOTE_FORK));
}

/// The source of `knote_fork`'s child notes.
struct Tracking;

impl Source for Tracking {
    fn ident(&self, _kev: &Kevent) -> Result<Ident, Errno> {
        Ok(Ident::Other)
    }

    fn filter(&self, kev: &Kevent) -> Result<Box<dyn Filter>, Errno> {
        proc_filter(kev)
    }
}

/// `NOTE_EXEC` for `p`.
pub(crate) fn proc_execed(p: &Process) {
    p.klist.knote(u64::from(NOTE_EXEC));
}

/// `NOTE_EXIT` for `p`, once its status is set (`exit1`), and for its
/// process descriptor (`procdesc_exit`), whose pollers see it hang up.
pub(crate) fn proc_exited(p: &Process) {
    p.klist.knote(u64::from(NOTE_EXIT));
    p.pd_sel.notify(u64::from(NOTE_EXIT | NOTE_PDSIGCHLD));
}

/// `NOTE_REAP` for `p`, reaped (`proc_reap`); nothing more comes.
pub(crate) fn proc_reaped(p: &Process) {
    p.klist.knote(u64::from(NOTE_REAP));
    p.klist.clear();
    p.pd_sel.knote(u64::from(NOTE_REAP));
}

/// `EVFILT_SIGNAL` (`sig_filtops`): how many times the signal was sent to
/// the process since last reported, caught, ignored, or blocked alike.
pub(crate) struct SignalFilter(Weak<Process>);

impl Filter for SignalFilter {
    fn attach(&self, kn: &Arc<Knote>) -> Result<(), Errno> {
        let p = self.0.upgrade().ok_or(Errno::ESRCH)?;
        kn.trigger(|n| {
            n.kev.flags |= EV_CLEAR;
            false
        });
        p.klist.add(kn);
        Ok(())
    }

    fn detach(&self, kn: &Knote) {
        if let Some(p) = self.0.upgrade() {
            p.klist.remove(kn);
        }
    }

    fn event(&self, kn: &Knote, note: &mut Note, hint: u64) -> bool {
        let hint = u32::try_from(hint).unwrap_or(0);
        if hint & NOTE_SIGNAL != 0 && u64::from(hint & !NOTE_SIGNAL) == kn.ident {
            note.kev.data += 1;
        }
        note.kev.data != 0
    }

    fn fork(&self, _kn: &Knote) -> Option<Box<dyn Filter>> {
        Some(Box::new(Self(Weak::clone(&self.0))))
    }
}

/// Signal `sig` was sent to `p` (`KNOTE(&p->p_klist, NOTE_SIGNAL | sig)`).
pub(crate) fn signal_sent(p: &Process, sig: u32) {
    p.klist.knote(u64::from(NOTE_SIGNAL | sig));
}

/// `EVFILT_FS` (`fs_filtops`): mounts and unmounts.
struct FsFilter;

/// The knotes watching file systems (`fs_knlist`).
pub(crate) static FS_NOTES: Notify = Notify::new();

impl Filter for FsFilter {
    fn attach(&self, kn: &Arc<Knote>) -> Result<(), Errno> {
        FS_NOTES.add(kn);
        Ok(())
    }

    fn detach(&self, kn: &Knote) {
        FS_NOTES.remove(kn);
    }

    fn event(&self, _kn: &Knote, note: &mut Note, hint: u64) -> bool {
        note.kev.fflags |= u32::try_from(hint).unwrap_or(0);
        note.kev.fflags != 0
    }

    fn fork(&self, _kn: &Knote) -> Option<Box<dyn Filter>> {
        Some(Box::new(Self))
    }
}

/// `EVFILT_PROCDESC` (`procdesc_kqops`): the described process's exit,
/// forks, and reaping.
pub(crate) struct ProcdescFilter(pub(crate) Arc<File>);

impl ProcdescFilter {
    fn process(&self) -> &Arc<Process> {
        match self.0.kind() {
            crate::fs::file::FileKind::Process(pd) => pd.process(),
            _ => unreachable!("a process descriptor filter on a process descriptor"),
        }
    }
}

impl Filter for ProcdescFilter {
    fn attach(&self, kn: &Arc<Knote>) -> Result<(), Errno> {
        self.process().pd_sel.add(kn);
        Ok(())
    }

    fn detach(&self, kn: &Knote) {
        self.process().pd_sel.remove(kn);
    }

    fn event(&self, _kn: &Knote, note: &mut Note, hint: u64) -> bool {
        let p = self.process();
        // The check after registration: an exit already happened.
        let event = if hint == 0 {
            if p.has_exited() {
                NOTE_EXIT | NOTE_PDSIGCHLD
            } else {
                0
            }
        } else {
            u32::try_from(hint).unwrap_or(0) & NOTE_PCTRLMASK
        };
        let (active, done) = process_event(note, event, Some(p));
        if !done && note.kev.fflags & NOTE_FORK != 0 {
            note.kev.data = i64::from(p.last_child());
        }
        active
    }

    fn fork(&self, _kn: &Knote) -> Option<Box<dyn Filter>> {
        Some(Box::new(Self(Arc::clone(&self.0))))
    }
}
