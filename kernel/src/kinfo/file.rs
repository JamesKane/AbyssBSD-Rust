// SPDX-License-Identifier: BSD-2-Clause
//! Descriptors as `struct kinfo_file`: `kern.proc.filedesc` (`procstat
//! -f`, `fstat`, libutil's `kinfo_getfile`), `kern.proc.cwd`,
//! `fcntl(F_KINFO)`, and `kern.ipc.posix_shm_list`.
//!
//! A process's list starts with what is not a descriptor: its program
//! (`text`), its controlling terminal (`ctty`), its working directory,
//! root, and jail directory, then its descriptors in order, each with its
//! type's part (`fo_fill_kinfo`), its path where it has one (a file's from
//! the name cache, a socket's domain, a POSIX shared memory object's
//! name, a terminal's), and its Capsicum rights. The sysctl packs each
//! record to its path's end (`KERN_FILEDESC_PACK_KINFO`); `F_KINFO` gives
//! one whole.
//!
//! Behavior reference: sys/kern/kern_descrip.c (`kern_proc_filedesc_out`,
//! `export_file_to_kinfo`, `export_vnode_to_kinfo`, `pack_kinfo`,
//! `F_KINFO`), sys/kern/vfs_vnops.c (`vn_fill_kinfo_vnode`), and each
//! type's `fo_fill_kinfo`.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::capsicum::CapRights;
use abi::errno::Errno;
use abi::kinfo::*;
use sched::Sleepable;
use sysctl::{Number, ReadError, Tree};

use super::{Access, arg, process, sleepable, text, zeroed};
use crate::fs::file::{File, FileKind};
use crate::proc::Process;
use crate::uaccess::{Plain, bytes_of};

// SAFETY: integers, byte arrays, and padding fields, no implicit padding
// (each member's size is asserted in abi; `abi-check` the layout).
unsafe impl<U: Plain> Plain for KinfoFile<U> {}
// SAFETY: as above.
unsafe impl Plain for KfFile {}
// SAFETY: as above.
unsafe impl Plain for KfSock {}
// SAFETY: as above.
unsafe impl Plain for KfPipe {}
// SAFETY: as above.
unsafe impl Plain for KfPts {}
// SAFETY: as above.
unsafe impl Plain for KfProc {}
// SAFETY: as above.
unsafe impl Plain for KfEventfd {}
// SAFETY: as above.
unsafe impl Plain for KfTimerfd {}
// SAFETY: as above.
unsafe impl Plain for KfJail {}
// SAFETY: as above.
unsafe impl Plain for KfKqueue {}
// SAFETY: as above.
unsafe impl Plain for KfNone {}

/// An opaque identity for an object (FreeBSD's kernel addresses, which
/// `procstat` and `fstat` only compare).
fn identity<T>(x: &T) -> u64 {
    core::ptr::from_ref(x).addr() as u64
}

/// `kf_flags` from a file's flags (`xlate_fflags`).
fn kf_flags(fflags: u32) -> i32 {
    use abi::fcntl::{FREAD, FWRITE, O_APPEND, O_ASYNC, O_FSYNC, O_NONBLOCK};
    let mut k = 0;
    for (have, kf) in [
        (FREAD, KF_FLAG_READ),
        (FWRITE, KF_FLAG_WRITE),
        (O_APPEND, KF_FLAG_APPEND),
        (O_ASYNC, KF_FLAG_ASYNC),
        (O_FSYNC, KF_FLAG_FSYNC),
        (O_NONBLOCK, KF_FLAG_NONBLOCK),
    ] {
        if fflags & have != 0 {
            k |= kf;
        }
    }
    k
}

/// `vntype_to_kinfo`.
fn vtype(k: vfs::Kind) -> i32 {
    match k {
        vfs::Kind::Regular => KF_VTYPE_VREG,
        vfs::Kind::Directory => KF_VTYPE_VDIR,
        vfs::Kind::Symlink => KF_VTYPE_VLNK,
        vfs::Kind::CharDevice => KF_VTYPE_VCHR,
        vfs::Kind::BlockDevice => KF_VTYPE_VBLK,
        vfs::Kind::Fifo => KF_VTYPE_VFIFO,
        vfs::Kind::Socket => KF_VTYPE_VSOCK,
    }
}

/// A record with no descriptor part yet: the generic fields of
/// `export_file_to_kinfo`, for a file of `fflags` at `fd`.
/// No rights, as `cap_rights_init_zero` makes them: each word's index
/// bits, and nothing else (a reader checks them).
const NO_RIGHTS: CapRights = CapRights {
    cr_rights: [
        abi::capsicum::CAP_ALL0 & !0x0000_0fff_ffff_ffff,
        abi::capsicum::CAP_ALL1 & !0x0000_0000_007f_ffff,
    ],
};

fn record<U: Plain>(fd: i32, fflags: u32) -> Box<KinfoFile<U>> {
    let mut k: Box<KinfoFile<U>> = zeroed();
    k.kf_cap_rights = NO_RIGHTS;
    k.kf_structsize = i32::try_from(KINFO_FILE_SIZE).unwrap_or(0);
    k.kf_type = KF_TYPE_UNKNOWN;
    k.kf_fd = fd;
    k.kf_flags = kf_flags(fflags);
    k
}

/// `v`'s path from the caller's root, as `vn_fullpath` finds it, into
/// `path` (left empty if it has none).
fn fullpath(s: &Sleepable, v: &Arc<vfs::Vnode>, path: &mut [u8; PATH_MAX]) {
    let root = Process::current().map_or_else(crate::fs::root, |me| {
        Arc::clone(crate::fs::root_of(&me).as_ref())
    });
    if let Ok(p) = crate::fs::namespace().path_of(s, v, &root) {
        text(path, &p);
    }
}

/// A vnode's record (`vn_fill_kinfo_vnode`): its type, path, and
/// attributes. `kf_status` says whether the attributes could be read.
pub(crate) fn vnode_kinfo(
    s: &Sleepable,
    p: &Process,
    v: &Arc<vfs::Vnode>,
) -> Box<KinfoFile<KfFile>> {
    let _ = p;
    let mut k: Box<KinfoFile<KfFile>> = record(0, 0);
    k.kf_type = KF_TYPE_VNODE;
    fullpath(s, v, &mut k.kf_path);
    if let Ok(st) = v.stat(s) {
        k.kf_un.kf_file_type = vtype(v.kind());
        k.kf_un.kf_file_fsid = st.st_dev;
        k.kf_un.kf_file_fsid_freebsd11 = u32::try_from(st.st_dev & 0xffff_ffff).unwrap_or(0);
        k.kf_un.kf_file_fileid = st.st_ino;
        k.kf_un.kf_file_mode = st.st_mode;
        k.kf_un.kf_file_size = u64::try_from(st.st_size).unwrap_or(0);
        // Only a device has one (`va_rdev` stays `NODEV`).
        let device = matches!(v.kind(), vfs::Kind::CharDevice | vfs::Kind::BlockDevice);
        k.kf_un.kf_file_rdev = if device { st.st_rdev } else { NODEV };
        k.kf_un.kf_file_rdev_freebsd11 = u32::try_from(st.st_rdev & 0xffff_ffff).unwrap_or(0);
        k.kf_un.kf_file_nlink = st.st_nlink;
        k.kf_status |= KF_ATTR_VALID;
    } else {
        k.kf_un.kf_file_type = vtype(v.kind());
    }
    k
}

/// The bytes of a record, packed to its path (`pack_kinfo`) or whole.
#[allow(
    clippy::boxed_local,
    reason = "a kilobyte and more: kept off the stack"
)]
fn finish<U: Plain>(mut k: Box<KinfoFile<U>>, packed: bool) -> Vec<u8> {
    let path_at = core::mem::offset_of!(KinfoFile<KfNone>, kf_path);
    let len = if packed {
        let n = k.kf_path.iter().position(|&b| b == 0).unwrap_or(PATH_MAX);
        (path_at + n + 1).next_multiple_of(8)
    } else {
        KINFO_FILE_SIZE
    };
    k.kf_structsize = i32::try_from(len).unwrap_or(0);
    bytes_of(&*k)[..len].to_vec()
}

/// A not-a-descriptor vnode entry (`export_vnode_to_kinfo`): no reference
/// count, no offset, no rights.
fn vnode_entry(s: &Sleepable, p: &Process, v: &Arc<vfs::Vnode>, fd: i32, fflags: u32) -> Vec<u8> {
    let mut k = vnode_kinfo(s, p, v);
    k.kf_fd = fd;
    k.kf_flags = kf_flags(fflags);
    k.kf_ref_count = -1;
    k.kf_offset = -1;
    finish(k, true)
}

/// Descriptor `fd` of `p`, open on `f` with `rights`, as its record
/// (`export_file_to_kinfo`): packed for the sysctl, whole for `F_KINFO`.
/// `None` where its type could not fill it (no `KF_ATTR_VALID`).
pub(crate) fn file_record(
    s: &Sleepable,
    p: &Process,
    fd: i32,
    f: &Arc<File>,
    rights: CapRights,
    packed: bool,
) -> Option<Vec<u8>> {
    let fflags = f.flags();
    // The descriptor's reference plus the one held here (`fhold`).
    let refs = i32::try_from(Arc::strong_count(f)).unwrap_or(i32::MAX);
    let offset = i64::try_from(f.offset_hint()).unwrap_or(i64::MAX);
    macro_rules! out {
        ($k:expr) => {{
            let mut k = $k;
            k.kf_fd = fd;
            k.kf_flags = kf_flags(fflags);
            k.kf_ref_count = refs;
            k.kf_offset = offset;
            k.kf_cap_rights = rights;
            Some(finish(k, packed))
        }};
    }
    match f.kind() {
        FileKind::Vnode(vf) => {
            let mut k = vnode_kinfo(s, p, vf.vnode.vnode());
            let valid = k.kf_status & KF_ATTR_VALID != 0;
            k.kf_type = KF_TYPE_VNODE;
            if !valid {
                return None;
            }
            let rec = out!(k);
            // `FHASLOCK`: the file holds a `flock` lock.
            let owner = vf.lock_owner();
            let locked = vf
                .vnode
                .vnode()
                .lockf()
                .held()
                .iter()
                .any(|l| l.owner == owner);
            rec.map(|mut r| {
                if locked {
                    let at = core::mem::offset_of!(KinfoFile<KfNone>, kf_flags);
                    let f = i32::from_ne_bytes(r[at..at + 4].try_into().unwrap_or([0; 4]))
                        | KF_FLAG_HASLOCK;
                    r[at..at + 4].copy_from_slice(&f.to_ne_bytes());
                }
                r
            })
        }
        FileKind::Fifo(ff) => {
            let mut k = vnode_kinfo(s, p, ff.vnode.vnode());
            k.kf_type = KF_TYPE_FIFO;
            out!(k)
        }
        FileKind::Socket(sf) => {
            let mut k: Box<KinfoFile<KfSock>> = record(fd, fflags);
            k.kf_type = KF_TYPE_SOCKET;
            let so = &sf.so;
            let info = so.info();
            k.kf_un.kf_sock_domain0 = i32::from(abi::socket::AF_UNIX);
            k.kf_un.kf_sock_type0 = match so.ty {
                socket::Type::Stream => abi::socket::SOCK_STREAM,
                socket::Type::Dgram => abi::socket::SOCK_DGRAM,
                socket::Type::Seqpacket => abi::socket::SOCK_SEQPACKET,
            }
            .cast_signed();
            k.kf_un.kf_sock_protocol0 = 0;
            k.kf_un.kf_sock_pcb = identity(so.as_ref());
            if let Some(peer) = info.peer {
                k.kf_un.kf_sock_unpconn = peer as u64;
                k.kf_un.kf_sock_rcv_sb_state = info.rcv_state;
                k.kf_un.kf_sock_snd_sb_state = info.snd_state;
                k.kf_un.kf_sock_sendq = info.sendq;
                k.kf_un.kf_sock_recvq = info.recvq;
            }
            let local = so.sockname();
            let n = local.len().min(128);
            k.kf_un.kf_sa_local[..n].copy_from_slice(&local[..n]);
            // Unconnected, its peer's name is the empty one
            // (`uipc_peeraddr`'s `sun_noname`).
            let peer = so.peername().unwrap_or_else(|_| socket::noname());
            let n = peer.len().min(128);
            k.kf_un.kf_sa_peer[..n].copy_from_slice(&peer[..n]);
            text(&mut k.kf_path, b"local");
            k.kf_status |= KF_ATTR_VALID;
            out!(k)
        }
        FileKind::Pipe(end) => {
            let mut k: Box<KinfoFile<KfPipe>> = record(fd, fflags);
            k.kf_type = KF_TYPE_PIPE;
            let pipe = end.pipe();
            let side = end.side();
            k.kf_un.kf_pipe_addr = identity(pipe.as_ref()) + side as u64;
            k.kf_un.kf_pipe_peer = identity(pipe.as_ref()) + (1 - side) as u64;
            let cnt = u32::try_from(pipe.len(side)).unwrap_or(u32::MAX);
            k.kf_un.kf_pipe_buffer_cnt = cnt;
            k.kf_un.kf_pipe_buffer_in = cnt;
            k.kf_un.kf_pipe_buffer_out = 0;
            k.kf_un.kf_pipe_buffer_size =
                u32::try_from(pipe.len(side) + pipe.room(side)).unwrap_or(u32::MAX);
            k.kf_status |= KF_ATTR_VALID;
            out!(k)
        }
        FileKind::Kqueue(kf) => {
            let mut k: Box<KinfoFile<KfKqueue>> = record(fd, fflags);
            k.kf_type = KF_TYPE_KQUEUE;
            k.kf_un.kf_kqueue_addr = identity(kf.kq.as_ref());
            k.kf_un.kf_kqueue_count = i32::try_from(kf.kq.pending()).unwrap_or(i32::MAX);
            k.kf_un.kf_kqueue_state = 0;
            k.kf_status |= KF_ATTR_VALID;
            out!(k)
        }
        FileKind::Shm(shm) => {
            let mut k: Box<KinfoFile<KfFile>> = record(fd, fflags);
            fill_shm(s, shm, &mut k);
            out!(k)
        }
        FileKind::Process(pd) => {
            let mut k: Box<KinfoFile<KfProc>> = record(fd, fflags);
            k.kf_type = KF_TYPE_PROCDESC;
            k.kf_un.kf_pid = pd.process().pid();
            k.kf_status |= KF_ATTR_VALID;
            out!(k)
        }
        FileKind::Jail(jd) => {
            let mut k: Box<KinfoFile<KfJail>> = record(fd, fflags);
            k.kf_type = KF_TYPE_JAILDESC;
            k.kf_un.kf_jid = jd.jid();
            k.kf_status |= KF_ATTR_VALID;
            out!(k)
        }
        FileKind::Eventfd(e) => {
            let mut k: Box<KinfoFile<KfEventfd>> = record(fd, fflags);
            k.kf_type = KF_TYPE_EVENTFD;
            let (value, flags) = e.kinfo();
            k.kf_un.kf_eventfd_value = value;
            k.kf_un.kf_eventfd_flags = flags;
            k.kf_un.kf_eventfd_addr = identity(e.as_ref());
            k.kf_status |= KF_ATTR_VALID;
            out!(k)
        }
        FileKind::Timerfd(t) => {
            let mut k: Box<KinfoFile<KfTimerfd>> = record(fd, fflags);
            k.kf_type = KF_TYPE_TIMERFD;
            k.kf_un.kf_timerfd_clockid = t.clock();
            // `tfd_flags`: what `timerfd_create` was given that shows.
            k.kf_un.kf_timerfd_flags = if fflags & abi::fcntl::O_NONBLOCK != 0 {
                abi::fcntl::O_NONBLOCK
            } else {
                0
            };
            k.kf_un.kf_timerfd_addr = identity(t.as_ref());
            k.kf_status |= KF_ATTR_VALID;
            out!(k)
        }
        FileKind::Pts(m) => {
            let mut k: Box<KinfoFile<KfPts>> = record(fd, fflags);
            k.kf_type = KF_TYPE_PTS;
            let name = m.tty().name();
            let rdev = crate::fs::devfs::rdev_of(name.as_bytes()).unwrap_or(NODEV);
            k.kf_un.kf_pts_dev = rdev;
            k.kf_un.kf_pts_dev_freebsd11 = u32::try_from(rdev & 0xffff_ffff).unwrap_or(0);
            text(&mut k.kf_path, name.as_bytes());
            k.kf_status |= KF_ATTR_VALID;
            out!(k)
        }
    }
}

/// A POSIX shared memory object's part (`shm_fill_kinfo_locked`): its
/// mode and size in `kf_file`, its name as the path.
fn fill_shm(s: &Sleepable, shm: &crate::fs::shm::Shm, k: &mut KinfoFile<KfFile>) {
    k.kf_type = KF_TYPE_SHM;
    let (mode, size, path) = shm.kinfo(s);
    k.kf_un.kf_file_mode = abi::stat::S_IFREG | mode;
    k.kf_un.kf_file_size = size;
    if let Some(p) = path {
        text(&mut k.kf_path, &p);
    }
    k.kf_status |= KF_ATTR_VALID;
}

/// `p`'s descriptors and the rest of its list, packed, as
/// `kern.proc.filedesc` returns them (`kern_proc_filedesc_out`). For core
/// dumps' `NT_PROCSTAT_FILES` too.
pub(crate) fn files(s: &Sleepable, p: &Process) -> Vec<u8> {
    use abi::fcntl::{FREAD, FWRITE};
    let mut out = Vec::new();
    if let Some(v) = p.text_vnode() {
        out.extend(vnode_entry(s, p, &v, KF_FD_TYPE_TEXT, FREAD));
    }
    if let Some(name) = crate::tty::controlling_node(p.sid()) {
        let path = alloc::format!("/dev/{name}");
        if let Some(v) = crate::fs::devfs::vnode_of(s, name.as_bytes()) {
            let mut rec = vnode_kinfo(s, p, &v);
            if rec.kf_path[0] == 0 {
                text(&mut rec.kf_path, path.as_bytes());
            }
            rec.kf_fd = KF_FD_TYPE_CTTY;
            rec.kf_flags = kf_flags(FREAD | FWRITE);
            rec.kf_ref_count = -1;
            rec.kf_offset = -1;
            out.extend(finish(rec, true));
        }
    }
    if !p.is_zombie() {
        let cwd = Arc::clone(p.cwd.lock().vnode());
        out.extend(vnode_entry(s, p, &cwd, KF_FD_TYPE_CWD, FREAD));
        let root = Arc::clone(crate::fs::root_of(p).as_ref());
        out.extend(vnode_entry(s, p, &root, KF_FD_TYPE_ROOT, FREAD));
        let prison = p.jail();
        let jroot = prison
            .as_ref()
            .and_then(|j| j.as_any().downcast_ref::<crate::prison::Prison>())
            .map(crate::prison::Prison::root);
        if let Some(jroot) = jroot
            && !Arc::ptr_eq(&jroot, &crate::fs::root())
        {
            out.extend(vnode_entry(s, p, &jroot, KF_FD_TYPE_JAIL, FREAD));
        }
    }
    let slots: Vec<(i32, Arc<File>, CapRights)> = {
        let t = p.files.lock(s);
        (0..i32::try_from(t.len()).unwrap_or(0))
            .filter_map(|fd| {
                let slot = t.slot(fd).ok()?;
                Some((fd, Arc::clone(&slot.file), slot.caps().rights))
            })
            .collect()
    };
    for (fd, f, rights) in slots {
        if let Some(rec) = file_record(s, p, fd, &f, rights, true) {
            out.extend(rec);
        }
    }
    out
}

/// `fcntl(F_KINFO)`: descriptor `fd`'s whole record, with no rights, as
/// FreeBSD gives it; `EBADF` if its type could not fill it.
pub(crate) fn f_kinfo(
    s: &Sleepable,
    p: &Process,
    fd: i32,
    f: &Arc<File>,
) -> Result<Vec<u8>, Errno> {
    file_record(s, p, fd, f, NO_RIGHTS, false).ok_or(Errno::EBADF)
}

/// Every named POSIX shared memory object (`kern.ipc.posix_shm_list`),
/// whole records with their names, as `sysctl_posix_shm_list` gives them.
fn shm_list(s: &Sleepable) -> Vec<u8> {
    let mut out = Vec::new();
    for shm in crate::fs::shm::named(s) {
        let mut k: Box<KinfoFile<KfFile>> = record(0, 0);
        k.kf_fd = 0;
        k.kf_flags = 0;
        fill_shm(s, &shm, &mut k);
        out.extend(finish(k, false));
    }
    out
}

/// Adds `kern.proc.filedesc`, `kern.proc.cwd`, and
/// `kern.ipc.posix_shm_list`.
pub(crate) fn add_nodes(t: &mut Tree, kern: sysctl::NodeId, procs: sysctl::NodeId) {
    t.add_handler(
        procs,
        Number::Fixed(node::FILEDESC),
        "filedesc",
        "Process filedesc entries",
        Arc::new(|r: &[i32], out: &mut Vec<u8>| {
            let p = process(arg(r)?, Access::Debug)?;
            out.extend(files(&sleepable(), &p));
            Ok(())
        }),
    )
    .expect("kern.proc.filedesc");
    t.add_handler(
        procs,
        Number::Fixed(node::CWD),
        "cwd",
        "Process current working directory",
        Arc::new(|r: &[i32], out: &mut Vec<u8>| {
            let p = process(arg(r)?, Access::Debug)?;
            if p.is_zombie() {
                return Err(ReadError::Failed(Errno::ESRCH));
            }
            let s = sleepable();
            let cwd = Arc::clone(p.cwd.lock().vnode());
            // `kern_proc_cwd_out` packs nothing (its flags are 0).
            let mut k = vnode_kinfo(&s, &p, &cwd);
            k.kf_fd = KF_FD_TYPE_CWD;
            k.kf_flags = kf_flags(abi::fcntl::FREAD);
            k.kf_ref_count = -1;
            k.kf_offset = -1;
            out.extend(finish(k, false));
            Ok(())
        }),
    )
    .expect("kern.proc.cwd");
    let ipc = t.child(kern, "ipc").expect("kern.ipc");
    t.add_handler(
        ipc,
        Number::Auto,
        "posix_shm_list",
        "POSIX SHM list",
        Arc::new(|r: &[i32], out: &mut Vec<u8>| {
            if !r.is_empty() {
                return Err(ReadError::Invalid);
            }
            out.extend(shm_list(&sleepable()));
            Ok(())
        }),
    )
    .expect("kern.ipc.posix_shm_list");
}
