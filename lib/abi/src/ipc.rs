// SPDX-License-Identifier: BSD-2-Clause
//! System V IPC: permissions and keys (`sys/ipc.h`), shared memory
//! (`sys/shm.h`), semaphores (`sys/sem.h`), and message queues
//! (`sys/msg.h`).

use crate::layout::{Arches, ConstValue};

/// `struct ipc_perm`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IpcPerm {
    pub cuid: u32,
    pub cgid: u32,
    pub uid: u32,
    pub gid: u32,
    pub mode: u16,
    pub seq: u16,
    pub _pad: u32,
    pub key: i64,
}

/// `struct shmid_ds`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShmidDs {
    pub shm_perm: IpcPerm,
    pub shm_segsz: u64,
    pub shm_lpid: i32,
    pub shm_cpid: i32,
    pub shm_nattch: u32,
    pub _pad: u32,
    pub shm_atime: i64,
    pub shm_dtime: i64,
    pub shm_ctime: i64,
}

/// `struct semid_ds`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SemidDs {
    pub sem_perm: IpcPerm,
    pub sem_base: u64,
    pub sem_nsems: u16,
    pub _pad: [u16; 3],
    pub sem_otime: i64,
    pub sem_ctime: i64,
}

/// `struct sembuf`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sembuf {
    pub sem_num: u16,
    pub sem_op: i16,
    pub sem_flg: i16,
}

/// `struct msqid_ds`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MsqidDs {
    pub msg_perm: IpcPerm,
    pub msg_first: u64,
    pub msg_last: u64,
    pub msg_cbytes: u64,
    pub msg_qnum: u64,
    pub msg_qbytes: u64,
    pub msg_lspid: i32,
    pub msg_lrpid: i32,
    pub msg_stime: i64,
    pub msg_rtime: i64,
    pub msg_ctime: i64,
}

/// Access bits.
pub const IPC_R: u16 = 0o400;
pub const IPC_W: u16 = 0o200;
pub const IPC_M: u16 = 0o10000;

pub const IPC_CREAT: u32 = 0o1000;
pub const IPC_EXCL: u32 = 0o2000;
pub const IPC_NOWAIT: u32 = 0o4000;
pub const IPC_PRIVATE: i64 = 0;

pub const IPC_RMID: u32 = 0;
pub const IPC_SET: u32 = 1;
pub const IPC_STAT: u32 = 2;
pub const IPC_INFO: u32 = 3;

pub const SHM_RDONLY: u32 = 0o10000;
pub const SHM_RND: u32 = 0o20000;
pub const SHM_REMAP: u32 = 0o40000;
pub const SHM_STAT: u32 = 13;
pub const SHM_INFO: u32 = 14;

pub const SEM_UNDO: i16 = 0o10000;
pub const GETNCNT: u32 = 3;
pub const GETPID: u32 = 4;
pub const GETVAL: u32 = 5;
pub const GETALL: u32 = 6;
pub const GETZCNT: u32 = 7;
pub const SETVAL: u32 = 8;
pub const SETALL: u32 = 9;
pub const SEM_STAT: u32 = 10;
pub const SEM_INFO: u32 = 11;

pub const MSG_NOERROR: u32 = 0o10000;

const fn c(c_name: &'static str, header: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header,
        value,
        arches: Arches::All,
    }
}

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("IPC_R", "sys/ipc.h", IPC_R as i64),
    c("IPC_W", "sys/ipc.h", IPC_W as i64),
    c("IPC_M", "sys/ipc.h", IPC_M as i64),
    c("IPC_CREAT", "sys/ipc.h", IPC_CREAT as i64),
    c("IPC_EXCL", "sys/ipc.h", IPC_EXCL as i64),
    c("IPC_NOWAIT", "sys/ipc.h", IPC_NOWAIT as i64),
    c("IPC_PRIVATE", "sys/ipc.h", IPC_PRIVATE),
    c("IPC_RMID", "sys/ipc.h", IPC_RMID as i64),
    c("IPC_SET", "sys/ipc.h", IPC_SET as i64),
    c("IPC_STAT", "sys/ipc.h", IPC_STAT as i64),
    c("IPC_INFO", "sys/ipc.h", IPC_INFO as i64),
    c("SHM_RDONLY", "sys/shm.h", SHM_RDONLY as i64),
    c("SHM_RND", "sys/shm.h", SHM_RND as i64),
    c("SHM_REMAP", "sys/shm.h", SHM_REMAP as i64),
    c("SHM_STAT", "sys/shm.h", SHM_STAT as i64),
    c("SHM_INFO", "sys/shm.h", SHM_INFO as i64),
    c("SEM_UNDO", "sys/sem.h", SEM_UNDO as i64),
    c("GETNCNT", "sys/sem.h", GETNCNT as i64),
    c("GETPID", "sys/sem.h", GETPID as i64),
    c("GETVAL", "sys/sem.h", GETVAL as i64),
    c("GETALL", "sys/sem.h", GETALL as i64),
    c("GETZCNT", "sys/sem.h", GETZCNT as i64),
    c("SETVAL", "sys/sem.h", SETVAL as i64),
    c("SETALL", "sys/sem.h", SETALL as i64),
    c("SEM_STAT", "sys/sem.h", SEM_STAT as i64),
    c("SEM_INFO", "sys/sem.h", SEM_INFO as i64),
    c("MSG_NOERROR", "sys/msg.h", MSG_NOERROR as i64),
];
