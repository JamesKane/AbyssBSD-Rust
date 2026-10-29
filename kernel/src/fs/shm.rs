// SPDX-License-Identifier: BSD-2-Clause
//! POSIX shared memory (`shm_open2`, `shm_unlink`, `shm_rename`; libc's
//! `shm_open` and `memfd_create` build on them): memory objects with a
//! size, an owner, a mode, and seals, named by paths in one table (or
//! anonymous), opened as descriptors that read, write, map, and truncate
//! like regular files.
//!
//! The data is an anonymous VM object, read and written through the page
//! cache as tmpfs's files are, and mapped as itself. Seals forbid
//! shrinking, growing, or writing, and once `F_SEAL_SEAL` is set, more
//! seals; a write seal waits until no shared mapping may write.
//!
//! Behavior reference: sys/kern/uipc_shm.c.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use abi::errno::Errno;
use abi::fcntl::{F_SEAL_GROW, F_SEAL_SEAL, F_SEAL_SHRINK, F_SEAL_WRITE, FREAD, FWRITE};
use abi::mman::{MAP_SHARED, SHM_GROW_ON_WRITE};
use abi::stat::{S_IFREG, Stat};
use abi::time::Timespec;
use sched::Sleepable;
use sched::lock::{LockClass, Mutex};
use vfs::Uio;
use vfs::ops::{VREAD, VWRITE};
use vm::PAGE_SIZE;
use vm::map::Prot;
use vm::object::Object;

use crate::cred::{Priv, Ucred};

static NAMES_LOCK: LockClass = LockClass::new("shm names", 330);
static SHM_LOCK: LockClass = LockClass::new("shm", 340);

/// Named objects by path (FreeBSD's `shm_dictionary`).
static NAMES: Mutex<BTreeMap<Vec<u8>, Arc<Shm>>> = Mutex::new(&NAMES_LOCK, BTreeMap::new());

/// Inode numbers, one per object made (`shm_ino_unr`).
static NEXT_INO: AtomicU64 = AtomicU64::new(1);

/// The device number `fstat` reports for every object (FreeBSD gives
/// them one devfs inode number).
const SHM_DEV: u64 = 0x5348_4d00;

/// The longest path (`MAXPATHLEN`).
pub(crate) const MAXPATHLEN: usize = 1024;

/// A shared-memory object (FreeBSD's `struct shmfd`).
pub(crate) struct Shm {
    object: Arc<Object>,
    ino: u64,
    st: Mutex<State>,
}

struct State {
    size: u64,
    seals: u32,
    /// The `shm_open2` flags it was made with.
    flags: u32,
    mode: u16,
    uid: u32,
    gid: u32,
    atime: Timespec,
    mtime: Timespec,
    ctime: Timespec,
    birthtime: Timespec,
    /// Its name while it has one.
    path: Option<Vec<u8>>,
}

/// Limits a transfer to the bytes before the end.
struct Limited<'a> {
    inner: &'a mut dyn Uio,
    left: usize,
}

impl Uio for Limited<'_> {
    fn resid(&self) -> usize {
        self.inner.resid().min(self.left)
    }

    fn copy_out(&mut self, data: &[u8]) -> Result<usize, Errno> {
        let n = data.len().min(self.left);
        let moved = self.inner.copy_out(&data[..n])?;
        self.left -= moved;
        Ok(moved)
    }

    fn copy_in(&mut self, buf: &mut [u8]) -> Result<usize, Errno> {
        let n = buf.len().min(self.left);
        let moved = self.inner.copy_in(&mut buf[..n])?;
        self.left -= moved;
        Ok(moved)
    }
}

/// Whether `cred` may open an object owned by `uid`/`gid` with `mode` for
/// `fflags` (`shm_access`, `vaccess`).
fn access(st: &State, cred: &Ucred, fflags: u32) -> Result<(), Errno> {
    let mut want = 0;
    if fflags & FREAD != 0 {
        want |= VREAD;
    }
    if fflags & FWRITE != 0 {
        want |= VWRITE;
    }
    let a = vfs::Attr {
        mode: st.mode,
        uid: st.uid,
        gid: st.gid,
        ..vfs::Attr::default()
    };
    vfs::ops::access(&a, vfs::Kind::Regular, cred, want)
}

impl Shm {
    /// A new empty object (`shm_alloc`).
    fn new(cred: &Ucred, mode: u16, seals: u32, flags: u32) -> Arc<Self> {
        let now = super::now();
        let object = Object::anonymous(crate::mm::space::page_ops());
        object.set_limit(0);
        let shm = Arc::new(Self {
            object,
            ino: NEXT_INO.fetch_add(1, Ordering::Relaxed),
            st: Mutex::new(
                &SHM_LOCK,
                State {
                    size: 0,
                    seals,
                    flags,
                    mode,
                    uid: cred.uid,
                    gid: cred.gid,
                    atime: now,
                    mtime: now,
                    ctime: now,
                    birthtime: now,
                    path: None,
                },
            ),
        });
        // Its mappings name it (`kern.proc.vmmap`).
        crate::kinfo::vm::note_shm(&shm.object, &shm);
        shm
    }

    /// Reads at `offset`, up to the end.
    pub(crate) fn read(
        &self,
        s: &Sleepable,
        offset: u64,
        uio: &mut dyn Uio,
    ) -> Result<usize, Errno> {
        let size = self.st.lock(s).size;
        vfs::pagecache::read(&self.object, size, offset, uio)
    }

    /// Writes at `offset` (at the end if `append`), up to the end unless
    /// the object grows on write (`SHM_GROW_ON_WRITE`). Returns the bytes
    /// written and where they started.
    pub(crate) fn write(
        &self,
        s: &Sleepable,
        offset: Option<u64>,
        uio: &mut dyn Uio,
    ) -> Result<(usize, u64), Errno> {
        let mut st = self.st.lock(s);
        let offset = offset.unwrap_or(st.size);
        let want = uio.resid() as u64;
        let grow = st.flags & SHM_GROW_ON_WRITE != 0;
        let end = match offset.checked_add(want) {
            Some(e) if i64::try_from(e).is_ok() => e,
            _ if grow => return Err(Errno::EFBIG),
            _ => st.size,
        };
        if st.seals & F_SEAL_WRITE != 0 {
            return Err(Errno::EPERM);
        }
        if grow && end > st.size {
            self.resize(&mut st, end)?;
        }
        if offset >= st.size {
            return Ok((0, offset));
        }
        let mut lim = Limited {
            inner: uio,
            left: usize::try_from(st.size - offset).unwrap_or(usize::MAX),
        };
        let n = vfs::pagecache::write(&self.object, offset, &mut lim)?;
        if n > 0 {
            st.mtime = super::now();
            st.ctime = st.mtime;
        }
        Ok((n, offset))
    }

    /// Sets the size, as the seals allow (`shm_dotruncate_locked`).
    fn resize(&self, st: &mut State, len: u64) -> Result<(), Errno> {
        if len == st.size {
            return Ok(());
        }
        let seal = if len < st.size {
            F_SEAL_SHRINK
        } else {
            F_SEAL_GROW
        };
        if st.seals & seal != 0 {
            return Err(Errno::EPERM);
        }
        vfs::pagecache::resize(&self.object, st.size, len);
        self.object.set_limit(len.div_ceil(PAGE_SIZE));
        st.size = len;
        st.ctime = super::now();
        st.mtime = st.ctime;
        Ok(())
    }

    /// `ftruncate`.
    pub(crate) fn truncate(&self, s: &Sleepable, len: u64) -> Result<(), Errno> {
        self.resize(&mut self.st.lock(s), len)
    }

    /// `posix_fallocate`: grows to cover `[offset, offset + len)`.
    pub(crate) fn allocate(&self, s: &Sleepable, offset: u64, len: u64) -> Result<(), Errno> {
        let end = offset.checked_add(len).ok_or(Errno::EFBIG)?;
        let mut st = self.st.lock(s);
        if end > st.size {
            self.resize(&mut st, end)
                .map_err(|e| if e == Errno::ENOMEM { Errno::ENOSPC } else { e })?;
        }
        Ok(())
    }

    /// Its mode, size, and name if it has one, for process information
    /// (`shm_fill_kinfo`).
    pub(crate) fn kinfo(&self, s: &Sleepable) -> (u16, u64, Option<Vec<u8>>) {
        let st = self.st.lock(s);
        (st.mode, st.size, st.path.clone())
    }

    /// `fstat` (`shm_stat`).
    pub(crate) fn stat(&self, s: &Sleepable) -> Stat {
        let st = self.st.lock(s);
        Stat {
            st_dev: SHM_DEV,
            st_ino: self.ino,
            st_nlink: Arc::strong_count(&self.object) as u64,
            st_mode: S_IFREG | st.mode,
            st_uid: st.uid,
            st_gid: st.gid,
            st_atim: st.atime,
            st_mtim: st.mtime,
            st_ctim: st.ctime,
            st_birthtim: st.birthtime,
            st_size: st.size.cast_signed(),
            // FreeBSD 15's: the pages it holds, in `st_blksize` units.
            st_blocks: i64::try_from(self.object.resident()).unwrap_or(i64::MAX),
            st_blksize: 4096,
            ..Stat::default()
        }
    }

    /// What `mmap` maps, and the most rights it may have (`shm_mmap`):
    /// a private mapping may always write; a shared one only from a
    /// descriptor open for writing, and not once write-sealed. The flag
    /// says the mapping counts toward the write mappings a write seal
    /// waits out.
    pub(crate) fn mmap(
        &self,
        s: &Sleepable,
        fflags: u32,
        flags: u32,
        prot: Prot,
    ) -> Result<(Arc<Object>, Prot, bool), Errno> {
        let mut st = self.st.lock(s);
        let mut max = Prot::NONE;
        if fflags & FREAD != 0 {
            max = max.union(Prot::EXEC).union(Prot::READ);
        }
        let writecnt = if flags & MAP_SHARED == 0 {
            max = max.union(Prot::WRITE);
            false
        } else {
            if fflags & FWRITE != 0 && st.seals & F_SEAL_WRITE == 0 {
                max = max.union(Prot::WRITE);
            }
            let w = max.contains(Prot::WRITE);
            if !w && prot.contains(Prot::WRITE) {
                return Err(Errno::EACCES);
            }
            w
        };
        st.atime = super::now();
        Ok((Arc::clone(&self.object), max, writecnt))
    }

    /// `fchmod`: the owner (or with `PRIV_VFS_ADMIN`) sets the permission
    /// bits.
    pub(crate) fn chmod(&self, s: &Sleepable, cred: &Ucred, mode: u16) -> Result<(), Errno> {
        let mut st = self.st.lock(s);
        if cred.uid != st.uid {
            cred.priv_check(Priv::VfsAdmin)?;
        }
        st.mode = mode & 0o777;
        st.ctime = super::now();
        Ok(())
    }

    /// `fchown` (`shm_chown`): giving it away, or to a group one is not
    /// in, needs `PRIV_VFS_CHOWN`.
    pub(crate) fn chown(
        &self,
        s: &Sleepable,
        cred: &Ucred,
        uid: Option<u32>,
        gid: Option<u32>,
    ) -> Result<(), Errno> {
        let mut st = self.st.lock(s);
        let uid = uid.unwrap_or(st.uid);
        let gid = gid.unwrap_or(st.gid);
        if (uid != st.uid && uid != cred.uid) || (gid != st.gid && !cred.in_group(gid)) {
            cred.priv_check(Priv::VfsChown)?;
        }
        st.uid = uid;
        st.gid = gid;
        st.ctime = super::now();
        Ok(())
    }

    /// `fspacectl(SPACECTL_DEALLOC)` (`shm_deallocate`): zeros
    /// `[offset, offset + len)`, up to the end, giving whole pages back.
    /// Returns where the range now starts and what is left of it (none).
    pub(crate) fn deallocate(
        &self,
        s: &Sleepable,
        offset: u64,
        len: u64,
    ) -> Result<(u64, u64), Errno> {
        let st = self.st.lock(s);
        if st.seals & F_SEAL_WRITE != 0 {
            return Err(Errno::EPERM);
        }
        if offset >= st.size {
            return Ok((offset, 0));
        }
        let end = offset.saturating_add(len).min(st.size);
        // Zeros part of page `index`, `[from, to)` within it.
        let zero = |index: u64, from: usize, to: usize| {
            if let Some(pfn) = self.object.page_here(index) {
                self.object.access(pfn, &mut |p| p[from..to].fill(0));
            }
        };
        let page = PAGE_SIZE;
        let within = |v: u64| usize::try_from(v % page).unwrap_or(0);
        let (first, last) = (offset / page, end / page);
        let (head, tail) = (within(offset), within(end));
        if first == last {
            zero(first, head, tail);
        } else {
            let whole_from = if head != 0 {
                zero(first, head, within(page - 1) + 1);
                first + 1
            } else {
                first
            };
            if whole_from < last {
                self.object.deallocate(whole_from, last);
            }
            if tail != 0 {
                zero(last, 0, tail);
            }
        }
        Ok((end, 0))
    }

    /// `F_ADD_SEALS` (`shm_add_seals`).
    pub(crate) fn add_seals(&self, s: &Sleepable, seals: u32) -> Result<(), Errno> {
        let mut st = self.st.lock(s);
        // Even seals already set fail once sealing is.
        if st.seals & F_SEAL_SEAL != 0 {
            return Err(Errno::EPERM);
        }
        let new = seals & !st.seals;
        if new & F_SEAL_WRITE != 0 && self.object.writemappings() > 0 {
            return Err(Errno::EBUSY);
        }
        st.seals |= new;
        Ok(())
    }

    /// `F_GET_SEALS`.
    pub(crate) fn seals(&self, s: &Sleepable) -> u32 {
        self.st.lock(s).seals
    }

    /// `lseek` (`shm_seek`): from the start, the offset, or the end, but
    /// never before the start or past the end.
    pub(crate) fn seek(
        &self,
        s: &Sleepable,
        cur: u64,
        offset: i64,
        whence: u32,
    ) -> Result<u64, Errno> {
        use abi::fcntl::{SEEK_CUR, SEEK_END, SEEK_SET};
        let size = self.st.lock(s).size;
        let base = match whence {
            SEEK_SET => 0,
            SEEK_CUR => i64::try_from(cur).map_err(|_| Errno::EOVERFLOW)?,
            SEEK_END => i64::try_from(size).map_err(|_| Errno::EOVERFLOW)?,
            _ => return Err(Errno::EINVAL),
        };
        let new = base.checked_add(offset).ok_or(Errno::EOVERFLOW)?;
        match u64::try_from(new) {
            Ok(n) if n <= size => Ok(n),
            _ => Err(Errno::EINVAL),
        }
    }

    /// The object behind it (`UMTX_OP_SHM`'s descriptors map it).
    pub(crate) fn object(&self) -> &Arc<Object> {
        &self.object
    }
}

/// `shm_open2`'s work (`kern_shm_open2`): opens the object named `path`
/// (`None` for a new anonymous one) for `fflags` (`FREAD`, `FWRITE`),
/// making it as `oflags` say (`O_CREAT`, `O_EXCL`, `O_TRUNC`) with
/// permission bits `mode`.
pub(crate) fn open(
    s: &Sleepable,
    path: Option<Vec<u8>>,
    fflags: u32,
    oflags: u32,
    mode: u16,
    shmflags: u32,
    cred: &Ucred,
) -> Result<Arc<Shm>, Errno> {
    use abi::fcntl::{O_CREAT, O_EXCL, O_TRUNC};
    use abi::mman::SHM_ALLOW_SEALING;
    let seals = if shmflags & SHM_ALLOW_SEALING != 0 {
        0
    } else {
        F_SEAL_SEAL
    };
    let Some(path) = path else {
        // A read-only anonymous object is pointless.
        if fflags & FWRITE == 0 {
            return Err(Errno::EINVAL);
        }
        return Ok(Shm::new(cred, mode, seals, shmflags));
    };
    let mut names = NAMES.lock(s);
    if let Some(shm) = names.get(&path) {
        let shm = Arc::clone(shm);
        let mut st = shm.st.lock(s);
        let add = seals & !st.seals;
        if st.seals & F_SEAL_SEAL != 0 && add != 0 {
            return Err(Errno::EPERM);
        }
        if oflags & (O_CREAT | O_EXCL) == O_CREAT | O_EXCL {
            return Err(Errno::EEXIST);
        }
        if shmflags != 0 && shmflags != st.flags {
            return Err(Errno::EINVAL);
        }
        access(&st, cred, fflags)?;
        if oflags & O_TRUNC != 0 && fflags & FWRITE != 0 {
            shm.resize(&mut st, 0)?;
        }
        st.seals |= add;
        drop(st);
        return Ok(shm);
    }
    if oflags & O_CREAT == 0 {
        return Err(Errno::ENOENT);
    }
    let shm = Shm::new(cred, mode, seals, shmflags);
    shm.st.lock(s).path = Some(path.clone());
    names.insert(path, Arc::clone(&shm));
    Ok(shm)
}

/// Takes `path`'s name away (`shm_remove`): who may read and write it may
/// unlink it.
fn remove(
    s: &Sleepable,
    names: &mut BTreeMap<Vec<u8>, Arc<Shm>>,
    path: &[u8],
    cred: &Ucred,
) -> Result<Arc<Shm>, Errno> {
    let shm = names.get(path).ok_or(Errno::ENOENT)?;
    {
        let mut st = shm.st.lock(s);
        access(&st, cred, FREAD | FWRITE)?;
        st.path = None;
    }
    names.remove(path).ok_or(Errno::ENOENT)
}

/// Every named object, by name (`kern.ipc.posix_shm_list`).
pub(crate) fn named(s: &Sleepable) -> Vec<Arc<Shm>> {
    NAMES.lock(s).values().cloned().collect()
}

fn insert(s: &Sleepable, names: &mut BTreeMap<Vec<u8>, Arc<Shm>>, path: Vec<u8>, shm: Arc<Shm>) {
    shm.st.lock(s).path = Some(path.clone());
    names.insert(path, shm);
}

/// `shm_unlink`.
pub(crate) fn unlink(s: &Sleepable, path: &[u8], cred: &Ucred) -> Result<(), Errno> {
    remove(s, &mut NAMES.lock(s), path, cred).map(drop)
}

/// `shm_rename` (`sys_shm_rename`): moves `from`'s object to `to`,
/// replacing what is there unless `noreplace` (`EEXIST`), or swapping the
/// two if `exchange`.
pub(crate) fn rename(
    s: &Sleepable,
    from: &[u8],
    to: &[u8],
    noreplace: bool,
    exchange: bool,
    cred: &Ucred,
) -> Result<(), Errno> {
    if from == to {
        return Ok(());
    }
    let mut names = NAMES.lock(s);
    if !names.contains_key(from) {
        return Err(Errno::ENOENT);
    }
    if noreplace && names.contains_key(to) {
        return Err(Errno::EEXIST);
    }
    let moving = remove(s, &mut names, from, cred)?;
    let displaced = match remove(s, &mut names, to, cred) {
        Ok(d) => Some(d),
        Err(Errno::ENOENT) => None,
        Err(e) => {
            // Put it back and give up.
            insert(s, &mut names, from.to_vec(), moving);
            return Err(e);
        }
    };
    insert(s, &mut names, to.to_vec(), moving);
    if exchange && let Some(d) = displaced {
        insert(s, &mut names, from.to_vec(), d);
    }
    Ok(())
}
