// SPDX-License-Identifier: BSD-2-Clause
//! POSIX shared memory's system calls (`shm_open2`, `shm_unlink`,
//! `shm_rename`), `posix_fallocate`, and `_umtx_op`'s `UMTX_OP_SHM`, which
//! gives libthr one page of shared memory per process-shared pthread
//! object, found by the object's key.
//!
//! Behavior reference: sys/kern/uipc_shm.c, sys/kern/kern_umtx.c
//! (`umtx_shm`), sys/kern/vfs_syscalls.c (`kern_posix_fallocate`).

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use abi::errno::Errno;
use abi::fcntl::{
    FREAD, FWRITE, O_ACCMODE, O_CLOEXEC, O_CLOFORK, O_CREAT, O_EXCL, O_RDONLY, O_RDWR, O_TRUNC,
};
use abi::mman::{
    SHM_ALLOW_SEALING, SHM_ANON, SHM_GROW_ON_WRITE, SHM_LARGEPAGE, SHM_RENAME_EXCHANGE,
    SHM_RENAME_NOREPLACE,
};
use abi::umtx::{UMTX_SHM_ALIVE, UMTX_SHM_CREAT, UMTX_SHM_DESTROY, UMTX_SHM_LOOKUP};
use sched::Sleepable;
use sched::lock::{LockClass, Mutex};
use umtx::{Env, Id, Share, Type};
use vm::object::Object;

use super::{SysResult, int, process};
use crate::fs::fdtable::FdFlags;
use crate::fs::file::{File, FileKind};
use crate::fs::shm::{self, MAXPATHLEN, Shm};
use crate::uaccess::{StrError, copy_in_str_vec};
use crate::uthread::UserThread;

/// A path as `shm_copyin_path` takes it: starting with `/`.
fn path(at: u64) -> Result<Vec<u8>, Errno> {
    let p = copy_in_str_vec(at, MAXPATHLEN).map_err(|e| match e {
        StrError::Fault => Errno::EFAULT,
        StrError::TooLong => Errno::ENAMETOOLONG,
    })?;
    if p.first() != Some(&b'/') {
        return Err(Errno::EINVAL);
    }
    Ok(p)
}

/// Installs a descriptor for `shm`, open for `fflags`.
fn install(s: &Sleepable, shm: Arc<Shm>, fflags: u32, cloexec: bool) -> SysResult {
    let p = process()?;
    let limit = super::io::max_files(&p);
    let fd = p.files.lock(s).alloc(
        File::new(FileKind::Shm(shm), fflags)?,
        FdFlags {
            cloexec,
            clofork: false,
            resolve_beneath: false,
        },
        0,
        limit,
    )?;
    Ok(u64::from(fd.cast_unsigned()))
}

/// `shm_open2(path, flags, mode, shmflags, name)`.
pub(super) fn shm_open2(
    s: &Sleepable,
    upath: u64,
    flags: u64,
    mode: u64,
    shmflags: u64,
    _name: u64,
) -> SysResult {
    let flags = int(flags).cast_unsigned();
    let shmflags = int(shmflags).cast_unsigned();
    if shmflags & !(SHM_ALLOW_SEALING | SHM_GROW_ON_WRITE | SHM_LARGEPAGE) != 0 {
        return Err(Errno::EINVAL);
    }
    let acc = flags & O_ACCMODE;
    if acc != O_RDONLY && acc != O_RDWR {
        return Err(Errno::EINVAL);
    }
    if flags & !(O_ACCMODE | O_CREAT | O_EXCL | O_TRUNC | O_CLOEXEC | O_CLOFORK) != 0 {
        return Err(Errno::EINVAL);
    }
    // Objects of superpages are not made here.
    if shmflags & SHM_LARGEPAGE != 0 {
        return Err(Errno::ENOTTY);
    }
    let path = if upath == SHM_ANON {
        None
    } else {
        // Only anonymous objects in capability mode.
        let path = path(upath)?;
        crate::capsicum::refuse(crate::ktrace::CapFail::Namei(&path))?;
        Some(path)
    };
    let p = process()?;
    // Only the permission bits matter, so the truncation is harmless.
    #[allow(clippy::cast_possible_truncation, reason = "a mode_t")]
    let mode = int(mode).cast_unsigned() as u16;
    let cmode = mode & !p.umask.load(core::sync::atomic::Ordering::Relaxed) & 0o777;
    let fflags = if acc == O_RDWR { FREAD | FWRITE } else { FREAD };
    let shm = shm::open(s, path, fflags, flags, cmode, shmflags, &p.cred())?;
    install(s, shm, fflags, flags & O_CLOEXEC != 0)
}

/// `shm_unlink(path)`.
pub(super) fn shm_unlink(s: &Sleepable, upath: u64) -> SysResult {
    let path = path(upath)?;
    shm::unlink(s, &path, &process()?.cred())?;
    Ok(0)
}

/// `shm_rename(from, to, flags)`.
pub(super) fn shm_rename(s: &Sleepable, from: u64, to: u64, flags: u64) -> SysResult {
    let flags = int(flags).cast_unsigned();
    if flags & !(SHM_RENAME_NOREPLACE | SHM_RENAME_EXCHANGE) != 0 {
        return Err(Errno::EINVAL);
    }
    let (noreplace, exchange) = (
        flags & SHM_RENAME_NOREPLACE != 0,
        flags & SHM_RENAME_EXCHANGE != 0,
    );
    if noreplace && exchange {
        return Err(Errno::EINVAL);
    }
    // To or from anonymous makes no sense.
    if from == SHM_ANON || to == SHM_ANON {
        return Err(Errno::EINVAL);
    }
    let (from, to) = (path(from)?, path(to)?);
    shm::rename(s, &from, &to, noreplace, exchange, &process()?.cred())?;
    Ok(0)
}

/// `posix_fallocate(fd, offset, len)`, which returns its error rather
/// than setting `errno` (`kern_posix_fallocate`).
pub(super) fn posix_fallocate(s: &Sleepable, fd: u64, offset: u64, len: u64) -> SysResult {
    let r = (|| {
        let (offset, len) = (offset.cast_signed(), len.cast_signed());
        if offset < 0 || len <= 0 {
            return Err(Errno::EINVAL);
        }
        if offset > i64::MAX - len {
            return Err(Errno::EFBIG);
        }
        let f = super::io::current_file(s, fd, &crate::capsicum::rights::PWRITE)?;
        if f.flags() & FWRITE == 0 {
            return Err(Errno::EBADF);
        }
        match f.kind() {
            FileKind::Shm(shm) => shm.allocate(s, offset.cast_unsigned(), len.cast_unsigned()),
            FileKind::Pipe(_) | FileKind::Fifo(_) => Err(Errno::ESPIPE),
            FileKind::Vnode(vf) if vf.device.is_none() => {
                let v = vf.vnode.vnode();
                if v.kind() != vfs::Kind::Regular {
                    return Err(Errno::ENODEV);
                }
                // Space is not reserved ahead: the file just covers the
                // range.
                let end = (offset + len).cast_unsigned();
                if end > v.size(s)? {
                    let a = vfs::SetAttr {
                        size: Some(end),
                        ..vfs::SetAttr::default()
                    };
                    vfs::ops::setattr(s, v, &a, &vfs::Cred::root(), false)?;
                }
                Ok(())
            }
            _ => Err(Errno::ENODEV),
        }
    })();
    Ok(r.err().map_or(0, |e| e as u64))
}

/// `fspacectl(fd, cmd, rqsr, flags, rmsr)`: `SPACECTL_DEALLOC` on shared
/// memory; `rmsr`, if given, gets what is left of the range
/// (`kern_fspacectl`).
pub(super) fn fspacectl(
    s: &Sleepable,
    fd: u64,
    cmd: u64,
    rqsr: u64,
    flags: u64,
    rmsr: u64,
) -> SysResult {
    if rqsr == 0 {
        return Err(Errno::EINVAL);
    }
    let r = crate::uaccess::UserPtr::<[i64; 2]>::new(rqsr)
        .read()
        .map_err(|_| Errno::EFAULT)?;
    let put = |v: [i64; 2]| {
        if rmsr == 0 {
            return Ok(());
        }
        crate::uaccess::UserPtr::<[i64; 2]>::new(rmsr)
            .write(&v)
            .map_err(|_| Errno::EFAULT)
    };
    put(r)?;
    let [offset, len] = r;
    if int(cmd).cast_unsigned() != abi::fcntl::SPACECTL_DEALLOC
        || offset < 0
        || len <= 0
        || offset > i64::MAX - len
        || int(flags) != 0
    {
        return Err(Errno::EINVAL);
    }
    let f = super::io::current_file(s, fd, &crate::capsicum::rights::PWRITE)?;
    let shm = match f.kind() {
        FileKind::Shm(m) => m,
        FileKind::Pipe(_)
        | FileKind::Fifo(_)
        | FileKind::Socket(_)
        | FileKind::Inet(_)
        | FileKind::Netlink(_)
        | FileKind::Process(_)
        | FileKind::Jail(_)
        | FileKind::Kqueue(_)
        | FileKind::Timerfd(_)
        | FileKind::Eventfd(_)
        | FileKind::Inotify(_)
        | FileKind::Pts(_) => return Err(Errno::ESPIPE),
        FileKind::Vnode(_) => return Err(Errno::ENODEV),
    };
    if f.flags() & FWRITE == 0 {
        return Err(Errno::EBADF);
    }
    let (off, left) = shm.deallocate(s, offset.cast_unsigned(), len.cast_unsigned())?;
    put([off.cast_signed(), left.cast_signed()])?;
    Ok(0)
}

static REGS_LOCK: LockClass = LockClass::new("umtx shm", 320);

/// One page of shared memory registered for a key (`struct
/// umtx_shm_reg`): the memory object holding the key's word and its
/// offset, and the page. It lasts while the key's object does, or until
/// destroyed.
struct Reg {
    object: Weak<Object>,
    id: Id,
    shm: Arc<Shm>,
}

static REGS: Mutex<Vec<Reg>> = Mutex::new(&REGS_LOCK, Vec::new());

/// The objects `UMTX_SHM_DESTROY` destroyed pages of, so their mappings
/// report dead (`OBJ_UMTXDEAD`).
static DEAD: Mutex<Vec<Weak<Object>>> = Mutex::new(&REGS_LOCK, Vec::new());

/// `UMTX_OP_SHM`: makes (`UMTX_SHM_CREAT`) or finds (`UMTX_SHM_LOOKUP`)
/// the page registered for the word at `addr` and returns a descriptor
/// for it; or destroys the registration (`UMTX_SHM_DESTROY`); or says
/// whether the page mapped at `addr` is still registered
/// (`UMTX_SHM_ALIVE`: `ENOTTY` once destroyed).
pub(super) fn umtx_shm(s: &Sleepable, t: &UserThread, addr: u64, flags: u32) -> SysResult {
    let which = flags & (UMTX_SHM_CREAT | UMTX_SHM_LOOKUP | UMTX_SHM_DESTROY | UMTX_SHM_ALIVE);
    if which.count_ones() != 1 {
        return Err(Errno::EINVAL);
    }
    if which == UMTX_SHM_ALIVE {
        let space = t.process().space();
        let object = space
            .object_for(s, addr::VirtAddr::new(addr), vm::fault::Access::Read, true)
            .map_err(|_| Errno::EFAULT)?;
        let Some((object, _)) = object else {
            return Err(Errno::EINVAL);
        };
        let dead = DEAD
            .lock(s)
            .iter()
            .any(|w| w.upgrade().is_some_and(|o| Arc::ptr_eq(&o, &object)));
        return if dead { Err(Errno::ENOTTY) } else { Ok(0) };
    }
    let cx = crate::umtx::Ctx { s, t };
    let key = cx.key(addr, Type::Shm, Share::Process)?;
    let object = key
        .hold
        .clone()
        .and_then(|h| h.downcast::<Object>().ok())
        .ok_or(Errno::EFAULT)?;
    let mut regs = REGS.lock(s);
    // Pages whose key's object went went with it.
    regs.retain(|r| r.object.strong_count() > 0);
    let found = regs.iter().position(|r| r.id == key.id);
    match which {
        UMTX_SHM_DESTROY => {
            let r = regs.remove(found.ok_or(Errno::ESRCH)?);
            drop(regs);
            let mut dead = DEAD.lock(s);
            dead.retain(|w| w.strong_count() > 0);
            dead.push(Arc::downgrade(r.shm.object()));
            Ok(0)
        }
        _ => {
            let shm = match found {
                Some(i) => Arc::clone(&regs[i].shm),
                None if which == UMTX_SHM_CREAT => {
                    let p = t.process();
                    let shm = shm::open(s, None, FREAD | FWRITE, 0, 0, 0, &p.cred())?;
                    shm.truncate(s, vm::PAGE_SIZE)?;
                    regs.push(Reg {
                        object: Arc::downgrade(&object),
                        id: key.id,
                        shm: Arc::clone(&shm),
                    });
                    shm
                }
                None => return Err(Errno::ESRCH),
            };
            drop(regs);
            install(s, shm, FREAD | FWRITE, true)
        }
    }
}
