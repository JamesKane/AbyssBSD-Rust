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

use crate::gpio::{Gpio, GpioMode, GpioPins, GpioSource, PinMode, Restriction};
use crate::i2c::{I2c, I2cDevice, I2cSource};
use crate::manager::{Manager, Provided};
use crate::provider::{
    Clock, ClockHandle, ClockSource, DomainSource, PowerDomain, PowerVote, Reset, ResetHandle,
    ResetSource,
};
use crate::space::{AddressSpace, Range, Sharing};
use crate::types::{
    Announce, AttachError, Busy, Concurrency, DeviceId, Ident, Ids, Line, LineDetail, LineKind,
    Resource, ResourceError, StopReason,
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
    /// The units FreeBSD's `device.hints` wire for this driver: a device
    /// at a hint's I/O port takes its unit, and the others skip the
    /// hinted units (`acpi_hint_device_unit`, `devclass_alloc_unit`).
    const HINTS: &'static [Hint] = &[];

    /// The unit the device must take, for a driver whose units are
    /// numbers the system already gave (FreeBSD's `acpi_cpu_probe` sets a
    /// processor's unit to its CPU's ID with `device_set_unit`). `None`
    /// takes the next free unit, as does a unit already taken.
    fn unit(ident: &Ident, bus_data: Option<&(dyn Any + Send + Sync)>) -> Option<u32> {
        let _ = (ident, bus_data);
        None
    }

    /// The device description (`%desc`, `devinfo`'s `<…>`).
    fn describe(ident: &Ident) -> String;

    /// The description, from the identity and the bus data, for a driver
    /// that reads the device to describe it (as FreeBSD's `hms_probe`
    /// does with the report descriptor). By default, [`Driver::describe`].
    fn describe_with(ident: &Ident, bus_data: Option<&(dyn Any + Send + Sync)>) -> String {
        let _ = bus_data;
        Self::describe(ident)
    }

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
    /// Its hinted units ([`Driver::HINTS`]).
    pub hints: &'static [Hint],
    pub(crate) announce: Announce,
    pub(crate) describe: fn(&Ident, Option<&(dyn Any + Send + Sync)>) -> String,
    pub(crate) unit: fn(&Ident, Option<&(dyn Any + Send + Sync)>) -> Option<u32>,
    pub(crate) attach: fn(&mut AttachCx<'_>) -> Result<Box<dyn Bound>, AttachError>,
}

/// A unit wired by `device.hints` (`hint.<driver>.<unit>.port`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hint {
    pub unit: u32,
    pub port: u16,
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
            hints: D::HINTS,
            announce: D::ANNOUNCE,
            describe: D::describe_with,
            unit: D::unit,
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
    sharing: Sharing,
    mgr: &'static Manager,
    ledger: Arc<Ledger>,
}

impl<B> MmioGrant<B> {
    /// Whether firmware or another device shares the range (§8.1).
    #[must_use]
    pub fn sharing(&self) -> Sharing {
        self.sharing
    }
}

impl<B> core::fmt::Debug for MmioGrant<B> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MmioGrant")
            .field("pa", &format_args!("{:#x}", self.pa))
            .field("len", &self.len)
            .field("sharing", &self.sharing)
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
        self.mgr.release_claim(Range {
            space: AddressSpace::Memory,
            base: self.pa,
            len: self.len,
        });
        self.ledger.give_back();
    }
}

/// The resource-space range of ports `[base, base + len)`.
fn io_range(base: u16, len: u16) -> Range {
    Range {
        space: AddressSpace::IoPort,
        base: base.into(),
        len: len.into(),
    }
}

/// A granted range of I/O ports (x86); released when dropped.
pub struct IoGrant {
    base: u16,
    len: u16,
    sharing: Sharing,
    mgr: &'static Manager,
    ledger: Arc<Ledger>,
}

impl core::fmt::Debug for IoGrant {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IoGrant")
            .field("base", &format_args!("{:#x}", self.base))
            .field("len", &self.len)
            .field("sharing", &self.sharing)
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

    /// Whether firmware shares the ports (§8.1).
    #[must_use]
    pub fn sharing(&self) -> Sharing {
        self.sharing
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
        self.mgr.release_claim(io_range(self.base, self.len));
        self.ledger.give_back();
    }
}

/// A granted interrupt input, to install a handler on in `start`.
#[derive(Debug)]
pub struct IrqLine {
    resource: Resource,
    ledger: Arc<Ledger>,
    /// The GPIO pin it is, held as long as the interrupt is.
    pin: Option<GpioPins>,
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

    /// Claims and maps the `index`th memory resource, at least `min` bytes;
    /// a firmware-shared range only if `firmware_ok`.
    fn map_memory(
        &mut self,
        index: usize,
        min: usize,
        firmware_ok: bool,
    ) -> Result<(usize, u64, u64, Sharing), ResourceError> {
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
        let range = Range {
            space: AddressSpace::Memory,
            base,
            len,
        };
        let sharing = self.mgr.claim(range, firmware_ok)?;
        let Some(va) = self.mgr.platform().map_device(base, len) else {
            self.mgr.release_claim(range);
            return Err(ResourceError::MapFailed);
        };
        self.ledger.issue();
        Ok((va, base, len, sharing))
    }

    fn mmio_grant<B: RegisterBlock>(
        &mut self,
        index: usize,
        firmware_ok: bool,
    ) -> Result<MmioGrant<B>, ResourceError> {
        let (va, pa, len, sharing) = self.map_memory(index, B::SIZE, firmware_ok)?;
        Ok(MmioGrant {
            // SAFETY: the platform mapped `len >= B::SIZE` bytes of this
            // device's registers for us (the claim: alone, or shared as
            // `sharing` says), until the grant unmaps them.
            regs: unsafe { Mmio::block(va) },
            va,
            len,
            pa,
            sharing,
            mgr: self.mgr,
            ledger: Arc::clone(&self.ledger),
        })
    }

    fn mmio_bytes_grant(
        &mut self,
        index: usize,
        firmware_ok: bool,
    ) -> Result<MmioGrant<Bytes>, ResourceError> {
        let (va, pa, len, sharing) = self.map_memory(index, 1, firmware_ok)?;
        let bytes = usize::try_from(len).map_err(|_| ResourceError::MapFailed)?;
        Ok(MmioGrant {
            // SAFETY: as in `mmio_grant`, for all `len` bytes.
            regs: unsafe { Mmio::new(va, bytes) },
            va,
            len,
            pa,
            sharing,
            mgr: self.mgr,
            ledger: Arc::clone(&self.ledger),
        })
    }

    /// Maps the `index`th memory resource as register block `B`. A range
    /// AML also touches is refused ([`ResourceError::FirmwareShared`]);
    /// one the platform declares shared with another device is granted,
    /// and [`MmioGrant::sharing`] says so.
    pub fn mmio<B: RegisterBlock>(&mut self, index: usize) -> Result<MmioGrant<B>, ResourceError> {
        self.mmio_grant(index, false)
    }

    /// Maps the `index`th memory resource whole, untyped, for registers
    /// at offsets the device itself reports (capability structures).
    /// Shared ranges as in [`AttachCx::mmio`].
    pub fn mmio_bytes(&mut self, index: usize) -> Result<MmioGrant<Bytes>, ResourceError> {
        self.mmio_bytes_grant(index, false)
    }

    /// [`AttachCx::mmio`], also granting a range AML reads or writes
    /// (DRIVER-MODEL.md §8.1), for a driver written to share it: one that
    /// only reads, or uses only registers the firmware's regions leave
    /// alone. [`MmioGrant::sharing`] tells which it got.
    pub fn mmio_shared<B: RegisterBlock>(
        &mut self,
        index: usize,
    ) -> Result<MmioGrant<B>, ResourceError> {
        self.mmio_grant(index, true)
    }

    /// [`AttachCx::mmio_bytes`], also granting a firmware-shared range, as
    /// [`AttachCx::mmio_shared`].
    pub fn mmio_bytes_shared(&mut self, index: usize) -> Result<MmioGrant<Bytes>, ResourceError> {
        self.mmio_bytes_grant(index, true)
    }

    fn io_grant(&mut self, index: usize, firmware_ok: bool) -> Result<IoGrant, ResourceError> {
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
        let sharing = self.mgr.claim(io_range(base, len), firmware_ok)?;
        self.ledger.issue();
        Ok(IoGrant {
            base,
            len,
            sharing,
            mgr: self.mgr,
            ledger: Arc::clone(&self.ledger),
        })
    }

    /// Claims the `index`th I/O-port resource. Ports AML also touches are
    /// refused ([`ResourceError::FirmwareShared`]).
    pub fn io_ports(&mut self, index: usize) -> Result<IoGrant, ResourceError> {
        self.io_grant(index, false)
    }

    /// [`AttachCx::io_ports`], also granting ports AML reads or writes,
    /// for a driver written to share them (a Super I/O whose
    /// environment controller the firmware's thermal methods also read).
    pub fn io_ports_shared(&mut self, index: usize) -> Result<IoGrant, ResourceError> {
        self.io_grant(index, true)
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
            pin: None,
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
            pin: None,
        })
    }

    /// The resources the bus described for the device, to hand on to a
    /// child (a transport bus passes its interrupt on).
    #[must_use]
    pub fn resources(&self) -> &[Resource] {
        self.resources
    }

    /// A DMA tag with the device's constraints and those of every bus
    /// above it ([`StartCx::set_dma_constraints`]: an ACPI device's
    /// `_CCA`, say). The broker chooses the backend (docs/IOMMU.md §5):
    /// IOMMU when the device's requester has a translated context, else
    /// direct or bounce by the device's reach.
    pub fn dma_tag(&mut self, c: DmaConstraints) -> Result<DmaTag, ResourceError> {
        self.tag_for(c, crate::iommu::Want::Translate)
    }

    /// A DMA tag that hands the device physical addresses, its stream
    /// passing through the IOMMU: for a virtio device that does not accept
    /// platform translation (`VIRTIO_F_ACCESS_PLATFORM`), which by the
    /// virtio specification uses physical addresses whatever the platform
    /// does (docs/IOMMU.md §4.4).
    pub fn dma_tag_pass_through(&mut self, c: DmaConstraints) -> Result<DmaTag, ResourceError> {
        self.tag_for(c, crate::iommu::Want::PassThrough)
    }

    fn tag_for(
        &mut self,
        c: DmaConstraints,
        want: crate::iommu::Want,
    ) -> Result<DmaTag, ResourceError> {
        if self.injected() {
            return Err(ResourceError::MapFailed);
        }
        let ctx = self
            .mgr
            .iommu_context(self.s, self.node, want)
            .map_err(ResourceError::Iommu)?;
        let c = c.intersect(&self.mgr.dma_limits(self.s, self.node));
        let p = self.mgr.platform();
        Ok(match ctx {
            Some(crate::iommu::Context::Translated(d)) => DmaTag::translated(p.dma_memory(), c, d),
            _ => DmaTag::new(p.dma_memory(), c, p.top_of_ram()),
        })
    }

    /// Gives the device's requester its context on the IOMMU unit that
    /// translates it, if any (docs/IOMMU.md §4.4): [`AttachCx::dma_tag`]
    /// does, and so must a path that makes the device write memory
    /// without a tag (MSI).
    ///
    /// # Errors
    ///
    /// [`ResourceError::Iommu`] if the unit could not make it: the
    /// device's DMA would abort.
    pub fn iommu_context(&mut self) -> Result<Option<crate::iommu::Context>, ResourceError> {
        self.mgr
            .iommu_context(self.s, self.node, crate::iommu::Want::Translate)
            .map_err(ResourceError::Iommu)
    }

    /// [`AttachCx::iommu_context`] for a device that uses physical
    /// addresses (a virtio device that does not accept platform
    /// translation, as [`AttachCx::dma_tag_pass_through`]): its requester's
    /// first request makes the stream pass through.
    ///
    /// # Errors
    ///
    /// As [`AttachCx::iommu_context`].
    pub fn iommu_context_pass_through(
        &mut self,
    ) -> Result<Option<crate::iommu::Context>, ResourceError> {
        self.mgr
            .iommu_context(self.s, self.node, crate::iommu::Want::PassThrough)
            .map_err(ResourceError::Iommu)
    }

    /// The device's `index`th mailbox channel, in firmware's order (ACPI
    /// `_DSD` `mboxes`; an SCMI agent's sending channel comes first).
    pub fn mailbox(
        &mut self,
        index: usize,
    ) -> Result<crate::mailbox::MailboxChannel, ResourceError> {
        if self.injected() {
            return Err(ResourceError::NoSuchLine);
        }
        let l = self
            .mgr
            .lines(self.s, self.node)
            .into_iter()
            .filter(|l| l.kind == LineKind::Mailbox)
            .nth(index)
            .ok_or(ResourceError::NoSuchLine)?;
        match self.mgr.provider(self.s, l.supplier, LineKind::Mailbox)? {
            Provided::Mailbox(m) => Ok(m.channel(l.id)),
            _ => Err(ResourceError::NoProvider),
        }
    }

    /// The device's line of `kind` its driver asks for by `name`: `None`
    /// asks for its unnamed line, or its only one.
    fn line(&self, kind: LineKind, name: Option<&str>) -> Result<Line, ResourceError> {
        let lines = self.mgr.lines(self.s, self.node);
        let mut mine = lines.into_iter().filter(|l| l.kind == kind);
        let found = match name {
            Some(n) => mine.find(|l| l.name.as_deref() == Some(n)),
            None => {
                let all: Vec<Line> = mine.collect();
                match all.iter().position(|l| l.name.is_none()) {
                    Some(i) => all.into_iter().nth(i),
                    None if all.len() == 1 => all.into_iter().next(),
                    None => None,
                }
            }
        };
        found.ok_or(ResourceError::NoSuchLine)
    }

    /// A handle to the clock its driver asks for by `name` (firmware's
    /// clock name; `None` for its only one), not yet enabled
    /// (DRIVER-MODEL.md §8.5). The clock's provider attached first: the
    /// line's supplier is a required edge.
    pub fn clock(&mut self, name: Option<&str>) -> Result<ClockHandle, ResourceError> {
        let l = self.line(LineKind::Clock, name)?;
        match self.mgr.provider(self.s, l.supplier, LineKind::Clock)? {
            Provided::Clocks(c) => Ok(c.handle(l.id)),
            _ => Err(ResourceError::NoProvider),
        }
    }

    /// A handle to the reset line its driver asks for by `name`, which
    /// joins the line's holders without changing it.
    pub fn reset(&mut self, name: Option<&str>) -> Result<ResetHandle, ResourceError> {
        let l = self.line(LineKind::Reset, name)?;
        match self.mgr.provider(self.s, l.supplier, LineKind::Reset)? {
            Provided::Resets(r) => Ok(r.handle(self.s, l.id)),
            _ => Err(ResourceError::NoProvider),
        }
    }

    /// A vote on the power domain its driver asks for by `name`, for
    /// [`crate::provider::PerfLevel::OFF`] until it votes.
    pub fn power_domain(&mut self, name: Option<&str>) -> Result<PowerVote, ResourceError> {
        let l = self.line(LineKind::PowerDomain, name)?;
        match self
            .mgr
            .provider(self.s, l.supplier, LineKind::PowerDomain)?
        {
            Provided::Domains(d) => Ok(d.handle(self.s, l.id)),
            _ => Err(ResourceError::NoProvider),
        }
    }

    /// The device's `index`th GPIO line whose use `pick` accepts, and its
    /// controller.
    fn gpio_line(
        &self,
        index: usize,
        pick: impl Fn(&GpioMode) -> bool,
    ) -> Result<(crate::gpio::GpioUse, Arc<GpioSource>), ResourceError> {
        let use_ = self
            .mgr
            .lines(self.s, self.node)
            .into_iter()
            .filter_map(|l| match l.detail {
                LineDetail::Gpio(g) if l.kind == LineKind::Gpio && pick(&g.mode) => {
                    Some((l.supplier, g))
                }
                _ => None,
            })
            .nth(index)
            .ok_or(ResourceError::NoSuchLine)?;
        match self.mgr.provider(self.s, use_.0, LineKind::Gpio)? {
            Provided::Gpio(g) => Ok((use_.1, g)),
            _ => Err(ResourceError::NoProvider),
        }
    }

    /// Claims the pins of the device's `index`th GPIO I/O line (ACPI
    /// `GpioIo`), in firmware's order (DRIVER-MODEL.md §9.3). The pins
    /// are configured as the driver says, not here: a `GpioIo` gives no
    /// direction beyond its restriction.
    pub fn gpio(&mut self, index: usize) -> Result<GpioPins, ResourceError> {
        if self.injected() {
            return Err(ResourceError::NoSuchLine);
        }
        let (u, src) = self.gpio_line(index, |m| matches!(m, GpioMode::Io(_)))?;
        let GpioMode::Io(r) = u.mode else {
            return Err(ResourceError::NoSuchLine);
        };
        src.claim(self.s, &u.pins, r, u.shared)
            .map_err(ResourceError::Provider)
    }

    /// The device's `index`th GPIO interrupt (ACPI `GpioInt`), in
    /// firmware's order: its pin claimed and made an input with the
    /// descriptor's bias, granted as a line in its controller's domain,
    /// as FreeBSD's `gpio_alloc_intr_resource` does. A descriptor listing
    /// several pins grants its first.
    pub fn gpio_interrupt(&mut self, index: usize) -> Result<IrqLine, ResourceError> {
        if self.injected() {
            return Err(ResourceError::NoSuchLine);
        }
        let (u, src) = self.gpio_line(index, |m| matches!(m, GpioMode::Interrupt { .. }))?;
        let GpioMode::Interrupt {
            trigger, polarity, ..
        } = u.mode
        else {
            return Err(ResourceError::NoSuchLine);
        };
        let domain = src.interrupts().ok_or(ResourceError::NoProvider)?;
        let pin = *u.pins.first().ok_or(ResourceError::NoSuchLine)?;
        let held = src
            .claim(self.s, &[pin], Restriction::Input, u.shared)
            .map_err(ResourceError::Provider)?;
        match held.configure(self.s, 0, PinMode::Input, u.pull) {
            // A bias the controller cannot set is the pin controller's,
            // which firmware has set up.
            Ok(()) | Err(crate::provider::ProviderError::Unsupported) => {}
            Err(e) => return Err(ResourceError::Provider(e)),
        }
        self.ledger.issue();
        Ok(IrqLine {
            resource: Resource::Irq {
                domain,
                hw: pin,
                trigger,
                polarity,
            },
            ledger: Arc::clone(&self.ledger),
            pin: Some(held),
        })
    }

    /// The device's `index`th I2C connection (ACPI `I2cSerialBusV2`), in
    /// firmware's order: the device at its address on its bus.
    pub fn i2c(&mut self, index: usize) -> Result<I2cDevice, ResourceError> {
        if self.injected() {
            return Err(ResourceError::NoSuchLine);
        }
        let (sup, target) = self
            .mgr
            .lines(self.s, self.node)
            .into_iter()
            .filter_map(|l| match l.detail {
                LineDetail::I2c(t) if l.kind == LineKind::I2c => Some((l.supplier, t)),
                _ => None,
            })
            .nth(index)
            .ok_or(ResourceError::NoSuchLine)?;
        match self.mgr.provider(self.s, sup, LineKind::I2c)? {
            Provided::I2c(i) => Ok(i.device(target)),
            _ => Err(ResourceError::NoProvider),
        }
    }

    /// The I2C controller this device's parent publishes: what an
    /// `iicbus` drives (FreeBSD's `device_get_parent(iicbus)`).
    pub fn parent_i2c(&self) -> Result<Arc<I2cSource>, ResourceError> {
        let parent = self
            .mgr
            .parent(self.node, self.s)
            .ok_or(ResourceError::NoProvider)?;
        match self.mgr.provider(self.s, parent, LineKind::I2c)? {
            Provided::I2c(i) => Ok(i),
            _ => Err(ResourceError::NoProvider),
        }
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
    /// A handler, its trampoline, and the GPIO pin it is on.
    Irq(Installed, Box<dyn Any + Send>, Option<GpioPins>),
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
    /// The device's unit number.
    #[must_use]
    pub fn unit(&self) -> u32 {
        self.mgr.unit(self.binding.node, self.s).unwrap_or(0)
    }

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
        // The line's grant is now held by the installed handler, and its
        // pin with it.
        let mut line = line;
        let pin = line.pin.take();
        drop(line);
        self.binding.ledger.issue();
        self.binding.sources.push(Source::Irq(installed, shim, pin));
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
    /// Makes child `consumer` depend on `supplier` (another node), for a
    /// firmware dependency that names a sibling added after it, with the
    /// edge's kind and where firmware said so. Children do not attach
    /// before their parent's `start` returns, so a required, power, or
    /// settle edge added here always holds the consumer back.
    pub fn add_dependency(
        &mut self,
        consumer: DeviceId,
        supplier: DeviceId,
        kind: devtree::graph::EdgeKind,
        origin: devtree::graph::Origin,
    ) {
        if self.children_contains(consumer) {
            self.mgr
                .add_dependency(self.s, consumer, supplier, kind, origin);
        }
    }

    /// Makes whatever node stands for the namespace object at `path`
    /// depend on child `supplier`: a dependency of a PCI function's ACPI
    /// companion (DRIVER-MODEL.md §5.2), which applies to the PCI node
    /// once the PCI bus adds it.
    pub fn add_companion_dependency(
        &mut self,
        path: String,
        supplier: DeviceId,
        kind: devtree::graph::EdgeKind,
        origin: devtree::graph::Origin,
    ) {
        if self.children_contains(supplier) {
            self.mgr
                .add_companion_dependency(self.s, path, supplier, kind, origin);
        }
    }

    /// Sets the clock, reset, and power-domain lines child `consumer`
    /// consumes, as firmware describes them; its driver asks for them by
    /// name in attach. The bus also makes each line's supplier a required
    /// dependency of the child.
    pub fn set_lines(&mut self, consumer: DeviceId, lines: Vec<Line>) {
        if self.children_contains(consumer) {
            self.mgr.set_lines(self.s, consumer, lines);
        }
    }

    /// Puts DMA limits on child `consumer` and its descendants: their
    /// tags satisfy these as well as their drivers' own. A bus says what
    /// firmware says of the path to memory (ACPI `_CCA`).
    pub fn set_dma_constraints(&mut self, consumer: DeviceId, c: DmaConstraints) {
        if self.children_contains(consumer) {
            self.mgr.set_dma(self.s, consumer, c);
        }
    }

    /// Records that child `consumer`'s DMA, and its descendants', goes
    /// through an IOMMU unit as `tr` says (an IORT root complex or named
    /// component; docs/IOMMU.md §4.4).
    pub fn set_dma_translation(&mut self, consumer: DeviceId, tr: crate::iommu::Translation) {
        if self.children_contains(consumer) {
            self.mgr.set_translation(self.s, consumer, tr);
        }
    }

    /// Records child `consumer`'s requester ID: the ID its DMA carries to
    /// the root complex, as FreeBSD's `iommu_get_requester` finds it
    /// (`%iommu` shows it).
    pub fn set_requester_id(&mut self, consumer: DeviceId, rid: u16) {
        if self.children_contains(consumer) {
            self.mgr.set_requester(self.s, consumer, rid);
        }
    }

    /// Records what to do when a child's requester first gets a
    /// translated context (the PCI bus turns relaxed ordering off,
    /// docs/IOMMU.md §4.4).
    pub fn set_requester_hook(
        &mut self,
        consumer: DeviceId,
        hook: Arc<dyn crate::iommu::RequesterHook>,
    ) {
        if self.children_contains(consumer) {
            self.mgr.set_requester_hook(self.s, consumer, hook);
        }
    }

    /// Records a range firmware reads or writes behind the drivers' backs
    /// (an AML operation region; DRIVER-MODEL.md §8.1). A child's driver
    /// is granted an overlapping range only if it asks knowingly. Called
    /// before the children that might hold it are added; the record goes
    /// when this device detaches.
    pub fn share_with_firmware(&mut self, r: Range) {
        self.mgr.share_with_firmware(self.binding.node, r);
    }

    /// Records a range the platform description declares shared between
    /// two of the children: both may hold grants overlapping in it.
    pub fn declare_shared(&mut self, r: Range) {
        self.mgr.declare_shared(self.binding.node, r);
    }

    /// Publishes this device as a clock provider: consumers whose lines
    /// name it get handles from it. It is revoked when the device
    /// detaches, after its consumers have.
    pub fn provide_clocks(&mut self, imp: Arc<dyn Clock>) {
        let src = ClockSource::new(imp);
        self.mgr
            .provide(self.s, self.binding.node, Provided::Clocks(src));
    }

    /// Publishes this device as a reset controller.
    pub fn provide_resets(&mut self, imp: Arc<dyn Reset>) {
        let src = ResetSource::new(imp);
        self.mgr
            .provide(self.s, self.binding.node, Provided::Resets(src));
    }

    /// Publishes this device as a power-domain provider.
    pub fn provide_power_domains(&mut self, imp: Arc<dyn PowerDomain>) {
        let src = DomainSource::new(imp);
        self.mgr
            .provide(self.s, self.binding.node, Provided::Domains(src));
    }

    /// Publishes this device as a GPIO controller: consumers whose GPIO
    /// lines name it claim pins from it, and their `GpioInt`s interrupt in
    /// the domain [`Gpio::interrupts`] returns.
    pub fn provide_gpio(&mut self, imp: Arc<dyn Gpio>) -> Arc<GpioSource> {
        let src = GpioSource::new(imp);
        self.mgr
            .provide(self.s, self.binding.node, Provided::Gpio(Arc::clone(&src)));
        src
    }

    /// Publishes this device as a mailbox: clients whose mailbox lines
    /// name it get channels of it.
    pub fn provide_mailbox(&mut self, imp: Arc<dyn crate::mailbox::Mailbox>) {
        let src = crate::mailbox::MailboxSource::new(imp);
        self.mgr
            .provide(self.s, self.binding.node, Provided::Mailbox(src));
    }

    /// Publishes this device as an IOMMU unit: the broker asks it for the
    /// contexts of the devices it translates (docs/IOMMU.md §4.4).
    pub fn provide_iommu(&mut self, imp: Arc<dyn crate::iommu::Iommu>) {
        let src = crate::iommu::IommuSource::new(imp);
        self.mgr
            .provide(self.s, self.binding.node, Provided::Iommu(src));
    }

    /// Publishes this device as an I2C controller.
    pub fn provide_i2c(&mut self, imp: Arc<dyn I2c>) {
        let src = I2cSource::new(imp);
        self.mgr
            .provide(self.s, self.binding.node, Provided::I2c(src));
    }

    /// Adds the `iicbus` child an I2C controller's attach adds in FreeBSD
    /// (`device_add_child(dev, "iicbus", DEVICE_UNIT_ANY)`), once the
    /// controller is published ([`StartCx::provide_i2c`]). Its bus data
    /// names the controller's ACPI path, if it has one, so the bus finds
    /// the devices firmware puts on it.
    pub fn add_iicbus(&mut self) -> DeviceId {
        let acpi = self
            .mgr
            .ident(self.binding.node, self.s)
            .and_then(|i| i.acpi_path().map(String::from));
        self.add_child(
            Ident::Child {
                class: "iicbus",
                desc: "Philips I2C bus",
            },
            Some(Arc::new(crate::i2c::IicbusOf {
                acpi,
                unit: self.unit(),
            })),
            Vec::new(),
        )
    }

    /// Takes over the node another bus (of class `from_class`) added for
    /// the namespace object at `path`, if it has not attached, as this
    /// device's child with identity `ident(old)`: FreeBSD's
    /// `acpi_iicbus` deleting `acpi0`'s child for a device on its bus
    /// and adding its own. The node keeps its bus data, resources, lines,
    /// and firmware dependencies; it gets a new id and is matched again.
    /// `None` if there is no such node, it is attached or attaching, or
    /// `ident` declines it.
    pub fn take_child(
        &mut self,
        path: &str,
        from_class: &str,
        ident: &dyn Fn(&Ident) -> Option<Ident>,
    ) -> Option<DeviceId> {
        let id = self
            .mgr
            .take_child(self.s, path, from_class, self.binding.node, ident)?;
        self.binding.sources.push(Source::Child(id));
        Some(id)
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
                Source::Irq(installed, shim, pin) => {
                    installed.remove(s);
                    drop(shim);
                    if let Some(p) = pin {
                        p.release(s);
                    }
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
