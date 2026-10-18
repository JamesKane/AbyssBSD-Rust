// SPDX-License-Identifier: BSD-2-Clause
//! The GICv3 Interrupt Translation Service: message-signalled interrupts
//! on arm64 (DRIVER-MODEL.md §8.3).
//!
//! A function's MSI-X write to the ITS's translation register carries its
//! device ID (its requester ID, mapped through IORT) and the message data,
//! an event ID. The ITS looks the pair up and raises an LPI at the
//! collection (one per CPU) the event is mapped to. LPIs are configured in
//! memory: a byte each in a table every redistributor shares, plus a
//! pending table per redistributor.
//!
//! Setup builds those tables, the ITS's device and collection tables, its
//! command queue, and a collection per CPU. Composing a message maps the
//! device on first use (MAPD, with its interrupt translation table),
//! allocates an event and an LPI, enables the LPI, and maps the event to
//! it (MAPTI). Releasing undoes that (DISCARD). Commands complete
//! synchronously; the lock serializes them.
//!
//! The tables must be coherent with the CPU's caches: if the GIC will not
//! snoop them (it drops their shareability), MSI stays off and functions
//! use INTx.
//!
//! Behavior reference: sys/arm64/arm64/gicv3_its.c (the GICv3
//! architecture specification, IHI 0069, chapter 5, for the commands).

#![allow(
    clippy::cast_possible_truncation,
    reason = "register fields are narrow, and this is a 64-bit target"
)]

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use acpi_tables::iort::Iort;
use acpi_tables::madt::MadtEntry;
use addr::PhysAddr;
use hal::CpuId;
use regs::{Bits, Field, Mmio};
use sync::SpinLock;
use vm::dma::{DmaCoherent, DmaConstraints, DmaTag};

use crate::arch::gic;

regs::register_block! {
    /// The ITS control frame (IHI 0069 §12.19).
    struct Gits: 0x1_0000 {
        CTLR @ 0x0000 => u32, Rw;
        IIDR @ 0x0004 => u32, Ro;
        TYPER @ 0x0008 => u64, Ro;
        CBASER @ 0x0080 => u64, Rw;
        CWRITER @ 0x0088 => u64, Rw;
        CREADR @ 0x0090 => u64, Ro;
        BASER @ 0x0100 => [u64; 8], Rw;
    }
}

/// The ITS registers span the control and translation frames.
const ITS_SIZE: u64 = 0x2_0000;
/// `GITS_TRANSLATER`, in the translation frame.
const TRANSLATER: u64 = 0x1_0040;

const CTLR_ENABLED: Field<u32> = Field::new(0, 1);
const CTLR_QUIESCENT: Field<u32> = Field::new(31, 1);
const TYPER_ITT_ENTRY: Field<u64> = Field::new(4, 4);
const TYPER_EVENT_BITS: Field<u64> = Field::new(8, 5);
const TYPER_DEVICE_BITS: Field<u64> = Field::new(13, 5);
const TYPER_PTA: Field<u64> = Field::new(19, 1);
const BASER_TYPE: Field<u64> = Field::new(56, 3);
const BASER_ENTRY: Field<u64> = Field::new(48, 5);
const BASER_PAGE_SIZE: Field<u64> = Field::new(8, 2);
const BASER_SHAREABILITY: Field<u64> = Field::new(10, 2);
const BASER_VALID: u64 = 1 << 63;
/// Inner write-back cacheable, for `GITS_BASER<n>` and `GITS_CBASER`.
const BASER_CACHE: u64 = 0b111 << 59;
const BASER_INNER_SHAREABLE: u64 = 0b01 << 10;
/// `GITS_BASER<n>` table types.
const TYPE_DEVICES: u64 = 1;
const TYPE_COLLECTIONS: u64 = 4;

/// LPI interrupt ID bits: LPIs 8192..16384.
const ID_BITS: u32 = 14;
/// One past the last LPI (the dispatch table's size on arm64).
pub(crate) const LPI_END: u32 = 1 << ID_BITS;
const LPIS: usize = (LPI_END - gic::FIRST_LPI) as usize;
/// Events per device (each device's interrupt translation table).
const EVENTS: u32 = 256;
/// Command queue: 4 KiB of 32-byte commands.
const QUEUE_BYTES: usize = 4096;
const COMMAND: usize = 32;
/// Polls of `GITS_CREADR` before a command is given up on.
const COMMAND_SPINS: u32 = 10_000_000;

/// A device's interrupt translation table and the events used in it.
struct Device {
    _itt: DmaCoherent<u8>,
    events: [u64; EVENTS as usize / 64],
}

struct Its {
    regs: Mmio<Gits>,
    translater: u64,
    queue: DmaCoherent<u64>,
    next: usize,
    device_limit: u32,
    events: u32,
    itt_bytes: usize,
    /// The redistributor target of each CPU's collection (`RDbase`).
    targets: Vec<u64>,
    /// The LPI configuration table.
    config: DmaCoherent<u8>,
    lpis: Vec<u64>,
    devices: BTreeMap<u32, Device>,
    /// LPI to (device, event).
    routes: BTreeMap<u32, (u32, u32)>,
    iort: Option<Iort<'static>>,
    tag: DmaTag,
    /// Pending, device, and collection tables, owned by the hardware.
    _tables: Vec<DmaCoherent<u8>>,
}

static ITS: SpinLock<Option<Its>> = SpinLock::new(None);

/// Why the ITS was not set up (printed with `Debug`).
#[derive(Debug)]
#[allow(dead_code, reason = "the fields are read through Debug")]
enum ItsError {
    NoIts,
    NoLpis,
    Map,
    NoMemory,
    Lpis(gic::LpiError),
    NotCoherent,
    Command,
}

fn bit_alloc(bits: &mut [u64], limit: usize) -> Option<usize> {
    let i = (0..limit).find(|&i| bits[i / 64] & (1 << (i % 64)) == 0)?;
    bits[i / 64] |= 1 << (i % 64);
    Some(i)
}

fn bit_free(bits: &mut [u64], i: usize) {
    if let Some(w) = bits.get_mut(i / 64) {
        *w &= !(1 << (i % 64));
    }
}

impl Its {
    /// Runs one command and waits for the ITS to have consumed it.
    fn command(&mut self, c: [u64; 4]) -> Result<(), ItsError> {
        let slots = QUEUE_BYTES / COMMAND;
        let at = self.next * 4;
        for (i, &dw) in c.iter().enumerate() {
            self.queue.write(at + i, dw);
        }
        self.next = (self.next + 1) % slots;
        let offset = (self.next * COMMAND) as u64;
        // The command in memory before the ITS is told of it.
        crate::arch::cpu::write_barrier();
        self.regs.write(Gits::CWRITER, offset);
        for _ in 0..COMMAND_SPINS {
            if self.regs.read(Gits::CREADR) & 0xf_ffe0 == offset {
                return Ok(());
            }
            core::hint::spin_loop();
        }
        Err(ItsError::Command)
    }

    fn sync(&mut self, cpu: usize) -> Result<(), ItsError> {
        let rd = self.targets.get(cpu).copied().unwrap_or(0);
        self.command([0x05, 0, rd << 16, 0])
    }

    /// The ITS device ID of `from`.
    fn device_id(&self, from: pci::PciAddress) -> Option<u32> {
        let rid = u16::from(from.bus) << 8 | u16::from(from.slot) << 3 | u16::from(from.function);
        match &self.iort {
            Some(t) => t.its_for_pci(from.domain, rid).map(|t| t.device_id),
            // Without IORT, the requester ID itself (QEMU's mapping).
            None => Some(u32::from(rid)),
        }
    }

    /// Maps device `id` (MAPD) if it is not yet.
    fn map_device(&mut self, id: u32) -> Result<(), ItsError> {
        if self.devices.contains_key(&id) {
            return Ok(());
        }
        if id >= self.device_limit {
            return Err(ItsError::NoMemory);
        }
        let itt = self
            .tag
            .coherent::<u8>(self.itt_bytes)
            .map_err(|_| ItsError::NoMemory)?;
        // Size: event ID bits minus one.
        let size = u64::from(self.events.trailing_zeros() - 1);
        let pa = itt.bus_addr() & 0x000f_ffff_ffff_ff00;
        self.command([0x08 | u64::from(id) << 32, size, (1 << 63) | pa, 0])?;
        self.devices.insert(
            id,
            Device {
                _itt: itt,
                events: [0; EVENTS as usize / 64],
            },
        );
        Ok(())
    }

    fn compose(&mut self, from: pci::PciAddress, cpu: CpuId) -> Result<(u64, u32, u32), ItsError> {
        let id = self.device_id(from).ok_or(ItsError::NoIts)?;
        let icid = cpu.index();
        if icid >= self.targets.len() {
            return Err(ItsError::NoIts);
        }
        self.map_device(id)?;
        let limit = self.events as usize;
        let dev = self.devices.get_mut(&id).ok_or(ItsError::NoIts)?;
        let event = bit_alloc(&mut dev.events, limit).ok_or(ItsError::NoMemory)?;
        let Some(n) = bit_alloc(&mut self.lpis, LPIS) else {
            bit_free(&mut dev.events, event);
            return Err(ItsError::NoMemory);
        };
        // Both fit: at most 256 events and 8192 LPIs.
        let (event, lpi) = (event as u32, gic::FIRST_LPI + n as u32);
        self.config.write(n, gic::PRIORITY | 1);
        self.command([
            0x0a | u64::from(id) << 32,
            u64::from(event) | u64::from(lpi) << 32,
            icid as u64,
            0,
        ])?;
        // The configuration change, then everything before it.
        self.command([0x0c | u64::from(id) << 32, u64::from(event), 0, 0])?;
        self.sync(icid)?;
        self.routes.insert(lpi, (id, event));
        Ok((self.translater, event, lpi))
    }

    fn release(&mut self, cpu: CpuId, lpi: u32) {
        let Some((id, event)) = self.routes.remove(&lpi) else {
            return;
        };
        let _ = self.command([0x0f | u64::from(id) << 32, u64::from(event), 0, 0]);
        let _ = self.sync(cpu.index());
        let n = (lpi - gic::FIRST_LPI) as usize;
        self.config.write(n, 0);
        bit_free(&mut self.lpis, n);
        if let Some(d) = self.devices.get_mut(&id) {
            bit_free(&mut d.events, event as usize);
        }
    }
}

/// Programs one `GITS_BASER<n>` with a table of `bytes`, trying 4 KiB
/// pages and then whatever page size the ITS insists on.
fn set_baser(
    regs: Mmio<Gits>,
    n: usize,
    tag: &DmaTag,
    bytes: usize,
) -> Result<DmaCoherent<u8>, ItsError> {
    let table = tag.coherent::<u8>(bytes).map_err(|_| ItsError::NoMemory)?;
    let pa = table.bus_addr() & 0x0000_ffff_ffff_f000;
    let base = regs.read(Gits::BASER.at(n));
    for page_size in 0..3u64 {
        let page = [0x1000, 0x4000, 0x1_0000][page_size as usize];
        let pages = (bytes.div_ceil(page) as u64).clamp(1, 256);
        let v = BASER_VALID
            | BASER_CACHE
            | (base & (0b111 << 56))
            | pa
            | BASER_INNER_SHAREABLE
            | page_size << 8
            | (pages - 1);
        regs.write(Gits::BASER.at(n), v);
        let got = regs.read(Gits::BASER.at(n));
        if got.field(BASER_PAGE_SIZE) == page_size {
            if got.field(BASER_SHAREABILITY) == 0 {
                return Err(ItsError::NotCoherent);
            }
            return Ok(table);
        }
    }
    Err(ItsError::NoMemory)
}

fn setup() -> Result<Its, ItsError> {
    let madt = crate::acpi::find(*b"APIC")
        .and_then(|t| acpi_tables::madt::parse(&t).ok())
        .ok_or(ItsError::NoIts)?;
    let entry = madt
        .entries()
        .filter_map(Result::ok)
        .find_map(|e| match e {
            MadtEntry::GicIts(i) => Some(i),
            _ => None,
        })
        .ok_or(ItsError::NoIts)?;
    let id_bits = gic::lpi_id_bits().ok_or(ItsError::NoLpis)?;
    if id_bits < ID_BITS {
        return Err(ItsError::NoLpis);
    }
    let va = crate::devices::map_reserved(PhysAddr::new(entry.physical_base), ITS_SIZE)
        .map_err(|_| ItsError::Map)?;
    // SAFETY: the control frame is mapped for good, and only this module
    // touches it.
    let regs: Mmio<Gits> = unsafe { Mmio::block(va.get() as usize) };
    // Pending tables need 64 KiB alignment; the rest is happy with it.
    let tag = DmaTag::new(
        &crate::mm::dma::KERNEL_DMA,
        DmaConstraints {
            addr_bits: 48,
            align: 0x1_0000,
            ..DmaConstraints::UNLIMITED
        },
        crate::mm::dma::top_of_ram(),
    );
    let small = DmaTag::new(
        &crate::mm::dma::KERNEL_DMA,
        DmaConstraints {
            addr_bits: 48,
            ..DmaConstraints::UNLIMITED
        },
        crate::mm::dma::top_of_ram(),
    );
    let mut tables = Vec::new();

    // The LPI configuration table (every LPI disabled), then each
    // redistributor's pending table.
    let config = small.coherent::<u8>(LPIS).map_err(|_| ItsError::NoMemory)?;
    let cpus = sched::online_cpus();
    let mut rds = Vec::new();
    for i in 0..u32::try_from(cpus).unwrap_or(0) {
        let cpu = CpuId(i);
        let (gicr, gicr_pa) = crate::intc::redistributor(cpu).ok_or(ItsError::NoLpis)?;
        let pending = tag
            .coherent::<u8>(LPI_END as usize / 8)
            .map_err(|_| ItsError::NoMemory)?;
        // SAFETY: a mapped redistributor; both tables are ours, zeroed,
        // and handed to the GIC for good.
        let snooped =
            unsafe { gic::enable_lpis(gicr, config.bus_addr(), ID_BITS, pending.bus_addr()) }
                .map_err(ItsError::Lpis)?;
        if !snooped {
            return Err(ItsError::NotCoherent);
        }
        tables.push(pending);
        rds.push((gicr, gicr_pa));
    }

    // The ITS: off while its tables are set.
    if regs.read(Gits::CTLR).has(CTLR_ENABLED) {
        regs.modify(Gits::CTLR, |c| c.with(CTLR_ENABLED, 0));
    }
    while !regs.read(Gits::CTLR).has(CTLR_QUIESCENT) {
        core::hint::spin_loop();
    }
    let typer = regs.read(Gits::TYPER);
    let itt_entry = typer.field(TYPER_ITT_ENTRY) as usize + 1;
    let event_bits = typer.field(TYPER_EVENT_BITS) as u32 + 1;
    let device_bits = typer.field(TYPER_DEVICE_BITS) as u32 + 1;
    let events = EVENTS.min(1 << event_bits.min(31));
    let mut device_limit = 1u32 << device_bits.min(31);
    for n in 0..8 {
        let v = regs.read(Gits::BASER.at(n));
        let entry = v.field(BASER_ENTRY) as usize + 1;
        let bytes = match v.field(BASER_TYPE) {
            TYPE_DEVICES => {
                // A flat table, at most 256 pages of 64 KiB.
                let most = 256 * 0x1_0000 / entry;
                device_limit = device_limit.min(u32::try_from(most).unwrap_or(u32::MAX));
                device_limit as usize * entry
            }
            TYPE_COLLECTIONS => hal::MAX_CPUS * entry,
            _ => continue,
        };
        tables.push(set_baser(regs, n, &tag, bytes)?);
    }
    let queue = small
        .coherent::<u64>(QUEUE_BYTES / 8)
        .map_err(|_| ItsError::NoMemory)?;
    regs.write(
        Gits::CBASER,
        BASER_VALID
            | BASER_CACHE
            | (queue.bus_addr() & 0x000f_ffff_ffff_f000)
            | BASER_INNER_SHAREABLE,
    );
    if regs.read(Gits::CBASER).field(BASER_SHAREABILITY) == 0 {
        return Err(ItsError::NotCoherent);
    }
    regs.write(Gits::CWRITER, 0);
    regs.modify(Gits::CTLR, |c| c.with(CTLR_ENABLED, 1));

    let pta = typer.has(TYPER_PTA);
    let targets = rds
        .iter()
        .map(|&(gicr, pa)| {
            if pta {
                pa >> 16
            } else {
                u64::from(gic::processor_number(gicr))
            }
        })
        .collect();
    let iort = crate::acpi::find(*b"IORT").and_then(|t| acpi_tables::iort::parse(&t).ok());
    let mut its = Its {
        regs,
        translater: entry.physical_base + TRANSLATER,
        queue,
        next: 0,
        device_limit,
        events,
        itt_bytes: (events as usize * itt_entry).max(256),
        targets,
        config,
        lpis: alloc::vec![0; LPIS / 64],
        devices: BTreeMap::new(),
        routes: BTreeMap::new(),
        iort,
        tag: small,
        _tables: tables,
    };
    // A collection per CPU, numbered as the CPUs are.
    for icid in 0..its.targets.len() {
        let rd = its.targets[icid];
        its.command([0x09, 0, (1 << 63) | rd << 16 | icid as u64, 0])?;
        its.sync(icid)?;
    }
    kprintln!(
        "its: GICv3 ITS at {:#x} (IIDR {:#010x}), {} LPIs, {device_bits}-bit devices, {} collection(s){}",
        entry.physical_base,
        regs.read(Gits::IIDR),
        LPIS,
        its.targets.len(),
        if its.iort.is_some() { ", IORT" } else { "" }
    );
    Ok(its)
}

/// The ITS as the PCI MSI domain's message composer.
struct ItsMsi;

impl pci::msi::MsiPlatform for ItsMsi {
    fn compose(&self, from: pci::PciAddress, cpu: CpuId) -> Option<(u64, u32, u32)> {
        let mut its = ITS.lock();
        match its.as_mut()?.compose(from, cpu) {
            Ok(m) => Some(m),
            Err(e) => {
                kprintln!("its: no message for {from:?}: {e:?}");
                None
            }
        }
    }

    fn release(&self, _: pci::PciAddress, cpu: CpuId, root: u32) {
        if let Some(its) = ITS.lock().as_mut() {
            its.release(cpu, root);
        }
    }

    fn map(&self, pa: u64, len: u64) -> Option<usize> {
        let va = crate::mm::map_device(PhysAddr::new(pa), len).ok()?;
        usize::try_from(va.get()).ok()
    }

    fn unmap(&self, va: usize, len: u64) {
        crate::mm::unmap_device(addr::VirtAddr::new(va as u64), len);
    }
}

/// Sets up the ITS and makes it the MSI composer. Call once, with every
/// CPU online (each gets a collection). Without an ITS, or with tables
/// the GIC will not snoop, functions keep using INTx.
pub(crate) fn init() {
    match setup() {
        Ok(its) => {
            *ITS.lock() = Some(its);
            static MSI: ItsMsi = ItsMsi;
            pci::msi::set_platform(&MSI);
        }
        Err(ItsError::NoIts) => {}
        Err(e) => kprintln!("its: not used: {e:?}"),
    }
}
