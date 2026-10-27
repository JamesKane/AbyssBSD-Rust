// SPDX-License-Identifier: BSD-2-Clause
//! System V IPC: shared memory segments, semaphore sets, and message
//! queues, each named by an id (a slot index and the slot's sequence
//! number, so a stale id finds nothing) and optionally a key, with
//! FreeBSD's `kern.ipc` limits.
//!
//! A shared memory segment is an anonymous VM object; `shmat` maps it
//! shared, and each address space records what it attached, which a fork
//! copies and an exit or exec detaches. A segment removed while attached
//! lives on, keyless, until the last detach. Semaphore operations apply
//! all or none; `SEM_UNDO` records the opposite adjustment per process,
//! applied when it exits. Messages are kept whole, charged against the
//! queue's byte limit and the system's segments and headers as FreeBSD's
//! message pool is.
//!
//! Every object lives under one lock per kind, never held across user
//! memory: arguments are copied in first, results out after.
//!
//! Behavior reference: sys/kern/sysv_ipc.c, sys/kern/sysv_shm.c,
//! sys/kern/sysv_sem.c, sys/kern/sysv_msg.c.

use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use abi::errno::Errno;
use abi::ipc::*;
use sched::{Sleepable, WaitQueue};
use sync::PreemptLock;
use vm::PAGE_SIZE;
use vm::object::Object;

use crate::cred::Ucred;

/// FreeBSD's default limits (`kern.ipc.*`).
pub(crate) mod limits {
    pub(crate) const SHMMAX: u64 = 131_072 * vm::PAGE_SIZE;
    pub(crate) const SHMMIN: u64 = 1;
    pub(crate) const SHMMNI: usize = 192;
    pub(crate) const SHMSEG: usize = 128;
    /// In pages.
    pub(crate) const SHMALL: u64 = 131_072;
    pub(crate) const SEMMNI: usize = 50;
    pub(crate) const SEMMNS: usize = 340;
    pub(crate) const SEMMNU: usize = 150;
    pub(crate) const SEMMSL: usize = SEMMNS;
    pub(crate) const SEMOPM: usize = 100;
    pub(crate) const SEMUME: usize = 50;
    pub(crate) const SEMVMX: i32 = 32767;
    pub(crate) const SEMAEM: i32 = 16384;
    pub(crate) const MSGSSZ: usize = 8;
    pub(crate) const MSGSEG: usize = 2048;
    pub(crate) const MSGMAX: usize = MSGSSZ * MSGSEG;
    pub(crate) const MSGMNB: u64 = 2048;
    pub(crate) const MSGMNI: usize = 40;
    pub(crate) const MSGTQL: usize = 40;
}
use limits::*;

fn now() -> i64 {
    crate::fs::now().tv_sec
}

/// `ipcperm`: the owner or creator has the mode's owner bits (and may
/// administer, `IPC_M`), the group the group bits, others the rest; the
/// superuser anything.
fn ipcperm(p: &IpcPerm, cred: &Ucred, want: u16) -> Result<(), Errno> {
    let (bits, mut granted) = if cred.uid == p.cuid || cred.uid == p.uid {
        (p.mode, IPC_M)
    } else if cred.in_group(p.gid) || cred.in_group(p.cgid) {
        (p.mode << 3, 0)
    } else {
        (p.mode << 6, 0)
    };
    granted |= bits & (IPC_R | IPC_W);
    if granted & want == want || cred.is_root() {
        Ok(())
    } else if want & IPC_M != 0 {
        Err(Errno::EPERM)
    } else {
        Err(Errno::EACCES)
    }
}

fn new_perm(key: i64, mode: u32, cred: &Ucred) -> IpcPerm {
    IpcPerm {
        cuid: cred.uid,
        cgid: cred.gid,
        uid: cred.uid,
        gid: cred.gid,
        #[allow(clippy::cast_possible_truncation, reason = "the low nine bits")]
        mode: (mode & 0o777) as u16,
        key,
        ..IpcPerm::default()
    }
}

/// `IPC_SET`: the new owner and permission bits.
fn set_perm(p: &mut IpcPerm, new: &IpcPerm) {
    p.uid = new.uid;
    p.gid = new.gid;
    p.mode = (p.mode & !0o777) | (new.mode & 0o777);
}

/// The slot an id names.
fn slot(id: i32) -> usize {
    (id.cast_unsigned() & 0xffff) as usize
}

/// The sequence number in an id.
fn seq_of(id: i32) -> u16 {
    u16::try_from(id.cast_unsigned() >> 16).unwrap_or(u16::MAX)
}

/// Fixed slots of objects, ids `seq << 16 | slot`.
struct Table<T> {
    slots: Vec<(u16, Option<T>)>,
}

impl<T> Table<T> {
    const fn new() -> Self {
        Self { slots: Vec::new() }
    }

    fn ensure(&mut self, n: usize) {
        while self.slots.len() < n {
            self.slots.push((0, None));
        }
    }

    /// The live object `id` names.
    fn get(&mut self, id: i32) -> Option<&mut T> {
        let (ix, seq) = (slot(id), seq_of(id));
        match self.slots.get_mut(ix) {
            Some((s, Some(t))) if *s == seq => Some(t),
            _ => None,
        }
    }

    /// Puts `t` in a free slot, the lowest; returns its id.
    fn insert(&mut self, cap: usize, t: T) -> Result<i32, Errno> {
        self.ensure(cap);
        let ix = self.slots[..cap]
            .iter()
            .position(|(_, x)| x.is_none())
            .ok_or(Errno::ENOSPC)?;
        let slot = &mut self.slots[ix];
        slot.0 = (slot.0 + 1) & 0x7fff;
        slot.1 = Some(t);
        #[allow(
            clippy::cast_possible_wrap,
            clippy::cast_possible_truncation,
            reason = "below 2^31"
        )]
        Ok((i32::from(slot.0) << 16) | ix as i32)
    }

    fn remove(&mut self, id: i32) -> Option<T> {
        let ix = slot(id);
        self.slots.get_mut(ix)?.1.take()
    }

    fn iter(&self) -> impl Iterator<Item = (i32, &T)> {
        self.slots.iter().enumerate().filter_map(|(ix, (seq, t))| {
            #[allow(
                clippy::cast_possible_wrap,
                clippy::cast_possible_truncation,
                reason = "below 2^31"
            )]
            t.as_ref().map(|t| ((i32::from(*seq) << 16) | ix as i32, t))
        })
    }

    fn count(&self) -> usize {
        self.slots.iter().filter(|(_, t)| t.is_some()).count()
    }
}

/// Sleepers on an object, and a count each change bumps.
struct Waiters {
    wq: WaitQueue,
    epoch: AtomicU64,
}

impl Waiters {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            wq: WaitQueue::new(),
            epoch: AtomicU64::new(0),
        })
    }

    fn wake(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.wq.wake_all();
    }

    /// Sleeps until the next [`wake`](Self::wake) after `epoch`, a signal
    /// (`EINTR`), or `limit` nanoseconds.
    fn sleep(&self, s: &Sleepable, epoch: u64, limit: Option<u64>) -> Result<bool, Errno> {
        match crate::signal::sleep(
            s,
            &self.wq,
            || self.epoch.load(Ordering::Acquire) != epoch,
            limit,
        ) {
            Ok(crate::signal::Woke::Done) => Ok(true),
            Ok(crate::signal::Woke::TimedOut) => Ok(false),
            Err(_) => Err(Errno::EINTR),
        }
    }
}

// ---- Shared memory ----

struct Segment {
    ds: ShmidDs,
    object: Arc<Object>,
    /// `IPC_RMID` came while attached (`SHMSEG_REMOVED`).
    removed: bool,
}

struct ShmState {
    segs: Table<Segment>,
    /// Pages of segments in use (`shm_committed`).
    committed: u64,
}

static SHM: PreemptLock<ShmState> = PreemptLock::new(ShmState {
    segs: Table::new(),
    committed: 0,
});

/// What an address space attached: where, and which segment.
pub(crate) type Attached = Vec<(u64, i32)>;

fn shm_drop_attach(st: &mut ShmState, id: i32) {
    let Some(seg) = st.segs.get(id) else {
        return;
    };
    seg.ds.shm_nattch -= 1;
    seg.ds.shm_dtime = now();
    if seg.ds.shm_nattch == 0 && seg.removed {
        let pages = seg.ds.shm_segsz.div_ceil(PAGE_SIZE);
        st.committed -= pages;
        st.segs.remove(id);
    }
}

/// A fork copied `attached` into the child.
pub(crate) fn shm_forked(attached: &Attached) {
    let mut st = SHM.lock();
    for &(_, id) in attached {
        if let Some(seg) = st.segs.get(id) {
            seg.ds.shm_nattch += 1;
        }
    }
}

/// An address space went (`shmexit`): its segments are detached.
pub(crate) fn shm_exited(attached: Attached) {
    let mut st = SHM.lock();
    for (_, id) in attached {
        shm_drop_attach(&mut st, id);
    }
}

/// `shmget(key, size, shmflg)`.
pub(crate) fn shmget(key: i64, size: u64, flg: u32, cred: &Ucred, pid: i32) -> Result<i32, Errno> {
    let mut st = SHM.lock();
    st.segs.ensure(SHMMNI);
    if key != IPC_PRIVATE {
        let found = st
            .segs
            .iter()
            .find(|(_, s)| !s.removed && s.ds.shm_perm.key == key)
            .map(|(id, s)| (id, s.ds.shm_segsz));
        if let Some((id, segsz)) = found {
            if flg & (IPC_CREAT | IPC_EXCL) == IPC_CREAT | IPC_EXCL {
                return Err(Errno::EEXIST);
            }
            if size != 0 && size > segsz {
                return Err(Errno::EINVAL);
            }
            return Ok(id);
        }
        if flg & IPC_CREAT == 0 {
            return Err(Errno::ENOENT);
        }
    }
    if !(SHMMIN..=SHMMAX).contains(&size) {
        return Err(Errno::EINVAL);
    }
    if st.segs.count() >= SHMMNI {
        return Err(Errno::ENOSPC);
    }
    let size = size.next_multiple_of(PAGE_SIZE);
    let pages = size / PAGE_SIZE;
    if st.committed + pages > SHMALL {
        return Err(Errno::ENOMEM);
    }
    let mut ds = ShmidDs {
        shm_perm: new_perm(key, flg, cred),
        shm_segsz: size,
        shm_cpid: pid,
        shm_ctime: now(),
        ..ShmidDs::default()
    };
    let id = st.segs.insert(
        SHMMNI,
        Segment {
            ds,
            object: Object::anonymous(crate::mm::space::page_ops()),
            removed: false,
        },
    )?;
    ds.shm_perm.seq = seq_of(id);
    if let Some(seg) = st.segs.get(id) {
        seg.ds.shm_perm.seq = ds.shm_perm.seq;
    }
    st.committed += pages;
    Ok(id)
}

/// `shmat`'s check and reservation: the segment's object and size, with
/// the attach counted (undone by [`shm_unattach`] if the mapping fails).
pub(crate) fn shm_attach(
    id: i32,
    rdonly: bool,
    attached: usize,
    cred: &Ucred,
    pid: i32,
) -> Result<(Arc<Object>, u64), Errno> {
    let mut st = SHM.lock();
    let seg = st.segs.get(id).ok_or(Errno::EINVAL)?;
    let want = if rdonly { IPC_R } else { IPC_R | IPC_W };
    ipcperm(&seg.ds.shm_perm, cred, want)?;
    if attached >= SHMSEG {
        return Err(Errno::EMFILE);
    }
    seg.ds.shm_lpid = pid;
    seg.ds.shm_atime = now();
    seg.ds.shm_nattch += 1;
    Ok((Arc::clone(&seg.object), seg.ds.shm_segsz))
}

/// Undoes [`shm_attach`] after a failed mapping, or detaches (`shmdt`).
pub(crate) fn shm_unattach(id: i32) {
    shm_drop_attach(&mut SHM.lock(), id);
}

/// The size of segment `id`, attached or removed.
pub(crate) fn shm_size(id: i32) -> Option<u64> {
    SHM.lock().segs.get(id).map(|s| s.ds.shm_segsz)
}

/// `shmctl(shmid, cmd, buf)`: `IPC_STAT`'s result.
pub(crate) fn shmctl(
    id: i32,
    cmd: u32,
    set: Option<&ShmidDs>,
    cred: &Ucred,
) -> Result<Option<ShmidDs>, Errno> {
    let mut st = SHM.lock();
    let seg = st.segs.get(id).ok_or(Errno::EINVAL)?;
    match cmd {
        IPC_STAT => {
            ipcperm(&seg.ds.shm_perm, cred, IPC_R)?;
            Ok(Some(seg.ds))
        }
        IPC_SET => {
            ipcperm(&seg.ds.shm_perm, cred, IPC_M)?;
            if let Some(new) = set {
                set_perm(&mut seg.ds.shm_perm, &new.shm_perm);
            }
            seg.ds.shm_ctime = now();
            Ok(None)
        }
        IPC_RMID => {
            ipcperm(&seg.ds.shm_perm, cred, IPC_M)?;
            seg.ds.shm_perm.key = IPC_PRIVATE;
            seg.removed = true;
            if seg.ds.shm_nattch == 0 {
                let pages = seg.ds.shm_segsz.div_ceil(PAGE_SIZE);
                st.committed -= pages;
                st.segs.remove(id);
            }
            Ok(None)
        }
        _ => Err(Errno::EINVAL),
    }
}

// ---- Semaphores ----

#[derive(Clone, Copy, Default)]
struct Sem {
    val: i32,
    pid: i32,
    ncnt: u16,
    zcnt: u16,
}

struct SemSet {
    ds: SemidDs,
    sems: Vec<Sem>,
    waiters: Arc<Waiters>,
}

/// One undo entry: a process's adjustment to one semaphore.
struct Undo {
    pid: i32,
    set: i32,
    num: u16,
    adj: i32,
}

struct SemState {
    sets: Table<SemSet>,
    /// Semaphores in all sets (`semtot`).
    total: usize,
    undo: Vec<Undo>,
}

static SEM: PreemptLock<SemState> = PreemptLock::new(SemState {
    sets: Table::new(),
    total: 0,
    undo: Vec::new(),
});

fn sem_remove(st: &mut SemState, id: i32) {
    if let Some(set) = st.sets.remove(id) {
        st.total -= set.sems.len();
        st.undo.retain(|u| u.set != id);
        set.waiters.wake();
    }
}

/// `semget(key, nsems, semflg)`.
pub(crate) fn semget(key: i64, nsems: i32, flg: u32, cred: &Ucred) -> Result<i32, Errno> {
    let mut st = SEM.lock();
    st.sets.ensure(SEMMNI);
    if key != IPC_PRIVATE {
        let found = st
            .sets
            .iter()
            .find(|(_, s)| s.ds.sem_perm.key == key)
            .map(|(id, s)| (id, s.ds.sem_perm, s.sems.len()));
        if let Some((id, perm, n)) = found {
            if flg & (IPC_CREAT | IPC_EXCL) == IPC_CREAT | IPC_EXCL {
                return Err(Errno::EEXIST);
            }
            #[allow(clippy::cast_possible_truncation, reason = "the owner bits")]
            ipcperm(&perm, cred, (flg & 0o700) as u16)?;
            if nsems > 0 && n < nsems.cast_unsigned() as usize {
                return Err(Errno::EINVAL);
            }
            return Ok(id);
        }
        if flg & IPC_CREAT == 0 {
            return Err(Errno::ENOENT);
        }
    }
    let n = usize::try_from(nsems).map_err(|_| Errno::EINVAL)?;
    if n == 0 || n > SEMMSL {
        return Err(Errno::EINVAL);
    }
    if n > SEMMNS - st.total {
        return Err(Errno::ENOSPC);
    }
    let id = st.sets.insert(
        SEMMNI,
        SemSet {
            ds: SemidDs {
                sem_perm: new_perm(key, flg, cred),
                #[allow(clippy::cast_possible_truncation, reason = "at most SEMMSL")]
                sem_nsems: n as u16,
                sem_ctime: now(),
                ..SemidDs::default()
            },
            sems: alloc::vec![Sem::default(); n],
            waiters: Waiters::new(),
        },
    )?;
    if let Some(set) = st.sets.get(id) {
        set.ds.sem_perm.seq = seq_of(id);
    }
    st.total += n;
    Ok(id)
}

/// Adds `adj` to process `pid`'s undo for semaphore `num` of `set`
/// (`semundo_adjust`).
fn undo_adjust(st: &mut SemState, pid: i32, set: i32, num: u16, adj: i32) -> Result<(), Errno> {
    if let Some(i) = st
        .undo
        .iter()
        .position(|u| u.pid == pid && u.set == set && u.num == num)
    {
        let new = st.undo[i].adj + adj;
        if !(-SEMAEM..=SEMAEM).contains(&new) {
            return Err(Errno::ERANGE);
        }
        if new == 0 {
            st.undo.swap_remove(i);
        } else {
            st.undo[i].adj = new;
        }
        return Ok(());
    }
    if adj == 0 {
        return Ok(());
    }
    if st.undo.iter().filter(|u| u.pid == pid).count() >= SEMUME {
        return Err(Errno::EINVAL);
    }
    let procs = {
        let mut p: Vec<i32> = st.undo.iter().map(|u| u.pid).collect();
        p.sort_unstable();
        p.dedup();
        p
    };
    if !procs.contains(&pid) && procs.len() >= SEMMNU {
        return Err(Errno::ENOSPC);
    }
    if !(-SEMAEM..=SEMAEM).contains(&adj) {
        return Err(Errno::ERANGE);
    }
    st.undo.push(Undo { pid, set, num, adj });
    Ok(())
}

/// `semop(semid, sops, nsops)`, the operations copied in: all at once, or
/// sleeping until they can be.
pub(crate) fn semop(
    s: &Sleepable,
    id: i32,
    ops: &[Sembuf],
    cred: &Ucred,
    pid: i32,
) -> Result<(), Errno> {
    let mut st = SEM.lock();
    let set = st.sets.get(id).ok_or(Errno::EINVAL)?;
    let mut want = 0;
    let mut undos = false;
    for op in ops {
        if usize::from(op.sem_num) >= set.sems.len() {
            return Err(Errno::EFBIG);
        }
        if op.sem_flg & SEM_UNDO != 0 && op.sem_op != 0 {
            undos = true;
        }
        want |= if op.sem_op == 0 { IPC_R } else { IPC_W };
    }
    ipcperm(&set.ds.sem_perm, cred, want)?;
    loop {
        let set = st.sets.get(id).ok_or(Errno::EIDRM)?;
        let mut wake = false;
        let mut blocked = None;
        let mut error = None;
        let mut done = 0;
        for (i, op) in ops.iter().enumerate() {
            let sem = &mut set.sems[usize::from(op.sem_num)];
            let delta = i32::from(op.sem_op);
            if delta < 0 {
                if sem.val + delta < 0 {
                    blocked = Some(i);
                    break;
                }
                sem.val += delta;
                if sem.val == 0 && sem.zcnt > 0 {
                    wake = true;
                }
            } else if delta == 0 {
                if sem.val != 0 {
                    blocked = Some(i);
                    break;
                }
            } else if sem.val + delta > SEMVMX {
                error = Some(Errno::ERANGE);
                break;
            } else {
                if sem.ncnt > 0 {
                    wake = true;
                }
                sem.val += delta;
            }
            done = i + 1;
        }
        if done == ops.len() {
            if undos {
                let mut applied = 0;
                let mut failed = None;
                for op in ops {
                    if op.sem_flg & SEM_UNDO != 0
                        && op.sem_op != 0
                        && let Err(e) =
                            undo_adjust(&mut st, pid, id, op.sem_num, -i32::from(op.sem_op))
                    {
                        failed = Some(e);
                        break;
                    }
                    applied += 1;
                }
                if let Some(e) = failed {
                    // Take back the undos made, then the operations.
                    for op in ops[..applied].iter().rev() {
                        if op.sem_flg & SEM_UNDO != 0 && op.sem_op != 0 {
                            let _ = undo_adjust(&mut st, pid, id, op.sem_num, i32::from(op.sem_op));
                        }
                    }
                    if let Some(set) = st.sets.get(id) {
                        for op in ops {
                            set.sems[usize::from(op.sem_num)].val -= i32::from(op.sem_op);
                        }
                    }
                    return Err(e);
                }
            }
            let set = st.sets.get(id).ok_or(Errno::EIDRM)?;
            for op in ops {
                set.sems[usize::from(op.sem_num)].pid = pid;
            }
            set.ds.sem_otime = now();
            if wake {
                set.waiters.wake();
            }
            return Ok(());
        }
        // Take back what was done.
        for op in &ops[..done] {
            set.sems[usize::from(op.sem_num)].val -= i32::from(op.sem_op);
        }
        if let Some(e) = error {
            return Err(e);
        }
        let i = blocked.unwrap_or(0);
        let op = ops[i];
        if i32::from(op.sem_flg) & IPC_NOWAIT.cast_signed() != 0 {
            return Err(Errno::EAGAIN);
        }
        let num = usize::from(op.sem_num);
        let zero = op.sem_op == 0;
        if zero {
            set.sems[num].zcnt += 1;
        } else {
            set.sems[num].ncnt += 1;
        }
        let waiters = Arc::clone(&set.waiters);
        let epoch = waiters.epoch.load(Ordering::Acquire);
        drop(st);
        let r = waiters.sleep(s, epoch, None);
        st = SEM.lock();
        let Some(set) = st.sets.get(id) else {
            return Err(Errno::EIDRM);
        };
        if !Arc::ptr_eq(&set.waiters, &waiters) {
            return Err(Errno::EIDRM);
        }
        if zero {
            set.sems[num].zcnt -= 1;
        } else {
            set.sems[num].ncnt -= 1;
        }
        r?;
    }
}

/// `__semctl`'s argument, copied in or to copy out.
pub(crate) enum SemArg<'a> {
    None,
    Val(i32),
    /// `GETALL`'s values out, `SETALL`'s in.
    Array(&'a mut Vec<u16>),
    Ds(&'a mut SemidDs),
}

/// `__semctl(semid, semnum, cmd, arg)`: the value it returns.
pub(crate) fn semctl(
    id: i32,
    num: i32,
    cmd: u32,
    arg: SemArg<'_>,
    cred: &Ucred,
) -> Result<i64, Errno> {
    let mut st = SEM.lock();
    if cmd == SEM_STAT {
        // `semid` is a slot.
        let ix = usize::try_from(id).map_err(|_| Errno::EINVAL)?;
        if ix >= SEMMNI {
            return Err(Errno::EINVAL);
        }
        st.sets.ensure(SEMMNI);
        let (seq, set) = &st.sets.slots[ix];
        let set = set.as_ref().ok_or(Errno::EINVAL)?;
        ipcperm(&set.ds.sem_perm, cred, IPC_R)?;
        if let SemArg::Ds(d) = arg {
            *d = set.ds;
        }
        #[allow(
            clippy::cast_possible_wrap,
            clippy::cast_possible_truncation,
            reason = "below 2^31"
        )]
        return Ok(i64::from((i32::from(*seq) << 16) | ix as i32));
    }
    let set = st.sets.get(id).ok_or(Errno::EINVAL)?;
    let one = |set: &SemSet| {
        usize::try_from(num)
            .ok()
            .filter(|&n| n < set.sems.len())
            .ok_or(Errno::EINVAL)
    };
    match cmd {
        IPC_RMID => {
            ipcperm(&set.ds.sem_perm, cred, IPC_M)?;
            sem_remove(&mut st, id);
            Ok(0)
        }
        IPC_SET => {
            ipcperm(&set.ds.sem_perm, cred, IPC_M)?;
            if let SemArg::Ds(d) = arg {
                set_perm(&mut set.ds.sem_perm, &d.sem_perm);
            }
            set.ds.sem_ctime = now();
            Ok(0)
        }
        IPC_STAT => {
            ipcperm(&set.ds.sem_perm, cred, IPC_R)?;
            if let SemArg::Ds(d) = arg {
                *d = set.ds;
            }
            Ok(0)
        }
        GETNCNT | GETPID | GETVAL | GETZCNT => {
            ipcperm(&set.ds.sem_perm, cred, IPC_R)?;
            let sem = set.sems[one(set)?];
            Ok(i64::from(match cmd {
                GETNCNT => i32::from(sem.ncnt),
                GETPID => sem.pid,
                GETVAL => sem.val,
                _ => i32::from(sem.zcnt),
            }))
        }
        GETALL => {
            ipcperm(&set.ds.sem_perm, cred, IPC_R)?;
            if let SemArg::Array(a) = arg {
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "0 to SEMVMX"
                )]
                a.extend(set.sems.iter().map(|s| s.val as u16));
            }
            Ok(0)
        }
        SETVAL => {
            ipcperm(&set.ds.sem_perm, cred, IPC_W)?;
            let n = one(set)?;
            let SemArg::Val(v) = arg else {
                return Err(Errno::EINVAL);
            };
            if !(0..=SEMVMX).contains(&v) {
                return Err(Errno::ERANGE);
            }
            set.sems[n].val = v;
            set.waiters.wake();
            #[allow(clippy::cast_possible_truncation, reason = "below SEMMSL")]
            st.undo.retain(|u| !(u.set == id && u.num == n as u16));
            Ok(0)
        }
        SETALL => {
            ipcperm(&set.ds.sem_perm, cred, IPC_W)?;
            let SemArg::Array(a) = arg else {
                return Err(Errno::EINVAL);
            };
            let mut r = Ok(0);
            for (sem, &v) in set.sems.iter_mut().zip(a.iter()) {
                if i32::from(v) > SEMVMX {
                    r = Err(Errno::ERANGE);
                    break;
                }
                sem.val = i32::from(v);
            }
            set.waiters.wake();
            st.undo.retain(|u| u.set != id);
            r
        }
        _ => Err(Errno::EINVAL),
    }
}

/// The number of semaphores in set `id`, for `SETALL` and `GETALL`'s
/// arrays.
pub(crate) fn sem_count(id: i32) -> Option<usize> {
    SEM.lock().sets.get(id).map(|s| s.sems.len())
}

/// Process `pid` exits (`semexit`): its undo adjustments are applied.
pub(crate) fn sem_exited(pid: i32) {
    let mut st = SEM.lock();
    if !st.undo.iter().any(|u| u.pid == pid) {
        return;
    }
    let mine: Vec<Undo> = {
        let (mine, rest) = core::mem::take(&mut st.undo)
            .into_iter()
            .partition(|u| u.pid == pid);
        st.undo = rest;
        mine
    };
    for u in mine {
        if let Some(set) = st.sets.get(u.set)
            && let Some(sem) = set.sems.get_mut(usize::from(u.num))
        {
            sem.val = (sem.val + u.adj).clamp(0, SEMVMX);
            set.waiters.wake();
        }
    }
}

// ---- Message queues ----

struct Msg {
    mtype: i64,
    text: Vec<u8>,
}

struct Queue {
    ds: MsqidDs,
    msgs: VecDeque<Msg>,
    waiters: Arc<Waiters>,
}

struct MsgState {
    queues: Table<Queue>,
    /// Free message segments and headers (`nfree_msgmaps`,
    /// `free_msghdrs`).
    free_segs: usize,
    free_hdrs: usize,
}

static MSG: PreemptLock<MsgState> = PreemptLock::new(MsgState {
    queues: Table::new(),
    free_segs: MSGSEG,
    free_hdrs: MSGTQL,
});

fn segs(n: usize) -> usize {
    n.div_ceil(MSGSSZ)
}

/// `msgget(key, msgflg)`.
pub(crate) fn msgget(key: i64, flg: u32, cred: &Ucred) -> Result<i32, Errno> {
    let mut st = MSG.lock();
    st.queues.ensure(MSGMNI);
    if key != IPC_PRIVATE {
        let found = st
            .queues
            .iter()
            .find(|(_, q)| q.ds.msg_perm.key == key)
            .map(|(id, q)| (id, q.ds.msg_perm));
        if let Some((id, perm)) = found {
            if flg & (IPC_CREAT | IPC_EXCL) == IPC_CREAT | IPC_EXCL {
                return Err(Errno::EEXIST);
            }
            #[allow(clippy::cast_possible_truncation, reason = "the owner bits")]
            ipcperm(&perm, cred, (flg & 0o700) as u16)?;
            return Ok(id);
        }
        if flg & IPC_CREAT == 0 {
            return Err(Errno::ENOENT);
        }
    }
    let id = st.queues.insert(
        MSGMNI,
        Queue {
            ds: MsqidDs {
                msg_perm: new_perm(key, flg, cred),
                msg_qbytes: MSGMNB,
                msg_ctime: now(),
                ..MsqidDs::default()
            },
            msgs: VecDeque::new(),
            waiters: Waiters::new(),
        },
    )?;
    if let Some(q) = st.queues.get(id) {
        q.ds.msg_perm.seq = seq_of(id);
    }
    Ok(id)
}

fn msg_free(st: &mut MsgState, m: &Msg) {
    st.free_segs += segs(m.text.len());
    st.free_hdrs += 1;
}

/// `msgsnd`, the text copied in: queues it, waiting for room.
pub(crate) fn msgsnd(
    s: &Sleepable,
    id: i32,
    mtype: i64,
    text: Vec<u8>,
    flg: u32,
    cred: &Ucred,
    pid: i32,
) -> Result<(), Errno> {
    let mut st = MSG.lock();
    let q = st.queues.get(id).ok_or(Errno::EINVAL)?;
    ipcperm(&q.ds.msg_perm, cred, IPC_W)?;
    let n = text.len();
    loop {
        let (free_segs, free_hdrs) = (st.free_segs, st.free_hdrs);
        let q = st.queues.get(id).ok_or(Errno::EIDRM)?;
        if n as u64 > q.ds.msg_qbytes {
            return Err(Errno::EINVAL);
        }
        let room =
            n as u64 + q.ds.msg_cbytes <= q.ds.msg_qbytes && segs(n) <= free_segs && free_hdrs > 0;
        if room {
            break;
        }
        if flg & IPC_NOWAIT != 0 {
            return Err(Errno::EAGAIN);
        }
        let waiters = Arc::clone(&q.waiters);
        let epoch = waiters.epoch.load(Ordering::Acquire);
        drop(st);
        // FreeBSD looks again every second whatever happens.
        waiters.sleep(s, epoch, Some(1_000_000_000))?;
        st = MSG.lock();
        match st.queues.get(id) {
            Some(q) if Arc::ptr_eq(&q.waiters, &waiters) => {}
            _ => return Err(Errno::EIDRM),
        }
    }
    if mtype < 1 {
        return Err(Errno::EINVAL);
    }
    st.free_segs -= segs(n);
    st.free_hdrs -= 1;
    let q = st.queues.get(id).ok_or(Errno::EIDRM)?;
    q.msgs.push_back(Msg { mtype, text });
    q.ds.msg_cbytes += n as u64;
    q.ds.msg_qnum += 1;
    q.ds.msg_lspid = pid;
    q.ds.msg_stime = now();
    q.waiters.wake();
    Ok(())
}

/// `msgrcv`: takes the first message of the type asked (any for 0; at
/// most `-msgtyp` for a negative one), up to `size` bytes of it unless
/// `MSG_NOERROR` lets it be cut short (`E2BIG`). Returns its type and
/// text.
pub(crate) fn msgrcv(
    s: &Sleepable,
    id: i32,
    size: usize,
    msgtyp: i64,
    flg: u32,
    cred: &Ucred,
    pid: i32,
) -> Result<(i64, Vec<u8>), Errno> {
    let mut st = MSG.lock();
    let q = st.queues.get(id).ok_or(Errno::EINVAL)?;
    ipcperm(&q.ds.msg_perm, cred, IPC_R)?;
    loop {
        let q = st.queues.get(id).ok_or(Errno::EIDRM)?;
        let at = q
            .msgs
            .iter()
            .position(|m| msgtyp == 0 || m.mtype == msgtyp || m.mtype <= -msgtyp);
        if let Some(i) = at {
            if size < q.msgs[i].text.len() && flg & MSG_NOERROR == 0 {
                return Err(Errno::E2BIG);
            }
            let m = q.msgs.remove(i).ok_or(Errno::EIDRM)?;
            q.ds.msg_cbytes -= m.text.len() as u64;
            q.ds.msg_qnum -= 1;
            q.ds.msg_lrpid = pid;
            q.ds.msg_rtime = now();
            q.waiters.wake();
            msg_free(&mut st, &m);
            let mut text = m.text;
            text.truncate(size);
            return Ok((m.mtype, text));
        }
        if flg & IPC_NOWAIT != 0 {
            return Err(Errno::ENOMSG);
        }
        let waiters = Arc::clone(&q.waiters);
        let epoch = waiters.epoch.load(Ordering::Acquire);
        drop(st);
        waiters.sleep(s, epoch, None)?;
        st = MSG.lock();
        match st.queues.get(id) {
            Some(q) if Arc::ptr_eq(&q.waiters, &waiters) => {}
            _ => return Err(Errno::EIDRM),
        }
    }
}

/// `msgctl(msqid, cmd, buf)`: `IPC_STAT`'s result.
pub(crate) fn msgctl(
    id: i32,
    cmd: u32,
    set: Option<&MsqidDs>,
    cred: &Ucred,
) -> Result<Option<MsqidDs>, Errno> {
    let mut st = MSG.lock();
    let q = st.queues.get(id).ok_or(Errno::EINVAL)?;
    match cmd {
        IPC_RMID => {
            ipcperm(&q.ds.msg_perm, cred, IPC_M)?;
            if let Some(q) = st.queues.remove(id) {
                for m in &q.msgs {
                    msg_free(&mut st, m);
                }
                q.waiters.wake();
            }
            Ok(None)
        }
        IPC_SET => {
            ipcperm(&q.ds.msg_perm, cred, IPC_M)?;
            let new = set.ok_or(Errno::EINVAL)?;
            // Raising the limit takes privilege; past the system's, it
            // stops there.
            if new.msg_qbytes > q.ds.msg_qbytes && !cred.is_root() {
                return Err(Errno::EPERM);
            }
            let qbytes = new.msg_qbytes.min(MSGMNB);
            if qbytes == 0 {
                return Err(Errno::EINVAL);
            }
            set_perm(&mut q.ds.msg_perm, &new.msg_perm);
            q.ds.msg_qbytes = qbytes;
            q.ds.msg_ctime = now();
            Ok(None)
        }
        IPC_STAT => {
            ipcperm(&q.ds.msg_perm, cred, IPC_R)?;
            Ok(Some(q.ds))
        }
        _ => Err(Errno::EINVAL),
    }
}
