// SPDX-License-Identifier: BSD-2-Clause
//! A Direct Rendering Manager device (`/dev/dri/card<N>`) and its open files.

use alloc::boxed::Box;
use alloc::collections::{BTreeSet, VecDeque};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use abi::errno::Errno;
use sched::{Sleepable, WaitQueue};
use sync::{Once, SpinLock};
use vfs::{CharDevice, Uio};
use vm::object::{Object, PageIndex};

use crate::driver::DrmDriver;
use crate::gem::{HandleTable, OffsetSpace};

/// `card<N>`'s mode and group: `video` (44), as FreeBSD's ports expect;
/// devfs rules may widen it.
const CARD_MODE: u16 = 0o660;
const GID_VIDEO: u32 = 44;

/// The most event bytes a file may have queued (`event_space`).
const EVENT_SPACE: usize = 4096;

static UNITS: SpinLock<BTreeSet<u32>> = SpinLock::new(BTreeSet::new());
/// The registered cards by unit, for timer callbacks that name one.
static CARDS: SpinLock<alloc::collections::BTreeMap<u32, alloc::sync::Weak<Card>>> =
    SpinLock::new(alloc::collections::BTreeMap::new());

/// The card with unit `unit`, if it is registered.
pub(crate) fn card_by_unit(unit: u32) -> Option<Arc<Card>> {
    CARDS.lock().get(&unit)?.upgrade()
}

/// A registered Direct Rendering Manager device.
pub struct Card {
    unit: u32,
    driver: Arc<dyn DrmDriver>,
    offsets: SpinLock<OffsetSpace>,
    /// The open file with modesetting rights: the master.
    master: SpinLock<Option<u64>>,
    next_file: AtomicU64,
    revoked: AtomicBool,
    /// Every open file, so a revoke can end them.
    files: SpinLock<Vec<alloc::sync::Weak<Open>>>,
    path: Once<String>,
    /// The magic numbers files have asked for, until the master
    /// authenticates them.
    magics: SpinLock<alloc::collections::BTreeMap<u32, alloc::sync::Weak<Open>>>,
    next_magic: AtomicU32,
}

impl core::fmt::Debug for Card {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Card(card{})", self.unit)
    }
}

/// Registers `driver` as the lowest free `card<N>`.
///
/// # Errors
///
/// `ENXIO` if the core has no environment, no devfs, or no free unit.
pub fn register(driver: Arc<dyn DrmDriver>) -> Result<Arc<Card>, Errno> {
    if crate::env().is_none() {
        return Err(Errno::ENXIO);
    }
    let unit = {
        let mut u = UNITS.lock();
        let n = (0..).find(|n| !u.contains(n)).ok_or(Errno::ENXIO)?;
        u.insert(n);
        n
    };
    let card = Arc::new(Card {
        unit,
        driver,
        offsets: SpinLock::new(OffsetSpace::new()),
        master: SpinLock::new(None),
        next_file: AtomicU64::new(1),
        revoked: AtomicBool::new(false),
        files: SpinLock::new(Vec::new()),
        path: Once::new(),
        magics: SpinLock::new(alloc::collections::BTreeMap::new()),
        next_magic: AtomicU32::new(1),
    });
    let path = format!("dri/card{unit}");
    let node = Arc::new(Node(Arc::clone(&card)));
    if vfs::devices::make_dev(path.as_bytes(), CARD_MODE, 0, GID_VIDEO, node).is_none() {
        UNITS.lock().remove(&unit);
        return Err(Errno::ENXIO);
    }
    let _ = card.path.set(path);
    CARDS.lock().insert(unit, Arc::downgrade(&card));
    if let Some(k) = card.driver.kms() {
        k.attach(unit);
    }
    Ok(card)
}

/// The kernel has panicked: every card's display goes back to the surface
/// the console draws on ([`DrmDriver::panic_restore`]). Takes no lock it
/// could wait for: if the list is held (by a processor that is stopped),
/// nothing is restored.
pub fn panic_restore() {
    let Some(cards) = CARDS.try_lock() else {
        return;
    };
    for c in cards.values().filter_map(alloc::sync::Weak::upgrade) {
        c.driver.panic_restore();
    }
}

/// Takes away every card whose driver is the firmware's framebuffer
/// ([`DrmDriver::firmware_framebuffer`]), for a native driver that now
/// shows the same screen (docs/GRAPHICS.md §7.5). Returns how many went.
pub fn retire_firmware_cards(s: &Sleepable) -> usize {
    let cards: Vec<Arc<Card>> = CARDS
        .lock()
        .values()
        .filter_map(alloc::sync::Weak::upgrade)
        .filter(|c| c.driver.firmware_framebuffer())
        .collect();
    for c in &cards {
        c.unregister(s);
    }
    cards.len()
}

impl Card {
    /// Its unit number: `card<N>`.
    #[must_use]
    pub fn unit(&self) -> u32 {
        self.unit
    }

    /// Its driver.
    #[must_use]
    pub fn driver(&self) -> &Arc<dyn DrmDriver> {
        &self.driver
    }

    /// Takes the device away: its node goes, every open file is dead
    /// (`ENODEV`, `POLLHUP`), and the unit is free again. Buffers that
    /// userland still maps stay valid until it unmaps them.
    pub fn unregister(&self, s: &Sleepable) {
        if self.revoked.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(p) = self.path.get() {
            vfs::devices::destroy_dev(s, p.as_bytes());
        }
        let files: Vec<_> = self.files.lock().drain(..).collect();
        for f in files.iter().filter_map(alloc::sync::Weak::upgrade) {
            f.hangup();
        }
        *self.master.lock() = None;
        if let Some(k) = self.driver.kms() {
            k.shutdown();
        }
        CARDS.lock().remove(&self.unit);
        UNITS.lock().remove(&self.unit);
    }

    /// The open file with number `id`.
    pub(crate) fn file(&self, id: u64) -> Option<Arc<Open>> {
        self.files
            .lock()
            .iter()
            .filter_map(alloc::sync::Weak::upgrade)
            .find(|f| f.id == id)
    }

    /// A new open file, for a client inside the kernel (the console,
    /// tests); devfs makes the same for each open. The first file
    /// opened becomes master if there is none.
    ///
    /// # Errors
    ///
    /// `ENODEV` if the card is gone.
    pub fn open_file(self: &Arc<Self>) -> Result<Arc<Open>, Errno> {
        if self.is_revoked() {
            return Err(Errno::ENODEV);
        }
        let id = self.next_file.fetch_add(1, Ordering::Relaxed);
        let open = Arc::new_cyclic(|me| Open {
            me: me.clone(),
            card: Arc::clone(self),
            id,
            handles: SpinLock::new(HandleTable::new()),
            events: SpinLock::new(Events::default()),
            wait: WaitQueue::new(),
            notify: vfs::event::Notify::new(),
            hung_up: AtomicBool::new(false),
            client_caps: AtomicU8::new(0),
            magic: AtomicU32::new(0),
            authenticated: AtomicBool::new(false),
            name: SpinLock::new(None),
        });
        self.adopt_master(id);
        {
            let mut files = self.files.lock();
            files.retain(|w| w.strong_count() > 0);
            files.push(Arc::downgrade(&open));
        }
        Ok(open)
    }

    /// Whether the device is gone.
    #[must_use]
    pub fn is_revoked(&self) -> bool {
        self.revoked.load(Ordering::Acquire)
    }

    pub(crate) fn is_master(&self, file: u64) -> bool {
        *self.master.lock() == Some(file)
    }

    /// Makes `file` the master if there is none (`drm_master_open`).
    fn adopt_master(&self, file: u64) {
        let mut m = self.master.lock();
        if m.is_none() {
            *m = Some(file);
        }
    }

    pub(crate) fn set_master(&self, file: u64) -> Result<(), Errno> {
        let mut m = self.master.lock();
        match *m {
            Some(f) if f == file => Ok(()),
            Some(_) => Err(Errno::EINVAL),
            None => {
                *m = Some(file);
                Ok(())
            }
        }
    }

    pub(crate) fn drop_master(&self, file: u64) -> Result<(), Errno> {
        let mut m = self.master.lock();
        if *m == Some(file) {
            *m = None;
            Ok(())
        } else {
            Err(Errno::EINVAL)
        }
    }

    pub(crate) fn alloc_offset(&self, pages: u64) -> Result<PageIndex, Errno> {
        self.offsets.lock().alloc(pages)
    }
}

/// The device node; each open is an [`Open`].
struct Node(Arc<Card>);

impl CharDevice for Node {
    fn instance(&self, _: &Sleepable) -> Result<Option<Arc<dyn CharDevice>>, Errno> {
        let open: Arc<dyn CharDevice> = self.0.open_file()?;
        Ok(Some(open))
    }
}

#[derive(Default)]
struct Events {
    queue: VecDeque<Vec<u8>>,
    /// Bytes queued or promised to a commit in flight.
    bytes: usize,
    /// Events promised and not yet sent: queue room is already made.
    reserved: usize,
}

/// `DRM_CLIENT_CAP_*` a file turned on.
pub(crate) mod client {
    pub(crate) const STEREO_3D: u8 = 1 << 0;
    pub(crate) const UNIVERSAL_PLANES: u8 = 1 << 1;
    pub(crate) const ATOMIC: u8 = 1 << 2;
    pub(crate) const ASPECT_RATIO: u8 = 1 << 3;
    pub(crate) const WRITEBACK: u8 = 1 << 4;
}

/// An open Direct Rendering Manager file (`drm_file`): its handles, events, and client
/// capabilities. Closing it drops them and gives up mastership.
pub struct Open {
    pub(crate) me: alloc::sync::Weak<Open>,
    pub(crate) card: Arc<Card>,
    pub(crate) id: u64,
    pub(crate) handles: SpinLock<HandleTable>,
    events: SpinLock<Events>,
    wait: WaitQueue,
    notify: vfs::event::Notify,
    hung_up: AtomicBool,
    pub(crate) client_caps: AtomicU8,
    /// The magic number it asked for (`GET_MAGIC`), 0 before.
    magic: AtomicU32,
    /// Whether the master vouched for it (`AUTH_MAGIC`).
    authenticated: AtomicBool,
    /// The name the client gave itself (`SET_CLIENT_NAME`).
    pub(crate) name: SpinLock<Option<String>>,
}

impl core::fmt::Debug for Open {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Open(card{}, file {})", self.card.unit, self.id)
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        let _ = self.card.drop_master(self.id);
        if let Some(k) = self.card.driver().kms() {
            k.release_file(self.id);
        }
        self.card.driver().file_closed(self.id);
    }
}

impl Open {
    /// Queues an event for this file (a vblank, a completed flip).
    ///
    /// # Errors
    ///
    /// `ENOMEM` if the file already has [`EVENT_SPACE`] bytes queued;
    /// the event is dropped.
    pub fn queue_event(&self, event: &[u8]) -> Result<(), Errno> {
        {
            let mut e = self.events.lock();
            if e.bytes + event.len() > EVENT_SPACE {
                return Err(Errno::ENOMEM);
            }
            let mut v = Vec::new();
            v.try_reserve_exact(event.len())
                .map_err(|_| Errno::ENOMEM)?;
            v.extend_from_slice(event);
            e.bytes += event.len();
            e.queue.push_back(v);
        }
        self.wait.wake_all();
        self.notify.notify(0);
        Ok(())
    }

    /// Promises room for an event of `len` bytes, so that sending it
    /// later cannot fail or allocate (a completion path must not).
    ///
    /// # Errors
    ///
    /// `ENOMEM` if the file has no room.
    pub(crate) fn reserve_event(&self, len: usize) -> Result<(), Errno> {
        let mut e = self.events.lock();
        if e.bytes + len > EVENT_SPACE {
            return Err(Errno::ENOMEM);
        }
        let want = e.queue.len() + e.reserved + 1;
        let have = e.queue.capacity();
        if want > have {
            let extra = want - e.queue.len();
            e.queue.try_reserve(extra).map_err(|_| Errno::ENOMEM)?;
        }
        e.bytes += len;
        e.reserved += 1;
        Ok(())
    }

    /// Gives back a promise of `len` bytes that will not be used.
    pub(crate) fn cancel_event(&self, len: usize) {
        let mut e = self.events.lock();
        e.bytes = e.bytes.saturating_sub(len);
        e.reserved = e.reserved.saturating_sub(1);
    }

    /// Sends an event that [`Open::reserve_event`] promised room for.
    pub(crate) fn send_reserved(&self, event: Vec<u8>) {
        {
            let mut e = self.events.lock();
            e.reserved = e.reserved.saturating_sub(1);
            e.queue.push_back(event);
        }
        self.wait.wake_all();
        self.notify.notify(0);
    }

    /// This file's magic number, made on first ask: what it gives the
    /// master to be authenticated (`drm_getmagic`).
    pub(crate) fn magic(self: &Arc<Self>) -> u32 {
        let have = self.magic.load(Ordering::Relaxed);
        if have != 0 {
            return have;
        }
        // Each number differs from the last and is never 0; the
        // multiplier spreads them (they are names, not secrets).
        let n = self.card.next_magic.fetch_add(1, Ordering::Relaxed);
        let m = n.wrapping_mul(0x9e37_79b9) | 1;
        match self
            .magic
            .compare_exchange(0, m, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {
                self.card.magics.lock().insert(m, Arc::downgrade(self));
                m
            }
            Err(other) => other,
        }
    }

    /// The master vouches for the file that asked for `magic`
    /// (`drm_authmagic`): the number is spent.
    pub(crate) fn authenticate(&self, magic: u32) -> Result<(), Errno> {
        let file = self.card.magics.lock().remove(&magic);
        let file = file.and_then(|w| w.upgrade()).ok_or(Errno::EINVAL)?;
        file.authenticated.store(true, Ordering::Release);
        Ok(())
    }

    /// Whether the file may use the calls that need the master's
    /// say-so: it is the master, or was authenticated.
    #[must_use]
    pub fn is_authenticated(&self) -> bool {
        self.authenticated.load(Ordering::Acquire) || self.is_master()
    }

    fn hangup(&self) {
        self.hung_up.store(true, Ordering::Release);
        self.wait.wake_all();
        self.notify.notify(0);
    }

    fn ready(&self) -> bool {
        !self.events.lock().queue.is_empty()
    }

    /// Sets a display controller from inside the kernel: `a` is the
    /// older set-controller call's argument (a framebuffer of 0 turns it
    /// off), `connectors` the connectors' numbers. Waits for the frame.
    ///
    /// # Errors
    ///
    /// As the set-controller ioctl: `EACCES` unless this file is master,
    /// `ENOENT` for an object that does not exist, `EINVAL` for a state
    /// that cannot be shown.
    pub fn set_crtc(
        &self,
        s: &Sleepable,
        a: &abi::drm::DrmModeCrtc,
        connectors: &[u32],
    ) -> Result<(), Errno> {
        crate::atomic::set_crtc_with(self, s, a, connectors)
    }

    /// Whether this file is the device's master.
    #[must_use]
    pub fn is_master(&self) -> bool {
        self.card.is_master(self.id)
    }

    /// The file's identity.
    #[must_use]
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The buffer `handle` names in this file.
    #[must_use]
    pub fn gem(&self, handle: u32) -> Option<Arc<crate::gem::Gem>> {
        self.handles.lock().get(handle).cloned()
    }

    /// Whether the client turned on `cap` (`client::*`).
    pub(crate) fn has_client_cap(&self, cap: u8) -> bool {
        self.client_caps.load(Ordering::Relaxed) & cap != 0
    }
}

struct ReadFilter(Arc<Open>);

impl vfs::event::Filter for ReadFilter {
    fn attach(&self, kn: &Arc<vfs::event::Knote>) -> Result<(), Errno> {
        self.0.notify.add(kn);
        Ok(())
    }

    fn detach(&self, kn: &vfs::event::Knote) {
        self.0.notify.remove(kn);
    }

    fn event(&self, _kn: &vfs::event::Knote, note: &mut vfs::event::Note, _hint: u64) -> bool {
        if self.0.hung_up.load(Ordering::Acquire) {
            note.kev.flags |= abi::event::EV_EOF;
            return true;
        }
        let e = self.0.events.lock();
        note.kev.data = i64::try_from(e.bytes).unwrap_or(i64::MAX);
        !e.queue.is_empty()
    }
}

/// The per-open view of an [`Open`] that devfs holds: `Arc<Open>` itself
/// implements the device operations.
impl CharDevice for Open {
    fn read(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        _: u64,
        nonblock: bool,
    ) -> Result<usize, Errno> {
        if self.hung_up.load(Ordering::Acquire) {
            return Err(Errno::ENODEV);
        }
        if !self.ready() {
            if nonblock {
                return Err(Errno::EAGAIN);
            }
            self.wait
                .wait_until_interruptible(s, || {
                    self.ready() || self.hung_up.load(Ordering::Acquire)
                })
                .map_err(|_| Errno::EINTR)?;
            if self.hung_up.load(Ordering::Acquire) {
                return Err(Errno::ENODEV);
            }
        }
        // Whole events only, as many as fit.
        let mut done = 0;
        loop {
            let ev = {
                let mut e = self.events.lock();
                match e.queue.front() {
                    Some(f) if f.len() <= uio.resid() => {
                        let f = e.queue.pop_front().unwrap_or_default();
                        e.bytes -= f.len();
                        f
                    }
                    _ => break,
                }
            };
            done += uio.copy_out(&ev)?;
        }
        Ok(done)
    }

    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::EINVAL)
    }

    fn poll(&self, events: i16, w: Option<&Arc<dyn vfs::event::Wake>>) -> i16 {
        use abi::event::{POLLHUP, POLLIN, POLLRDNORM};
        if self.hung_up.load(Ordering::Acquire) {
            return POLLHUP;
        }
        if events & (POLLIN | POLLRDNORM) == 0 {
            return 0;
        }
        if let Some(w) = w {
            self.notify.record(w);
        }
        if self.ready() {
            events & (POLLIN | POLLRDNORM)
        } else {
            0
        }
    }

    fn kqfilter(&self, kev: &abi::event::Kevent) -> Result<Box<dyn vfs::event::Filter>, Errno> {
        // The filter holds the file; devfs holds `Arc<Open>`, so find it
        // again through the card's list.
        if kev.filter != abi::event::EVFILT_READ {
            return Err(Errno::EINVAL);
        }
        let me = self
            .card
            .files
            .lock()
            .iter()
            .filter_map(alloc::sync::Weak::upgrade)
            .find(|f| f.id == self.id)
            .ok_or(Errno::ENODEV)?;
        Ok(Box::new(ReadFilter(me)))
    }

    fn mmap(&self, page: u64, pages: u64) -> Result<Arc<Object>, Errno> {
        if self.card.is_revoked() {
            return Err(Errno::ENODEV);
        }
        let h = self.handles.lock();
        let gem = h.by_offset(page).ok_or(Errno::EINVAL)?;
        gem.map_range(page, pages)
    }

    fn ioctl_file(
        &self,
        s: &Sleepable,
        req: u32,
        arg: &mut [u8],
        _fflag: u32,
    ) -> Result<usize, Errno> {
        crate::ioctl::dispatch(self, s, req, arg)
    }

    fn revoke(&self, _s: &Sleepable) {
        self.hangup();
    }
}
