// SPDX-License-Identifier: BSD-2-Clause
//! Drivers, bindings, and the contexts drivers run in (DRIVER-MODEL.md §7,
//! §10).
//!
//! A driver implements [`Driver`]: `attach` builds the instance from its
//! granted resources, `start` gives it its ways in (interrupt handlers,
//! children, publications) now that it has a fixed address, and `stop`
//! quiesces the hardware. There is no detach callback: the framework runs
//! the detach steps itself, and dropping the instance releases every
//! grant it owns.
//!
//! Everything a driver can be entered through is created by
//! [`StartCx`] and recorded in its [`Binding`], so detach can take it all
//! down, in reverse order, before the instance is dropped.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::any::Any;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, Ordering};

use irq::{Filter, Installed};
use regs::{Bytes, Mmio, RegisterBlock};
use sched::lock::{LockClass, Mutex};
use sched::{IrqCx, Sleepable};
use vm::dma::{DmaConstraints, DmaTag};

use crate::manager::Manager;
use crate::types::{
    Announce, AttachError, Busy, Concurrency, DeviceId, Ident, Ids, Resource, ResourceError,
    StopReason,
};

/// A device driver.
pub trait Driver: Sized + Send + Sync + 'static {
    /// The driver name, which is also the devclass: `"nvme"`.
    const NAME: &'static str;
    /// The match table.
    const IDS: Ids;
    /// Drivers this one gives way to on an equal rank.
    const YIELDS: &'static [&'static str] = &[];
    /// How the framework enters the driver.
    const CONCURRENCY: Concurrency = Concurrency::Serialized;
    /// How the device is announced.
    const ANNOUNCE: Announce = Announce::Description;

    /// The device description (`%desc`, `devinfo`'s `<…>`).
    fn describe(ident: &Ident) -> String;

    /// Claims resources, resets and identifies the hardware, and builds
    /// the instance. On error everything claimed is dropped.
    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError>;

    /// Installs handlers, adds children, publishes. On error the framework
    /// detaches what was started.
    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let _ = cx;
        Ok(())
    }

    /// Stops the hardware: no more DMA, interrupts masked at the device.
    /// Must not fail.
    fn stop(&self, cx: &mut StopCx<'_>, why: StopReason);

    /// Asked before a voluntary detach.
    fn may_detach(&self) -> Result<(), Busy> {
        Ok(())
    }
}

/// A driver, type-erased, for the build's driver table.
#[derive(Clone, Copy)]
pub struct DriverDecl {
    /// The driver name.
    pub name: &'static str,
    /// The match table.
    pub ids: Ids,
    /// Drivers it yields to on an equal rank.
    pub yields: &'static [&'static str],
    pub(crate) announce: Announce,
    pub(crate) describe: fn(&Ident) -> String,
    pub(crate) attach: fn(&mut AttachCx<'_>) -> Result<Box<dyn Bound>, AttachError>,
}

impl core::fmt::Debug for DriverDecl {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DriverDecl")
            .field("name", &self.name)
            .finish()
    }
}

impl DriverDecl {
    /// The declaration for driver `D`.
    #[must_use]
    pub const fn of<D: Driver>() -> Self {
        Self {
            name: D::NAME,
            ids: D::IDS,
            yields: D::YIELDS,
            announce: D::ANNOUNCE,
            describe: D::describe,
            attach: attach_erased::<D>,
        }
    }
}

fn attach_erased<D: Driver>(cx: &mut AttachCx<'_>) -> Result<Box<dyn Bound>, AttachError> {
    let d = D::attach(cx)?;
    Ok(Box::new(Binding::<D> {
        driver: Some(NonNull::from(Box::leak(Box::new(d)))),
        serializer: Mutex::new(&SERIALIZER, ()),
        sources: Vec::new(),
        ledger: Arc::clone(&cx.ledger),
        node: cx.node,
    }))
}

/// The lock class of every binding serializer: above the manager's locks,
/// below drivers' own.
pub(crate) static SERIALIZER: LockClass = LockClass::new("binding serializer", 400);

/// Grants issued to a binding and returned by it, checked at detach
/// (DRIVER-MODEL.md §13.1).
#[derive(Debug, Default)]
pub struct Ledger {
    issued: AtomicU32,
    returned: AtomicU32,
}

impl Ledger {
    fn issue(&self) {
        self.issued.fetch_add(1, Ordering::Relaxed);
    }

    fn give_back(&self) {
        self.returned.fetch_add(1, Ordering::Relaxed);
    }

    /// Grants not yet returned.
    #[must_use]
    pub fn outstanding(&self) -> u32 {
        self.issued
            .load(Ordering::Relaxed)
            .saturating_sub(self.returned.load(Ordering::Relaxed))
    }
}

/// A granted register block (typed, or [`Bytes`]); unmapped when
/// dropped.
pub struct MmioGrant<B> {
    regs: Mmio<B>,
    va: usize,
    len: u64,
    pa: u64,
    mgr: &'static Manager,
    ledger: Arc<Ledger>,
}

impl<B> core::fmt::Debug for MmioGrant<B> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MmioGrant")
            .field("pa", &format_args!("{:#x}", self.pa))
            .field("len", &self.len)
            .finish()
    }
}

impl<B> core::ops::Deref for MmioGrant<B> {
    type Target = Mmio<B>;
    fn deref(&self) -> &Mmio<B> {
        &self.regs
    }
}

impl<B> Drop for MmioGrant<B> {
    fn drop(&mut self) {
        self.mgr.platform().unmap_device(self.va, self.len);
        self.mgr.release_claim(self.pa);
        self.ledger.give_back();
    }
}

/// A granted range of I/O ports (x86); released when dropped.
pub struct IoGrant {
    base: u16,
    len: u16,
    mgr: &'static Manager,
    ledger: Arc<Ledger>,
}

impl core::fmt::Debug for IoGrant {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IoGrant")
            .field("base", &format_args!("{:#x}", self.base))
            .field("len", &self.len)
            .finish()
    }
}

impl IoGrant {
    fn port(&self, offset: u16, width: u16) -> u16 {
        assert!(
            offset.is_multiple_of(width)
                && u32::from(offset) + u32::from(width) <= u32::from(self.len),
            "I/O port offset out of range"
        );
        self.base + offset
    }

    /// The number of ports.
    #[must_use]
    pub fn len(&self) -> u16 {
        self.len
    }

    /// Whether the range is empty (never: resources have ports).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Reads the 8-bit port at `offset`.
    #[must_use]
    pub fn read8(&self, offset: u16) -> u8 {
        self.mgr
            .platform()
            .io_read(self.port(offset, 1), 1)
            .to_le_bytes()[0]
    }

    /// Reads the 16-bit port at `offset`.
    #[must_use]
    pub fn read16(&self, offset: u16) -> u16 {
        let [a, b, ..] = self
            .mgr
            .platform()
            .io_read(self.port(offset, 2), 2)
            .to_le_bytes();
        u16::from_le_bytes([a, b])
    }

    /// Reads the 32-bit port at `offset`.
    #[must_use]
    pub fn read32(&self, offset: u16) -> u32 {
        self.mgr.platform().io_read(self.port(offset, 4), 4)
    }

    /// Writes the 8-bit port at `offset`.
    pub fn write8(&self, offset: u16, v: u8) {
        self.mgr
            .platform()
            .io_write(self.port(offset, 1), 1, v.into());
    }

    /// Writes the 16-bit port at `offset`.
    pub fn write16(&self, offset: u16, v: u16) {
        self.mgr
            .platform()
            .io_write(self.port(offset, 2), 2, v.into());
    }

    /// Writes the 32-bit port at `offset`.
    pub fn write32(&self, offset: u16, v: u32) {
        self.mgr.platform().io_write(self.port(offset, 4), 4, v);
    }
}

impl Drop for IoGrant {
    fn drop(&mut self) {
        self.mgr
            .release_claim(crate::manager::IO_SPACE | u64::from(self.base));
        self.ledger.give_back();
    }
}

/// A granted interrupt input, to install a handler on in `start`.
#[derive(Debug)]
pub struct IrqLine {
    resource: Resource,
    ledger: Arc<Ledger>,
}

impl Drop for IrqLine {
    fn drop(&mut self) {
        self.ledger.give_back();
    }
}

/// Fault injection for verifier mode: a seeded generator deciding which
/// grants fail, so attach error paths actually run.
#[derive(Debug)]
pub(crate) struct Faults {
    pub(crate) state: u64,
    /// Fail one grant in this many (0: never).
    pub(crate) one_in: u64,
}

impl Faults {
    fn fail(&mut self) -> bool {
        if self.one_in == 0 {
            return false;
        }
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        self.state.is_multiple_of(self.one_in)
    }
}

/// What `attach` works with: the node's resources, its bus data, and
/// thread context (it may sleep, for device resets).
pub struct AttachCx<'a> {
    pub(crate) mgr: &'static Manager,
    pub(crate) node: DeviceId,
    pub(crate) ident: &'a Ident,
    pub(crate) resources: &'a [Resource],
    pub(crate) bus_data: Option<Arc<dyn Any + Send + Sync>>,
    pub(crate) unit: u32,
    pub(crate) s: &'a Sleepable,
    pub(crate) ledger: Arc<Ledger>,
    pub(crate) faults: Option<&'a mut Faults>,
}

impl core::fmt::Debug for AttachCx<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AttachCx")
            .field("node", &self.node)
            .field("unit", &self.unit)
            .finish_non_exhaustive()
    }
}

impl<'a> AttachCx<'a> {
    /// The device's identity.
    #[must_use]
    pub fn ident(&self) -> &Ident {
        self.ident
    }

    /// The node being attached (a bus's children's parent).
    #[must_use]
    pub fn node(&self) -> DeviceId {
        self.node
    }

    /// The unit number chosen for the device.
    #[must_use]
    pub fn unit(&self) -> u32 {
        self.unit
    }

    /// The thread context, for bounded sleeps during attach.
    #[must_use]
    pub fn sleepable(&self) -> &'a Sleepable {
        self.s
    }

    /// The parent bus's typed data for this child, if it attached one of
    /// type `T` (in place of FreeBSD's ivars).
    #[must_use]
    pub fn bus_data<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.bus_data.as_deref()?.downcast_ref::<T>()
    }

    /// The parent bus's data for this child as a shared `Arc<T>`, for a
    /// driver that keeps it.
    #[must_use]
    pub fn bus_data_arc<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        Arc::clone(self.bus_data.as_ref()?).downcast::<T>().ok()
    }

    fn injected(&mut self) -> bool {
        self.faults.as_mut().is_some_and(|f| f.fail())
    }

    /// Claims and maps the `index`th memory resource, at least `min` bytes.
    fn map_memory(&mut self, index: usize, min: usize) -> Result<(usize, u64, u64), ResourceError> {
        if self.injected() {
            return Err(ResourceError::MapFailed);
        }
        let (base, len) = self
            .resources
            .iter()
            .filter_map(|r| match r {
                Resource::Memory { base, len, .. } => Some((*base, *len)),
                _ => None,
            })
            .nth(index)
            .ok_or(ResourceError::NoSuchResource)?;
        if len < min as u64 {
            return Err(ResourceError::TooSmall);
        }
        if self.mgr.platform().sensitive(base, len) {
            return Err(ResourceError::Sensitive);
        }
        self.mgr.claim(base, len)?;
        let Some(va) = self.mgr.platform().map_device(base, len) else {
            self.mgr.release_claim(base);
            return Err(ResourceError::MapFailed);
        };
        self.ledger.issue();
        Ok((va, base, len))
    }

    /// Maps the `index`th memory resource as register block `B`.
    pub fn mmio<B: RegisterBlock>(&mut self, index: usize) -> Result<MmioGrant<B>, ResourceError> {
        let (va, pa, len) = self.map_memory(index, B::SIZE)?;
        Ok(MmioGrant {
            // SAFETY: the platform mapped `len >= B::SIZE` bytes of this
            // device's registers for us alone (the claim), until the grant
            // unmaps them.
            regs: unsafe { Mmio::block(va) },
            va,
            len,
            pa,
            mgr: self.mgr,
            ledger: Arc::clone(&self.ledger),
        })
    }

    /// Maps the `index`th memory resource whole, untyped, for registers
    /// at offsets the device itself reports (capability structures).
    pub fn mmio_bytes(&mut self, index: usize) -> Result<MmioGrant<Bytes>, ResourceError> {
        let (va, pa, len) = self.map_memory(index, 1)?;
        let bytes = usize::try_from(len).map_err(|_| ResourceError::MapFailed)?;
        Ok(MmioGrant {
            // SAFETY: as in `mmio`, for all `len` bytes.
            regs: unsafe { Mmio::new(va, bytes) },
            va,
            len,
            pa,
            mgr: self.mgr,
            ledger: Arc::clone(&self.ledger),
        })
    }

    /// Claims the `index`th I/O-port resource.
    pub fn io_ports(&mut self, index: usize) -> Result<IoGrant, ResourceError> {
        if self.injected() {
            return Err(ResourceError::NoSuchResource);
        }
        let (base, len) = self
            .resources
            .iter()
            .filter_map(|r| match r {
                Resource::IoPort { base, len } => Some((*base, *len)),
                _ => None,
            })
            .nth(index)
            .ok_or(ResourceError::NoSuchResource)?;
        self.mgr
            .claim(crate::manager::IO_SPACE | u64::from(base), len.into())?;
        self.ledger.issue();
        Ok(IoGrant {
            base,
            len,
            mgr: self.mgr,
            ledger: Arc::clone(&self.ledger),
        })
    }

    /// The `index`th interrupt resource.
    pub fn irq(&mut self, index: usize) -> Result<IrqLine, ResourceError> {
        if self.injected() {
            return Err(ResourceError::NoSuchResource);
        }
        let r = *self
            .resources
            .iter()
            .filter(|r| matches!(r, Resource::Irq { .. }))
            .nth(index)
            .ok_or(ResourceError::NoSuchResource)?;
        self.ledger.issue();
        Ok(IrqLine {
            resource: r,
            ledger: Arc::clone(&self.ledger),
        })
    }

    /// A line for an interrupt the parent bus set up for this device during
    /// attach (message-signalled vectors), granted like a described one.
    pub fn irq_line(&mut self, r: Resource) -> Result<IrqLine, ResourceError> {
        if !matches!(r, Resource::Irq { .. }) {
            return Err(ResourceError::NoSuchResource);
        }
        self.ledger.issue();
        Ok(IrqLine {
            resource: r,
            ledger: Arc::clone(&self.ledger),
        })
    }

    /// The resources the bus described for the device, to hand on to a
    /// child (a transport bus passes its interrupt on).
    #[must_use]
    pub fn resources(&self) -> &[Resource] {
        self.resources
    }

    /// A DMA tag with the device's constraints (and, later, its bus's).
    pub fn dma_tag(&mut self, c: DmaConstraints) -> Result<DmaTag, ResourceError> {
        if self.injected() {
            return Err(ResourceError::MapFailed);
        }
        let p = self.mgr.platform();
        Ok(DmaTag::new(p.dma_memory(), c, p.top_of_ram()))
    }
}

/// The trampoline data for one interrupt handler.
struct IrqShim<D> {
    driver: *const D,
    serializer: *const Mutex<()>,
    filter: Option<fn(&D, usize, &IrqCx) -> Filter>,
    thread: Option<fn(&D, usize, &Sleepable)>,
    arg: usize,
}

fn filter_shim<D: Driver>(p: usize, cx: &IrqCx) -> Filter {
    // SAFETY: `p` is the shim boxed by `StartCx::interrupt`, which lives
    // (as does the instance it points to) until detach has removed the
    // handler synchronously.
    let shim = unsafe { &*core::ptr::with_exposed_provenance::<IrqShim<D>>(p) };
    match shim.filter {
        // SAFETY: as above.
        Some(f) => f(unsafe { &*shim.driver }, shim.arg, cx),
        None => Filter::Schedule,
    }
}

fn thread_shim<D: Driver>(p: usize, s: &Sleepable) {
    // SAFETY: as in `filter_shim`; the interrupt thread is joined before
    // the shim is freed.
    let shim = unsafe { &*core::ptr::with_exposed_provenance::<IrqShim<D>>(p) };
    if let Some(f) = shim.thread {
        // SAFETY: as above.
        let d = unsafe { &*shim.driver };
        if D::CONCURRENCY == Concurrency::Serialized {
            // SAFETY: the serializer lives in the binding, alive as above.
            let _g = unsafe { &*shim.serializer }.lock(s);
            f(d, shim.arg, s);
        } else {
            f(d, shim.arg, s);
        }
    }
}

/// An async source recorded in a binding, for teardown.
enum Source {
    Irq(Installed, Box<dyn Any + Send>),
    Child(DeviceId),
}

/// What `start` works with.
pub struct StartCx<'a, D: Driver> {
    pub(crate) binding: &'a mut Binding<D>,
    pub(crate) mgr: &'static Manager,
    pub(crate) s: &'a Sleepable,
}

impl<D: Driver> core::fmt::Debug for StartCx<'_, D> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StartCx").field("driver", &D::NAME).finish()
    }
}

impl<D: Driver> StartCx<'_, D> {
    /// The thread context.
    #[must_use]
    pub fn sleepable(&self) -> &Sleepable {
        self.s
    }

    /// The node being started.
    #[must_use]
    pub fn node(&self) -> DeviceId {
        self.binding.node
    }

    /// Installs a handler on `line`: a filter in interrupt context, a
    /// thread handler in the line's interrupt thread (serialized with the
    /// driver's other thread-context entry points unless it is
    /// [`Concurrency::Concurrent`]), or both. The handlers are plain
    /// functions of the instance: they capture nothing, so nothing outlives
    /// detach.
    pub fn interrupt(
        &mut self,
        line: IrqLine,
        filter: Option<fn(&D, usize, &IrqCx) -> Filter>,
        thread: Option<fn(&D, usize, &Sleepable)>,
        arg: usize,
    ) -> Result<(), ResourceError> {
        let Resource::Irq {
            domain,
            hw,
            trigger,
            polarity,
        } = line.resource
        else {
            return Err(ResourceError::NoSuchResource);
        };
        let driver: *const D = self.binding.driver.expect("started instance").as_ptr();
        let shim = Box::new(IrqShim {
            driver,
            serializer: &raw const self.binding.serializer,
            filter,
            thread,
            arg,
        });
        let arg = core::ptr::from_ref::<IrqShim<D>>(&shim).expose_provenance();
        let handler = irq::Handler {
            name: D::NAME,
            filter: filter.map(|_| filter_shim::<D> as irq::FilterFn),
            thread: thread.map(|_| thread_shim::<D> as irq::ThreadFn),
            arg,
        };
        let installed = irq::install(domain, hw, trigger, polarity, hal::CpuId::BOOT, handler)
            .map_err(ResourceError::Irq)?;
        // The line's grant is now held by the installed handler.
        drop(line);
        self.binding.ledger.issue();
        self.binding.sources.push(Source::Irq(installed, shim));
        Ok(())
    }

    /// Adds a child device on this (bus) driver's node, with its identity,
    /// typed bus data, and resources. The manager matches it at once.
    pub fn add_child(
        &mut self,
        ident: Ident,
        bus_data: Option<Arc<dyn Any + Send + Sync>>,
        resources: Vec<Resource>,
    ) -> DeviceId {
        self.add_child_with_deps(ident, bus_data, resources, &[])
    }

    /// Adds a disabled channel (FreeBSD's `device_disable`): named with
    /// its class's next unit, never attached.
    pub fn add_disabled_child(&mut self, ident: Ident) -> DeviceId {
        let id = self
            .mgr
            .add_disabled_child(self.s, self.binding.node, ident);
        self.binding.sources.push(Source::Child(id));
        id
    }

    /// [`StartCx::add_child`] with firmware dependencies (`_DEP`, resource
    /// sources) beyond this parent.
    pub fn add_child_with_deps(
        &mut self,
        ident: Ident,
        bus_data: Option<Arc<dyn Any + Send + Sync>>,
        resources: Vec<Resource>,
        deps: &[(DeviceId, devtree::graph::EdgeKind)],
    ) -> DeviceId {
        let id = self
            .mgr
            .add_child(self.s, self.binding.node, ident, bus_data, resources, deps);
        self.binding.sources.push(Source::Child(id));
        id
    }
}

impl<D: Driver> StartCx<'_, D> {
    /// Makes child `consumer` wait for `supplier` (another node), for a
    /// firmware dependency that names a sibling added after it. Children
    /// do not attach before their parent's `start` returns, so edges added
    /// here always hold them back.
    pub fn add_dependency(
        &mut self,
        consumer: DeviceId,
        supplier: DeviceId,
        kind: devtree::graph::EdgeKind,
    ) {
        if self.children_contains(consumer) {
            self.mgr.add_dependency(self.s, consumer, supplier, kind);
        }
    }

    fn children_contains(&self, id: DeviceId) -> bool {
        self.binding
            .sources
            .iter()
            .any(|s| matches!(s, Source::Child(c) if *c == id))
    }
}

// SAFETY: the shim's raw pointers point into the binding, which the
// installed handler never outlives (see `Binding::detach`).
unsafe impl<D: Driver> Send for IrqShim<D> {}

/// What `stop` works with: thread context for bounded waits.
#[derive(Debug)]
pub struct StopCx<'a> {
    pub(crate) s: &'a Sleepable,
}

impl StopCx<'_> {
    /// The thread context.
    #[must_use]
    pub fn sleepable(&self) -> &Sleepable {
        self.s
    }
}

/// A driver instance and everything created for it. The instance lives
/// in its own allocation, owned through a raw pointer (from
/// `Box::into_raw`) so handlers can point at it while the binding's other
/// fields change; it is freed at detach step 6, after every handler is
/// gone.
pub(crate) struct Binding<D: Driver> {
    driver: Option<NonNull<D>>,
    serializer: Mutex<()>,
    sources: Vec<Source>,
    ledger: Arc<Ledger>,
    node: DeviceId,
}

/// What a detach left behind.
#[derive(Debug)]
pub struct DetachReport {
    /// Grants the instance did not give back (a driver bug: it moved a
    /// grant somewhere that outlives it).
    pub leaked_grants: u32,
}

/// A binding, type-erased.
pub(crate) trait Bound: Send + Sync {
    fn start(&mut self, mgr: &'static Manager, s: &Sleepable) -> Result<(), AttachError>;
    fn may_detach(&self) -> Result<(), Busy>;
    /// Children created by `start` (detached first).
    fn children(&self) -> Vec<DeviceId>;
    /// Steps 3 to 7 of the detach protocol.
    fn detach(self: Box<Self>, why: StopReason, s: &Sleepable) -> DetachReport;
}

// SAFETY: the instance is `Send + Sync`; the sources are only touched by
// the thread running start or detach, which owns the binding.
unsafe impl<D: Driver> Sync for Binding<D> {}
// SAFETY: as above.
unsafe impl<D: Driver> Send for Binding<D> {}

impl<D: Driver> Bound for Binding<D> {
    fn start(&mut self, mgr: &'static Manager, s: &Sleepable) -> Result<(), AttachError> {
        let d: *const D = self.driver.expect("instance").as_ptr();
        let mut cx = StartCx {
            binding: self,
            mgr,
            s,
        };
        // SAFETY: the instance lives until detach; `start` gets it by
        // shared reference while the context holds the binding's other
        // fields mutably (sources, ledger), which do not contain it.
        unsafe { &*d }.start(&mut cx)
    }

    fn may_detach(&self) -> Result<(), Busy> {
        // SAFETY: the instance lives until detach, which needs `&mut self`.
        self.driver
            .map_or(Ok(()), |d| unsafe { d.as_ref() }.may_detach())
    }

    fn children(&self) -> Vec<DeviceId> {
        self.sources
            .iter()
            .filter_map(|s| match s {
                Source::Child(id) => Some(*id),
                Source::Irq(..) => None,
            })
            .collect()
    }

    fn detach(mut self: Box<Self>, why: StopReason, s: &Sleepable) -> DetachReport {
        // 3. Revoke publications (none yet: classes arrive in P3.7).
        // 4. Stop the hardware.
        if let Some(d) = self.driver {
            // SAFETY: alive until step 6 below.
            unsafe { d.as_ref() }.stop(&mut StopCx { s }, why);
        }
        // 5. Tear down async sources, newest first.
        while let Some(src) = self.sources.pop() {
            match src {
                Source::Irq(installed, shim) => {
                    installed.remove(s);
                    drop(shim);
                    self.ledger.give_back();
                }
                Source::Child(_) => {}
            }
        }
        // 6. Drop the instance: its grants go back as fields drop.
        if let Some(d) = self.driver.take() {
            // SAFETY: from `Box::leak`; every handler pointing at it was
            // removed in step 5, and nothing else holds it.
            drop(unsafe { Box::from_raw(d.as_ptr()) });
        }
        // 7. Check the ledger.
        DetachReport {
            leaked_grants: self.ledger.outstanding(),
        }
    }
}

impl<D: Driver> Drop for Binding<D> {
    fn drop(&mut self) {
        // Detach frees the instance; a binding dropped without detach (a
        // failed start that was not torn down) would leak it rather than
        // free memory handlers may still use.
        debug_assert!(self.driver.is_none() || self.sources.is_empty());
    }
}
