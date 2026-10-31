// SPDX-License-Identifier: BSD-2-Clause
//! Prisons (docs/MULTIUSER.md §10.4, §10.5): prison0, the system itself,
//! and the jails under it. Each holds the host identity its processes
//! see (`kern.hostname` and the rest), its securelevel, its root, and the
//! policy settings the `security` crate's checks read through
//! [`security::Jail`].
//!
//! prison0's allow flags and securelevel are the `security` crate's
//! globals, so a credential in prison0 carries no prison at all.
//!
//! A jail lives while it has processes, or while it persists; once
//! neither, it is gone (FreeBSD keeps a dying jail while anything still
//! references it; nothing here does). `jail_remove` kills its processes
//! and those of the jails below it.
//!
//! Behavior reference: sys/kern/kern_jail.c, sys/kern/kern_mib.c
//! (`sysctl_hostname`, `sysctl_kern_securelvl`).

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use abi::errno::Errno;
use abi::sysctl::{ctlflag, ctltype, kern};
use sched::Sleepable;
use security::Jail;
use sync::{Once, SpinLock};
use sysctl::{Number, Value};
use vfs::Vnode;

use crate::cred::Ucred;

/// `MAXHOSTNAMELEN`: a host, domain, or jail name's room, with its NUL.
pub(crate) const MAXHOSTNAMELEN: usize = 256;
/// `HOSTUUIDLEN`: a host UUID's room, with its NUL.
pub(crate) const HOSTUUIDLEN: usize = 64;
/// `OSRELEASELEN`.
pub(crate) const OSRELEASELEN: usize = 32;
/// `MAXPATHLEN`.
pub(crate) const MAXPATHLEN: usize = 1024;
/// The largest jail id (`JAIL_MAX`).
pub(crate) const JAIL_MAX: i32 = 999_999;
/// A UUID until `/etc/rc.d/hostid` sets one (`DEFAULT_HOSTUUID`).
pub(crate) const DEFAULT_HOSTUUID: &str = "00000000-0000-0000-0000-000000000000";
/// What a new jail may do (`JAIL_DEFAULT_ALLOW`), within its parent's.
pub(crate) const DEFAULT_ALLOW: u32 = security::allow::SET_HOSTNAME
    | security::allow::RESERVED_PORTS
    | security::allow::UNPRIV_DEBUG
    | security::allow::SUSER;
/// A new jail's `enforce_statfs` (`JAIL_DEFAULT_ENFORCE_STATFS`).
pub(crate) const DEFAULT_ENFORCE_STATFS: i32 = 2;
/// The most a jail's `meta` or `env` holds, with its NUL
/// (`security.jail.meta_maxbufsize`, `jm_maxbufsize_hard`).
pub(crate) static META_HARD: AtomicU32 = AtomicU32::new(4096);
/// What `security.jail.param.meta` and `env` announce
/// (`jm_maxbufsize_soft`): the hard limit, or a larger one it was lowered
/// from while jails existed.
pub(crate) static META_SOFT: AtomicU32 = AtomicU32::new(4096);

/// `PR_*` flags (`pr_flags`).
pub(crate) mod flags {
    /// It may exist with no processes.
    pub(crate) const PERSIST: u32 = 0x0000_0001;
    /// It has a host identity of its own (`host=new`).
    pub(crate) const HOST: u32 = 0x0000_0002;
    /// It has a network stack of its own (`vnet=new`, `PR_VNET`).
    pub(crate) const VNET: u32 = 0x0000_0010;
}

/// The `allow.mount.<fs>` flags, past the static ones (FreeBSD allocates
/// them as file systems register; `prison_add_vfs`).
pub(crate) mod mount_allow {
    pub(crate) const DEVFS: u32 = 0x0100_0000;
    pub(crate) const TMPFS: u32 = 0x0200_0000;
    pub(crate) const NULLFS: u32 = 0x0400_0000;
    pub(crate) const PROCFS: u32 = 0x0800_0000;
    pub(crate) const FDESCFS: u32 = 0x1000_0000;
    pub(crate) const ALL: u32 = DEVFS | TMPFS | NULLFS | PROCFS | FDESCFS;
}

/// A prison's settings, behind its lock (`pr_mtx`).
#[derive(Clone, Debug)]
pub(crate) struct Settings {
    /// Its full name, from prison0 (`a.b`); prison0's is `0`.
    pub(crate) name: String,
    /// Any bytes but NUL, as FreeBSD keeps them (not necessarily UTF-8).
    pub(crate) hostname: Vec<u8>,
    pub(crate) domainname: Vec<u8>,
    pub(crate) hostuuid: String,
    pub(crate) hostid: u64,
    /// `PR_*` flags.
    pub(crate) flags: u32,
    /// Its own securelevel (prison0's is `security::prison0`'s).
    pub(crate) securelevel: i32,
    pub(crate) enforce_statfs: i32,
    /// Its `PR_ALLOW_*` flags and `allow.mount.*` (prison0's are
    /// `security::prison0`'s).
    pub(crate) allow: u32,
    pub(crate) children_max: i32,
    pub(crate) devfs_ruleset: i32,
    pub(crate) osrelease: String,
    pub(crate) osreldate: i32,
    /// `jail_remove` was called, or it has no reason to live (`dying`).
    pub(crate) dying: bool,
    /// Its own network stack was torn down (once, as it died).
    pub(crate) vnet_gone: bool,
    /// `env` and `meta`, the free-form key-value metadata.
    pub(crate) env: Vec<u8>,
    pub(crate) meta: Vec<u8>,
}

/// A prison.
#[derive(Debug)]
pub(crate) struct Prison {
    id: i32,
    /// `None` for prison0.
    parent: Option<Arc<Prison>>,
    /// Its root (`pr_root`); `None` for prison0's, the system's.
    root: Option<Arc<Vnode>>,
    /// Its root's path from the system's root (`pr_path`).
    path: Vec<u8>,
    st: SpinLock<Settings>,
    /// Its processes (`pr_uref`, less persistence).
    procs: AtomicUsize,
    /// The jail descriptors naming it (`pr_descs`).
    descs: SpinLock<Vec<Arc<DescState>>>,
    /// Its root CPU set (`pr_cpuset`): set 1 for prison0.
    cpuset: i32,
    /// Its network stack (`pr_vnet`): its own with `vnet=new`, else its
    /// parent's.
    vnet: Arc<net::Vnet>,
}

impl Prison {
    pub(crate) const fn id(&self) -> i32 {
        self.id
    }

    /// Its network stack.
    pub(crate) fn vnet(&self) -> &Arc<net::Vnet> {
        &self.vnet
    }

    pub(crate) fn is_prison0(&self) -> bool {
        self.parent.is_none()
    }

    /// Its root CPU set's id (`cpuset.id`).
    pub(crate) const fn cpuset(&self) -> i32 {
        self.cpuset
    }

    /// A copy of its settings, with prison0's globals filled in.
    pub(crate) fn settings(&self) -> Settings {
        let mut st = self.st.lock().clone();
        if self.is_prison0() {
            st.securelevel = security::prison0::SECURELEVEL.load(Ordering::Relaxed);
            st.allow = security::prison0::ALLOW.load(Ordering::Relaxed) | mount_allow::ALL;
        }
        st
    }

    /// Its parent, prison0's own being none.
    pub(crate) fn parent(&self) -> Option<&Arc<Self>> {
        self.parent.as_ref()
    }

    /// Its root directory.
    pub(crate) fn root(&self) -> Arc<Vnode> {
        self.root.clone().unwrap_or_else(crate::fs::root)
    }

    /// Whether `self` is `other` or one of its ancestors.
    pub(crate) fn encloses(&self, other: &Self) -> bool {
        let mut cur = Some(other);
        while let Some(p) = cur {
            if core::ptr::eq(p, self) {
                return true;
            }
            cur = p.parent.as_deref();
        }
        false
    }

    /// How far below `self` `other` is (0 for itself).
    fn depth_below(&self, other: &Self) -> Option<i32> {
        let mut cur = Some(other);
        let mut d = 0;
        while let Some(p) = cur {
            if core::ptr::eq(p, self) {
                return Some(d);
            }
            d += 1;
            cur = p.parent.as_deref();
        }
        None
    }

    /// Whether it is alive (`prison_isalive`).
    pub(crate) fn is_alive(&self) -> bool {
        self.is_prison0() || !self.st.lock().dying
    }

    /// Its name as `viewer` sees it (`prison_name`): from `viewer`'s own.
    pub(crate) fn name_from(&self, viewer: &Self) -> String {
        let name = self.st.lock().name.clone();
        if viewer.is_prison0() || core::ptr::eq(self, viewer) {
            return if core::ptr::eq(self, viewer) && !viewer.is_prison0() {
                String::new()
            } else {
                name
            };
        }
        let own = viewer.st.lock().name.clone();
        name.strip_prefix(&own)
            .and_then(|r| r.strip_prefix('.'))
            .map_or(name.clone(), String::from)
    }

    /// Its path as `viewer` sees it (`prison_path`): from `viewer`'s root.
    pub(crate) fn path_from(&self, viewer: &Self) -> Vec<u8> {
        if viewer.path == b"/" {
            return self.path.clone();
        }
        match self.path.strip_prefix(&viewer.path[..]) {
            Some([]) => b"/".to_vec(),
            Some(r) if r.first() == Some(&b'/') => r.to_vec(),
            _ => self.path.clone(),
        }
    }

    /// How many jails are below it (`pr_childcount`).
    pub(crate) fn children_cur(self: &Arc<Self>) -> usize {
        descendants(self).len()
    }

    /// The prison holding its host identity: itself if it has one of its
    /// own, else the nearest ancestor that does (prison0 always does).
    fn host(self: &Arc<Self>) -> Arc<Self> {
        let mut p = Arc::clone(self);
        while !p.is_prison0() && p.st.lock().flags & flags::HOST == 0 {
            p = Arc::clone(p.parent.as_ref().expect("a jail has a parent"));
        }
        p
    }

    /// Changes the host identity as `f` does, in the prison that holds it
    /// and every jail sharing it (`sysctl_hostname`'s copy to
    /// descendants): `EPERM` without `allow.set_hostname`.
    pub(crate) fn set_host(self: &Arc<Self>, f: impl Fn(&mut Settings)) -> Result<(), Errno> {
        if self.settings().allow & security::allow::SET_HOSTNAME == 0 {
            return Err(Errno::EPERM);
        }
        let host = self.host();
        f(&mut host.st.lock());
        for p in descendants(&host) {
            // A jail with its own identity keeps it, and so do its own.
            if p.host().id == host.id {
                f(&mut p.st.lock());
            }
        }
        Ok(())
    }

    /// Raises its securelevel to `level` and every descendant's to at
    /// least that (`sysctl_kern_securelvl`); `EPERM` to lower it.
    pub(crate) fn raise_securelevel(self: &Arc<Self>, level: i32) -> Result<(), Errno> {
        if level < self.settings().securelevel {
            return Err(Errno::EPERM);
        }
        self.set_securelevel(level);
        Ok(())
    }

    /// Sets its securelevel, and raises its descendants' to at least it.
    fn set_securelevel(self: &Arc<Self>, level: i32) {
        if self.is_prison0() {
            security::prison0::SECURELEVEL.store(level, Ordering::Relaxed);
        } else {
            self.st.lock().securelevel = level;
        }
        for p in descendants(self) {
            let mut st = p.st.lock();
            st.securelevel = st.securelevel.max(level);
        }
    }

    /// Changes its settings as `f` does (`jail_set`'s update).
    pub(crate) fn update(&self, f: impl FnOnce(&mut Settings)) {
        f(&mut self.st.lock());
    }

    /// A process joined it.
    pub(crate) fn proc_joined(&self) {
        self.procs.fetch_add(1, Ordering::AcqRel);
    }

    /// A process left it: a jail left with none, and not persisting,
    /// dies.
    pub(crate) fn proc_left(self: &Arc<Self>) {
        if self.procs.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.maybe_die();
        }
    }

    /// Dies if it has no processes and does not persist.
    pub(crate) fn maybe_die(self: &Arc<Self>) {
        if self.is_prison0() {
            return;
        }
        let persist = {
            let st = self.st.lock();
            st.flags & flags::PERSIST != 0 && !st.dying
        };
        if !persist && self.procs.load(Ordering::Acquire) == 0 {
            let own_vnet = {
                let mut st = self.st.lock();
                let first = !st.vnet_gone;
                st.dying = true;
                st.vnet_gone = true;
                first && st.flags & flags::VNET != 0
            };
            self.started_dying();
            // `vnet_destroy`: lent interfaces go home, its own go.
            if own_vnet {
                net::clone::shutdown(&self.vnet);
            }
            // Its children go with it once theirs are gone too; those
            // with none go now.
            let mut jails = JAILS.lock();
            let has_children = jails
                .iter()
                .any(|j| j.parent.as_ref().is_some_and(|p| Arc::ptr_eq(p, self)));
            if !has_children {
                jails.retain(|j| !Arc::ptr_eq(j, self));
            }
            drop(jails);
            if !has_children && let Some(p) = &self.parent {
                p.maybe_die_after_child();
            }
        }
    }

    /// It started dying (`prison_cleanup_locked`): its descriptors see
    /// it removed (`NOTE_JAIL_REMOVE`, `POLLHUP`) and let it go.
    fn started_dying(&self) {
        let descs = core::mem::take(&mut *self.descs.lock());
        for d in descs {
            *d.prison.lock() = None;
            d.removed.store(true, Ordering::Release);
            d.sel.notify(0);
        }
    }

    /// A child jail is gone: a dying parent with nothing left goes too.
    fn maybe_die_after_child(self: &Arc<Self>) {
        if !self.is_prison0() && self.st.lock().dying {
            self.maybe_die();
        }
    }

    /// `jail_remove` (`prison_deref` with `PD_KILL`): no longer persists,
    /// and every process in it or below gets `SIGKILL`.
    pub(crate) fn remove(self: &Arc<Self>) {
        let mut all = descendants(self);
        all.push(Arc::clone(self));
        for j in &all {
            {
                let mut st = j.st.lock();
                st.flags &= !flags::PERSIST;
                st.dying = true;
            }
            j.started_dying();
        }
        for p in crate::proc::all() {
            if let Some(pj) = p
                .cred()
                .jail
                .as_ref()
                .and_then(|j| j.as_any().downcast_ref::<Prison>())
                && all.iter().any(|j| j.id == pj.id)
                && !p.has_exited()
            {
                let _ = crate::signal::post(
                    &p,
                    crate::signal::user_info(abi::signal::SIGKILL, 0, abi::signal::code::SI_KERNEL),
                );
            }
        }
        // Deepest first, so each parent sees its children gone.
        for j in all.iter().rev() {
            j.maybe_die();
        }
    }
}

/// A jail descriptor's state (`struct jaildesc`), shared with the prison
/// it names until that dies.
#[derive(Debug)]
pub(crate) struct DescState {
    prison: SpinLock<Option<Arc<Prison>>>,
    /// It was made with `JAIL_OWN_DESC`: closing it removes the jail.
    owning: bool,
    /// Its jail started dying (`JDF_REMOVED`).
    removed: AtomicBool,
    sel: event::Notify,
}

/// A jail descriptor (`DTYPE_JAILDESC`, kern_jaildesc.c): names a jail
/// for `jail_set`, `jail_get`, `jail_attach_jd`, and `jail_remove_jd`,
/// with its creator's credential for their privilege checks.
#[derive(Debug)]
pub(crate) struct JailDesc {
    st: Arc<DescState>,
    cred: Arc<Ucred>,
}

impl JailDesc {
    /// Its jail's id, 0 once the jail is gone (`jaildesc_fill_kinfo`).
    pub(crate) fn jid(&self) -> i32 {
        self.st.prison.lock().as_ref().map_or(0, |p| p.id())
    }

    /// `jaildesc_alloc`: an owning one takes `PRIV_JAIL_REMOVE`. Its file
    /// is writable with `PRIV_JAIL_SET`.
    pub(crate) fn new(cred: Arc<Ucred>, owning: bool) -> Result<(Self, u32), Errno> {
        use abi::fcntl::{FREAD, FWRITE};
        if owning {
            cred.priv_check(security::Priv::JailRemove)?;
        }
        let fflags = if cred.priv_check(security::Priv::JailSet).is_ok() {
            FREAD | FWRITE
        } else {
            FREAD
        };
        let st = Arc::new(DescState {
            prison: SpinLock::new(None),
            owning,
            removed: AtomicBool::new(false),
            sel: event::Notify::new(),
        });
        Ok((Self { st, cred }, fflags))
    }

    /// `jaildesc_set_prison`.
    pub(crate) fn set_prison(&self, pr: &Arc<Prison>) {
        pr.descs.lock().push(Arc::clone(&self.st));
        *self.st.prison.lock() = Some(Arc::clone(pr));
    }

    /// `jaildesc_find`: its jail and its creator's credential; `ENOENT`
    /// once the jail is dying.
    pub(crate) fn find(&self) -> Result<(Arc<Prison>, Arc<Ucred>), Errno> {
        let pr = self.st.prison.lock().clone().ok_or(Errno::ENOENT)?;
        if !pr.is_alive() {
            return Err(Errno::ENOENT);
        }
        Ok((pr, Arc::clone(&self.cred)))
    }

    /// `jaildesc_poll`: hung up once its jail is removed.
    pub(crate) fn poll(&self, w: Option<&Arc<dyn event::Wake>>) -> i16 {
        if self.st.removed.load(Ordering::Acquire) {
            return abi::event::POLLHUP;
        }
        if let Some(w) = w {
            self.st.sel.record(w);
        }
        0
    }

    /// `jaildesc_stat`: a regular file whose inode number is the jail's
    /// id, owner-accessible while it lives.
    pub(crate) fn stat(&self) -> abi::stat::Stat {
        let pr = self.st.prison.lock().clone();
        abi::stat::Stat {
            st_ino: pr.as_ref().map_or(0, |p| p.id.unsigned_abs().into()),
            st_mode: abi::stat::S_IFREG | if pr.is_some() { 0o700 } else { 0 },
            ..abi::stat::Stat::default()
        }
    }
}

impl Drop for JailDesc {
    /// `jaildesc_close`: an owning descriptor removes its jail; its
    /// pollers find it closed.
    fn drop(&mut self) {
        let pr = self.st.prison.lock().take();
        if let Some(pr) = pr {
            pr.descs.lock().retain(|d| !Arc::ptr_eq(d, &self.st));
            if self.st.owning && pr.is_alive() {
                pr.remove();
            }
        }
        self.st.sel.drain();
    }
}

impl Jail for Prison {
    fn id(&self) -> i32 {
        self.id
    }

    fn parent(&self) -> Option<Arc<dyn Jail>> {
        self.parent
            .as_ref()
            .filter(|p| !p.is_prison0())
            .map(|p| Arc::clone(p) as Arc<dyn Jail>)
    }

    fn allow(&self) -> u32 {
        self.st.lock().allow
    }

    fn enforce_statfs(&self) -> i32 {
        self.st.lock().enforce_statfs
    }

    fn securelevel(&self) -> i32 {
        self.st.lock().securelevel
    }

    fn vnet(&self) -> bool {
        self.st.lock().flags & flags::VNET != 0
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

static PRISON0: Once<Arc<Prison>> = Once::new();

/// Every jail (not prison0), in increasing id (`allprison`).
static JAILS: SpinLock<Vec<Arc<Prison>>> = SpinLock::new(Vec::new());

/// The id the last new jail took.
static LAST_ID: SpinLock<i32> = SpinLock::new(0);

/// prison0.
pub(crate) fn prison0() -> &'static Arc<Prison> {
    if PRISON0.get().is_none() {
        let _ = PRISON0.set(Arc::new(Prison {
            id: 0,
            parent: None,
            root: None,
            path: b"/".to_vec(),
            st: SpinLock::new(Settings {
                name: String::from("0"),
                hostname: Vec::new(),
                domainname: Vec::new(),
                hostuuid: String::from(DEFAULT_HOSTUUID),
                hostid: 0,
                flags: flags::PERSIST | flags::HOST,
                securelevel: -1,
                enforce_statfs: 0,
                allow: security::allow::PRISON0,
                children_max: JAIL_MAX,
                devfs_ruleset: 0,
                osrelease: String::from(abi::sysctl::identity::OSRELEASE),
                osreldate: abi::sysctl::identity::OSRELDATE,
                dying: false,
                vnet_gone: false,
                env: Vec::new(),
                meta: Vec::new(),
            }),
            procs: AtomicUsize::new(0),
            descs: SpinLock::new(Vec::new()),
            cpuset: crate::cpuset::DEFAULT,
            vnet: Arc::clone(net::vnet0()),
        }));
    }
    PRISON0.get().expect("prison0 set")
}

/// The prison credential `c` is in (`cr_prison`).
pub(crate) fn of(c: &Ucred) -> Arc<Prison> {
    c.jail
        .as_ref()
        .and_then(|j| j.as_any().downcast_ref::<Prison>())
        .and_then(|p| find_any(p.id))
        .unwrap_or_else(|| Arc::clone(prison0()))
}

/// The calling process's prison.
pub(crate) fn current() -> Arc<Prison> {
    of(&crate::fs::cred())
}

/// The prison with id `id`, alive or dying.
pub(crate) fn find_any(id: i32) -> Option<Arc<Prison>> {
    if id == 0 {
        return Some(Arc::clone(prison0()));
    }
    JAILS.lock().iter().find(|p| p.id == id).cloned()
}

/// The living jail with id `id` at or below `from` (`prison_find_child`;
/// `from` itself included for 0... no: only below).
pub(crate) fn find_child(from: &Prison, id: i32) -> Option<Arc<Prison>> {
    JAILS
        .lock()
        .iter()
        .find(|p| p.id == id && !core::ptr::eq(&***p, from) && from.encloses(p) && p.is_alive())
        .cloned()
}

/// The living jail named `name` below `from` (`prison_find_name`): a
/// name relative to `from`'s, or its id.
pub(crate) fn find_name(from: &Prison, name: &str) -> Option<Arc<Prison>> {
    let full = if from.is_prison0() {
        String::from(name)
    } else {
        alloc::format!("{}.{name}", from.st.lock().name)
    };
    let named = JAILS
        .lock()
        .iter()
        .find(|p| {
            !core::ptr::eq(&***p, from)
                && from.encloses(p)
                && p.is_alive()
                && p.st.lock().name == full
        })
        .cloned();
    named.or_else(|| name.parse::<i32>().ok().and_then(|id| find_child(from, id)))
}

/// The living jails below `from` with ids above `after`, in order (for
/// `lastjid`).
pub(crate) fn next_after(from: &Prison, after: i32, dying: bool) -> Option<Arc<Prison>> {
    JAILS
        .lock()
        .iter()
        .find(|p| {
            p.id > after
                && !core::ptr::eq(&***p, from)
                && from.encloses(p)
                && (dying || p.is_alive())
        })
        .cloned()
}

/// Every jail below `p`, parents before children.
pub(crate) fn descendants(p: &Arc<Prison>) -> Vec<Arc<Prison>> {
    let all = JAILS.lock().clone();
    let mut out: Vec<(i32, Arc<Prison>)> = all
        .into_iter()
        .filter(|j| !Arc::ptr_eq(j, p))
        .filter_map(|j| p.depth_below(&j).map(|d| (d, j)))
        .collect();
    out.sort_by_key(|(d, j)| (*d, j.id));
    out.into_iter().map(|(_, j)| j).collect()
}

/// What a new jail starts with, besides what `jail_set` sets.
pub(crate) struct New {
    pub(crate) id: i32,
    pub(crate) parent: Arc<Prison>,
    /// Its name below its parent's.
    pub(crate) name: String,
    pub(crate) root: Arc<Vnode>,
    pub(crate) path: Vec<u8>,
    pub(crate) osrelease: Option<String>,
    pub(crate) osreldate: Option<i32>,
    /// `vnet=new`: a network stack of its own.
    pub(crate) vnet: bool,
}

/// The next free jail id (`get_next_prid`), 0 if none.
pub(crate) fn next_id() -> i32 {
    let mut last = LAST_ID.lock();
    let jails = JAILS.lock();
    let mut id = *last % JAIL_MAX + 1;
    for _ in 0..JAIL_MAX {
        if !jails.iter().any(|j| j.id == id) {
            *last = id;
            return id;
        }
        id = id % JAIL_MAX + 1;
    }
    0
}

/// Whether a jail has id `id`, alive or dying.
pub(crate) fn id_taken(id: i32) -> bool {
    JAILS.lock().iter().any(|j| j.id == id)
}

/// Makes a jail, with its parent's settings where it inherits them, and
/// lists it.
pub(crate) fn create(n: New) -> Arc<Prison> {
    let ps = n.parent.settings();
    let name = if n.parent.is_prison0() {
        n.name
    } else {
        alloc::format!("{}.{}", ps.name, n.name)
    };
    let p = Arc::new(Prison {
        id: n.id,
        parent: Some(Arc::clone(&n.parent)),
        root: Some(n.root),
        path: n.path,
        st: SpinLock::new(Settings {
            name,
            hostname: Vec::new(),
            domainname: Vec::new(),
            hostuuid: String::from(DEFAULT_HOSTUUID),
            hostid: 0,
            flags: flags::HOST | if n.vnet { flags::VNET } else { 0 },
            securelevel: ps.securelevel,
            enforce_statfs: DEFAULT_ENFORCE_STATFS,
            allow: DEFAULT_ALLOW & ps.allow,
            children_max: 0,
            devfs_ruleset: ps.devfs_ruleset,
            osrelease: n.osrelease.unwrap_or(ps.osrelease),
            osreldate: n.osreldate.unwrap_or(ps.osreldate),
            dying: false,
            vnet_gone: false,
            env: Vec::new(),
            meta: Vec::new(),
        }),
        procs: AtomicUsize::new(0),
        descs: SpinLock::new(Vec::new()),
        cpuset: crate::cpuset::create_root(n.parent.cpuset()),
        vnet: if n.vnet {
            net::Vnet::new(crate::net::now_s())
        } else {
            Arc::clone(&n.parent.vnet)
        },
    });
    let mut jails = JAILS.lock();
    let at = jails
        .iter()
        .position(|j| j.id > p.id)
        .unwrap_or(jails.len());
    jails.insert(at, Arc::clone(&p));
    p
}

/// The credential `c` with its prison `p` (`cr_prison`).
pub(crate) fn cred_in(c: &Ucred, p: &Arc<Prison>) -> Ucred {
    let mut c = c.clone();
    c.jail = (!p.is_prison0()).then(|| Arc::clone(p) as Arc<dyn Jail>);
    c
}

/// A process with credential `c` began: its prison counts it.
pub(crate) fn proc_joined(c: &Ucred) {
    if c.jailed() {
        of(c).proc_joined();
    }
}

/// A process with credential `c` exited (`prison_proc_free`).
pub(crate) fn proc_left(c: &Ucred) {
    if c.jailed() {
        of(c).proc_left();
    }
}

/// A string node's value: its room, as `libjail` reads it.
fn room(n: usize) -> Value {
    Value::Str(alloc::boxed::Box::new(move |out| {
        out.push_str(&alloc::format!("{n}"));
    }))
}

/// The `security.jail.param` tree `libjail` learns parameter types from:
/// each parameter a node of its type and format, a string's value its
/// room (`SYSCTL_JAIL_PARAM*`).
fn param_tree(t: &mut sysctl::Tree, jail: sysctl::NodeId) {
    let param = t
        .add_node(jail, Number::Auto, "param", "Jail parameters")
        .expect("security.jail.param");
    let leaf = |t: &mut sysctl::Tree,
                parent,
                name: &str,
                kind: u32,
                format: &'static str,
                value: Value| {
        let id = t
            .add_formatted(
                parent,
                Number::Auto,
                name,
                "Jail parameter",
                kind,
                format,
                value,
            )
            .expect("a jail parameter");
        t.set_flags(id, ctlflag::WR);
    };
    let int = || Value::int(0);
    let int_kind = ctltype::INT;
    let str_kind = ctltype::STRING;
    leaf(t, param, "jid", int_kind, "I", int());
    leaf(t, param, "parent", int_kind, "I", int());
    leaf(t, param, "name", str_kind, "A", room(MAXHOSTNAMELEN));
    leaf(t, param, "path", str_kind, "A", room(MAXPATHLEN));
    leaf(t, param, "securelevel", int_kind, "I", int());
    leaf(t, param, "osreldate", int_kind, "I", int());
    leaf(t, param, "osrelease", str_kind, "A", room(OSRELEASELEN));
    leaf(t, param, "enforce_statfs", int_kind, "I", int());
    leaf(t, param, "devfs_ruleset", int_kind, "I", int());
    leaf(t, param, "persist", int_kind, "B", int());
    leaf(t, param, "dying", int_kind, "B", int());
    leaf(t, param, "vnet", int_kind, "E,jailsys", int());
    for name in ["env", "meta"] {
        let soft = Value::Str(alloc::boxed::Box::new(|out| {
            let n = META_SOFT.load(Ordering::Relaxed);
            out.push_str(&alloc::format!("{n}"));
        }));
        leaf(t, param, name, str_kind, "A,keyvalue", soft);
    }
    let children = t
        .add_node(param, Number::Auto, "children", "Number of child jails")
        .expect("children");
    leaf(t, children, "cur", int_kind, "I", int());
    leaf(t, children, "max", int_kind, "I", int());
    let host = t
        .add_node(param, Number::Auto, "host", "Jail host info")
        .expect("host");
    leaf(t, host, "", int_kind, "E,jailsys", int());
    leaf(t, host, "hostname", str_kind, "A", room(MAXHOSTNAMELEN));
    leaf(t, host, "domainname", str_kind, "A", room(MAXHOSTNAMELEN));
    leaf(t, host, "hostuuid", str_kind, "A", room(HOSTUUIDLEN));
    let hostid = t
        .add_formatted(
            host,
            Number::Auto,
            "hostid",
            "Jail host ID",
            ctltype::ULONG,
            "LU",
            Value::Ulong(alloc::boxed::Box::new(|| 0)),
        )
        .expect("hostid");
    t.set_flags(hostid, ctlflag::WR);
    for ipc in ["sysvmsg", "sysvsem", "sysvshm"] {
        let n = t
            .add_node(param, Number::Auto, ipc, "SYSV IPC")
            .expect("sysv");
        leaf(t, n, "", int_kind, "E,jailsys", int());
    }
    // `options INET` and `INET6`: the address lists are structures, whose
    // value is their element's size.
    for (family, size, format) in [("ip4", 4usize, "S,in_addr,a"), ("ip6", 16, "S,in6_addr,a")] {
        let n = t
            .add_node(param, Number::Auto, family, "Jail address virtualization")
            .expect("ip4/ip6");
        let sys = t
            .add_formatted(
                n,
                Number::Auto,
                "",
                "Jail parameter",
                int_kind,
                "E,jailsys",
                int(),
            )
            .expect("a jail parameter");
        t.set_flags(sys, ctlflag::RD | ctlflag::TUN);
        let addr = Value::Opaque {
            format,
            read: alloc::boxed::Box::new(move |out| out.extend_from_slice(&size.to_ne_bytes())),
        };
        leaf(t, n, "addr", ctltype::OPAQUE, format, addr);
        leaf(t, n, "saddrsel", int_kind, "B", int());
    }
    let allow = t
        .add_node(param, Number::Auto, "allow", "Jail permission flags")
        .expect("allow");
    for (name, _) in ALLOW_NAMES {
        leaf(t, allow, name, int_kind, "B", int());
    }
    let mount = t
        .add_node(
            allow,
            Number::Auto,
            "mount",
            "Jail mount/unmount permission flags",
        )
        .expect("allow.mount");
    leaf(t, mount, "", int_kind, "B", int());
    for (name, _) in MOUNT_NAMES {
        leaf(t, mount, name, int_kind, "B", int());
    }
}

/// The `allow.*` parameters and their flags (`pr_flag_allow`).
pub(crate) const ALLOW_NAMES: &[(&str, u32)] = {
    use security::allow::*;
    &[
        ("set_hostname", SET_HOSTNAME),
        ("sysvipc", SYSVIPC),
        ("raw_sockets", RAW_SOCKETS),
        ("chflags", CHFLAGS),
        ("quotas", QUOTAS),
        ("socket_af", SOCKET_AF),
        ("mlock", MLOCK),
        ("reserved_ports", RESERVED_PORTS),
        ("read_msgbuf", READ_MSGBUF),
        ("unprivileged_proc_debug", UNPRIV_DEBUG),
        ("unprivileged_parent_tampering", UNPRIV_PARENT_TAMPER),
        ("suser", SUSER),
        ("nfsd", NFSD),
        ("extattr", EXTATTR),
        ("adjtime", ADJTIME),
        ("settime", SETTIME),
        ("routing", ROUTING),
        ("setaudit", SETAUDIT),
    ]
};

/// The `allow.mount.<fs>` parameters.
pub(crate) const MOUNT_NAMES: &[(&str, u32)] = &[
    ("devfs", mount_allow::DEVFS),
    ("tmpfs", mount_allow::TMPFS),
    ("nullfs", mount_allow::NULLFS),
    ("procfs", mount_allow::PROCFS),
    ("fdescfs", mount_allow::FDESCFS),
];

/// The `kern` host sysctls, `kern.securelevel`, `security.jail`, and the
/// `security.bsd` knobs.
#[allow(clippy::too_many_lines, reason = "a list of nodes")]
pub(crate) fn sysctl_init(s: &Sleepable) {
    crate::sysctl::with(s, |t| {
        let root = t.root();
        let k = t.child(root, "kern").expect("kern node");
        let host = |field: fn(&Settings) -> &[u8]| {
            Value::ByteStr(alloc::boxed::Box::new(move |out| {
                out.extend_from_slice(field(&current().settings()));
            }))
        };
        type Field = (
            i32,
            &'static str,
            &'static str,
            usize,
            fn(&Settings) -> &[u8],
            fn(&mut Settings, Vec<u8>),
        );
        let names: [Field; 3] = [
            (
                kern::HOSTNAME,
                "hostname",
                "Hostname",
                MAXHOSTNAMELEN,
                |s| &s.hostname,
                |s, v| s.hostname = v,
            ),
            (
                kern::NISDOMAINNAME,
                "domainname",
                "Name of the current YP/NIS domain",
                MAXHOSTNAMELEN,
                |s| &s.domainname,
                |s, v| s.domainname = v,
            ),
            (
                kern::HOSTUUID,
                "hostuuid",
                "Host UUID",
                HOSTUUIDLEN,
                |s| s.hostuuid.as_bytes(),
                |s, v| s.hostuuid = String::from_utf8_lossy(&v).into_owned(),
            ),
        ];
        for (number, name, descr, max, get, set) in names {
            t.add_writable(
                k,
                Number::Fixed(number),
                name,
                descr,
                ctlflag::PRISON | ctlflag::CAPRD,
                host(get),
                sysctl::str_writer(max, move |b| current().set_host(|st| set(st, b.to_vec()))),
            )
            .expect("kern host names");
        }
        t.add_writable(
            k,
            Number::Fixed(kern::HOSTID),
            "hostid",
            "Host ID",
            ctlflag::PRISON | ctlflag::CAPRD,
            Value::Ulong(alloc::boxed::Box::new(|| current().settings().hostid)),
            alloc::sync::Arc::new(|b: &[u8]| {
                let v = b
                    .first_chunk::<8>()
                    .map(|c| u64::from_ne_bytes(*c))
                    .ok_or(Errno::EINVAL)?;
                current().set_host(|st| st.hostid = v)
            }),
        )
        .expect("kern.hostid");
        t.add_writable(
            k,
            Number::Fixed(kern::SECURELVL),
            "securelevel",
            "Current secure level",
            ctlflag::PRISON,
            Value::Int(alloc::boxed::Box::new(|| current().settings().securelevel)),
            sysctl::int_writer(|v| current().raise_securelevel(v)),
        )
        .expect("kern.securelevel");

        t.add_handler(
            k,
            Number::Auto,
            "msgbuf",
            "Contents of kernel message buffer",
            alloc::sync::Arc::new(|rest: &[i32], out: &mut Vec<u8>| {
                if !rest.is_empty() {
                    return Err(sysctl::ReadError::Invalid);
                }
                crate::fs::priv_check(crate::cred::Priv::Msgbuf)
                    .map_err(sysctl::ReadError::Failed)?;
                out.extend_from_slice(&crate::console::msgbuf());
                Ok(())
            }),
        )
        .expect("kern.msgbuf");
        t.add_writable(
            k,
            Number::Fixed(34),
            "logsigexit",
            "Log processes quitting on abnormal signals to syslog(3)",
            0,
            Value::Int(alloc::boxed::Box::new(|| {
                i32::from(crate::signal::LOGSIGEXIT.load(Ordering::Relaxed))
            })),
            sysctl::int_writer(|v| {
                crate::signal::LOGSIGEXIT.store(v != 0, Ordering::Relaxed);
                Ok(())
            }),
        )
        .expect("kern.logsigexit");
        let sec = t
            .add_node(root, Number::Auto, "security", "Security")
            .expect("security");
        let jail = t
            .add_node(sec, Number::Auto, "jail", "Jails")
            .expect("security.jail");
        t.add_value(
            jail,
            Number::Auto,
            "jailed",
            "Process in jail?",
            Value::Int(alloc::boxed::Box::new(|| {
                i32::from(!current().is_prison0())
            })),
        )
        .expect("security.jail.jailed");
        t.add_value(
            jail,
            Number::Auto,
            "vnet",
            "Jail owns vnet?",
            Value::Int(alloc::boxed::Box::new(|| {
                i32::from(current().settings().flags & flags::VNET != 0)
            })),
        )
        .expect("security.jail.vnet");
        let children = t
            .add_node(
                jail,
                Number::Auto,
                "children",
                "Limits and stats of child jails",
            )
            .expect("security.jail.children");
        t.add_value(
            children,
            Number::Auto,
            "max",
            "Maximum number of child jails",
            Value::Int(alloc::boxed::Box::new(|| current().settings().children_max)),
        )
        .expect("children.max");
        t.add_value(
            children,
            Number::Auto,
            "cur",
            "Current number of child jails",
            Value::Int(alloc::boxed::Box::new(|| {
                i32::try_from(current().children_cur()).unwrap_or(i32::MAX)
            })),
        )
        .expect("children.cur");
        {
            t.add_writable(
                jail,
                Number::Auto,
                "meta_maxbufsize",
                "Maximum buffer size of each meta and env",
                0,
                Value::Uint(alloc::boxed::Box::new(|| META_HARD.load(Ordering::Relaxed))),
                // `jm_sysctl_meta_maxbufsize`: the announced size follows
                // a raise, and a cut once no jail could hold more.
                sysctl::int_writer(|v| {
                    let v = v.cast_unsigned();
                    META_HARD.store(v, Ordering::Relaxed);
                    if v >= META_SOFT.load(Ordering::Relaxed) || JAILS.lock().is_empty() {
                        META_SOFT.store(v, Ordering::Relaxed);
                    }
                    Ok(())
                }),
            )
            .expect("security.jail.meta_maxbufsize");
        }
        t.add_value(
            jail,
            Number::Auto,
            "env",
            "Meta information provided by parent jail",
            Value::Str(alloc::boxed::Box::new(|out| {
                out.push_str(&String::from_utf8_lossy(&current().settings().env));
            })),
        )
        .expect("security.jail.env");
        param_tree(t, jail);

        let bsd = t
            .add_node(sec, Number::Auto, "bsd", "BSD security policy")
            .expect("security.bsd");
        let knob = |t: &mut sysctl::Tree,
                    name: &str,
                    descr: &str,
                    b: &'static core::sync::atomic::AtomicBool| {
            t.add_writable(
                bsd,
                Number::Auto,
                name,
                descr,
                0,
                Value::Int(alloc::boxed::Box::new(|| {
                    i32::from(b.load(Ordering::Relaxed))
                })),
                sysctl::int_writer(|v| {
                    b.store(v != 0, Ordering::Relaxed);
                    Ok(())
                }),
            )
            .expect("security.bsd knob");
        };
        use security::knobs as kn;
        knob(
            t,
            "see_other_uids",
            "Unprivileged processes may see subjects/objects with different real uid",
            &kn::SEE_OTHER_UIDS,
        );
        knob(
            t,
            "see_other_gids",
            "Unprivileged processes may see subjects/objects with different real gid",
            &kn::SEE_OTHER_GIDS,
        );
        knob(
            t,
            "see_jail_proc",
            "Unprivileged processes may see subjects/objects with different jail ids",
            &kn::SEE_JAIL_PROC,
        );
        knob(
            t,
            "conservative_signals",
            "Unprivileged processes prevented from sending certain signals to processes whose credentials have changed",
            &kn::CONSERVATIVE_SIGNALS,
        );
        knob(
            t,
            "unprivileged_mlock",
            "Allow non-root users to call mlock(2)",
            &kn::UNPRIVILEGED_MLOCK,
        );
        knob(
            t,
            "unprivileged_read_msgbuf",
            "Unprivileged processes may read the kernel message buffer",
            &kn::UNPRIVILEGED_READ_MSGBUF,
        );
        knob(
            t,
            "hardlink_check_uid",
            "Unprivileged processes cannot create hard links to files owned by other users",
            &kn::HARDLINK_CHECK_UID,
        );
        knob(
            t,
            "hardlink_check_gid",
            "Unprivileged processes cannot create hard links to files owned by other groups",
            &kn::HARDLINK_CHECK_GID,
        );
        // Two prison0 allow flags.
        let flag = |t: &mut sysctl::Tree, name: &str, descr: &str, bit: u32| {
            t.add_writable(
                bsd,
                Number::Auto,
                name,
                descr,
                0,
                Value::Int(alloc::boxed::Box::new(move || {
                    i32::from(security::prison0::ALLOW.load(Ordering::Relaxed) & bit != 0)
                })),
                sysctl::int_writer(move |v| {
                    if v != 0 {
                        security::prison0::ALLOW.fetch_or(bit, Ordering::Relaxed);
                    } else {
                        security::prison0::ALLOW.fetch_and(!bit, Ordering::Relaxed);
                    }
                    Ok(())
                }),
            )
            .expect("security.bsd prison0 flag");
        };
        flag(
            t,
            "suser_enabled",
            "Processes with uid 0 have privilege",
            security::allow::SUSER,
        );
        flag(
            t,
            "unprivileged_proc_debug",
            "Unprivileged processes may use process debugging facilities",
            security::allow::UNPRIV_DEBUG,
        );
    });
}
