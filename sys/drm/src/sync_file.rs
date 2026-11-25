// SPDX-License-Identifier: BSD-2-Clause
//! Sync files: a descriptor that names one or more fences. It becomes
//! readable (`poll`, `EVFILT_READ`) once every fence has finished, which is
//! how a compositor or a client waits for a GPU job from its event loop.
//!
//! A syncobj exports its fence as one (`SYNCOBJ_HANDLE_TO_FD` with
//! `EXPORT_SYNC_FILE`) and takes one back (`FD_TO_HANDLE` with
//! `IMPORT_SYNC_FILE`); `SYNC_IOC_MERGE` joins two into a third and
//! `SYNC_IOC_FILE_INFO` describes one.
//!
//! Behavior reference: docs/GRAPHICS.md §5.1, and the `sync_file` uapi
//! (`linux/sync_file.h`) as userland sees it.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use abi::errno::Errno;
use abi::event::{POLLIN, POLLRDNORM};
use vfs::event::{Notify, Wake};

use crate::syncobj::Fence;

/// The bytes of `struct sync_merge_data`: name, the other descriptor, the
/// new descriptor (out), flags, padding.
pub const MERGE_LEN: usize = 48;
/// The bytes of `struct sync_file_info`.
pub const INFO_LEN: usize = 56;
/// The bytes of one `struct sync_fence_info`.
const FENCE_INFO_LEN: usize = 72;
/// Most fences one sync file holds (a merge past this is refused).
pub const MAX_FENCES: usize = 1024;

/// A sync file: the fences it waits for.
pub struct SyncFile {
    me: Weak<SyncFile>,
    fences: Vec<Arc<dyn Fence>>,
    notify: Notify,
}

impl core::fmt::Debug for SyncFile {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "SyncFile({} fences)", self.fences.len())
    }
}

/// What a fence calls when it may have finished: the sync file looks
/// again.
struct Look(Weak<SyncFile>);

impl Wake for Look {
    fn wake(&self) {
        if let Some(f) = self.0.upgrade() {
            f.look();
        }
    }
}

impl SyncFile {
    /// A sync file for `fences` (none: one that is already done).
    #[must_use]
    pub fn new(fences: Vec<Arc<dyn Fence>>) -> Arc<Self> {
        let f = Arc::new_cyclic(|me| Self {
            me: me.clone(),
            fences,
            notify: Notify::new(),
        });
        f.arm();
        f
    }

    /// Asks every unfinished fence to say when it may have.
    fn arm(&self) {
        let w: Arc<dyn Wake> = Arc::new(Look(self.me.clone()));
        for fence in &self.fences {
            if !fence.signaled() {
                fence.register(&w);
            }
        }
        // One may have finished between the look and the asking.
        if self.signaled() {
            self.notify.notify(0);
        }
    }

    /// A fence said it may have finished: tell the waiters if all have, or
    /// ask again.
    fn look(&self) {
        if self.signaled() {
            self.notify.notify(0);
        } else {
            self.arm();
        }
    }

    /// Whether every fence has finished.
    #[must_use]
    pub fn signaled(&self) -> bool {
        self.fences.iter().all(|f| f.signaled())
    }

    /// How many fences it holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.fences.len()
    }

    /// Whether it holds none (it is done).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fences.is_empty()
    }

    /// Its fences, as one: finished when all of them have.
    #[must_use]
    pub fn fence(&self) -> Arc<dyn Fence> {
        match self.fences.as_slice() {
            [one] => Arc::clone(one),
            _ => Arc::new(All(self.fences.clone())),
        }
    }

    /// The waiters' list, for `kqueue`.
    #[must_use]
    pub fn notify(&self) -> &Notify {
        &self.notify
    }

    /// `poll`: readable once it is done. A poller is recorded to be woken.
    #[must_use]
    pub fn poll(&self, events: i16, w: Option<&Arc<dyn Wake>>) -> i16 {
        if let Some(w) = w {
            self.notify.record(w);
        }
        if self.signaled() {
            events & (POLLIN | POLLRDNORM)
        } else {
            0
        }
    }

    /// The two joined: the fences of both, one of any that both have.
    ///
    /// # Errors
    ///
    /// `ENOMEM` past [`MAX_FENCES`].
    pub fn merge(a: &Self, b: &Self) -> Result<Arc<Self>, Errno> {
        let mut all: Vec<Arc<dyn Fence>> = Vec::new();
        for f in a.fences.iter().chain(b.fences.iter()) {
            if !all.iter().any(|g| Arc::ptr_eq(g, f)) {
                all.push(Arc::clone(f));
            }
        }
        if all.len() > MAX_FENCES {
            return Err(Errno::ENOMEM);
        }
        Ok(Self::new(all))
    }

    /// `SYNC_IOC_FILE_INFO`: the name, the status (1 finished, 0 still
    /// going), how many fences, and, when the caller gave room for them,
    /// each one's description. `arg` is the request's structure.
    pub(crate) fn info(&self, arg: &mut [u8]) -> Result<usize, Errno> {
        if arg.len() < INFO_LEN {
            return Err(Errno::EINVAL);
        }
        // No flags are defined.
        if u32_at(arg, 36)? != 0 {
            return Err(Errno::EINVAL);
        }
        let want = u32_at(arg, 40)?;
        let ptr = u64_at(arg, 48)?;
        let status = i32::from(self.signaled());
        arg[..32].fill(0);
        arg[..4].copy_from_slice(b"sync");
        arg[32..36].copy_from_slice(&status.to_le_bytes());
        let have = u32::try_from(self.fences.len()).unwrap_or(u32::MAX);
        arg[40..44].copy_from_slice(&have.to_le_bytes());
        // The first call asks how many; the second brings room for them.
        if ptr == 0 || want == 0 {
            return Ok(0);
        }
        if want < have {
            return Err(Errno::EINVAL);
        }
        let env = crate::env().ok_or(Errno::ENXIO)?;
        let mut out = alloc::vec![0u8; self.fences.len() * FENCE_INFO_LEN];
        for (f, e) in self
            .fences
            .iter()
            .zip(out.as_chunks_mut::<FENCE_INFO_LEN>().0.iter_mut())
        {
            e[..5].copy_from_slice(b"fence");
            e[32..35].copy_from_slice(b"drm");
            e[64..68].copy_from_slice(&i32::from(f.signaled()).to_le_bytes());
        }
        env.user.copy_out(ptr, &out)?;
        Ok(0)
    }

    /// `SYNC_IOC_MERGE`: a new descriptor for this file's fences and
    /// those of the descriptor `fd2` in the request.
    pub(crate) fn merge_request(
        &self,
        s: &sched::Sleepable,
        arg: &mut [u8],
    ) -> Result<usize, Errno> {
        if arg.len() < MERGE_LEN {
            return Err(Errno::EINVAL);
        }
        // No flags are defined, and the padding is zero.
        if u32_at(arg, 40)? != 0 || u32_at(arg, 44)? != 0 {
            return Err(Errno::EINVAL);
        }
        let env = crate::env().ok_or(Errno::ENXIO)?;
        let fd2 = i32::from_le_bytes(arg[32..36].try_into().map_err(|_| Errno::EINVAL)?);
        let other = (env.import_syncfile)(s, fd2)?;
        let merged = Self::merge(self, &other)?;
        let fd = (env.export_syncfile)(s, merged, true)?;
        arg[36..40].copy_from_slice(&fd.to_le_bytes());
        Ok(0)
    }
}

/// Several fences as one: finished when every one is.
struct All(Vec<Arc<dyn Fence>>);

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

    fn register(&self, w: &Arc<dyn Wake>) {
        for f in &self.0 {
            if !f.signaled() {
                f.register(w);
            }
        }
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

/// `SYNC_IOC_MERGE` and `SYNC_IOC_FILE_INFO` on a sync file: `req` is the
/// request number, `arg` its structure (copied in and out by the caller).
///
/// # Errors
///
/// `ENOTTY` for any other request.
pub fn ioctl(f: &SyncFile, s: &sched::Sleepable, req: u32, arg: &mut [u8]) -> Result<usize, Errno> {
    match req {
        abi::drm::SYNC_IOC_MERGE => f.merge_request(s, arg),
        abi::drm::SYNC_IOC_FILE_INFO => f.info(arg),
        _ => Err(Errno::ENOTTY),
    }
}
