// SPDX-License-Identifier: BSD-2-Clause
//! Control messages in flight, the descriptors they carry, and the
//! collector that frees sockets kept alive only by messages in their own
//! (or each other's) buffers.
//!
//! Behavior reference: sys/kern/uipc_usrreq.c (`unp_internalize`,
//! `unp_externalize`, `unp_addsockcred`, `unp_gc`, `unp_dispose`).

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicIsize, Ordering};

use abi::socket::{SCM_CREDS, SCM_CREDS2, SCM_RIGHTS, SOL_SOCKET, cmsg_space};

use crate::Socket;
use crate::buf::{MCLBYTES, MLEN, MSIZE};

/// An open file, as messages carry it: the kernel's file. A UNIX-domain
/// socket's says so, for the collector.
pub trait Passable: Send + Sync {
    fn unix(&self) -> Option<&Arc<Socket>>;
    /// The file as the kernel's own type, received.
    fn into_any(self: Arc<Self>) -> Arc<dyn core::any::Any + Send + Sync>;
}

/// Descriptors in flight (`net.local.inflight`).
static INFLIGHT: AtomicIsize = AtomicIsize::new(0);

/// How many descriptors are in flight.
pub fn inflight() -> isize {
    INFLIGHT.load(Ordering::Relaxed)
}

/// One descriptor in a message, holding its file (`struct filedescent`).
pub struct InFlight {
    file: Option<Arc<dyn Passable>>,
    /// Its descriptor flags as sent (`fde_flags`).
    pub flags: u32,
}

impl core::fmt::Debug for InFlight {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("InFlight")
            .field("flags", &self.flags)
            .finish_non_exhaustive()
    }
}

impl InFlight {
    /// `file` sent (`unp_internalize_fp`).
    pub fn new(file: Arc<dyn Passable>, flags: u32) -> Self {
        if let Some(so) = file.unix() {
            so.msgcount.fetch_add(1, Ordering::Relaxed);
            *so.file.lock() = Some(Arc::downgrade(&file));
        }
        INFLIGHT.fetch_add(1, Ordering::Relaxed);
        Self {
            file: Some(file),
            flags,
        }
    }

    /// The file, received (`unp_externalize_fp`).
    pub fn take(mut self) -> Arc<dyn Passable> {
        let f = self.file.take().unwrap_or_else(|| unreachable!());
        Self::landed(&f);
        f
    }

    fn landed(f: &Arc<dyn Passable>) {
        if let Some(so) = f.unix() {
            so.msgcount.fetch_sub(1, Ordering::Relaxed);
        }
        INFLIGHT.fetch_sub(1, Ordering::Relaxed);
    }

    fn file(&self) -> &Arc<dyn Passable> {
        self.file.as_ref().unwrap_or_else(|| unreachable!())
    }
}

impl Drop for InFlight {
    /// Discarded unreceived (`unp_discard`): the file goes.
    fn drop(&mut self) {
        if let Some(f) = self.file.take() {
            Self::landed(&f);
        }
    }
}

/// A control message as a buffer holds it (internalized).
#[derive(Debug)]
pub enum Control {
    /// `SCM_RIGHTS`.
    Rights(Vec<InFlight>),
    /// Anything else: its level, type, and data (credentials and
    /// timestamps, made at the send).
    Raw { level: i32, ty: i32, data: Vec<u8> },
}

impl Control {
    /// The length it is counted as (`m_len`, a `CMSG_SPACE`; a descriptor
    /// in flight is a pointer).
    pub(crate) fn len(&self) -> u32 {
        let n = match self {
            Self::Rights(r) => r.len() * 8,
            Self::Raw { data, .. } => data.len(),
        };
        u32::try_from(cmsg_space(n)).unwrap_or(u32::MAX)
    }

    /// The memory it is counted as.
    pub(crate) fn cost(&self) -> u32 {
        if self.len() > MLEN {
            MSIZE + MCLBYTES
        } else {
            MSIZE
        }
    }

    pub(crate) fn is(&self, level: i32, ty: i32) -> bool {
        match self {
            Self::Rights(_) => level == SOL_SOCKET && ty == SCM_RIGHTS,
            Self::Raw {
                level: l, ty: t, ..
            } => *l == level && *t == ty,
        }
    }

    /// Whether it carries the sender's `SCM_CREDS` (`cmsgcred`), which a
    /// receiver's `LOCAL_CREDS` replaces.
    pub(crate) fn is_creds(&self) -> bool {
        self.is(SOL_SOCKET, SCM_CREDS)
    }

    fn rights(&self) -> &[InFlight] {
        match self {
            Self::Rights(r) => r,
            Self::Raw { .. } => &[],
        }
    }
}

/// A sender's credential, as the kernel gives it for `SCM_CREDS` and
/// `LOCAL_PEERCRED`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cred {
    pub pid: i32,
    pub ruid: u32,
    pub euid: u32,
    pub rgid: u32,
    pub egid: u32,
    /// FreeBSD 15's `cr_groups`: the effective group first, then the
    /// supplementary ones.
    pub groups: Vec<u32>,
}

/// `unp_addsockcred`'s message: `SCM_CREDS2` (`sockcred2`) for
/// `LOCAL_CREDS_PERSISTENT`, else `SCM_CREDS` (`sockcred`).
pub(crate) fn sockcred(c: &Cred, persistent: bool) -> Control {
    let n = c.groups.len().min(abi::socket::CMGROUP_MAX);
    let mut d = Vec::new();
    let mut put = |v: u32| d.extend_from_slice(&v.to_ne_bytes());
    if persistent {
        put(0);
        put(c.pid.cast_unsigned());
    }
    put(c.ruid);
    put(c.euid);
    put(c.rgid);
    put(c.egid);
    put(u32::try_from(n).unwrap_or(0));
    for &g in &c.groups[..n] {
        put(g);
    }
    // `SOCKCREDSIZE` keeps room for one group even with none.
    if n == 0 {
        put(0);
    }
    Control::Raw {
        level: SOL_SOCKET,
        ty: if persistent { SCM_CREDS2 } else { SCM_CREDS },
        data: d,
    }
}

/// `unp_internalize`'s `SCM_CREDS`: the sender's `cmsgcred`.
#[must_use]
pub fn cmsgcred(c: &Cred) -> Control {
    let n = c.groups.len().min(abi::socket::CMGROUP_MAX);
    let mut d = Vec::new();
    d.extend_from_slice(&c.pid.to_ne_bytes());
    d.extend_from_slice(&c.ruid.to_ne_bytes());
    d.extend_from_slice(&c.euid.to_ne_bytes());
    d.extend_from_slice(&c.rgid.to_ne_bytes());
    d.extend_from_slice(&i16::try_from(n).unwrap_or(0).to_ne_bytes());
    d.extend_from_slice(&[0, 0]);
    for i in 0..abi::socket::CMGROUP_MAX {
        d.extend_from_slice(
            &c.groups
                .get(i)
                .copied()
                .filter(|_| i < n)
                .unwrap_or(0)
                .to_ne_bytes(),
        );
    }
    Control::Raw {
        level: SOL_SOCKET,
        ty: SCM_CREDS,
        data: d,
    }
}

/// Every UNIX-domain socket, for the collector (`unp_shead` and the
/// others).
static ALL: sync::SpinLock<Vec<Weak<Socket>>> = sync::SpinLock::new(Vec::new());

pub(crate) fn register(so: &Arc<Socket>) {
    let mut all = ALL.lock();
    if all.len() >= 64 && all.len().is_power_of_two() {
        all.retain(|w| w.strong_count() > 0);
    }
    all.push(Arc::downgrade(so));
}

/// The sockets one socket's buffers hold in flight (for a listener, its
/// queued connections' buffers).
fn held(so: &Socket) -> Vec<Arc<Socket>> {
    let mut out = Vec::new();
    so.scan(&mut |r: &InFlight| {
        if let Some(s) = r.file().unix() {
            out.push(Arc::clone(s));
        }
    });
    out
}

/// `unp_gc`: frees the sockets nothing outside messages in flight reaches.
/// Returns how many it claimed.
pub fn gc() -> usize {
    if inflight() == 0 {
        return 0;
    }
    let all: Vec<Arc<Socket>> = ALL.lock().iter().filter_map(Weak::upgrade).collect();
    // Candidates: every reference to their file is in flight.
    let mut dead: Vec<(Arc<Socket>, isize)> = Vec::new();
    for so in &all {
        let count = so.msgcount.load(Ordering::Relaxed);
        if count == 0 {
            continue;
        }
        let Some(f) = so.file.lock().as_ref().and_then(Weak::upgrade) else {
            continue;
        };
        // Less ours, just taken.
        if Arc::strong_count(&f) - 1 == count {
            dead.push((Arc::clone(so), isize::try_from(count).unwrap_or(isize::MAX)));
        }
    }
    let index = |dead: &[(Arc<Socket>, isize)], s: &Arc<Socket>| {
        dead.iter().position(|(d, _)| Arc::ptr_eq(d, s))
    };
    // References from candidates' own buffers don't count.
    let edges: Vec<Vec<Arc<Socket>>> = dead.iter().map(|(so, _)| held(so)).collect();
    for e in &edges {
        for s in e {
            if let Some(i) = index(&dead, s) {
                dead[i].1 -= 1;
            }
        }
    }
    // What is still referenced from outside lives, and what it holds.
    let mut live = alloc::vec![false; dead.len()];
    loop {
        let mut marked = false;
        for i in 0..dead.len() {
            if !live[i] && dead[i].1 > 0 {
                live[i] = true;
                marked = true;
                for s in &edges[i] {
                    if let Some(j) = index(&dead, s) {
                        dead[j].1 += 1;
                    }
                }
            }
        }
        if !marked {
            break;
        }
    }
    let mut n = 0;
    for (i, (so, _)) in dead.iter().enumerate() {
        if !live[i] && !so.is_listening() {
            so.cantrcvmore();
            drop(so.dispose());
            n += 1;
        }
    }
    n
}

/// The rights of `items` (for scans).
pub(crate) fn rights_of(c: &Control) -> &[InFlight] {
    c.rights()
}
