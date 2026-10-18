// SPDX-License-Identifier: BSD-2-Clause
//! Multiple APIC Description Table (ACPI 6.5 §5.2.12): the interrupt
//! controllers and the processors attached to them.
//!
//! Behavior reference: `sys/x86/acpica/madt.c`, `sys/arm64/arm64/gic_v3_acpi.c`,
//! `sys/arm64/acpica/acpi_machdep.c`.

use crate::raw::{Entries, Fields, LenField, Raw};
use crate::{Error, HEADER_LEN, Sdt};

const LOCAL_ADDRESS: usize = HEADER_LEN;
const FLAGS: usize = HEADER_LEN + 4;
const ENTRIES: usize = HEADER_LEN + 8;

const SHORT: Error = Error::Malformed("MADT entry too short");
const WALK: Error = Error::Malformed("MADT entry length");

/// A parsed MADT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Madt<'a> {
    bytes: &'a [u8],
}

/// Parses a MADT (signature `APIC`).
///
/// # Errors
///
/// [`Error::BadSignature`] for another table, [`Error::BadLength`] if the
/// fixed fields are missing.
pub fn parse<'a>(t: &Sdt<'a>) -> Result<Madt<'a>, Error> {
    if t.signature() != *b"APIC" {
        return Err(Error::BadSignature);
    }
    if t.bytes.len() < ENTRIES {
        return Err(Error::BadLength(*b"APIC"));
    }
    Ok(Madt { bytes: t.bytes })
}

impl<'a> Madt<'a> {
    /// The 32-bit local APIC (x86) address from the header. See
    /// [`Madt::local_apic_address`] for the effective one.
    #[must_use]
    pub fn local_interrupt_controller_address(&self) -> u32 {
        Fields::new(self.bytes, SHORT)
            .u32(LOCAL_ADDRESS)
            .unwrap_or(0)
    }

    /// The local APIC address: a Local APIC Address Override if one is
    /// present, otherwise the header field.
    #[must_use]
    pub fn local_apic_address(&self) -> u64 {
        self.entries()
            .map_while(Result::ok)
            .find_map(|e| match e {
                MadtEntry::LocalApicAddressOverride(a) => Some(a),
                _ => None,
            })
            .unwrap_or_else(|| u64::from(self.local_interrupt_controller_address()))
    }

    /// The raw multiple APIC flags.
    #[must_use]
    pub fn flags(&self) -> u32 {
        Fields::new(self.bytes, SHORT).u32(FLAGS).unwrap_or(0)
    }

    /// `PCAT_COMPAT`: the system also has dual 8259 PICs that must be masked.
    #[must_use]
    pub fn pcat_compat(&self) -> bool {
        self.flags() & 1 != 0
    }

    /// The interrupt controller structures. A malformed structure length
    /// yields one error and ends the walk; a known structure too short for
    /// its fields yields an error and the walk continues.
    #[must_use]
    pub fn entries(&self) -> MadtEntries<'a> {
        MadtEntries(Entries::new(self.bytes, ENTRIES, None, LenField::U8, WALK))
    }

    /// The processors (local APIC, local x2APIC, and GICC entries), up to
    /// the first malformed structure. Firmware may describe a processor with
    /// both an APIC and an x2APIC entry; no deduplication is done.
    pub fn cpus(&self) -> impl Iterator<Item = Cpu> + 'a {
        self.entries().map_while(Result::ok).filter_map(|e| e.cpu())
    }

    /// Processors that can run: local APIC and x2APIC entries that are
    /// enabled or online capable, and enabled GICC entries.
    #[must_use]
    pub fn enabled_cpu_count(&self) -> usize {
        self.cpus()
            .filter(|c| match c.hw_id {
                HwId::Apic(_) => c.enabled || c.online_capable,
                HwId::Mpidr(_) | HwId::GicCpuInterface(_) => c.enabled,
            })
            .count()
    }

    /// The first GIC distributor.
    #[must_use]
    pub fn gic_distributor(&self) -> Option<GicDistributor> {
        self.entries().map_while(Result::ok).find_map(|e| match e {
            MadtEntry::GicDistributor(d) => Some(d),
            _ => None,
        })
    }
}

/// Iterator returned by [`Madt::entries`].
#[derive(Clone, Debug)]
pub struct MadtEntries<'a>(Entries<'a>);

impl<'a> Iterator for MadtEntries<'a> {
    type Item = Result<MadtEntry<'a>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        Some(self.0.next()?.and_then(decode))
    }
}

/// An interrupt controller structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MadtEntry<'a> {
    /// Processor Local APIC (type 0).
    LocalApic(LocalApic),
    /// I/O APIC (type 1).
    IoApic(IoApic),
    /// Interrupt Source Override (type 2).
    InterruptSourceOverride(InterruptSourceOverride),
    /// Non-maskable Interrupt Source (type 3).
    NmiSource(NmiSource),
    /// Local APIC NMI (type 4).
    LocalApicNmi(LocalApicNmi),
    /// Local APIC Address Override (type 5): the 64-bit local APIC address.
    LocalApicAddressOverride(u64),
    /// Processor Local x2APIC (type 9).
    LocalX2Apic(LocalX2Apic),
    /// Local x2APIC NMI (type 10).
    LocalX2ApicNmi(LocalX2ApicNmi),
    /// GIC CPU Interface, GICC (type 11).
    GicCpuInterface(GicCpuInterface),
    /// GIC Distributor, GICD (type 12).
    GicDistributor(GicDistributor),
    /// GIC MSI Frame (type 13).
    GicMsiFrame(GicMsiFrame),
    /// GIC Redistributor discovery range, GICR (type 14).
    GicRedistributor(GicRedistributor),
    /// GIC Interrupt Translation Service, ITS (type 15).
    GicIts(GicIts),
    /// A structure this parser does not decode.
    Other {
        /// The structure type.
        kind: u8,
        /// The whole structure, header included.
        bytes: &'a [u8],
    },
}

impl MadtEntry<'_> {
    /// The processor this entry describes, if it is a LAPIC, x2APIC, or
    /// GICC entry.
    #[must_use]
    pub fn cpu(&self) -> Option<Cpu> {
        match *self {
            Self::LocalApic(a) => Some(Cpu {
                uid: u32::from(a.processor_uid),
                hw_id: HwId::Apic(u32::from(a.apic_id)),
                enabled: a.flags.enabled(),
                online_capable: a.flags.online_capable(),
            }),
            Self::LocalX2Apic(a) => Some(Cpu {
                uid: a.processor_uid,
                hw_id: HwId::Apic(a.x2apic_id),
                enabled: a.flags.enabled(),
                online_capable: a.flags.online_capable(),
            }),
            Self::GicCpuInterface(g) => Some(Cpu {
                uid: g.processor_uid,
                hw_id: g
                    .mpidr
                    .map_or(HwId::GicCpuInterface(g.cpu_interface_number), HwId::Mpidr),
                enabled: g.flags.enabled(),
                online_capable: g.flags.online_capable(),
            }),
            _ => None,
        }
    }
}

/// A processor, from whichever MADT entry describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cpu {
    /// The ACPI processor UID, matching the `_UID` of its processor device
    /// and the IDs in SRAT and PPTT.
    pub uid: u32,
    /// The hardware identifier used to address it.
    pub hw_id: HwId,
    /// Usable at boot.
    pub enabled: bool,
    /// Can be brought online later (only meaningful when not enabled).
    pub online_capable: bool,
}

/// How a processor is addressed by hardware.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HwId {
    /// A local APIC or x2APIC ID.
    Apic(u32),
    /// An Arm MPIDR affinity value.
    Mpidr(u64),
    /// A GIC CPU interface number, for ACPI 5.0 GICC entries that predate
    /// the MPIDR field.
    GicCpuInterface(u32),
}

/// Local APIC / x2APIC flags (ACPI 6.5 Table 5.23).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessorFlags(pub u32);

impl ProcessorFlags {
    /// The processor is usable.
    #[must_use]
    pub const fn enabled(self) -> bool {
        self.0 & 1 != 0
    }

    /// The processor can be enabled at runtime (ignored when enabled).
    #[must_use]
    pub const fn online_capable(self) -> bool {
        self.0 & 2 != 0
    }
}

/// Processor Local APIC structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalApic {
    /// ACPI processor UID.
    pub processor_uid: u8,
    /// Local APIC ID.
    pub apic_id: u8,
    /// Flags.
    pub flags: ProcessorFlags,
}

/// I/O APIC structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoApic {
    /// I/O APIC ID.
    pub id: u8,
    /// Physical address of its registers.
    pub address: u32,
    /// The first global system interrupt it handles.
    pub gsi_base: u32,
}

/// Interrupt input polarity from MPS INTI flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Polarity {
    /// Conforms to the bus's specification.
    Conforming,
    /// Active high.
    ActiveHigh,
    /// Reserved value 2.
    Reserved,
    /// Active low.
    ActiveLow,
}

/// Interrupt trigger mode from MPS INTI flags.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// Conforms to the bus's specification.
    Conforming,
    /// Edge triggered.
    Edge,
    /// Reserved value 2.
    Reserved,
    /// Level triggered.
    Level,
}

/// MPS INTI flags (ACPI 6.5 Table 5.26).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MpsIntiFlags(pub u16);

impl MpsIntiFlags {
    /// Polarity, bits 1:0.
    #[must_use]
    pub const fn polarity(self) -> Polarity {
        match self.0 & 3 {
            0 => Polarity::Conforming,
            1 => Polarity::ActiveHigh,
            2 => Polarity::Reserved,
            _ => Polarity::ActiveLow,
        }
    }

    /// Trigger mode, bits 3:2.
    #[must_use]
    pub const fn trigger(self) -> Trigger {
        match (self.0 >> 2) & 3 {
            0 => Trigger::Conforming,
            1 => Trigger::Edge,
            2 => Trigger::Reserved,
            _ => Trigger::Level,
        }
    }
}

/// Interrupt Source Override structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InterruptSourceOverride {
    /// Bus (0 for ISA).
    pub bus: u8,
    /// Bus-relative interrupt source (the ISA IRQ).
    pub source: u8,
    /// The global system interrupt it maps to.
    pub gsi: u32,
    /// Polarity and trigger mode.
    pub flags: MpsIntiFlags,
}

/// Non-maskable Interrupt Source structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NmiSource {
    /// Polarity and trigger mode.
    pub flags: MpsIntiFlags,
    /// The global system interrupt that is an NMI.
    pub gsi: u32,
}

/// Local APIC NMI structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalApicNmi {
    /// ACPI processor UID; 0xFF means all processors.
    pub processor_uid: u8,
    /// Polarity and trigger mode.
    pub flags: MpsIntiFlags,
    /// The LINT pin (0 or 1) wired to NMI.
    pub lint: u8,
}

/// Processor Local x2APIC structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalX2Apic {
    /// The x2APIC ID.
    pub x2apic_id: u32,
    /// Flags.
    pub flags: ProcessorFlags,
    /// ACPI processor UID.
    pub processor_uid: u32,
}

/// Local x2APIC NMI structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalX2ApicNmi {
    /// Polarity and trigger mode.
    pub flags: MpsIntiFlags,
    /// ACPI processor UID; `0xFFFF_FFFF` means all processors.
    pub processor_uid: u32,
    /// The LINT pin wired to NMI.
    pub lint: u8,
}

/// GICC flags (ACPI 6.5 Table 5.38).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GiccFlags(pub u32);

impl GiccFlags {
    /// The processor is usable.
    #[must_use]
    pub const fn enabled(self) -> bool {
        self.0 & 1 != 0
    }

    /// The performance interrupt is edge triggered (else level).
    #[must_use]
    pub const fn performance_interrupt_edge(self) -> bool {
        self.0 & 2 != 0
    }

    /// The VGIC maintenance interrupt is edge triggered (else level).
    #[must_use]
    pub const fn vgic_maintenance_edge(self) -> bool {
        self.0 & 4 != 0
    }

    /// The processor can be enabled at runtime (ACPI 6.5; ignored when
    /// enabled).
    #[must_use]
    pub const fn online_capable(self) -> bool {
        self.0 & 8 != 0
    }
}

/// GIC CPU Interface structure. Fields added after ACPI 5.0 are `None` when
/// the entry is too short to hold them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GicCpuInterface {
    /// GIC CPU interface number (`GICv2`).
    pub cpu_interface_number: u32,
    /// ACPI processor UID.
    pub processor_uid: u32,
    /// Flags.
    pub flags: GiccFlags,
    /// Parking protocol version (0: not supported).
    pub parking_protocol_version: u32,
    /// Performance monitoring interrupt GSIV.
    pub performance_interrupt: u32,
    /// Physical address of the parking protocol mailbox.
    pub parked_address: u64,
    /// `GICv2` CPU interface physical base (GICC).
    pub physical_base: u64,
    /// GICV base address (ACPI 5.1).
    pub gicv: Option<u64>,
    /// GICH base address (ACPI 5.1).
    pub gich: Option<u64>,
    /// VGIC maintenance interrupt GSIV (ACPI 5.1).
    pub vgic_maintenance_interrupt: Option<u32>,
    /// This processor's redistributor, when not described by GICR
    /// structures (ACPI 5.1; 0 if not used).
    pub gicr_base: Option<u64>,
    /// MPIDR affinity fields (ACPI 5.1).
    pub mpidr: Option<u64>,
    /// Processor power efficiency class (ACPI 6.0).
    pub efficiency_class: Option<u8>,
    /// Statistical Profiling Extension overflow GSIV (ACPI 6.3); `None`
    /// when absent or 0 (not supported).
    pub spe_overflow_interrupt: Option<u16>,
    /// Trace Buffer Extension interrupt GSIV (ACPI 6.5); `None` when absent
    /// or 0 (not supported).
    pub trbe_interrupt: Option<u16>,
}

/// GIC Distributor structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GicDistributor {
    /// GIC ID.
    pub id: u32,
    /// Physical base of the distributor registers.
    pub physical_base: u64,
    /// GIC version: 1–4, or 0 meaning "discover from hardware".
    pub version: u8,
}

/// GIC MSI Frame structure (`GICv2m`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GicMsiFrame {
    /// MSI frame ID.
    pub id: u32,
    /// Physical base of the frame.
    pub physical_base: u64,
    /// SPI count and base, when the flags say to use them instead of the
    /// frame's TYPER register.
    pub spis: Option<(u16, u16)>,
}

/// GIC Redistributor structure: a discovery range of redistributor frames.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GicRedistributor {
    /// Physical base of the range.
    pub base: u64,
    /// Length of the range in bytes.
    pub length: u32,
}

/// GIC ITS structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GicIts {
    /// ITS ID, matching IORT and SRAT references.
    pub id: u32,
    /// Physical base of the ITS registers.
    pub physical_base: u64,
}

fn nonzero16(v: Option<u16>) -> Option<u16> {
    v.filter(|&v| v != 0)
}

fn decode(raw: Raw<'_>) -> Result<MadtEntry<'_>, Error> {
    let f = Fields::new(raw.bytes, SHORT);
    Ok(match raw.kind {
        0 => MadtEntry::LocalApic(LocalApic {
            processor_uid: f.u8(2)?,
            apic_id: f.u8(3)?,
            flags: ProcessorFlags(f.u32(4)?),
        }),
        1 => MadtEntry::IoApic(IoApic {
            id: f.u8(2)?,
            address: f.u32(4)?,
            gsi_base: f.u32(8)?,
        }),
        2 => MadtEntry::InterruptSourceOverride(InterruptSourceOverride {
            bus: f.u8(2)?,
            source: f.u8(3)?,
            gsi: f.u32(4)?,
            flags: MpsIntiFlags(f.u16(8)?),
        }),
        3 => MadtEntry::NmiSource(NmiSource {
            flags: MpsIntiFlags(f.u16(2)?),
            gsi: f.u32(4)?,
        }),
        4 => MadtEntry::LocalApicNmi(LocalApicNmi {
            processor_uid: f.u8(2)?,
            flags: MpsIntiFlags(f.u16(3)?),
            lint: f.u8(5)?,
        }),
        5 => MadtEntry::LocalApicAddressOverride(f.u64(4)?),
        9 => MadtEntry::LocalX2Apic(LocalX2Apic {
            x2apic_id: f.u32(4)?,
            flags: ProcessorFlags(f.u32(8)?),
            processor_uid: f.u32(12)?,
        }),
        10 => MadtEntry::LocalX2ApicNmi(LocalX2ApicNmi {
            flags: MpsIntiFlags(f.u16(2)?),
            processor_uid: f.u32(4)?,
            lint: f.u8(8)?,
        }),
        11 => MadtEntry::GicCpuInterface(gicc(f)?),
        12 => MadtEntry::GicDistributor(GicDistributor {
            id: f.u32(4)?,
            physical_base: f.u64(8)?,
            version: f.u8(20)?,
        }),
        13 => {
            let use_spis = f.u32(16)? & 1 != 0;
            let spis = (f.u16(20)?, f.u16(22)?);
            MadtEntry::GicMsiFrame(GicMsiFrame {
                id: f.u32(4)?,
                physical_base: f.u64(8)?,
                spis: use_spis.then_some(spis),
            })
        }
        14 => MadtEntry::GicRedistributor(GicRedistributor {
            base: f.u64(4)?,
            length: f.u32(12)?,
        }),
        15 => MadtEntry::GicIts(GicIts {
            id: f.u32(4)?,
            physical_base: f.u64(8)?,
        }),
        kind => MadtEntry::Other {
            kind,
            bytes: raw.bytes,
        },
    })
}

/// Decodes a GICC. The ACPI 5.0 entry (40 bytes) is the minimum; later
/// fields are present only when the entry length covers them.
fn gicc(f: Fields<'_>) -> Result<GicCpuInterface, Error> {
    Ok(GicCpuInterface {
        cpu_interface_number: f.u32(4)?,
        processor_uid: f.u32(8)?,
        flags: GiccFlags(f.u32(12)?),
        parking_protocol_version: f.u32(16)?,
        performance_interrupt: f.u32(20)?,
        parked_address: f.u64(24)?,
        physical_base: f.u64(32)?,
        gicv: f.u64(40).ok(),
        gich: f.u64(48).ok(),
        vgic_maintenance_interrupt: f.u32(56).ok(),
        gicr_base: f.u64(60).ok(),
        mpidr: f.u64(68).ok(),
        efficiency_class: f.u8(76).ok(),
        spe_overflow_interrupt: nonzero16(f.u16(78).ok()),
        trbe_interrupt: nonzero16(f.u16(80).ok()),
    })
}
