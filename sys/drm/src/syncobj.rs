// SPDX-License-Identifier: BSD-2-Clause
//! Synchronisation objects (`DRM_IOCTL_SYNCOBJ_*`): a per-file handle for a
//! slot that holds one fence. A driver puts the fence of a submission in the
//! slot; a client waits on it, resets it, or signals it. This is the binary
//! kind: the timeline requests are not built, and the capability for them
//! says so. Exporting a slot to a descriptor is not built either.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::errno::Errno;
use sync::SpinLock;

use crate::card::Open;

const CREATE: u8 = 0xbf;
const DESTROY: u8 = 0xc0;
const HANDLE_TO_FD: u8 = 0xc1;
const FD_TO_HANDLE: u8 = 0xc2;
const WAIT: u8 = 0xc3;
const RESET: u8 = 0xc4;
const SIGNAL: u8 = 0xc5;

const CREATE_SIGNALED: u32 = 1;
const WAIT_ALL: u32 = 1;
const WAIT_FOR_SUBMIT: u32 = 2;
const WAIT_AVAILABLE: u32 = 4;
/// `HANDLE_TO_FD`: the descriptor is a sync file of the fence now in the
/// slot. `FD_TO_HANDLE`: the descriptor is a sync file whose fence goes in
/// the slot.
const SYNC_FILE_FLAG: u32 = 1;
/// Most handles one request may name.
const MAX_HANDLES: u32 = 4096;

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

    /// Asks to have `w` woken, from thread context, once it may have
    /// finished (a waiter then looks again, so a wake that is early or
    /// repeated is harmless). The default says nothing: a fence that can
    /// not announce itself leaves a sync file's waiters asleep.
    fn register(&self, w: &alloc::sync::Arc<dyn vfs::event::Wake>) {
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

/// The slot a handle names.
pub(crate) type Slot = Arc<SpinLock<Option<Arc<dyn Fence>>>>;

/// A file's synchronisation objects.
#[derive(Default)]
pub(crate) struct Table {
    slots: BTreeMap<u32, Slot>,
    next: u32,
}

impl core::fmt::Debug for Table {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Table({})", self.slots.len())
    }
}

impl Table {
    pub(crate) fn get(&self, handle: u32) -> Option<Slot> {
        self.slots.get(&handle).cloned()
    }

    fn insert(&mut self, fence: Option<Arc<dyn Fence>>) -> Result<u32, Errno> {
        if self.slots.len() >= 65536 {
            return Err(Errno::ENOSPC);
        }
        self.next = self.next.checked_add(1).ok_or(Errno::ENOSPC)?;
        self.slots.insert(self.next, Arc::new(SpinLock::new(fence)));
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

/// The handles an array request names, read from the caller's memory.
fn handles(f: &Open, ptr: u64, count: u32) -> Result<Vec<u32>, Errno> {
    if count == 0 || count > MAX_HANDLES {
        return Err(Errno::EINVAL);
    }
    let env = crate::env().ok_or(Errno::ENXIO)?;
    let mut raw = alloc::vec![0u8; count as usize * 4];
    env.user.copy_in(&mut raw, ptr)?;
    let _ = f;
    Ok(raw
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
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
        WAIT => wait(f, s, a),
        RESET => reset(f, a),
        SIGNAL => signal(f, a),
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
    a[..4].copy_from_slice(&h.to_le_bytes());
    Ok(0)
}

/// `SYNCOBJ_HANDLE_TO_FD`: a descriptor for the fence a handle holds. Only
/// the sync file form is built (`EXPORT_SYNC_FILE`); a descriptor for the
/// syncobj itself is not.
fn handle_to_fd(f: &Open, s: &sched::Sleepable, a: &mut [u8]) -> Result<usize, Errno> {
    let handle = u32_at(a, 0)?;
    let flags = u32_at(a, 4)?;
    if flags & !SYNC_FILE_FLAG != 0 {
        return Err(Errno::EINVAL);
    }
    if flags & SYNC_FILE_FLAG == 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    let env = crate::env().ok_or(Errno::ENXIO)?;
    let slot = f.syncobjs.lock().get(handle).ok_or(Errno::ENOENT)?;
    // An empty slot has no fence to export.
    let fence = slot.lock().clone().ok_or(Errno::EINVAL)?;
    let file = crate::SyncFile::new(alloc::vec![fence]);
    let fd = (env.export_syncfile)(s, file, true)?;
    a.get_mut(8..12)
        .ok_or(Errno::EINVAL)?
        .copy_from_slice(&fd.to_le_bytes());
    Ok(0)
}

/// `SYNCOBJ_FD_TO_HANDLE`: a sync file's fence goes in the slot a handle
/// names, or in a new syncobj if the handle is 0.
fn fd_to_handle(f: &Open, s: &sched::Sleepable, a: &mut [u8]) -> Result<usize, Errno> {
    let handle = u32_at(a, 0)?;
    let flags = u32_at(a, 4)?;
    let fd = i32::from_le_bytes(
        a.get(8..12)
            .ok_or(Errno::EINVAL)?
            .try_into()
            .map_err(|_| Errno::EINVAL)?,
    );
    if flags & !SYNC_FILE_FLAG != 0 {
        return Err(Errno::EINVAL);
    }
    if flags & SYNC_FILE_FLAG == 0 {
        return Err(Errno::EOPNOTSUPP);
    }
    let env = crate::env().ok_or(Errno::ENXIO)?;
    let fence = (env.import_syncfile)(s, fd)?.fence();
    if handle != 0 {
        let slot = f.syncobjs.lock().get(handle).ok_or(Errno::ENOENT)?;
        *slot.lock() = Some(fence);
    } else {
        let h = f.syncobjs.lock().insert(Some(fence))?;
        a[..4].copy_from_slice(&h.to_le_bytes());
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
    let list = handles(f, u64_at(a, 0)?, u32_at(a, 8)?)?;
    for s in slots(f, &list)? {
        *s.lock() = None;
    }
    Ok(0)
}

fn signal(f: &Open, a: &mut [u8]) -> Result<usize, Errno> {
    let list = handles(f, u64_at(a, 0)?, u32_at(a, 8)?)?;
    for s in slots(f, &list)? {
        *s.lock() = Some(Arc::new(Done));
    }
    Ok(0)
}

/// `SYNCOBJ_WAIT`: until one (or all) of the slots' fences has finished, or
/// the absolute time (nanoseconds of uptime) in the request.
fn wait(f: &Open, s: &sched::Sleepable, a: &mut [u8]) -> Result<usize, Errno> {
    let list = handles(f, u64_at(a, 0)?, u32_at(a, 16)?)?;
    let timeout = u64_at(a, 8)?;
    let flags = u32_at(a, 20)?;
    let env = crate::env().ok_or(Errno::ENXIO)?;
    let all = flags & WAIT_ALL != 0;
    let objects = slots(f, &list)?;
    loop {
        let mut missing = false;
        let mut first = None;
        let mut done = 0;
        let mut pending: Option<Arc<dyn Fence>> = None;
        for (i, slot) in objects.iter().enumerate() {
            let fence = slot.lock().clone();
            match fence {
                None => missing = true,
                Some(fe) => {
                    // "Available" means the fence exists, finished or not.
                    if flags & WAIT_AVAILABLE != 0 || fe.signaled() {
                        done += 1;
                        first.get_or_insert(i);
                    } else if pending.is_none() {
                        pending = Some(fe);
                    }
                }
            }
        }
        if missing && flags & WAIT_FOR_SUBMIT == 0 {
            return Err(Errno::EINVAL);
        }
        if (all && done == objects.len()) || (!all && first.is_some()) {
            let at = u32::try_from(first.unwrap_or(0)).unwrap_or(0);
            a[24..28].copy_from_slice(&at.to_le_bytes());
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
