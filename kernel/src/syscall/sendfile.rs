// SPDX-License-Identifier: BSD-2-Clause
//! `sendfile(2)` (`vn_sendfile`): a regular file's or a shared memory
//! object's pages queued on a stream socket.
//!
//! On a TCP socket the pages go without a copy: each mbuf refers to a page
//! the file's VM object lends (FreeBSD's unmapped `M_EXTPG` mbufs over
//! wired pages), which stays until the data is acknowledged. A page
//! written meanwhile goes out as it then is, as in FreeBSD. Pages not in
//! memory are read in first; FreeBSD 15.1's `SF_NODISKIO` only spares a
//! wait on busy pages, which these never are, so it changes nothing here,
//! nor do readahead hints (the pager reads ahead on its own) or
//! `SF_NOCACHE`. A UNIX-domain stream socket gets copies, as a `write` of
//! the same bytes would.
//!
//! Behavior reference: sys/kern/kern_sendfile.c.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering::Relaxed};

use abi::errno::Errno;
use abi::fcntl::{FREAD, O_NONBLOCK};
use abi::socket::SfHdtr;
use mbuf::{Chain, Mbuf};
use sched::Sleepable;
use vfs::{Uio, Vnode};
use vm::object::{LookupError, Object, PageRun};

use super::{SysResult, process};
use crate::capsicum::rights as R;
use crate::fs::file::{File, FileKind};
use crate::fs::uio::UserIo;
use crate::uaccess::{Plain, UserPtr};

// SAFETY: integers, the padding explicit fields.
unsafe impl Plain for SfHdtr {}

const PAGE: u64 = vm::PAGE_SIZE;
/// What a lent page costs its socket buffer (`ext_size`).
const PAGE_COST: u32 = 4096;
/// The most a UNIX-domain socket is handed at once.
const PIECE: u64 = 64 * 1024;

/// A page of a file lent to an mbuf.
struct FilePage(PageRun);

impl mbuf::Pages for FilePage {
    fn bytes(&self) -> &[u8] {
        self.0.contiguous().unwrap_or(&[])
    }
}

/// `sendfile(fd, s, offset, nbytes, hdtr, sbytes, flags)`.
#[allow(clippy::too_many_arguments, reason = "the system call's arguments")]
pub(super) fn sendfile(
    s: &Sleepable,
    fd: u64,
    sock_fd: u64,
    offset: u64,
    nbytes: u64,
    hdtr: u64,
    sbytes: u64,
    _flags: u64,
) -> SysResult {
    SF_SYSCALLS.fetch_add(1, Relaxed);
    // A start past the end sends only the header and trailer; a negative
    // one is refused.
    if offset.cast_signed() < 0 {
        return Err(Errno::EINVAL);
    }
    let (mut hdr, mut trl) = (None, None);
    if hdtr != 0 {
        let h = UserPtr::<SfHdtr>::new(hdtr)
            .read()
            .map_err(|_| Errno::EFAULT)?;
        if h.headers != 0 {
            hdr = Some(UserIo::vector(
                h.headers,
                u64::from(h.hdr_cnt.cast_unsigned()),
            )?);
        }
        if h.trailers != 0 {
            trl = Some(UserIo::vector(
                h.trailers,
                u64::from(h.trl_cnt.cast_unsigned()),
            )?);
        }
    }
    // Any offset may be read: `CAP_PREAD`.
    let f = super::io::file(s, &*process()?, fd, &R::PREAD)?;
    if f.flags() & FREAD == 0 {
        return Err(Errno::EBADF);
    }
    let mut sent = 0;
    let r = send(
        s,
        &f,
        sock_fd,
        hdr.as_mut(),
        trl.as_mut(),
        offset,
        nbytes,
        &mut sent,
    );
    // Not checked (FreeBSD bug 232210, which its tests expect).
    if sbytes != 0 {
        let _ = UserPtr::<u64>::new(sbytes).write(&sent);
    }
    match r {
        Ok(()) => Ok(0),
        Err(Errno::ERESTART) => Err(Errno::EINTR),
        Err(e) => Err(e),
    }
}

/// What is sent: a regular file's pages or a shared memory object's.
struct Source {
    object: Arc<Object>,
    vnode: Option<Arc<Vnode>>,
    shm: Option<Arc<crate::fs::shm::Shm>>,
}

impl Source {
    /// `sendfile_getobj`.
    fn of(s: &Sleepable, f: &File) -> Result<Self, Errno> {
        match f.kind() {
            FileKind::Vnode(_) => {
                let v = f.vnode().ok_or(Errno::EINVAL)?;
                if v.kind() != vfs::Kind::Regular {
                    return Err(Errno::EINVAL);
                }
                let object = crate::fs::object(s, v).ok_or(Errno::EINVAL)?;
                Ok(Self {
                    object,
                    vnode: Some(Arc::clone(v)),
                    shm: None,
                })
            }
            FileKind::Shm(m) => Ok(Self {
                object: Arc::clone(m.object()),
                vnode: None,
                shm: Some(Arc::clone(m)),
            }),
            _ => Err(Errno::EINVAL),
        }
    }

    fn size(&self, s: &Sleepable) -> Result<u64, Errno> {
        match (&self.vnode, &self.shm) {
            (Some(v), _) => v.size(s),
            (_, Some(m)) => Ok(m.size(s)),
            _ => Ok(0),
        }
    }
}

/// Where it goes.
enum Sink<'a> {
    Tcp(&'a Arc<inet::Socket>),
    Unix(&'a Arc<socket::Socket>),
}

fn errno(e: LookupError) -> Errno {
    match e {
        LookupError::OutOfMemory => Errno::ENOMEM,
        LookupError::PastEnd | LookupError::Io => Errno::EIO,
    }
}

/// Bytes of page `i` of `n` that a piece of `len` bytes from `off` takes
/// (`xfsize`): the first and last may be partial.
fn xfsize(i: u64, n: u64, off: u64, len: u64) -> u64 {
    if i == 0 {
        return (PAGE - off % PAGE).min(len);
    }
    if i == n - 1 && !(off + len).is_multiple_of(PAGE) {
        return (off + len) % PAGE;
    }
    PAGE
}

/// `sf_syscalls` and `sf_pages_valid` (`SFSTAT_INC`).
static SF_SYSCALLS: AtomicU64 = AtomicU64::new(0);
static SF_PAGES_VALID: AtomicU64 = AtomicU64::new(0);

/// `kern.ipc.sfstat`.
pub(crate) fn stats() -> abi::memstat::Sfstat {
    abi::memstat::Sfstat {
        sf_syscalls: SF_SYSCALLS.load(Relaxed),
        sf_pages_valid: SF_PAGES_VALID.load(Relaxed),
        ..abi::memstat::Sfstat::default()
    }
}

/// The pages of a record (`num_pages(max_frame_len)`).
const TLS_PAGES: u64 = ktls::MAX_FRAME as u64 / PAGE;

/// `vn_sendfile` from the source file to the socket open as `sock_fd`;
/// `sent` counts what was queued, also on error.
#[allow(clippy::too_many_arguments, reason = "the system call's state")]
fn send(
    s: &Sleepable,
    f: &File,
    sock_fd: u64,
    hdr: Option<&mut UserIo>,
    trl: Option<&mut UserIo>,
    offset: u64,
    nbytes: u64,
    sent: &mut u64,
) -> Result<(), Errno> {
    let src = Source::of(s, f)?;
    let mut size = src.size(s)?;
    // `sendfile_getsock`: a stream socket.
    let sf = super::socket::sock(s, sock_fd, &R::SEND)?;
    let sink = match sf.kind() {
        FileKind::Inet(i) if i.so.proto == inet::Proto::Tcp => Sink::Tcp(&i.so),
        FileKind::Socket(u) if u.so.ty == socket::Type::Stream => Sink::Unix(&u.so),
        _ => return Err(Errno::EINVAL),
    };
    // `SOCK_IO_SEND_LOCK` refuses a listening socket, before any length
    // is looked at.
    let listening = match sink {
        Sink::Tcp(so) => so.is_listening(),
        Sink::Unix(so) => so.is_listening(),
    };
    if listening {
        return Err(Errno::ENOTCONN);
    }
    let nonblock = sf.flags() & O_NONBLOCK != 0;
    let want = |size: u64| {
        let end = if nbytes == 0 {
            size
        } else {
            offset.saturating_add(nbytes).min(size)
        };
        end.saturating_sub(offset)
    };
    let mut rem = want(size);
    let mut off = offset;
    let mut hdr = hdr.filter(|h| h.resid() > 0);
    while rem > 0 {
        let room = match sink {
            Sink::Tcp(so) => u64::try_from(tcp_room(s, so, rem, nonblock)?).unwrap_or(u64::MAX),
            Sink::Unix(_) => PIECE,
        };
        let mut room = room;
        // A header first, as far as there is room for it.
        let mut head = Vec::new();
        if let Some(h) = hdr.as_deref_mut() {
            head.resize(
                h.resid().min(usize::try_from(room).unwrap_or(usize::MAX)),
                0,
            );
            let got = h.copy_in(&mut head)?;
            head.truncate(got);
            room -= got as u64;
            if h.resid() == 0 {
                hdr = None;
            }
        }
        let hdrlen = head.len() as u64;
        if room == 0 {
            // The header took all the room.
            put(
                s,
                &sink,
                nonblock,
                Chain::from_slice(&head).map_err(|_| Errno::ENOBUFS)?,
                &head,
            )?;
            *sent += hdrlen;
            continue;
        }
        if src.vnode.is_some() {
            // The file may have changed size.
            let now = src.size(s)?;
            if off >= now {
                break;
            }
            if now != size {
                size = now;
                rem = want(size) - (off - offset);
            }
        }
        // Whole pages where the piece is big enough.
        let mut space = room;
        if space > rem {
            space = rem;
        } else if space > PAGE {
            let lead = (PAGE - off % PAGE) % PAGE;
            space = (space - lead) / PAGE * PAGE + lead;
        }
        let npages = (space + off % PAGE).div_ceil(PAGE);
        match sink {
            Sink::Tcp(so) => {
                let mut chain = Chain::from_slice(&head).map_err(|_| Errno::ENOBUFS)?;
                // Over TLS the header is its own records and each run of
                // `TLS_PAGES` pages one more (`vn_sendfile`'s `max_pgs`).
                let tls = so.tls_tx_layout().is_some();
                if tls {
                    so.send_chain(core::mem::take(&mut chain))?;
                }
                for i in 0..npages {
                    let at = if i == 0 {
                        off
                    } else {
                        (off + i * PAGE) / PAGE * PAGE
                    };
                    let len = usize::try_from(xfsize(i, npages, off, space)).unwrap_or(0);
                    let within = usize::try_from(at % PAGE).unwrap_or(0);
                    let m = match src.object.lend_page(at / PAGE).map_err(errno)? {
                        Some(run) => {
                            Mbuf::with_pages(Arc::new(FilePage(run)), within, len, PAGE_COST)
                                .map_err(|_| Errno::ENOBUFS)?
                        }
                        None => {
                            // Pages with no fixed place: a copy.
                            let mut b = alloc::vec![0; len];
                            vfs::pagecache::read(
                                &src.object,
                                size,
                                at,
                                &mut vfs::ReadBuf::new(&mut b),
                            )?;
                            chain.append(Chain::from_slice(&b).map_err(|_| Errno::ENOBUFS)?);
                            continue;
                        }
                    };
                    SF_PAGES_VALID.fetch_add(1, Relaxed);
                    chain.push_back(m);
                    if tls && (i + 1) % TLS_PAGES == 0 {
                        so.send_chain(core::mem::take(&mut chain))?;
                    }
                }
                so.send_chain(chain)?;
            }
            Sink::Unix(_) => {
                let mut b = head;
                let n = usize::try_from(space).unwrap_or(0);
                b.resize(b.len() + n, 0);
                let at = b.len() - n;
                let got = vfs::pagecache::read(
                    &src.object,
                    size,
                    off,
                    &mut vfs::ReadBuf::new(&mut b[at..]),
                )?;
                b.truncate(at + got);
                put(s, &sink, nonblock, Chain::default(), &b)?;
            }
        }
        off += space;
        rem -= space;
        *sent += space + hdrlen;
    }
    // Trailers, as `writev` would send them.
    if let Some(t) = trl {
        *sent += sf.write(s, t)? as u64;
    }
    if *sent > 0
        && let Some(v) = &src.vnode
    {
        vfs::inotify::event(v, abi::inotify::IN_ACCESS);
    }
    Ok(())
}

/// `sendfile_wait_generic`: the send buffer's room for some of `need`
/// more bytes, waiting for it unless `nonblock`.
fn tcp_room(
    s: &Sleepable,
    so: &Arc<inet::Socket>,
    need: u64,
    nonblock: bool,
) -> Result<usize, Errno> {
    let need = usize::try_from(need).unwrap_or(usize::MAX);
    if nonblock {
        return so.sendfile_room(need, true)?.ok_or(Errno::EAGAIN);
    }
    let (timeo, _) = so.timeouts();
    let _w = sched::wait_message("sbwait");
    crate::fs::socket::wait_until(s, &so.wr, timeo, || {
        so.sendfile_room(need, false)?.ok_or(Errno::EAGAIN)
    })
}

/// Queues a piece: `chain` on a TCP socket, `bytes` copied to a UNIX-domain
/// one (all of them, waiting unless `nonblock`).
fn put(
    s: &Sleepable,
    sink: &Sink<'_>,
    nonblock: bool,
    chain: Chain,
    bytes: &[u8],
) -> Result<(), Errno> {
    match sink {
        Sink::Tcp(so) => so.send_chain(chain),
        Sink::Unix(so) => {
            let mut uio = vfs::WriteBuf::new(bytes);
            while uio.resid() > 0 {
                let n = crate::fs::socket::send(s, so, &mut uio, Vec::new(), None, 0, nonblock)?;
                if n == 0 {
                    return Err(Errno::EAGAIN);
                }
            }
            Ok(())
        }
    }
}
