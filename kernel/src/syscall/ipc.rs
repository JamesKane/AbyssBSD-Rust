// SPDX-License-Identifier: BSD-2-Clause
//! System V IPC's system calls: `shmget`, `shmat`, `shmdt`, `shmctl`,
//! `semget`, `semop`, `__semctl`, `msgget`, `msgsnd`, `msgrcv`, and
//! `msgctl`. They copy their arguments in and results out around
//! [`crate::sysv`], which keeps the objects.
//!
//! Behavior reference: sys/kern/sysv_shm.c, sys/kern/sysv_sem.c,
//! sys/kern/sysv_msg.c.

use alloc::vec::Vec;

use abi::errno::Errno;
use abi::ipc::*;
use addr::VirtAddr;
use sched::Sleepable;
use vm::PAGE_SIZE;
use vm::map::{Inherit, Prot};

use super::{SysResult, int, process};
use crate::mm::space::{AddressSpace, Anon};
use crate::sysv;
use crate::uaccess::{Plain, UserPtr, copy_in, copy_out};

// SAFETY: integers only, padding explicit.
unsafe impl Plain for IpcPerm {}
// SAFETY: integers only, padding explicit.
unsafe impl Plain for ShmidDs {}
// SAFETY: integers only, padding explicit.
unsafe impl Plain for SemidDs {}
// SAFETY: integers only, padding explicit.
unsafe impl Plain for MsqidDs {}

/// A `key_t` argument (a `long`).
fn key(a: u64) -> i64 {
    a.cast_signed()
}

/// `shmget(key, size, shmflg)`.
pub(super) fn shmget(k: u64, size: u64, flg: u64) -> SysResult {
    let p = process()?;
    let id = sysv::shmget(key(k), size, int(flg).cast_unsigned(), &p.cred(), p.pid())?;
    Ok(u64::from(id.cast_unsigned()))
}

/// `shmat(shmid, shmaddr, shmflg)`: the address it attached at.
pub(super) fn shmat(s: &Sleepable, id: u64, addr: u64, flg: u64) -> SysResult {
    let p = process()?;
    let (id, flg) = (int(id), int(flg).cast_unsigned());
    let space = p.space();
    let attached = space.sysv_shm.lock().len();
    let rdonly = flg & SHM_RDONLY != 0;
    let (object, size) = sysv::shm_attach(id, rdonly, attached, &p.cred(), p.pid())?;
    let prot = if rdonly { Prot::READ } else { Prot::RW };
    let fixed = if addr == 0 {
        None
    } else if flg & SHM_RND != 0 {
        Some(addr & !(PAGE_SIZE - 1))
    } else if addr.is_multiple_of(PAGE_SIZE) {
        Some(addr)
    } else {
        sysv::shm_unattach(id);
        return Err(Errno::EINVAL);
    };
    let a = Anon {
        fixed: fixed.map(VirtAddr::new),
        // Without SHM_REMAP, only into free space.
        exclusive: flg & SHM_REMAP == 0,
        hint: VirtAddr::new(p.mmap_base()),
        len: size.next_multiple_of(PAGE_SIZE),
        prot,
        max_prot: prot,
        inherit: Inherit::Share,
        ..Anon::default()
    };
    match space.map_object(s, &a, object, 0, false) {
        Ok(at) => {
            space.sysv_shm.lock().push((at.get(), id));
            Ok(at.get())
        }
        Err(_) => {
            sysv::shm_unattach(id);
            Err(Errno::ENOMEM)
        }
    }
}

/// `shmdt(shmaddr)`.
pub(super) fn shmdt(s: &Sleepable, addr: u64) -> SysResult {
    let p = process()?;
    let space: alloc::sync::Arc<AddressSpace> = p.space();
    let found = {
        let list = space.sysv_shm.lock();
        list.iter().position(|&(va, _)| va == addr).map(|i| list[i])
    };
    let Some((va, id)) = found else {
        return Err(Errno::EINVAL);
    };
    let size = sysv::shm_size(id).ok_or(Errno::EINVAL)?;
    let end = va
        .checked_add(size.next_multiple_of(PAGE_SIZE))
        .ok_or(Errno::EINVAL)?;
    space
        .unmap(s, VirtAddr::new(va)..VirtAddr::new(end))
        .map_err(|_| Errno::EINVAL)?;
    space.sysv_shm.lock().retain(|&(a, _)| a != va);
    sysv::shm_unattach(id);
    Ok(0)
}

/// `shmctl(shmid, cmd, buf)`; the Linux-only commands are `EINVAL`.
pub(super) fn shmctl(id: u64, cmd: u64, buf: u64) -> SysResult {
    let cmd = int(cmd).cast_unsigned();
    if matches!(cmd, IPC_INFO | SHM_INFO | SHM_STAT) {
        return Err(Errno::EINVAL);
    }
    let set = if cmd == IPC_SET {
        Some(
            UserPtr::<ShmidDs>::new(buf)
                .read()
                .map_err(|_| Errno::EFAULT)?,
        )
    } else {
        None
    };
    let p = process()?;
    if let Some(ds) = sysv::shmctl(int(id), cmd, set.as_ref(), &p.cred())? {
        UserPtr::<ShmidDs>::new(buf)
            .write(&ds)
            .map_err(|_| Errno::EFAULT)?;
    }
    Ok(0)
}

/// `semget(key, nsems, semflg)`.
pub(super) fn semget(k: u64, nsems: u64, flg: u64) -> SysResult {
    let p = process()?;
    let id = sysv::semget(key(k), int(nsems), int(flg).cast_unsigned(), &p.cred())?;
    Ok(u64::from(id.cast_unsigned()))
}

/// `semop(semid, sops, nsops)`.
pub(super) fn semop(s: &Sleepable, id: u64, sops: u64, nsops: u64) -> SysResult {
    let n = usize::try_from(nsops).map_err(|_| Errno::E2BIG)?;
    if n > sysv::limits::SEMOPM {
        return Err(Errno::E2BIG);
    }
    let mut raw = alloc::vec![0u8; n * 6];
    copy_in(&mut raw, sops).map_err(|_| Errno::EFAULT)?;
    let ops: Vec<Sembuf> = raw
        .as_chunks::<6>()
        .0
        .iter()
        .map(|c| Sembuf {
            sem_num: u16::from_ne_bytes([c[0], c[1]]),
            sem_op: i16::from_ne_bytes([c[2], c[3]]),
            sem_flg: i16::from_ne_bytes([c[4], c[5]]),
        })
        .collect();
    let p = process()?;
    sysv::semop(s, int(id), &ops, &p.cred(), p.pid())?;
    Ok(0)
}

/// `__semctl(semid, semnum, cmd, arg)`: `arg` points at a `union semun`
/// (a value, or a pointer to the buffer or array).
pub(super) fn semctl(id: u64, num: u64, cmd: u64, arg: u64) -> SysResult {
    let (id, num, cmd) = (int(id), int(num), int(cmd).cast_unsigned());
    let semun = if matches!(
        cmd,
        SEM_STAT | IPC_SET | IPC_STAT | GETALL | SETVAL | SETALL
    ) {
        UserPtr::<u64>::new(arg).read().map_err(|_| Errno::EFAULT)?
    } else {
        0
    };
    let p = process()?;
    let cred = p.cred();
    let r = match cmd {
        SEM_STAT | IPC_STAT => {
            let mut ds = SemidDs::default();
            let r = sysv::semctl(id, num, cmd, sysv::SemArg::Ds(&mut ds), &cred)?;
            // The kernel's own pointer is nobody else's business.
            ds.sem_base = 0;
            UserPtr::<SemidDs>::new(semun)
                .write(&ds)
                .map_err(|_| Errno::EFAULT)?;
            r
        }
        IPC_SET => {
            let mut ds = UserPtr::<SemidDs>::new(semun)
                .read()
                .map_err(|_| Errno::EFAULT)?;
            sysv::semctl(id, num, cmd, sysv::SemArg::Ds(&mut ds), &cred)?
        }
        GETALL => {
            let mut vals = Vec::new();
            sysv::semctl(id, num, cmd, sysv::SemArg::Array(&mut vals), &cred)?;
            let bytes: Vec<u8> = vals.iter().flat_map(|v| v.to_ne_bytes()).collect();
            copy_out(semun, &bytes).map_err(|_| Errno::EFAULT)?;
            0
        }
        SETALL => {
            let n = sysv::sem_count(id).ok_or(Errno::EINVAL)?;
            let mut raw = alloc::vec![0u8; n * 2];
            copy_in(&mut raw, semun).map_err(|_| Errno::EFAULT)?;
            let mut vals: Vec<u16> = raw
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_ne_bytes(*c))
                .collect();
            sysv::semctl(id, num, cmd, sysv::SemArg::Array(&mut vals), &cred)?
        }
        SETVAL => {
            // `union semun`'s `int val`, the low half.
            #[allow(clippy::cast_possible_truncation, reason = "an int")]
            let v = semun as u32 as i32;
            sysv::semctl(id, num, cmd, sysv::SemArg::Val(v), &cred)?
        }
        _ => sysv::semctl(id, num, cmd, sysv::SemArg::None, &cred)?,
    };
    Ok(r.cast_unsigned())
}

/// `msgget(key, msgflg)`.
pub(super) fn msgget(k: u64, flg: u64) -> SysResult {
    let p = process()?;
    let id = sysv::msgget(key(k), int(flg).cast_unsigned(), &p.cred())?;
    Ok(u64::from(id.cast_unsigned()))
}

/// `msgsnd(msqid, msgp, msgsz, msgflg)`: `msgp` is a `long` type and the
/// text.
pub(super) fn msgsnd(s: &Sleepable, id: u64, msgp: u64, size: u64, flg: u64) -> SysResult {
    let mtype = UserPtr::<i64>::new(msgp)
        .read()
        .map_err(|_| Errno::EFAULT)?;
    let n = usize::try_from(size).map_err(|_| Errno::EINVAL)?;
    if n > sysv::limits::MSGMAX {
        return Err(Errno::EINVAL);
    }
    let mut text = alloc::vec![0u8; n];
    copy_in(&mut text, msgp.wrapping_add(8)).map_err(|_| Errno::EFAULT)?;
    let p = process()?;
    sysv::msgsnd(
        s,
        int(id),
        mtype,
        text,
        int(flg).cast_unsigned(),
        &p.cred(),
        p.pid(),
    )?;
    Ok(0)
}

/// `msgrcv(msqid, msgp, msgsz, msgtyp, msgflg)`: the bytes of text.
pub(super) fn msgrcv(
    s: &Sleepable,
    id: u64,
    msgp: u64,
    size: u64,
    typ: u64,
    flg: u64,
) -> SysResult {
    let size = usize::try_from(size.cast_signed()).map_err(|_| Errno::EINVAL)?;
    let p = process()?;
    let (mtype, text) = sysv::msgrcv(
        s,
        int(id),
        size,
        typ.cast_signed(),
        int(flg).cast_unsigned(),
        &p.cred(),
        p.pid(),
    )?;
    copy_out(msgp.wrapping_add(8), &text).map_err(|_| Errno::EFAULT)?;
    UserPtr::<i64>::new(msgp)
        .write(&mtype)
        .map_err(|_| Errno::EFAULT)?;
    Ok(text.len() as u64)
}

/// `msgctl(msqid, cmd, buf)`.
pub(super) fn msgctl(id: u64, cmd: u64, buf: u64) -> SysResult {
    let cmd = int(cmd).cast_unsigned();
    let set = if cmd == IPC_SET {
        Some(
            UserPtr::<MsqidDs>::new(buf)
                .read()
                .map_err(|_| Errno::EFAULT)?,
        )
    } else {
        None
    };
    let p = process()?;
    if let Some(ds) = sysv::msgctl(int(id), cmd, set.as_ref(), &p.cred())? {
        UserPtr::<MsqidDs>::new(buf)
            .write(&ds)
            .map_err(|_| Errno::EFAULT)?;
    }
    Ok(0)
}
