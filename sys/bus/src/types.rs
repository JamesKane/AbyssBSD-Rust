// SPDX-License-Identifier: BSD-2-Clause
//! The device manager's vocabulary: identities, states, resources, and
//! the errors drivers and buses return.

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;

use devtree::ids::{
    AcpiId, AcpiIdent, AcpiKind, HidId, HidIdent, PciId, PciIdent, UsbId, UsbIdent, UsbPlace,
    VirtioId, VirtioIdent,
};
use devtree::matching::{Candidate, Matches, Rank};
use devtree::strings::PciAddress;

/// A device node, by slot and generation: a stale id never names a node
/// that reused the slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceId {
    pub(crate) index: u32,
    pub(crate) generation: u32,
}

/// A test bus child's identity (for the manager's own tests).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TestIdent {
    /// A model number drivers match on.
    pub model: u32,
}

/// A test bus match entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TestId {
    /// The model matched.
    pub model: u32,
    /// The rank given.
    pub rank: Rank,
}

impl Matches<TestIdent> for TestId {
    fn rank_for(&self, i: &TestIdent) -> Option<Rank> {
        (i.model == self.model).then_some(self.rank)
    }
}

/// What a bus knows about a child before any driver runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ident {
    /// The root of the tree.
    Root,
    /// A platform device with no bus address (`nexus0`, `apic0`).
    Platform,
    /// A child of `nexus0` that a driver claims by name (`acpi`).
    Nexus(&'static str),
    /// A PCI bus (`pci0`) behind a host or PCI-PCI bridge.
    PciBus {
        /// The segment.
        domain: u32,
        /// The bus number.
        bus: u8,
        /// Firmware describes it in ACPI (FreeBSD's `acpi_pci`).
        acpi: bool,
    },
    /// An ACPI namespace device and its path (`\\_SB_.PCI0`).
    Acpi(AcpiIdent, String),
    /// A PCI function at an address, with its ACPI path if it has a
    /// namespace object.
    Pci(PciIdent, PciAddress, Option<String>),
    /// A virtio device on a transport.
    Virtio(VirtioIdent),
    /// A USB interface, and where it is.
    Usb(UsbIdent, UsbPlace),
    /// A HID top-level collection on `hidbus`, and its device's name
    /// (its description, as FreeBSD sets it).
    Hid(HidIdent, String),
    /// A child a bus adds for a driver by name, with the description it
    /// knows for it (FreeBSD's identify routines: `ichwd` on `isa0`).
    Child {
        /// The class (the driver's name).
        class: &'static str,
        /// The description.
        desc: &'static str,
    },
    /// A controller's channel, a child with a fixed class (`ahcich` at
    /// `channel=0`), as FreeBSD adds them by name.
    Channel {
        /// The class (the driver's name).
        class: &'static str,
        /// The channel number.
        channel: u32,
    },
    /// A named child of a GPIO bus at some of its pins (`gpioc` at every
    /// pin, `gpio_aei` at its events'), claimed by class as a [`Self::Child`]
    /// is. Its location is `pins=` and the pins (FreeBSD's
    /// `gpiobus_child_location`), then, on an ACPI bus, `handle=` and the
    /// handle, `(unknown)` for none (`acpi_gpiobus_child_location`).
    Pins {
        /// The class (the driver's name).
        class: &'static str,
        /// The description.
        desc: &'static str,
        /// The pins, as [`crate::gpio::pin_list`] writes them.
        pins: String,
        /// The ACPI handle's path, if the bus is ACPI's.
        handle: Option<String>,
    },
    /// A test bus child.
    Test(TestIdent),
}

/// A driver's match table, for the bus it attaches to.
#[derive(Clone, Copy, Debug)]
pub enum Ids {
    /// Never matched; the driver is attached explicitly (adoption, root).
    None,
    /// Children claimed by name: `nexus0`'s, and channels by class.
    Nexus(&'static [&'static str]),
    /// PCI buses.
    PciBus,
    /// ACPI `_HID`/`_CID`s.
    Acpi(&'static [AcpiId]),
    /// ACPI objects of a kind that has no IDs (thermal zones).
    AcpiKind(AcpiKind),
    /// PCI vendor/device/class entries.
    Pci(&'static [PciId]),
    /// Virtio device types.
    Virtio(&'static [VirtioId]),
    /// USB devices and interfaces.
    Usb(&'static [UsbId]),
    /// HID top-level collections.
    Hid(&'static [HidId]),
    /// Test models.
    Test(&'static [TestId]),
}

impl Ids {
    /// The best rank this table gives `ident`, if any.
    #[must_use]
    pub fn rank_for(&self, ident: &Ident) -> Option<Rank> {
        fn best<I, Id: Matches<I>>(ids: &[Id], i: &I) -> Option<Rank> {
            let c = Candidate {
                name: "",
                ids,
                yields: &[],
            };
            match devtree::matching::select(core::slice::from_ref(&c), i, |_| false) {
                devtree::matching::Selection::Driver { rank, .. } => Some(rank),
                _ => None,
            }
        }
        match (self, ident) {
            (
                Self::Nexus(names),
                Ident::Nexus(n)
                | Ident::Channel { class: n, .. }
                | Ident::Child { class: n, .. }
                | Ident::Pins { class: n, .. },
            ) => names.contains(n).then_some(Rank::DEFAULT),
            (Self::PciBus, Ident::PciBus { .. }) => Some(Rank::DEFAULT),
            (Self::Acpi(t), Ident::Acpi(i, _)) => best(t, i),
            (Self::AcpiKind(k), Ident::Acpi(i, _)) => (i.kind == *k).then_some(Rank::DEFAULT),
            (Self::Pci(t), Ident::Pci(i, _, _)) => best(t, i),
            (Self::Virtio(t), Ident::Virtio(i)) => best(t, i),
            (Self::Usb(t), Ident::Usb(i, _)) => best(t, i),
            (Self::Hid(t), Ident::Hid(i, _)) => best(t, i),
            (Self::Test(t), Ident::Test(i)) => best(t, i),
            _ => None,
        }
    }
}

/// A node's lifecycle state (DRIVER-MODEL.md §4.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NodeState {
    /// Discovered; no driver (yet).
    NotPresent,
    /// A driver is chosen but suppliers are not attached.
    Waiting(WaitReason),
    /// Queued or running attach.
    Attaching,
    /// Attached.
    Attached,
    /// Detach in progress.
    Detaching(DetachReason),
    /// Attach failed.
    Failed(AttachError),
    /// Removed by its bus; awaiting deletion.
    Gone,
}

impl NodeState {
    /// FreeBSD's `device_state_t` for `devinfo`.
    #[must_use]
    pub fn freebsd(&self) -> i32 {
        use abi::bus::ds;
        match self {
            Self::NotPresent | Self::Waiting(_) | Self::Failed(_) | Self::Gone => ds::NOTPRESENT,
            Self::Attaching => ds::ATTACHING,
            Self::Attached | Self::Detaching(_) => ds::ATTACHED,
        }
    }
}

/// Why a node waits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WaitReason {
    /// These suppliers are not attached yet.
    Suppliers(Vec<DeviceId>),
    /// The driver needs something firmware did not describe.
    Need(Need),
}

/// Something a driver found it needs during attach, by name: never a
/// blind retry (DRIVER-MODEL.md §6.2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Need {
    /// The node of this firmware path (an ACPI path) must attach first.
    Firmware(String),
    /// The node with this id must attach first.
    Device(DeviceId),
}

/// Why a node is being detached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetachReason {
    /// `devctl detach`.
    Requested,
    /// `devctl detach -f`.
    Forced,
    /// Its parent or a supplier is detaching.
    Parent,
    /// The device was removed.
    Gone,
    /// System shutdown.
    Shutdown,
    /// A device reset.
    Reset,
    /// `devctl set driver`.
    DriverChange,
    /// Start failed after a successful attach.
    StartFailed,
}

/// What `stop` is told.
pub type StopReason = DetachReason;

/// Why an attach failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttachError {
    /// Not this driver's device after all: try the next-ranked driver.
    Decline,
    /// A named supplier is missing.
    Wait(Need),
    /// The hardware failed to reset or identify.
    Device(&'static str),
    /// A resource grant was refused.
    Resource(ResourceError),
    /// Out of memory.
    NoMemory,
}

impl From<ResourceError> for AttachError {
    fn from(e: ResourceError) -> Self {
        Self::Resource(e)
    }
}

/// A voluntary detach was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Busy;

/// How a driver may be entered (DRIVER-MODEL.md §6.5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Concurrency {
    /// The framework serializes thread-context entry points.
    Serialized,
    /// The driver handles concurrent entry itself.
    Concurrent,
}

/// How the framework announces an attaching device, just before its
/// `start` (FreeBSD's `device_print_child`, before `DEVICE_ATTACH`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Announce {
    /// `nvme0: <desc> on pci0`, or `usbus0 on xhci0` without a
    /// description.
    Description,
    /// `umass0 on uhub0` whatever the description: the driver prints its
    /// own, as FreeBSD's USB drivers do (`device_set_usb_desc`).
    Bare,
    /// As [`Announce::Description`] for unit 0, and silent for the rest
    /// (FreeBSD's `acpi_cpu` makes every processor but the first
    /// `device_quiet`).
    FirstUnit,
}

/// A hardware resource a bus describes for a child.
#[derive(Clone, Copy)]
pub enum Resource {
    /// A memory-mapped register range.
    Memory {
        /// Physical base.
        base: u64,
        /// Length in bytes.
        len: u64,
        /// May be mapped write-combining (a prefetchable BAR).
        prefetchable: bool,
    },
    /// An x86 I/O port range.
    IoPort {
        /// First port.
        base: u16,
        /// Number of ports.
        len: u16,
    },
    /// An interrupt input.
    Irq {
        /// The domain it belongs to.
        domain: &'static dyn irq::IrqDomain,
        /// The input number in the domain.
        hw: u32,
        /// Edge or level.
        trigger: irq::Trigger,
        /// Active high or low.
        polarity: irq::Polarity,
    },
}

impl fmt::Debug for Resource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Memory {
                base,
                len,
                prefetchable,
            } => {
                write!(
                    f,
                    "Memory({base:#x}+{len:#x}{})",
                    if *prefetchable { " pf" } else { "" }
                )
            }
            Self::IoPort { base, len } => write!(f, "IoPort({base:#x}+{len:#x})"),
            Self::Irq {
                domain,
                hw,
                trigger,
                ..
            } => {
                write!(f, "Irq({} {hw} {trigger:?})", domain.name())
            }
        }
    }
}

/// Why a resource was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceError {
    /// The node has no resource of that kind and index.
    NoSuchResource,
    /// The resource is smaller than the register block.
    TooSmall,
    /// Already granted to someone, and not declared shared.
    Claimed,
    /// AML also reads or writes the range: the driver must ask for it
    /// knowingly (`AttachCx::mmio_shared` and the others).
    FirmwareShared,
    /// A sensitive range leaf drivers may not map (interrupt controllers,
    /// IOMMUs, ECAM, consoles).
    Sensitive,
    /// The mapping could not be made.
    MapFailed,
    /// Interrupt installation failed.
    Irq(irq::IrqError),
    /// The node has no clock, reset, or power-domain line by that name, or
    /// no GPIO or I2C line at that index.
    NoSuchLine,
    /// The line's supplier has not published a provider of that kind.
    NoProvider,
    /// The provider refused.
    Provider(crate::provider::ProviderError),
}

/// What a [`Line`] reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LineKind {
    /// A clock ([`crate::provider::Clock`]).
    Clock,
    /// A reset line ([`crate::provider::Reset`]).
    Reset,
    /// A power domain ([`crate::provider::PowerDomain`]).
    PowerDomain,
    /// GPIO pins ([`crate::gpio::Gpio`]), for I/O or an interrupt.
    Gpio,
    /// A connection to an I2C bus ([`crate::i2c::I2c`]).
    I2c,
    /// A mailbox channel ([`crate::mailbox::Mailbox`]); the line's
    /// identifier is the channel number.
    Mailbox,
}

/// What a line carries beyond its supplier and identifier.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum LineDetail {
    /// Nothing: a clock, reset, or power domain.
    #[default]
    None,
    /// A GPIO descriptor's pins and use.
    Gpio(crate::gpio::GpioUse),
    /// The device's address on its bus.
    I2c(crate::i2c::I2cTarget),
}

/// A clock, reset, power-domain, GPIO, or I2C line a device consumes, as
/// its bus read it from firmware (DRIVER-MODEL.md §8.5, §9.3): its driver
/// asks for a clock, reset, or domain by name in attach, and for GPIO and
/// I2C lines by their index among the device's lines of that use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// What it reaches.
    pub kind: LineKind,
    /// The name the driver asks for; `None` for an unnamed line.
    pub name: Option<String>,
    /// The device serving it, which publishes the provider.
    pub supplier: DeviceId,
    /// The provider's identifier for it: a clock's or power domain's ID, a
    /// reset index, a GPIO descriptor's first pin, an I2C address.
    pub id: u32,
    /// The rest of a GPIO or I2C line.
    pub detail: LineDetail,
}

impl Ident {
    /// The node's ACPI namespace path, if it has one.
    #[must_use]
    pub fn acpi_path(&self) -> Option<&str> {
        match self {
            Self::Acpi(_, p) => Some(p),
            Self::Pci(_, _, p) => p.as_deref(),
            _ => None,
        }
    }

    /// FreeBSD's pnpinfo string for the node (DRIVER-MODEL.md §11.2).
    pub fn pnpinfo(&self, out: &mut String) {
        use devtree::strings;
        let _ = match self {
            Self::Root
            | Self::Platform
            | Self::Nexus(_)
            | Self::PciBus { .. }
            | Self::Channel { .. }
            | Self::Child { .. }
            | Self::Pins { .. } => Ok(()),
            Self::Acpi(i, _) => strings::acpi_pnpinfo(out, i),
            Self::Pci(i, _, _) => strings::pci_pnpinfo(out, i),
            Self::Virtio(i) => strings::virtio_pnpinfo(out, i),
            Self::Usb(i, p) => strings::usb_pnpinfo(out, i, p),
            Self::Hid(i, _) => strings::hid_pnpinfo(out, i),
            Self::Test(t) => fmt::Write::write_fmt(out, format_args!("model={}", t.model)),
        };
    }

    /// FreeBSD's location string for the node.
    pub fn location(&self, out: &mut String) {
        use devtree::strings;
        let _ = match self {
            Self::Root
            | Self::Platform
            | Self::Nexus(_)
            | Self::PciBus { .. }
            | Self::Virtio(_)
            | Self::Child { .. } => Ok(()),
            Self::Pins { pins, handle, .. } => {
                let w = fmt::Write::write_fmt(out, format_args!("pins={pins}"));
                match handle {
                    Some(h) => fmt::Write::write_fmt(out, format_args!(" handle={h}")),
                    None => w,
                }
            }
            Self::Acpi(_, path) => strings::acpi_location(out, path, None),
            Self::Pci(_, a, path) => strings::pci_location(out, a, path.as_deref(), None),
            Self::Channel { channel, .. } => {
                fmt::Write::write_fmt(out, format_args!("channel={channel}"))
            }
            Self::Usb(i, p) => strings::usb_location(out, i, p),
            Self::Hid(i, _) => strings::hid_location(out, i),
            Self::Test(_) => Ok(()),
        };
    }
}
