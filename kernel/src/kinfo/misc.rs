// SPDX-License-Identifier: BSD-2-Clause
//! The smaller `kern.proc` nodes `procstat` reads: a process's
//! arguments, environment, auxiliary vector, executable path, limits and
//! their use, umask, `osrel`, signal trampoline, `ps_strings`, groups,
//! `sigfastblock` word, map layout, ABI name, kqueues, and descriptor
//! count; and `kern.lockf`, every advisory lock (`procstat advlock`).
//!
//! Behavior reference: sys/kern/kern_proc.c (`sysctl_kern_proc_*`),
//! sys/kern/kern_resource.c (`getrlimitusage_one`),
//! sys/kern/kern_event.c (`kern_proc_kqueues_out`),
//! sys/kern/kern_lockf.c (`sysctl_kern_lockf`).

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::errno::Errno;
use abi::kinfo::*;
use abi::resource::rlimit;
use sched::Sleepable;
use sysctl::{Number, ReadError, Tree};

use super::{Access, arg, process, sleepable, text};
use crate::proc::Process;
use crate::uaccess::{Plain, bytes_of};

// SAFETY: integers and byte arrays, without padding (sizes asserted in
// abi; `abi-check` the layouts).
unsafe impl Plain for KinfoSigtramp {}
// SAFETY: as above.
unsafe impl Plain for KinfoVmLayout {}
// SAFETY: as above; the explicit pads are fields.
unsafe impl Plain for KinfoKnote {}
// SAFETY: as above.
unsafe impl Plain for KinfoLockf {}

/// The program's text and data sizes in pages (`vm_tsize`, `vm_dsize`),
/// from its program headers as exec sized them (`__elfN(enforce_limits)`):
/// the largest executable segment is the text, the last other one the
/// data, each from its page to the end of its memory, rounded up.
pub(crate) fn image_sizes(s: &Sleepable, p: &Process) -> (u64, u64) {
    image_layout(s, p).map_or((0, 0), |l| (l.1, l.3))
}

/// The text's and data's first page and sizes in pages, relative to the
/// program's base (0 for a fixed executable).
fn image_layout(s: &Sleepable, p: &Process) -> Option<(u64, u64, u64, u64)> {
    let v = p.text_vnode()?;
    let mut head = alloc::vec![0u8; 4096];
    let mut r = vfs::ReadBuf::new(&mut head);
    let n = v.read_at(s, 0, &mut r).ok()?;
    head.truncate(n);
    let elf = elf::Elf::parse_head(&head).ok()?;
    let page = ::vm::PAGE_SIZE;
    let (mut tsize, mut taddr, mut dsize, mut daddr) = (0, 0, 0, 0);
    for seg in elf.segments().flatten() {
        if seg.kind != elf::SegmentKind::Load {
            continue;
        }
        let addr = seg.vaddr & !(page - 1);
        let size = (seg.vaddr + seg.mem_size - addr).next_multiple_of(page);
        if seg.perms.exec && tsize < size {
            (tsize, taddr) = (size, addr);
        } else {
            (dsize, daddr) = (size, addr);
        }
    }
    if daddr == 0 && dsize == 0 {
        (dsize, daddr) = (tsize, taddr);
    }
    Some((taddr, tsize / page, daddr, dsize / page))
}

/// The NUL-terminated strings of `ptr`'s array of `n` pointers in `p`'s
/// memory, as `proc_getargv` and `proc_getenvv` read them through
/// `ps_strings`, up to `ARG_MAX` bytes.
fn strings_at(s: &Sleepable, p: &Process, ptr: u64, n: u64) -> Result<Vec<u8>, Errno> {
    const ARG_MAX: usize = 4096 * 128;
    let space = p.space();
    let read = |va: u64, buf: &mut [u8]| {
        space
            .peek(s, addr::VirtAddr::new(va), buf)
            .map_err(|_| Errno::EFAULT)
    };
    let mut out = Vec::new();
    for i in 0..n.min(65536) {
        let mut w = [0u8; 8];
        read(ptr + 8 * i, &mut w)?;
        let mut at = u64::from_ne_bytes(w);
        // One string, a chunk at a time, up to its NUL.
        loop {
            let mut chunk = [0u8; 256];
            let room = ::vm::PAGE_SIZE - at % ::vm::PAGE_SIZE;
            let len = usize::try_from(room).unwrap_or(256).min(chunk.len());
            read(at, &mut chunk[..len])?;
            if let Some(end) = chunk[..len].iter().position(|&b| b == 0) {
                out.extend_from_slice(&chunk[..=end]);
                break;
            }
            out.extend_from_slice(&chunk[..len]);
            at += len as u64;
            if out.len() > ARG_MAX {
                return Err(Errno::ENOMEM);
            }
        }
    }
    Ok(out)
}

/// `p`'s environment, from its memory (`proc_getenvv`).
fn environment(s: &Sleepable, p: &Process) -> Result<Vec<u8>, Errno> {
    let ps = ps_strings(p);
    let mut buf = [0u8; 32];
    p.space()
        .peek(s, addr::VirtAddr::new(ps), &mut buf)
        .map_err(|_| Errno::EFAULT)?;
    // `struct ps_strings`: argv, argc, envp, envc.
    let word = |o: usize| u64::from_ne_bytes(buf[o..o + 8].try_into().unwrap_or([0; 8]));
    let envp = word(16);
    let envc = u64::from(u32::from_ne_bytes(buf[24..28].try_into().unwrap_or([0; 4])));
    strings_at(s, p, envp, envc)
}

/// Where `p`'s `struct ps_strings` is (`PROC_PS_STRINGS`): right below
/// its stack's top.
pub(crate) fn ps_strings(p: &Process) -> u64 {
    p.layout().stack_top - core::mem::size_of::<abi::exec::PsStrings>() as u64
}

/// What `which`'s limit counts for `p` now (`getrlimitusage_one`), or
/// `None` where nothing is counted (FreeBSD's `ENXIO`).
fn usage_of(s: &Sleepable, p: &Process, which: u32) -> Option<u64> {
    let page = ::vm::PAGE_SIZE;
    let ruid = p.cred().ruid;
    Some(match which {
        rlimit::CPU => crate::time::ticks_to_ns(p.runtime()) / 1_000_000_000,
        rlimit::DATA => image_sizes(s, p).1 * page,
        rlimit::STACK => p.space().proc_stack(s).map_or(0, |st| st.size),
        rlimit::RSS => {
            let space = p.space();
            space
                .entries(s)
                .iter()
                .map(|e| space.resident(e.range()))
                .sum::<u64>()
                * page
        }
        rlimit::MEMLOCK => p.space().wired().count * page,
        rlimit::NPROC => u64::try_from(Process::count_of(ruid)).unwrap_or(0),
        rlimit::NOFILE => u64::try_from(p.files.lock(s).count()).unwrap_or(0),
        rlimit::VMEM => super::space_sizes(s, p).0,
        rlimit::NPTS => crate::uidinfo::count(ruid, crate::uidinfo::Kind::Ptys),
        rlimit::KQUEUES => crate::uidinfo::count(ruid, crate::uidinfo::Kind::Kqueues),
        rlimit::SBSIZE | rlimit::SWAP | rlimit::UMTXP | rlimit::PIPEBUF => 0,
        _ => return None,
    })
}

/// `p`'s kqueues' knotes, as `kern.proc.kq` returns them
/// (`kern_proc_kqueues_out`): for each kqueue descriptor, its knotes.
pub(crate) fn kqueues(s: &Sleepable, p: &Process, only: Option<i32>) -> Result<Vec<u8>, ReadError> {
    let files = p.files.lock(s);
    if let Some(fd) = only {
        let f = files.get(fd).map_err(ReadError::Failed)?;
        if !matches!(f.kind(), crate::fs::file::FileKind::Kqueue(_)) {
            return Err(ReadError::Invalid);
        }
    }
    let kqs: Vec<(i32, Arc<event::Kqueue>)> = (0..i32::try_from(files.len()).unwrap_or(0))
        .filter(|&fd| only.is_none_or(|o| o == fd))
        .filter_map(|fd| {
            let f = files.get(fd).ok()?;
            match f.kind() {
                crate::fs::file::FileKind::Kqueue(k) => Some((fd, Arc::clone(&k.kq))),
                _ => None,
            }
        })
        .collect();
    drop(files);
    let mut out = Vec::new();
    for (fd, kq) in kqs {
        for (kev, status) in kq.report() {
            let mut k: alloc::boxed::Box<KinfoKnote> = super::zeroed();
            k.knt_kq_fd = fd;
            k.knt_event = kev;
            k.knt_status = i32::try_from(status).unwrap_or(0);
            // `EVFILT_USER` has no object to hang on (`filt_userattach`
            // leaves it detached).
            if kev.filter == abi::event::EVFILT_USER {
                k.knt_status |= KNOTE_STATUS_DETACHED;
            }
            vnode_extdata(s, p, &mut k);
            out.extend_from_slice(bytes_of(&*k));
        }
    }
    Ok(out)
}

/// A vnode knote's file: its type, file system, inode, and path
/// (`KNOTE_EXTDATA_VNODE`); a pipe's, its inode (`KNOTE_EXTDATA_PIPE`).
fn vnode_extdata(s: &Sleepable, p: &Process, k: &mut KinfoKnote) {
    let filter = k.knt_event.filter;
    let fd = i32::try_from(k.knt_event.ident).unwrap_or(-1);
    let is_fd_filter = matches!(
        filter,
        abi::event::EVFILT_READ | abi::event::EVFILT_WRITE | abi::event::EVFILT_VNODE
    );
    if !is_fd_filter {
        return;
    }
    let Ok(f) = p.files.lock(s).get(fd) else {
        return;
    };
    match f.kind() {
        crate::fs::file::FileKind::Vnode(_) if filter == abi::event::EVFILT_VNODE => {
            if let Some(v) = f.vnode() {
                let kf = super::file::vnode_kinfo(s, p, v);
                k.knt_extdata = KNOTE_EXTDATA_VNODE;
                k.knt_vnode_type = kf.kf_un.kf_file_type;
                k.knt_vnode_fsid = kf.kf_un.kf_file_fsid;
                k.knt_vnode_fileid = kf.kf_un.kf_file_fileid;
                k.knt_vnode_fullpath = kf.kf_path;
            }
        }
        crate::fs::file::FileKind::Pipe(_) => {
            if let Ok(st) = f.stat(s) {
                k.knt_extdata = KNOTE_EXTDATA_PIPE;
                // `knt_pipe_ino`: the union's first word.
                k.knt_vnode_type =
                    i32::from_ne_bytes(st.st_ino.to_ne_bytes()[..4].try_into().unwrap_or([0; 4]));
                k._pad1 =
                    i32::from_ne_bytes(st.st_ino.to_ne_bytes()[4..].try_into().unwrap_or([0; 4]));
            }
        }
        _ => {}
    }
}

/// Every advisory lock held or waited for, as `kern.lockf` lists them.
fn lockf(s: &Sleepable) -> Vec<u8> {
    let mut out = Vec::new();
    let mut files_locked: Vec<Vec<u8>> = Vec::new();
    let mut seen = Vec::new();
    let mut procs = crate::proc::all();
    procs.sort_unstable_by_key(|p| p.pid());
    for p in &procs {
        let files = p.files.lock(s);
        let vnodes: Vec<Arc<vfs::Vnode>> = (0..i32::try_from(files.len()).unwrap_or(0))
            .filter_map(|fd| files.get(fd).ok()?.vnode().cloned())
            .collect();
        drop(files);
        for v in vnodes {
            if seen.iter().any(|w: &Arc<vfs::Vnode>| Arc::ptr_eq(w, &v)) {
                continue;
            }
            seen.push(Arc::clone(&v));
            let kf = super::file::vnode_kinfo(s, p, &v);
            let mut file = Vec::new();
            for l in v.lockf().held() {
                let mut k: alloc::boxed::Box<KinfoLockf> = super::zeroed();
                k.kl_structsize = i32::try_from(core::mem::size_of::<KinfoLockf>()).unwrap_or(0);
                k.kl_rw = match l.ty {
                    vfs::lockf::LockType::Exclusive => KLOCKF_RW_WRITE,
                    vfs::lockf::LockType::Shared => KLOCKF_RW_READ,
                };
                k.kl_type = match l.owner {
                    vfs::lockf::Owner::Process(_) => KLOCKF_TYPE_PID,
                    vfs::lockf::Owner::File(_) => KLOCKF_TYPE_FLOCK,
                };
                // A `flock` lock belongs to no process.
                k.kl_pid = match l.owner {
                    vfs::lockf::Owner::Process(_) => l.pid,
                    vfs::lockf::Owner::File(_) => -1,
                };
                k.kl_sysid = 0;
                k.kl_file_fsid = kf.kf_un.kf_file_fsid;
                k.kl_file_rdev = kf.kf_un.kf_file_rdev;
                k.kl_file_fileid = kf.kf_un.kf_file_fileid;
                k.kl_start = i64::try_from(l.start).unwrap_or(i64::MAX);
                // 0: to the end of the file.
                k.kl_len = if l.end == u64::MAX {
                    0
                } else {
                    i64::try_from(l.end - l.start + 1).unwrap_or(0)
                };
                k.kl_path = kf.kf_path;
                file.extend_from_slice(bytes_of(&*k));
            }
            files_locked.push(file);
        }
    }
    // The files locked last come first, as FreeBSD's list of lock states
    // takes each new one at its head.
    for f in files_locked.into_iter().rev() {
        out.extend(f);
    }
    out
}

/// A node `kern.proc.NAME.<pid>` read by `f`.
fn node(
    t: &mut Tree,
    procs: sysctl::NodeId,
    number: i32,
    name: &str,
    description: &str,
    f: impl Fn(&[i32], &mut Vec<u8>) -> Result<(), ReadError> + Send + Sync + 'static,
) {
    t.add_handler(procs, Number::Fixed(number), name, description, Arc::new(f))
        .expect("kern.proc node");
}

/// Adds the nodes to `kern.proc` (`procs`) and `kern`.
pub(crate) fn add_nodes(t: &mut Tree, kern: sysctl::NodeId, procs: sysctl::NodeId) {
    node(
        t,
        procs,
        select::ARGS,
        "args",
        "Process argument list",
        |r, out| {
            let p = process(arg(r)?, Access::See)?;
            out.extend_from_slice(&p.image().args);
            Ok(())
        },
    );
    node(
        t,
        procs,
        select::PATHNAME,
        "pathname",
        "Process executable path",
        |r, out| {
            let p = process(arg(r)?, Access::See)?;
            out.extend_from_slice(&p.image().path);
            out.push(0);
            Ok(())
        },
    );
    node(
        t,
        procs,
        select::AUXV,
        "auxv",
        "Process ELF auxiliary vector",
        |r, out| {
            let p = process(arg(r)?, Access::Debug)?;
            out.extend_from_slice(&p.image().auxv);
            Ok(())
        },
    );
    node(
        t,
        procs,
        node::ENV,
        "env",
        "Process environment",
        |r, out| {
            let p = process(arg(r)?, Access::Debug)?;
            if p.is_zombie() {
                return Ok(());
            }
            let env = environment(&sleepable(), &p).map_err(ReadError::Failed)?;
            out.extend_from_slice(&env);
            Ok(())
        },
    );
    node(
        t,
        procs,
        node::SV_NAME,
        "sv_name",
        "Process syscall vector name (ABI type)",
        |r, out| {
            process(arg(r)?, Access::See)?;
            out.extend_from_slice(b"FreeBSD ELF64\0");
            Ok(())
        },
    );
    node(
        t,
        procs,
        node::GROUPS,
        "groups",
        "Process groups",
        |r, out| {
            let p = process(arg(r)?, Access::See)?;
            let c = p.cred();
            out.extend_from_slice(&c.gid.to_ne_bytes());
            for g in c.groups() {
                out.extend_from_slice(&g.to_ne_bytes());
            }
            Ok(())
        },
    );
    node(
        t,
        procs,
        node::RLIMIT,
        "rlimit",
        "Process resource limits",
        |r, out| {
            let [pid, which] = r else {
                return Err(ReadError::Invalid);
            };
            let which = usize::try_from(*which)
                .ok()
                .filter(|&w| w < rlimit::NLIMITS)
                .ok_or(ReadError::Invalid)?;
            let p = process(*pid, Access::See)?;
            let l = p.limits.lock()[which];
            out.extend_from_slice(&l.rlim_cur.to_ne_bytes());
            out.extend_from_slice(&l.rlim_max.to_ne_bytes());
            Ok(())
        },
    );
    node(
        t,
        procs,
        node::RLIMIT_USAGE,
        "rlimit_usage",
        "Process limited resources usage info",
        |r, out| {
            let (pid, one) = match r {
                [pid] => (*pid, None),
                [pid, which] => (
                    *pid,
                    Some(u32::try_from(*which).map_err(|_| ReadError::Invalid)?),
                ),
                _ => return Err(ReadError::Invalid),
            };
            let p = process(pid, Access::Debug)?;
            let s = sleepable();
            let value = |w: u32| {
                usage_of(&s, &p, w).map_or(-1i64, |v| i64::try_from(v).unwrap_or(i64::MAX))
            };
            match one {
                Some(w) if w as usize >= rlimit::NLIMITS => return Err(ReadError::Invalid),
                Some(w) => out.extend_from_slice(&value(w).to_ne_bytes()),
                None => {
                    for w in (0..).take(rlimit::NLIMITS) {
                        out.extend_from_slice(&value(w).to_ne_bytes());
                    }
                }
            }
            Ok(())
        },
    );
    node(
        t,
        procs,
        node::PS_STRINGS,
        "ps_strings",
        "Process ps_strings location",
        |r, out| {
            let p = process(arg(r)?, Access::Debug)?;
            out.extend_from_slice(&ps_strings(&p).to_ne_bytes());
            Ok(())
        },
    );
    node(t, procs, node::UMASK, "umask", "Process umask", |r, out| {
        let pid = arg(r)?;
        let me = Process::current();
        let p = match me {
            Some(me) if pid == 0 || pid == me.pid() => me,
            _ => process(pid, Access::Debug)?,
        };
        let m = p.umask.load(core::sync::atomic::Ordering::Relaxed);
        out.extend_from_slice(&m.to_ne_bytes());
        Ok(())
    });
    node(
        t,
        procs,
        node::OSREL,
        "osrel",
        "Process binary osreldate",
        |r, out| {
            let p = process(arg(r)?, Access::See)?;
            out.extend_from_slice(&p.layout().osrel.to_ne_bytes());
            Ok(())
        },
    );
    node(
        t,
        procs,
        node::SIGTRAMP,
        "sigtramp",
        "Process signal trampoline location",
        |r, out| {
            let p = process(arg(r)?, Access::Debug)?;
            let start = crate::shared_page::sigcode_address(p.layout().shared_page);
            let k = KinfoSigtramp {
                ksigtramp_start: start,
                ksigtramp_end: start + crate::arch::user::sigcode().len() as u64,
                ksigtramp_spare: [0; 4],
            };
            out.extend_from_slice(bytes_of(&k));
            Ok(())
        },
    );
    node(
        t,
        procs,
        node::SIGFASTBLK,
        "sigfastblk",
        "Thread sigfastblock address",
        |r, out| {
            let id = arg(r)?;
            let p = process(id, Access::Debug)?;
            let t = if id > crate::proc::PID_MAX {
                p.find_thread(id)
            } else {
                p.first_thread()
            }
            .ok_or(ReadError::Failed(Errno::ESRCH))?;
            let word = t.sig.lock().fastblock;
            if word == 0 {
                return Err(ReadError::Failed(Errno::ENOTTY));
            }
            out.extend_from_slice(&word.to_ne_bytes());
            Ok(())
        },
    );
    node(
        t,
        procs,
        node::VM_LAYOUT,
        "vm_layout",
        "Process virtual address space layout info",
        |r, out| {
            let p = process(arg(r)?, Access::Debug)?;
            let s = sleepable();
            let space = p.space();
            let bounds = (crate::mm::space::USER_START, crate::mm::space::USER_END);
            let layout = p.layout();
            // The program's base: where its first page went.
            let base = super::vm::text_base(&s, &p);
            let (taddr, tsize, daddr, dsize) = image_layout(&s, &p).unwrap_or((0, 0, 0, 0));
            let stack = space.proc_stack(&s);
            let mut flags = 0;
            if space.wired().future {
                flags |= KMAP_FLAG_WIREFUTURE;
            }
            if layout.aslr {
                flags |= KMAP_FLAG_ASLR | KMAP_FLAG_ASLR_STACK | KMAP_FLAG_ASLR_SHARED_PAGE;
            }
            if space.wxorx() {
                flags |= KMAP_FLAG_WXORX;
            }
            let k = KinfoVmLayout {
                kvm_min_user_addr: bounds.0,
                kvm_max_user_addr: bounds.1,
                kvm_text_addr: base + taddr,
                kvm_text_size: tsize,
                kvm_data_addr: base + daddr,
                kvm_data_size: dsize,
                kvm_stack_addr: stack.as_ref().map_or(0, |st| st.area.start.get()),
                kvm_stack_size: stack
                    .as_ref()
                    .map_or(0, |st| st.size.div_ceil(::vm::PAGE_SIZE)),
                kvm_map_flags: flags,
                kvm_shp_addr: layout.shared_page,
                kvm_shp_size: ::vm::PAGE_SIZE,
                ..KinfoVmLayout::default()
            };
            out.extend_from_slice(bytes_of(&k));
            Ok(())
        },
    );
    node(t, procs, node::KQUEUE, "kq", "KQueue events", |r, out| {
        // `[pid]` for every kqueue, `[pid, kqfd]` for one.
        let (pid, one) = match r {
            [pid] => (*pid, None),
            [pid, fd] => (*pid, Some(*fd)),
            _ => return Err(ReadError::Invalid),
        };
        let p = process(pid, Access::Debug)?;
        out.extend_from_slice(&kqueues(&sleepable(), &p, one)?);
        Ok(())
    });
    node(
        t,
        procs,
        node::NFDS,
        "nfds",
        "Number of open file descriptors",
        |r, out| {
            // `KERN_PROC_NFDS`: only of the caller, named 0.
            if r != [0] {
                return Err(ReadError::Invalid);
            }
            let p = Process::current().ok_or(ReadError::Invalid)?;
            let s = sleepable();
            let n = i32::try_from(p.files.lock(&s).count()).unwrap_or(i32::MAX);
            out.extend_from_slice(&n.to_ne_bytes());
            Ok(())
        },
    );
    t.add_handler(
        kern,
        Number::Fixed(KERN_LOCKF),
        "lockf",
        "Advisory locks table",
        Arc::new(|r: &[i32], out: &mut Vec<u8>| {
            if !r.is_empty() {
                return Err(ReadError::Invalid);
            }
            out.extend_from_slice(&lockf(&sleepable()));
            Ok(())
        }),
    )
    .expect("kern.lockf");
    let _ = text;
}
