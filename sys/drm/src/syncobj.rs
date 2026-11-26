// SPDX-License-Identifier: BSD-2-Clause
//! Synchronisation objects (`DRM_IOCTL_SYNCOBJ_*`): a per-file handle for an
//! object that holds a fence (a binary object), or a timeline of fences
//! numbered by a 64-bit point (a timeline object). A driver puts the fence
//! of a submission in it; a client waits on it, resets it, signals it,
//! hands its fence to a sync file, to another object, or to an eventfd, or
//! shares the object itself through a descriptor.
//!
//! Behavior reference: the DRM uapi as userland sees it (`drm.h`) and
//! docs/GRAPHICS.md §5.1.

use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use abi::errno::Errno;
use sync::SpinLock;
use vfs::event::Wake;

use crate::card::Open;

const CREATE: u8 = 0xbf;
const DESTROY: u8 = 0xc0;
const HANDLE_TO_FD: u8 = 0xc1;
const FD_TO_HANDLE: u8 = 0xc2;
const WAIT: u8 = 0xc3;
const RESET: u8 = 0xc4;
const SIGNAL: u8 = 0xc5;
const TIMELINE_WAIT: u8 = 0xca;
const QUERY: u8 = 0xcb;
const TRANSFER: u8 = 0xcc;
const TIMELINE_SIGNAL: u8 = 0xcd;
const EVENTFD: u8 = 0xcf;

const CREATE_SIGNALED: u32 = 1;
const WAIT_ALL: u32 = 1;
const WAIT_FOR_SUBMIT: u32 = 2;
const WAIT_AVAILABLE: u32 = 4;
/// The request carries a deadline (`deadline_nsec`, absolute, in the same
/// clock as the timeout): the fences being waited for are told.
const WAIT_DEADLINE: u32 = 8;
/// `HANDLE_TO_FD`: the descriptor is a sync file of the fence now in the
/// object. `FD_TO_HANDLE`: the descriptor is a sync file whose fence goes
/// in the object.
const SYNC_FILE_FLAG: u32 = 1;
/// With either of the above: the fence is the one at the request's point.
const TIMELINE_FLAG: u32 = 2;
/// `QUERY`: the last point submitted, not the last one signalled.
const QUERY_LAST_SUBMITTED: u32 = 1;
/// Most handles one request may name.
const MAX_HANDLES: u32 = 4096;
/// Most points a timeline holds (an upper bound on memory; signalled
/// points are dropped as new ones come in).
const MAX_POINTS: usize = 4096;
/// How long a transfer waits for a submission that has not come, in
/// nanoseconds.
const SUBMIT_WAIT_NS: u64 = 10_000_000_000;

/// Drops a timeline's leading points that finished, all but the latest of
/// them (the query reports it).
fn drop_finished(points: &mut BTreeMap<u64, Arc<dyn Fence>>) {
    let done: Vec<u64> = points
        .iter()
        .take_while(|(_, f)| f.signaled())
        .map(|(k, _)| *k)
        .collect();
    for k in done.iter().take(done.len().saturating_sub(1)) {
        points.remove(k);
    }
}

/// Something that finishes once: a submission's completion.
pub trait Fence: Send + Sync {
    /// Whether it has finished.
    fn signaled(&self) -> bool;

    /// Waits up to `ns` nanoseconds for it to finish (returns early when it
    /// does); the default gives the processor up and looks again.
    fn wait(&self, s: &sched::Sleepable, ns: u64) -> bool {
        let _ = (s, ns);
        sched::yield_now();
        self.signaled()
    }

    /// Tells it that someone needs it finished by `ns` (nanoseconds of
    /// uptime, as the wait's timeout is), so a driver may raise the clock
    /// of what runs it. The default does nothing.
    fn set_deadline(&self, ns: u64) {
        let _ = ns;
    }

    /// Asks to have `w` woken, from thread context, once it may have
    /// finished (a waiter then looks again, so a wake that is early or
    /// repeated is harmless). The default says nothing: a fence that can
    /// not announce itself leaves a sync file's waiters asleep.
    fn register(&self, w: &Arc<dyn Wake>) {
        let _ = w;
    }
}

/// A fence that has already finished.
#[derive(Debug)]
pub struct Done;

impl Fence for Done {
    fn signaled(&self) -> bool {
        true
    }
}

/// Several fences as one: finished when every one is.
pub(crate) struct All(pub(crate) Vec<Arc<dyn Fence>>);

impl Fence for All {
    fn signaled(&self) -> bool {
        self.0.iter().all(|f| f.signaled())
    }

    fn wait(&self, s: &sched::Sleepable, ns: u64) -> bool {
        // The first unfinished one gets the time; the rest are asked
        // after.
        match self.0.iter().find(|f| !f.signaled()) {
            Some(f) => {
                let _ = f.wait(s, ns);
                self.signaled()
            }
            None => true,
        }
    }

    fn set_deadline(&self, ns: u64) {
        for f in &self.0 {
            if !f.signaled() {
                f.set_deadline(ns);
            }
        }
    }

    fn register(&self, w: &Arc<dyn Wake>) {
        for f in &self.0 {
            if !f.signaled() {
                f.register(w);
            }
        }
    }
}

/// What a synchronisation object holds.
enum State {
    /// One fence, or none yet.
    Binary(Option<Arc<dyn Fence>>),
    /// Fences by point, in order. A point is finished when its fence and
    /// every earlier one is.
    Timeline(BTreeMap<u64, Arc<dyn Fence>>),
}

/// A synchronisation object, shared by the handles that name it and the
/// descriptors made of it.
pub struct SyncObj {
    state: SpinLock<State>,
    /// Waiters for a fence to be put in (a wait for submission, an eventfd
    /// that asked for availability).
    submit_wakers: SpinLock<Vec<Arc<dyn Wake>>>,
}

impl core::fmt::Debug for SyncObj {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "SyncObj")
    }
}

impl SyncObj {
    fn new(fence: Option<Arc<dyn Fence>>) -> Arc<Self> {
        Arc::new(Self {
            state: SpinLock::new(State::Binary(fence)),
            submit_wakers: SpinLock::new(Vec::new()),
        })
    }

    /// Tells the waiters for a submission that a fence arrived.
    fn arrived(&self) {
        let ws: Vec<_> = core::mem::take(&mut *self.submit_wakers.lock());
        for w in ws {
            w.wake();
        }
    }

    /// Asks to have `w` woken when a fence is next put in.
    fn on_arrival(&self, w: &Arc<dyn Wake>) {
        self.submit_wakers.lock().push(Arc::clone(w));
    }

    /// The fence of the last submission put in (the latest point of a
    /// timeline), if any.
    #[must_use]
    pub fn fence(&self) -> Option<Arc<dyn Fence>> {
        self.find(0).ok().flatten()
    }

    /// Puts `fence` in a binary object (a timeline object becomes binary:
    /// its points are gone), or empties it.
    pub fn set(&self, fence: Option<Arc<dyn Fence>>) {
        let arrived = fence.is_some();
        *self.state.lock() = State::Binary(fence);
        if arrived {
            self.arrived();
        }
    }

    /// Puts `fence` at `point`: point 0 is the binary form, others a timeline
    /// (an empty or binary object turns into one) whose points only grow.
    ///
    /// # Errors
    ///
    /// `EINVAL` for a point not after the last, `ENOSPC` for too many.
    pub fn add_point(&self, point: u64, fence: Arc<dyn Fence>) -> Result<(), Errno> {
        if point == 0 {
            self.set(Some(fence));
            return Ok(());
        }
        {
            let mut st = self.state.lock();
            let points = match &mut *st {
                State::Timeline(p) => p,
                State::Binary(old) => {
                    // The old fence, if any, becomes point 0 of nothing: a
                    // timeline starts at its first point.
                    let _ = old.take();
                    *st = State::Timeline(BTreeMap::new());
                    match &mut *st {
                        State::Timeline(p) => p,
                        State::Binary(_) => return Err(Errno::EINVAL),
                    }
                }
            };
            if points.keys().next_back().is_some_and(|last| *last >= point) {
                return Err(Errno::EINVAL);
            }
            // Points that finished, with everything before them, are not
            // needed; keep the latest of them for the query. Dropped at every
            // new point, not only at the limit: a wait's fence is everything
            // up to its point, so the finished ones a timeline keeps make
            // every later wait and query longer (a renderer's timeline gains
            // a point per frame).
            drop_finished(points);
            if points.len() >= MAX_POINTS {
                return Err(Errno::ENOSPC);
            }
            points.insert(point, fence);
        }
        self.arrived();
        Ok(())
    }

    /// The fence to wait for to know `point` has been reached: for a binary
    /// object, its fence (point 0 only); for a timeline, everything up to
    /// the first point at or after `point` (0 is the latest). `Ok(None)`:
    /// nothing there yet (a wait may be for submission). `EINVAL`: a point
    /// a binary object does not have, or a timeline's point that is
    /// beyond its last (not yet submitted) is `Ok(None)`.
    ///
    /// # Errors
    ///
    /// `EINVAL` for a non-zero point of a binary object.
    pub fn find(&self, point: u64) -> Result<Option<Arc<dyn Fence>>, Errno> {
        match &*self.state.lock() {
            State::Binary(f) => {
                if point == 0 {
                    Ok(f.clone())
                } else {
                    Err(Errno::EINVAL)
                }
            }
            State::Timeline(points) => {
                let upto = if point == 0 {
                    points.keys().next_back().copied()
                } else {
                    points.range(point..).next().map(|(k, _)| *k)
                };
                let Some(upto) = upto else {
                    return Ok(None);
                };
                // A fence that finished stays finished: only the others are
                // waited for, and the point's own when all have.
                let list: Vec<Arc<dyn Fence>> = points
                    .range(..=upto)
                    .filter(|(_, f)| !f.signaled())
                    .map(|(_, f)| Arc::clone(f))
                    .collect();
                Ok(Some(match list.as_slice() {
                    [] => Arc::clone(&points[&upto]),
                    [one] => Arc::clone(one),
                    _ => Arc::new(All(list)),
                }))
            }
        }
    }

    /// `QUERY`: the last point that has finished (with all before it), or
    /// the last one submitted. A binary object is at point 0.
    #[must_use]
    pub fn query(&self, last_submitted: bool) -> u64 {
        match &*self.state.lock() {
            State::Binary(_) => 0,
            State::Timeline(points) => {
                if last_submitted {
                    return points.keys().next_back().copied().unwrap_or(0);
                }
                let mut at = 0;
                for (k, f) in points {
                    if !f.signaled() {
                        break;
                    }
                    at = *k;
                }
                at
            }
        }
    }
}

/// A file's synchronisation objects.
#[derive(Default)]
pub(crate) struct Table {
    slots: BTreeMap<u32, Arc<SyncObj>>,
    next: u32,
}

/// What a handle names.
pub(crate) type Slot = Arc<SyncObj>;

impl core::fmt::Debug for Table {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Table({})", self.slots.len())
    }
}

impl Table {
    pub(crate) fn get(&self, handle: u32) -> Option<Slot> {
        self.slots.get(&handle).cloned()
    }

    /// A handle for a new object holding `fence`.
    pub(crate) fn insert(&mut self, fence: Option<Arc<dyn Fence>>) -> Result<u32, Errno> {
        self.insert_object(SyncObj::new(fence))
    }

    /// A handle for an object that exists (another handle's, or a
    /// descriptor's).
    pub(crate) fn insert_object(&mut self, obj: Arc<SyncObj>) -> Result<u32, Errno> {
        if self.slots.len() >= 65536 {
            return Err(Errno::ENOSPC);
        }
        self.next = self.next.checked_add(1).ok_or(Errno::ENOSPC)?;
        self.slots.insert(self.next, obj);
        Ok(self.next)
    }
}

fn u32_at(b: &[u8], at: usize) -> Result<u32, Errno> {
    let s = b.get(at..at + 4).ok_or(Errno::EINVAL)?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn u64_at(b: &[u8], at: usize) -> Result<u64, Errno> {
    let s = b.get(at..at + 8).ok_or(Errno::EINVAL)?;
    Ok(u64::from_le_bytes([
        s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
    ]))
}

fn i32_at(b: &[u8], at: usize) -> Result<i32, Errno> {
    Ok(u32_at(b, at)?.cast_signed())
}

fn put32(b: &mut [u8], at: usize, v: u32) -> Result<(), Errno> {
    b.get_mut(at..at + 4)
        .ok_or(Errno::EINVAL)?
        .copy_from_slice(&v.to_le_bytes());
    Ok(())
}

/// An array of `count` words of `size` bytes (4 or 8) read from the caller's
/// memory at `ptr`.
fn user_array(ptr: u64, count: u32, size: usize) -> Result<Vec<u8>, Errno> {
    if count == 0 || count > MAX_HANDLES {
        return Err(Errno::EINVAL);
    }
    let env = crate::env().ok_or(Errno::ENXIO)?;
    let mut raw = alloc::vec![0u8; count as usize * size];
    env.user.copy_in(&mut raw, ptr)?;
    Ok(raw)
}

/// The handles an array request names, read from the caller's memory.
fn handles(ptr: u64, count: u32) -> Result<Vec<u32>, Errno> {
    Ok(user_array(ptr, count, 4)?
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
        .collect())
}

/// The points an array names (one 64-bit point each), or all zero if
/// `ptr` is 0.
fn points(ptr: u64, count: u32) -> Result<Vec<u64>, Errno> {
    if ptr == 0 {
        return Ok(alloc::vec![0; count as usize]);
    }
    Ok(user_array(ptr, count, 8)?
        .as_chunks::<8>()
        .0
        .iter()
        .map(|c| u64::from_le_bytes(*c))
        .collect())
}

fn slots(f: &Open, list: &[u32]) -> Result<Vec<Slot>, Errno> {
    let t = f.syncobjs.lock();
    list.iter()
        .map(|h| t.get(*h).ok_or(Errno::ENOENT))
        .collect()
}

/// Runs synchronisation request `n` for `f`, `None` if `n` is not one of
/// them.
pub(crate) fn dispatch(
    f: &Open,
    s: &sched::Sleepable,
    n: u8,
    a: &mut [u8],
) -> Option<Result<usize, Errno>> {
    Some(match n {
        CREATE => create(f, a),
        DESTROY => destroy(f, a),
        HANDLE_TO_FD => handle_to_fd(f, s, a),
        FD_TO_HANDLE => fd_to_handle(f, s, a),
        WAIT => wait(f, s, a, false),
        TIMELINE_WAIT => wait(f, s, a, true),
        RESET => reset(f, a),
        SIGNAL => signal(f, a),
        TIMELINE_SIGNAL => timeline_signal(f, a),
        QUERY => query(f, a),
        TRANSFER => transfer(f, s, a),
        EVENTFD => eventfd(f, s, a),
        _ => return None,
    })
}

fn create(f: &Open, a: &mut [u8]) -> Result<usize, Errno> {
    let flags = u32_at(a, 4)?;
    if flags & !CREATE_SIGNALED != 0 {
        return Err(Errno::EINVAL);
    }
    let fence: Option<Arc<dyn Fence>> = (flags & CREATE_SIGNALED != 0).then(|| {
        let d: Arc<dyn Fence> = Arc::new(Done);
        d
    });
    let h = f.syncobjs.lock().insert(fence)?;
    put32(a, 0, h)?;
    Ok(0)
}

/// `SYNCOBJ_HANDLE_TO_FD`: a descriptor for the object a handle names, or,
/// with `EXPORT_SYNC_FILE`, a sync file of its fence (the one at the
/// request's point, with `TIMELINE`).
fn handle_to_fd(f: &Open, s: &sched::Sleepable, a: &mut [u8]) -> Result<usize, Errno> {
    let handle = u32_at(a, 0)?;
    let flags = u32_at(a, 4)?;
    if flags & !(SYNC_FILE_FLAG | TIMELINE_FLAG) != 0 {
        return Err(Errno::EINVAL);
    }
    let env = crate::env().ok_or(Errno::ENXIO)?;
    let obj = f.syncobjs.lock().get(handle).ok_or(Errno::ENOENT)?;
    let fd = if flags & SYNC_FILE_FLAG != 0 {
        let point = if flags & TIMELINE_FLAG != 0 {
            u64_at(a, 16)?
        } else {
            0
        };
        // An object with no fence has nothing to export.
        let fence = obj.find(point)?.ok_or(Errno::EINVAL)?;
        let file = crate::SyncFile::new(alloc::vec![fence]);
        (env.export_syncfile)(s, file, true)?
    } else {
        if flags & TIMELINE_FLAG != 0 {
            return Err(Errno::EINVAL);
        }
        (env.export_syncobj)(s, obj, true)?
    };
    put32(a, 8, fd.cast_unsigned())?;
    Ok(0)
}

/// `SYNCOBJ_FD_TO_HANDLE`: a handle for the object a descriptor names, or,
/// with `IMPORT_SYNC_FILE`, a sync file's fence goes in the object the
/// handle names (at the request's point, with `TIMELINE`), or in a new one
/// if the handle is 0.
fn fd_to_handle(f: &Open, s: &sched::Sleepable, a: &mut [u8]) -> Result<usize, Errno> {
    let handle = u32_at(a, 0)?;
    let flags = u32_at(a, 4)?;
    let fd = i32_at(a, 8)?;
    if flags & !(SYNC_FILE_FLAG | TIMELINE_FLAG) != 0 {
        return Err(Errno::EINVAL);
    }
    let env = crate::env().ok_or(Errno::ENXIO)?;
    if flags & SYNC_FILE_FLAG == 0 {
        if flags & TIMELINE_FLAG != 0 {
            return Err(Errno::EINVAL);
        }
        let obj = (env.import_syncobj)(s, fd)?;
        let h = f.syncobjs.lock().insert_object(obj)?;
        put32(a, 0, h)?;
        return Ok(0);
    }
    let point = if flags & TIMELINE_FLAG != 0 {
        u64_at(a, 16)?
    } else {
        0
    };
    let fence = (env.import_syncfile)(s, fd)?.fence();
    if handle != 0 {
        let obj = f.syncobjs.lock().get(handle).ok_or(Errno::ENOENT)?;
        obj.add_point(point, fence)?;
    } else {
        let obj = SyncObj::new(None);
        obj.add_point(point, fence)?;
        let h = f.syncobjs.lock().insert_object(obj)?;
        put32(a, 0, h)?;
    }
    Ok(0)
}

fn destroy(f: &Open, a: &mut [u8]) -> Result<usize, Errno> {
    let h = u32_at(a, 0)?;
    f.syncobjs
        .lock()
        .slots
        .remove(&h)
        .map(|_| 0)
        .ok_or(Errno::EINVAL)
}

fn reset(f: &Open, a: &mut [u8]) -> Result<usize, Errno> {
    let list = handles(u64_at(a, 0)?, u32_at(a, 8)?)?;
    for s in slots(f, &list)? {
        s.set(None);
    }
    Ok(0)
}

fn signal(f: &Open, a: &mut [u8]) -> Result<usize, Errno> {
    let list = handles(u64_at(a, 0)?, u32_at(a, 8)?)?;
    for s in slots(f, &list)? {
        s.set(Some(Arc::new(Done)));
    }
    Ok(0)
}

/// `SYNCOBJ_TIMELINE_SIGNAL`: each named object's point finished (point 0
/// is the binary form).
fn timeline_signal(f: &Open, a: &mut [u8]) -> Result<usize, Errno> {
    let count = u32_at(a, 16)?;
    let list = handles(u64_at(a, 0)?, count)?;
    let pts = points(u64_at(a, 8)?, count)?;
    let objs = slots(f, &list)?;
    for (o, p) in objs.iter().zip(pts) {
        o.add_point(p, Arc::new(Done))?;
    }
    Ok(0)
}

/// `SYNCOBJ_QUERY`: for each object, its last finished point (or last
/// submitted, with the flag), written to the points array.
fn query(f: &Open, a: &mut [u8]) -> Result<usize, Errno> {
    let count = u32_at(a, 16)?;
    let flags = u32_at(a, 20)?;
    if flags & !QUERY_LAST_SUBMITTED != 0 {
        return Err(Errno::EINVAL);
    }
    let list = handles(u64_at(a, 0)?, count)?;
    let out = u64_at(a, 8)?;
    let objs = slots(f, &list)?;
    let env = crate::env().ok_or(Errno::ENXIO)?;
    let mut raw = Vec::with_capacity(objs.len() * 8);
    for o in &objs {
        raw.extend_from_slice(&o.query(flags & QUERY_LAST_SUBMITTED != 0).to_le_bytes());
    }
    env.user.copy_out(out, &raw)?;
    Ok(0)
}

/// `SYNCOBJ_TRANSFER`: the fence at the source's point goes to the
/// destination's point.
fn transfer(f: &Open, _s: &sched::Sleepable, a: &mut [u8]) -> Result<usize, Errno> {
    let src = u32_at(a, 0)?;
    let dst = u32_at(a, 4)?;
    let src_point = u64_at(a, 8)?;
    let dst_point = u64_at(a, 16)?;
    let flags = u32_at(a, 24)?;
    if flags & !(WAIT_FOR_SUBMIT | WAIT_AVAILABLE) != 0 {
        return Err(Errno::EINVAL);
    }
    let (so, d) = {
        let t = f.syncobjs.lock();
        (
            t.get(src).ok_or(Errno::ENOENT)?,
            t.get(dst).ok_or(Errno::ENOENT)?,
        )
    };
    let env = crate::env().ok_or(Errno::ENXIO)?;
    let start = (env.now_ns)();
    let fence = loop {
        if let Some(fe) = so.find(src_point)? {
            break fe;
        }
        if flags & WAIT_FOR_SUBMIT == 0 {
            return Err(Errno::EINVAL);
        }
        if (env.now_ns)().saturating_sub(start) > SUBMIT_WAIT_NS {
            return Err(Errno::ETIMEDOUT);
        }
        #[cfg(not(test))]
        if sched::current().is_interrupted() {
            return Err(Errno::EINTR);
        }
        sched::yield_now();
    };
    d.add_point(dst_point, fence)?;
    Ok(0)
}

/// What a wait finds for one entry of its list.
enum Found {
    /// The fence, which may or may not have finished.
    Fence(Arc<dyn Fence>),
    /// No fence at the point (yet).
    Missing,
}

/// `SYNCOBJ_WAIT` and `SYNCOBJ_TIMELINE_WAIT`: until one (or all) of the
/// objects' fences (at the points, for the timeline form) has finished, or
/// the absolute time (nanoseconds of uptime) in the request.
fn wait(f: &Open, s: &sched::Sleepable, a: &mut [u8], timeline: bool) -> Result<usize, Errno> {
    // The two layouts: the timeline's has the points array second.
    let (count_at, flags_at, first_at, timeout_at) = if timeline {
        (24, 28, 32, 16)
    } else {
        (16, 20, 24, 8)
    };
    let count = u32_at(a, count_at)?;
    let list = handles(u64_at(a, 0)?, count)?;
    let pts = if timeline {
        points(u64_at(a, 8)?, count)?
    } else {
        alloc::vec![0; count as usize]
    };
    let timeout = u64_at(a, timeout_at)?;
    let flags = u32_at(a, flags_at)?;
    if flags & !(WAIT_ALL | WAIT_FOR_SUBMIT | WAIT_AVAILABLE | WAIT_DEADLINE) != 0 {
        return Err(Errno::EINVAL);
    }
    // The deadline is the last field of the request: after the padding.
    let deadline = if flags & WAIT_DEADLINE != 0 {
        Some(u64_at(a, if timeline { 40 } else { 32 })?)
    } else {
        None
    };
    let env = crate::env().ok_or(Errno::ENXIO)?;
    let all = flags & WAIT_ALL != 0;
    let objects = slots(f, &list)?;
    // Each fence is told the deadline once.
    let mut told: Vec<*const ()> = Vec::new();
    loop {
        let mut missing = false;
        let mut first = None;
        let mut done = 0;
        let mut pending: Option<Arc<dyn Fence>> = None;
        for (i, (obj, point)) in objects.iter().zip(&pts).enumerate() {
            let found = match obj.find(*point)? {
                Some(fe) => Found::Fence(fe),
                None => Found::Missing,
            };
            match found {
                Found::Missing => missing = true,
                Found::Fence(fe) => {
                    // "Available" means the fence exists, finished or not.
                    if flags & WAIT_AVAILABLE != 0 || fe.signaled() {
                        done += 1;
                        first.get_or_insert(i);
                    } else {
                        if let Some(d) = deadline {
                            let key = Arc::as_ptr(&fe).cast::<()>();
                            if !told.contains(&key) {
                                told.push(key);
                                fe.set_deadline(d);
                            }
                        }
                        if pending.is_none() {
                            pending = Some(fe);
                        }
                    }
                }
            }
        }
        if missing && flags & WAIT_FOR_SUBMIT == 0 {
            return Err(Errno::EINVAL);
        }
        if (all && done == objects.len()) || (!all && first.is_some()) {
            let at = u32::try_from(first.unwrap_or(0)).unwrap_or(0);
            put32(a, first_at, at)?;
            return Ok(0);
        }
        if (env.now_ns)() >= timeout {
            return Err(Errno::ETIMEDOUT);
        }
        #[cfg(not(test))]
        if sched::current().is_interrupted() {
            return Err(Errno::EINTR);
        }
        match pending {
            // Sleep on the fence a little (its interrupt wakes it), then look
            // at everything again.
            Some(fe) => {
                let _ = fe.wait(s, 20_000_000);
            }
            None => sched::yield_now(),
        }
    }
}

/// Something that can be told once: an eventfd's counter.
pub trait Signal: Send + Sync {
    /// Adds one to the counter (and wakes whoever waits for it).
    fn signal(&self);
}

/// An eventfd to signal when a fence is there, or has finished.
struct EventWait {
    me: Weak<EventWait>,
    obj: Arc<SyncObj>,
    point: u64,
    available: bool,
    target: Arc<dyn Signal>,
    done: core::sync::atomic::AtomicBool,
}

impl EventWait {
    /// Looks at the fence, and signals the target or asks to be woken.
    fn check(&self) {
        use core::sync::atomic::Ordering::AcqRel;
        if self.done.load(core::sync::atomic::Ordering::Acquire) {
            return;
        }
        let me: Arc<dyn Wake> = match self.me.upgrade() {
            Some(m) => m,
            None => return,
        };
        match self.obj.find(self.point) {
            // Nothing at the point yet: ask to hear of the next fence.
            Ok(None) => {
                self.obj.on_arrival(&me);
                // One may have come in between.
                if self.obj.find(self.point).ok().flatten().is_some() {
                    self.check();
                }
            }
            Ok(Some(f)) => {
                if self.available || f.signaled() {
                    if !self.done.swap(true, AcqRel) {
                        self.target.signal();
                    }
                } else {
                    f.register(&me);
                    // It may have finished before it was asked.
                    if f.signaled() && !self.done.swap(true, AcqRel) {
                        self.target.signal();
                    }
                }
            }
            // The point cannot exist on this object: nothing to wait for.
            Err(_) => {}
        }
    }
}

impl Wake for EventWait {
    fn wake(&self) {
        self.check();
    }
}

/// `SYNCOBJ_EVENTFD`: add one to an eventfd when the object's fence at a
/// point has finished (or is there, with `WAIT_AVAILABLE`).
fn eventfd(f: &Open, s: &sched::Sleepable, a: &mut [u8]) -> Result<usize, Errno> {
    let handle = u32_at(a, 0)?;
    let flags = u32_at(a, 4)?;
    let point = u64_at(a, 8)?;
    let fd = i32_at(a, 16)?;
    if flags & !WAIT_AVAILABLE != 0 {
        return Err(Errno::EINVAL);
    }
    let env = crate::env().ok_or(Errno::ENXIO)?;
    let obj = f.syncobjs.lock().get(handle).ok_or(Errno::ENOENT)?;
    let target = (env.import_eventfd)(s, fd)?;
    // A point a binary object cannot have is an error now, not a wait.
    obj.find(point)?;
    let w = Arc::new_cyclic(|me| EventWait {
        me: me.clone(),
        obj,
        point,
        available: flags & WAIT_AVAILABLE != 0,
        target,
        done: core::sync::atomic::AtomicBool::new(false),
    });
    w.check();
    Ok(0)
}
