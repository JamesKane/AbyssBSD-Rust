// SPDX-License-Identifier: BSD-2-Clause
//! The `acpi` bus driver: `acpi0` on `nexus0`, with a child for every
//! namespace device that has a `_HID` or `_CID`, as FreeBSD's
//! `acpi_probe_child` adds them.
//!
//! - Children are added in namespace walk order, which is the order
//!   `devinfo` lists them in; attach order between suppliers and
//!   consumers comes from the dependency graph, not from FreeBSD's probe
//!   orders.
//! - A device whose `_STA` says it is absent is left out, except PCI link
//!   devices and the RTC, which FreeBSD always keeps.
//! - Children get the resources their `_CRS` consumes; ranges a bridge
//!   produces stay with the bridge's driver.
//! - What the namespace says a child depends on ([`crate::deps`]) becomes
//!   a dependency edge on the child that serves the supplier. A reference
//!   to nothing, or to a device left out, is logged.
//! - The clock and reset lines the platform description gives a child
//!   become its lines ([`bus::Line`]), which its driver asks for by name.
//! - So do the GPIO pins (`GpioIo`, `GpioInt`) and I2C connections
//!   (`I2cSerialBusV2`) its `_CRS` consumes, which its driver asks for by
//!   index. Their controllers are already its suppliers ([`crate::deps`]).
//!   A device on an I2C bus stays a child of `acpi0`: FreeBSD's
//!   `acpi_iicbus` moves it under `iicbus`, but here it is a node with
//!   the controller as a supplier (DRIVER-MODEL.md §5.3).

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use aml::Uid;
use aml::resource::Resource as AmlResource;
use bus::AddressSpace;
use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, Line, LineDetail, LineKind, Resource,
    StartCx, StopCx, StopReason,
};
use devtree::graph::{EdgeKind, Origin};
use devtree::ids::AcpiIdent;

use crate::deps::Provider;
use crate::{Ops, service};

/// What a child of `acpi0` carries for its driver (its bus data).
#[derive(Clone, Debug)]
pub struct AcpiDevice {
    /// The namespace node.
    pub node: aml::NodeId,
    /// Its path (`\_SB_.PCI0`).
    pub path: String,
    /// Every `_CRS` descriptor, including produced ranges (a host bridge's
    /// windows).
    pub crs: Vec<AmlResource>,
    /// A processor's UID, which the MADT matches: an `ACPI0007` device's
    /// `_UID`, a `Processor` object's ID.
    pub processor: Option<u32>,
}

/// A dependency on another child: the path of the device that serves the
/// supplier, the edge's kind, and where firmware said so.
pub(crate) type ChildDep = (String, EdgeKind, Origin);

/// A line a child consumes: its kind, its name, the path of the device
/// serving it, the provider's identifier, and what a GPIO or I2C line
/// adds.
pub(crate) type ChildLine = (LineKind, Option<String>, String, u32, LineDetail);

/// One device found in the namespace.
pub(crate) struct Found {
    pub(crate) device: AcpiDevice,
    pub(crate) ident: AcpiIdent,
    pub(crate) deps: Vec<ChildDep>,
    pub(crate) lines: Vec<ChildLine>,
    /// Whether its DMA snoops the CPU's caches (`_CCA`), if firmware says.
    pub(crate) coherent: Option<bool>,
}

/// IDs FreeBSD keeps even when `_STA` says absent.
fn always_present(ident: &AcpiIdent) -> bool {
    ident
        .hid
        .as_deref()
        .is_some_and(|h| h == "PNP0C0F" || h == "PNP0B00")
}

/// The identity of a device `acpi0` adds a child for, or `None` for one it
/// leaves out: no `_HID` or `_CID`, or absent by `_STA` (but for the IDs
/// FreeBSD always keeps).
pub(crate) fn enumerated(
    interp: &mut aml::Interpreter,
    host: &mut dyn aml::Host,
    node: aml::NodeId,
) -> Option<AcpiIdent> {
    if alloc::format!("{}", interp.path(node)) == "\\_TZ_" {
        return None;
    }
    let hid = interp.hid(node, host).ok().flatten();
    let cids = interp.cids(node, host).unwrap_or_default();
    if hid.is_none() && cids.is_empty() {
        return None;
    }
    let uid = interp.uid(node, host).ok().flatten().map(|u| match u {
        Uid::Integer(n) => alloc::format!("{n}"),
        Uid::String(s) => s,
    });
    let ident = AcpiIdent {
        hid,
        cids,
        uid,
        ..AcpiIdent::default()
    };
    let present = interp.sta(node, host).map_or(true, |sta| sta & 1 != 0);
    (present || always_present(&ident)).then_some(ident)
}

/// The ACPI companion of the device being attached: the namespace device
/// another bus's node stands for (a PCI function whose `_ADR` the
/// namespace names, as FreeBSD's `acpi_pci` finds its handle;
/// DRIVER-MODEL.md §5.2). Through it a PCI driver reads `_DSD` and
/// evaluates `_DSM`; the companion's dependencies already apply to the
/// node. `None` if the node has no namespace object or ACPI is not
/// running.
pub fn companion(cx: &AttachCx<'_>) -> Option<AcpiDevice> {
    let path = String::from(cx.ident().acpi_path()?);
    let parsed = aml::AmlPath::parse(&path).ok()?;
    let svc = service()?;
    svc.run(cx.sleepable(), move |interp, host| {
        let node = interp.lookup(&parsed)?;
        let crs = interp.crs(node, host).ok().flatten().unwrap_or_default();
        Some(AcpiDevice {
            node,
            path,
            crs,
            processor: None,
        })
    })
}

/// What the namespace walk found: the children, and what to log about
/// their dependencies.
pub(crate) struct Walk {
    pub(crate) found: Vec<Found>,
    /// References to objects that do not exist, and suppliers nothing will
    /// serve.
    pub(crate) notes: Vec<String>,
    /// The dependencies of present devices named only by `_ADR` (PCI
    /// functions' companions), by the consumer's path: the node another
    /// bus adds for that object takes them.
    pub(crate) companion_deps: Vec<(String, ChildDep)>,
    /// The ranges AML reads and writes, and those the platform
    /// description declares shared (DRIVER-MODEL.md §8.1).
    pub(crate) firmware_shared: Vec<bus::Range>,
    pub(crate) declared_shared: Vec<bus::Range>,
}

pub(crate) fn walk(
    interp: &mut aml::Interpreter,
    host: &mut dyn aml::Host,
    dma: &[crate::DmaController],
    platforms: &[&dyn acpi_platform::PlatformSource],
    id: &acpi_platform::FirmwareIdentity,
) -> Walk {
    let mut found = Vec::new();
    let mut companions = Vec::new();
    for node in interp.devices() {
        let path = alloc::format!("{}", interp.path(node));
        let Some(ident) = enumerated(interp, host, node) else {
            // A device only `_ADR` names stands for a node of another bus
            // (a PCI function), which takes its dependencies.
            if interp.child(node, "_ADR").is_some()
                && interp.sta(node, host).map_or(true, |sta| sta & 1 != 0)
            {
                companions.push(node);
            }
            continue;
        };
        // D0 before any driver sees it.
        let _ = crate::power::to_d0(interp, node, host);
        let crs = interp.crs(node, host).ok().flatten().unwrap_or_default();
        let coherent = cache_coherency(interp, host, node);
        // A processor device (`acpi_cpu` takes it, as a Processor object).
        let mut ident = ident;
        let processor = if ident.hid.as_deref() == Some("ACPI0007") {
            ident.kind = devtree::ids::AcpiKind::Processor;
            ident.uid.as_deref().and_then(|u| u.parse().ok())
        } else {
            None
        };
        found.push(Found {
            device: AcpiDevice {
                node,
                path,
                crs,
                processor,
            },
            ident,
            deps: Vec::new(),
            lines: Vec::new(),
            coherent,
        });
    }
    let mut notes = Vec::new();
    let mut nodes: Vec<aml::NodeId> = found.iter().map(|f| f.device.node).collect();
    nodes.extend_from_slice(&companions);
    let deps = crate::deps::build(interp, host, &nodes, dma, platforms, id);
    for p in &deps.platforms {
        notes.push(alloc::format!("acpi0: platform {p}"));
    }
    let mut companion_deps: Vec<(String, ChildDep)> = Vec::new();
    for e in &deps.edges {
        let what = e.via.describe();
        let consumer = alloc::format!("{}", interp.path(e.consumer));
        let deps = match found.iter_mut().find(|f| f.device.node == e.consumer) {
            Some(f) => Some(&mut f.deps),
            None => None,
        };
        match e.provider {
            Provider::Device(n) | Provider::Sub { owner: n, .. } => {
                let dep = (alloc::format!("{}", interp.path(n)), e.kind, e.via.origin());
                match deps {
                    Some(d) if dep.0 != consumer && !d.contains(&dep) => d.push(dep),
                    Some(_) => {}
                    // A supplier above a companion (its host bridge) is
                    // above the PCI node in the tree too.
                    None if consumer.starts_with(&dep.0) => {}
                    None => {
                        let c = (consumer, dep);
                        if !companion_deps.contains(&c) {
                            companion_deps.push(c);
                        }
                    }
                }
            }
            // acpi0 itself: the tree already orders it first.
            Provider::Bus(_) => {}
            Provider::Absent(n) => notes.push(alloc::format!(
                "acpi0: {consumer}: {what} names {}, which is not present",
                interp.path(n)
            )),
        }
    }
    for u in &deps.unresolved {
        notes.push(alloc::format!(
            "acpi0: {}: {} names {}, which does not exist",
            interp.path(u.consumer),
            u.via.describe(),
            u.name
        ));
    }
    // The lines drivers ask for by name; their suppliers are edges above.
    let p = &deps.platform;
    let clocks = p
        .clocks
        .iter()
        .map(|c| (LineKind::Clock, c.consumer, &c.name, c.provider, c.id));
    let resets = p
        .resets
        .iter()
        .map(|r| (LineKind::Reset, r.consumer, &r.name, r.controller, r.index));
    for (kind, consumer, name, supplier, id) in clocks.chain(resets) {
        if let Some(f) = found.iter_mut().find(|f| f.device.node == consumer) {
            let path = alloc::format!("{}", interp.path(supplier));
            f.lines
                .push((kind, name.clone(), path, id, LineDetail::None));
        }
    }
    // The GPIO and I2C lines each child's `_CRS` consumes, and the
    // mailbox channels its `_DSD` names.
    for f in &mut found {
        for (sup, line) in connection_lines(interp, f.device.node, &f.device.crs) {
            let path = alloc::format!("{}", interp.path(sup));
            f.lines.push((line.0, None, path, line.1, line.2));
        }
        for (sup, channel) in mailbox_lines(interp, host, f.device.node) {
            let path = alloc::format!("{}", interp.path(sup));
            f.lines
                .push((LineKind::Mailbox, None, path, channel, LineDetail::None));
        }
    }
    // Ranges two parties touch: what drivers of children overlapping
    // AML's must ask for knowingly, and how many overlaps between
    // children nothing declares (the first driver to map one wins).
    let regions = crate::ranges::firmware_regions(interp);
    let ranges: Vec<(aml::NodeId, Vec<bus::Range>)> = found
        .iter()
        .filter(|f| !crate::ranges::system_resource(&f.ident))
        .map(|f| (f.device.node, crate::ranges::consumed(&f.device.crs)))
        .collect();
    let declared_shared = crate::ranges::declared(&deps.platform.shared, &ranges);
    let (mut undeclared, mut declared) = (0usize, 0usize);
    let mut seen: Vec<(aml::NodeId, aml::NodeId)> = Vec::new();
    for o in crate::ranges::overlaps(&ranges, &regions, &declared_shared) {
        match o.with {
            crate::ranges::Party::Firmware(region) if !seen.contains(&(o.device, region)) => {
                seen.push((o.device, region));
                notes.push(alloc::format!(
                    "acpi0: {}: AML also touches its registers ({})",
                    interp.path(o.device),
                    interp.path(region)
                ));
            }
            crate::ranges::Party::Firmware(_) => {}
            crate::ranges::Party::Device(_) => undeclared += 1,
            crate::ranges::Party::Declared(_) => declared += 1,
        }
    }
    if undeclared + declared > 0 {
        notes.push(alloc::format!(
            "acpi0: {declared} declared and {undeclared} undeclared overlaps between devices' ranges"
        ));
    }
    for m in &deps.malformed {
        let at = m
            .entry
            .map(|k| alloc::format!(" entry {k}"))
            .unwrap_or_default();
        notes.push(alloc::format!(
            "acpi0: {}: {} {}{at} is malformed ({}), ignored",
            interp.path(m.carrier),
            m.source,
            m.table,
            m.why
        ));
    }
    // Processor objects and thermal zones, which have no IDs or resources
    // (FreeBSD's `acpi_probe_child` adds them with the devices; `acpi_cpu`
    // and `acpi_tz` take them).
    for node in interp.nodes() {
        let (kind, processor) = match interp.object_type(node) {
            Some(aml::ObjectType::ThermalZone) => (devtree::ids::AcpiKind::ThermalZone, None),
            Some(aml::ObjectType::Processor)
                if interp.sta(node, host).map_or(true, |sta| sta & 1 != 0) =>
            {
                let id = interp.processor_info(node).map(|p| u32::from(p.id));
                (devtree::ids::AcpiKind::Processor, id)
            }
            _ => continue,
        };
        found.push(Found {
            device: AcpiDevice {
                node,
                path: alloc::format!("{}", interp.path(node)),
                crs: Vec::new(),
                processor,
            },
            ident: AcpiIdent {
                kind,
                ..AcpiIdent::default()
            },
            deps: Vec::new(),
            lines: Vec::new(),
            coherent: None,
        });
    }
    Walk {
        found,
        notes,
        companion_deps,
        firmware_shared: regions.into_iter().map(|r| r.range).collect(),
        declared_shared,
    }
}

/// The GPIO and I2C connections a device's `_CRS` consumes, in order: each
/// one's controller, its kind, its identifier (first pin, address), and
/// its detail. One whose controller does not resolve is left out (the
/// dependency walk logs it).
fn connection_lines(
    interp: &aml::Interpreter,
    node: aml::NodeId,
    crs: &[AmlResource],
) -> Vec<(aml::NodeId, (LineKind, u32, LineDetail))> {
    use aml::resource::{GpioKind, SerialBusKind};
    use bus::gpio::{GpioMode, GpioUse, Restriction};
    let mut out = Vec::new();
    for r in crs {
        let (name, line) = match r {
            AmlResource::Gpio(g) if g.consumer => {
                let (mode, shared) = match g.kind {
                    GpioKind::Interrupt {
                        trigger: t,
                        polarity: p,
                        shared,
                        wake,
                    } => (
                        GpioMode::Interrupt {
                            trigger: trigger(t),
                            polarity: gpio_polarity(p),
                            wake,
                        },
                        shared,
                    ),
                    GpioKind::Io {
                        restriction,
                        shared,
                    } => (
                        GpioMode::Io(match restriction {
                            1 => Restriction::Input,
                            2 => Restriction::Output,
                            3 => Restriction::Preserve,
                            _ => Restriction::None,
                        }),
                        shared,
                    ),
                };
                let pull = gpio_pull(g.pin_config);
                let pins: Vec<u32> = g.pins.iter().map(|&p| u32::from(p)).collect();
                let Some(&first) = pins.first() else {
                    continue;
                };
                let detail = LineDetail::Gpio(GpioUse {
                    pins,
                    mode,
                    shared,
                    pull,
                    debounce_us: u32::from(g.debounce) * 10,
                });
                (&g.source.name, (LineKind::Gpio, first, detail))
            }
            AmlResource::SerialBus(b) if b.consumer => match b.bus {
                SerialBusKind::I2c {
                    ten_bit,
                    speed,
                    address,
                } => {
                    let target = bus::i2c::I2cTarget {
                        address,
                        ten_bit,
                        speed,
                    };
                    (
                        &b.source.name,
                        (LineKind::I2c, u32::from(address), LineDetail::I2c(target)),
                    )
                }
                _ => continue,
            },
            _ => continue,
        };
        if let Some(sup) = interp.resolve(node, name) {
            out.push((sup, line));
        }
    }
    out
}

/// Whether a device's DMA is cache-coherent (ACPI 6.5 §6.2.17 `_CCA`): its
/// own `_CCA`, or else the nearest ancestor's, since a `_CCA` covers the
/// devices below it. `None` when no `_CCA` says: such a device keeps its
/// driver's own constraints (coherent), which is what every machine
/// without `_CCA` has needed so far. FreeBSD's arm64 drivers that read it
/// (`pci_host_generic_acpi`, `ahci_generic`) take an absent one as
/// non-coherent.
pub(crate) fn cache_coherency(
    interp: &mut aml::Interpreter,
    host: &mut dyn aml::Host,
    node: aml::NodeId,
) -> Option<bool> {
    let mut at = Some(node);
    while let Some(n) = at {
        if interp.child(n, "_CCA").is_some() {
            return match interp.evaluate_child(n, "_CCA", Vec::new(), host) {
                Ok(Some(aml::Value::Integer(v))) => Some(v != 0),
                _ => None,
            };
        }
        at = interp.parent(n);
    }
    None
}

/// The mailbox channels a device's `_DSD` `mboxes` names, in order: pairs
/// of a mailbox and its channel number (the devicetree binding's
/// `#mbox-cells` = 1, which is what the Sky1's firmware gives). A pair
/// whose mailbox does not resolve is left out (the dependency walk logs
/// it).
pub(crate) fn mailbox_lines(
    interp: &mut aml::Interpreter,
    host: &mut dyn aml::Host,
    node: aml::NodeId,
) -> Vec<(aml::NodeId, u32)> {
    let Some(aml::Value::Package(items)) =
        crate::properties::property(interp, host, node, "mboxes")
    else {
        return Vec::new();
    };
    items
        .chunks(2)
        .filter_map(|pair| match pair {
            [mbox, aml::Value::Integer(ch)] => {
                Some((interp.resolve_value_name(mbox)?, u32::try_from(*ch).ok()?))
            }
            _ => None,
        })
        .collect()
}

/// A GPIO descriptor's `PinConfig` as a bias; vendor values (0x80 and up)
/// leave the pin as it is.
pub(crate) fn gpio_pull(config: u8) -> bus::gpio::Pull {
    use bus::gpio::Pull;
    match config {
        1 => Pull::Up,
        2 => Pull::Down,
        3 => Pull::None,
        _ => Pull::Default,
    }
}

pub(crate) fn gpio_polarity(p: aml::resource::Polarity) -> irq::Polarity {
    match p {
        aml::resource::Polarity::ActiveHigh => irq::Polarity::High,
        aml::resource::Polarity::ActiveLow => irq::Polarity::Low,
        aml::resource::Polarity::ActiveBoth => irq::Polarity::Both,
    }
}

pub(crate) fn trigger(t: aml::resource::Trigger) -> irq::Trigger {
    match t {
        aml::resource::Trigger::Edge => irq::Trigger::Edge,
        aml::resource::Trigger::Level => irq::Trigger::Level,
    }
}

fn polarity(p: aml::resource::Polarity) -> irq::Polarity {
    match p {
        aml::resource::Polarity::ActiveLow => irq::Polarity::Low,
        _ => irq::Polarity::High,
    }
}

/// The resources a device consumes, in `_CRS` order.
fn resources(ops: &dyn Ops, crs: &[AmlResource]) -> Vec<Resource> {
    let mut out: Vec<Resource> = crate::ranges::consumed(crs)
        .into_iter()
        .filter_map(|r| match r.space {
            AddressSpace::Memory => Some(Resource::Memory {
                base: r.base,
                len: r.len,
                prefetchable: false,
            }),
            AddressSpace::IoPort => Some(Resource::IoPort {
                base: u16::try_from(r.base).ok()?,
                len: u16::try_from(r.len).ok()?,
            }),
        })
        .collect();
    let irq_of = |gsi: u32, t: irq::Trigger, p: irq::Polarity, out: &mut Vec<Resource>| {
        if let Some((domain, hw)) = ops.gsi(gsi) {
            out.push(Resource::Irq {
                domain,
                hw,
                trigger: t,
                polarity: p,
            });
        }
    };
    for r in crs {
        match r {
            AmlResource::Irq(i) => {
                for bit in (0..16u8).filter(|b| i.mask & (1 << b) != 0) {
                    let (gsi, t, p) = ops.isa_irq(bit);
                    irq_of(gsi, t, p, &mut out);
                }
            }
            AmlResource::ExtendedIrq(e) if e.consumer && e.source.is_none() => {
                for &gsi in &e.interrupts {
                    irq_of(gsi, trigger(e.trigger), polarity(e.polarity), &mut out);
                }
            }
            _ => {}
        }
    }
    out
}

/// `acpi0`.
#[derive(Debug)]
pub struct AcpiBus {
    events: Option<crate::events::Events>,
    sci: sync::SpinLock<Option<bus::IrqLine>>,
}

fn sci_filter(d: &AcpiBus, _: usize, _: &sched::IrqCx) -> irq::Filter {
    match &d.events {
        Some(e) if e.filter() => irq::Filter::Schedule,
        _ => irq::Filter::Stray,
    }
}

fn sci_thread(d: &AcpiBus, _: usize, s: &sched::Sleepable) {
    if let Some(e) = &d.events {
        e.run(s);
    }
}

impl Driver for AcpiBus {
    const NAME: &'static str = "acpi";
    const IDS: Ids = Ids::Nexus(&["acpi"]);

    fn describe(_: &Ident) -> String {
        service()
            .map(|s| s.ops().oem_description())
            .unwrap_or_default()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let svc = service().ok_or(AttachError::Device("no ACPI namespace"))?;
        let ops = svc.ops();
        let events = ops
            .fixed_hardware()
            .map(|hw| crate::events::Events::start(ops, hw, svc, cx.sleepable()));
        let sci = events.as_ref().map(|e| cx.irq_line(e.sci())).transpose()?;
        Ok(Self {
            events,
            sci: sync::SpinLock::new(sci),
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let svc = service().ok_or(AttachError::Device("no ACPI namespace"))?;
        // Taken in its own statement: installing may sleep.
        let line = self.sci.lock().take();
        if let Some(line) = line {
            cx.interrupt(line, Some(sci_filter), Some(sci_thread), 0)?;
            if let Some(e) = &self.events {
                svc.ops().log(format_args!(
                    "acpi0: SCI on, {} GPE(s) enabled",
                    e.gpe_count()
                ));
            }
        }
        // The PM timer, as FreeBSD's acpi_timer identify adds it first.
        if let Some(bits) = svc.ops().fixed_hardware().and_then(|h| h.pm_timer_bits) {
            let desc = if bits == 32 {
                "32-bit timer at 3.579545MHz"
            } else {
                "24-bit timer at 3.579545MHz"
            };
            cx.add_child(
                Ident::Child {
                    class: "acpi_timer",
                    desc,
                },
                None,
                Vec::new(),
            );
        }
        let dma = svc.ops().dma_controllers();
        let platforms: Vec<&'static dyn acpi_platform::PlatformSource> =
            svc.ops().platforms().to_vec();
        let id = svc.ops().firmware_identity();
        let Walk {
            found,
            notes,
            companion_deps,
            firmware_shared,
            declared_shared,
        } = svc.run(cx.sleepable(), move |i, h| {
            walk(i, h, &dma, &platforms, &id)
        });
        // What the walk noticed (firmware naming what it lacks, the
        // platforms found): bring-up notes, said under `bootverbose`.
        if bus::bootverbose() {
            for note in &notes {
                svc.ops().log(format_args!("{note}"));
            }
        }
        // The shared ranges before any child that may be granted one.
        for r in firmware_shared {
            cx.share_with_firmware(r);
        }
        for r in declared_shared {
            cx.declare_shared(r);
        }
        // Every child first, in namespace order (FreeBSD's creation
        // order), then the edges between them: a dependency may name a
        // device that comes later. None attaches before this returns.
        let mut added: Vec<(String, bus::DeviceId, Vec<ChildDep>, Vec<ChildLine>)> = Vec::new();
        for f in found {
            let res = resources(svc.ops(), &f.device.crs);
            let path = f.device.path.clone();
            let id = cx.add_child(
                Ident::Acpi(f.ident, path.clone()),
                Some(Arc::new(f.device)),
                res,
            );
            if let Some(coherent) = f.coherent {
                cx.set_dma_constraints(
                    id,
                    vm::dma::DmaConstraints {
                        coherency: if coherent {
                            vm::dma::Coherency::Coherent
                        } else {
                            vm::dma::Coherency::NonCoherent
                        },
                        ..vm::dma::DmaConstraints::UNLIMITED
                    },
                );
            }
            added.push((path, id, f.deps, f.lines));
        }
        let child = |path: &str| added.iter().find(|a| a.0 == path).map(|a| a.1);
        for (_, id, deps, lines) in &added {
            for (d, kind, origin) in deps {
                if let Some(sup) = child(d) {
                    cx.add_dependency(*id, sup, *kind, *origin);
                }
            }
            let lines: Vec<Line> = lines
                .iter()
                .filter_map(|(kind, name, sup, line, detail)| {
                    Some(Line {
                        kind: *kind,
                        name: name.clone(),
                        supplier: child(sup)?,
                        id: *line,
                        detail: detail.clone(),
                    })
                })
                .collect();
            if !lines.is_empty() {
                cx.set_lines(*id, lines);
            }
        }
        // A PCI function's companion's dependencies, for the PCI bus's
        // node when it comes.
        for (consumer, (d, kind, origin)) in companion_deps {
            if let Some(sup) = child(&d) {
                cx.add_companion_dependency(consumer, sup, kind, origin);
            }
        }
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {}
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<AcpiBus>();
