// SPDX-License-Identifier: BSD-2-Clause
//! AHCI SATA controllers (`ahci`) and their channels (`ahcich`), each
//! drive published as `adaN`.
//!
//! - `ahci` resets the controller, takes its interrupt (MSI-X, MSI, or
//!   INTx), and adds an `ahcich` child per channel, disabled where the
//!   port is not implemented, as FreeBSD does. Its interrupt thread
//!   services each port that raised one.
//! - `ahcich` brings its port up (command list, received FIS area,
//!   spin-up, link), identifies the drive, and publishes it. Requests are
//!   NCQ commands when the drive supports them (up to its queue depth),
//!   else one DMA command at a time; flushes wait for the queue to drain.
//! - A port error fails what was outstanding and restarts the port.
//!
//! The port state lives in [`Port`], shared by the controller (which
//! services interrupts) and the channel (which submits), the way
//! FreeBSD's `ahci` calls into its channels.
//!
//! Behavior reference: sys/dev/ahci/ahci.c, ahci_pci.c; the AHCI 1.3.1
//! and ATA/ATAPI command set specifications for the registers.

#![no_std]
#![forbid(unsafe_code)]
#![allow(
    clippy::cast_possible_truncation,
    reason = "FIS and command header fields pack narrower values"
)]

extern crate alloc;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use bus::{
    AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, IrqLine, MmioGrant, Resource, StartCx,
    StopCx, StopReason,
};
use devtree::ids::PciId;
use irq::Filter;
use pci::BarKind;
use pci::bus::{Messages, PciChild};
use regs::Bytes;
use sched::lock::{LockClass, Mutex};
use sched::{IrqCx, Sleepable};
use storage::{Bio, BioError, BioOp, Disk, DiskInfo, DiskQueues};
use sync::SpinLock;
use vm::dma::{DmaCoherent, DmaConstraints, DmaTag, Mapped, dir};
use vm::object::IoBuf;

/// Generic host control registers.
const CAP: usize = 0x00;
const GHC: usize = 0x04;
const IS: usize = 0x08;
const PI: usize = 0x0c;
const GHC_HR: u32 = 1;
const GHC_IE: u32 = 1 << 1;
const GHC_AE: u32 = 1 << 31;
const CAP_S64A: u32 = 1 << 31;
const CAP_SNCQ: u32 = 1 << 30;
const CAP_SSS: u32 = 1 << 27;

/// Port registers, at 0x100 + 0x80 per port.
const PX_CLB: usize = 0x00;
const PX_FB: usize = 0x08;
const PX_IS: usize = 0x10;
const PX_IE: usize = 0x14;
const PX_CMD: usize = 0x18;
const PX_TFD: usize = 0x20;
const PX_SIG: usize = 0x24;
const PX_SSTS: usize = 0x28;
const PX_SCTL: usize = 0x2c;
const PX_SERR: usize = 0x30;
const PX_SACT: usize = 0x34;
const PX_CI: usize = 0x38;
const CMD_ST: u32 = 1;
const CMD_SUD: u32 = 1 << 1;
const CMD_POD: u32 = 1 << 2;
const CMD_FRE: u32 = 1 << 4;
const CMD_FR: u32 = 1 << 14;
const CMD_CR: u32 = 1 << 15;
const TFD_ERR: u32 = 1;
const TFD_DRQ: u32 = 1 << 3;
const TFD_BSY: u32 = 1 << 7;
/// Interrupts a port raises: D2H register FIS, PIO setup, set device
/// bits, descriptor processed, and the errors.
const IE_EVENTS: u32 = 0b1111 | 1 << 5;
const IS_ERRORS: u32 = 1 << 30 | 1 << 29 | 1 << 28 | 1 << 27 | 1 << 26 | 1 << 24;
/// An ATA disk's signature (ATAPI devices are not disks).
const SIG_ATA: u32 = 0x0000_0101;

/// ATA commands.
const ATA_IDENTIFY: u8 = 0xec;
const ATA_READ_DMA_EXT: u8 = 0x25;
const ATA_WRITE_DMA_EXT: u8 = 0x35;
const ATA_READ_FPDMA: u8 = 0x60;
const ATA_WRITE_FPDMA: u8 = 0x61;
const ATA_FLUSH_EXT: u8 = 0xea;

const PAGE: usize = 4096;
/// The largest transfer per request.
const MAX_TRANSFER: usize = 128 * 1024;
/// PRD entries per command: enough for the largest transfer, page by page.
const PRDS: usize = MAX_TRANSFER / PAGE + 1;
/// Port memory: the command list, the received FIS area, then a command
/// table per slot (128-byte aligned).
const LIST: usize = 0;
const FIS: usize = 0x400;
const TABLES: usize = 0x800;
const TABLE: usize = (0x80 + PRDS * 16).next_multiple_of(0x80);
const SLOTS: usize = 32;

static PORT_LOCK: LockClass = LockClass::new("ahci port", 500);

/// Known controllers, as FreeBSD's table names them (vendor, device).
const KNOWN: &[(u16, u16, &str)] = &[
    (0x8086, 0x2821, "Intel ICH8"),
    (0x8086, 0x2824, "Intel ICH8"),
    (0x8086, 0x2829, "Intel ICH8M"),
    (0x8086, 0x2922, "Intel ICH9"),
    (0x8086, 0x2923, "Intel ICH9"),
    (0x8086, 0x2924, "Intel ICH9"),
    (0x8086, 0x2925, "Intel ICH9"),
    (0x8086, 0x2927, "Intel ICH9"),
    (0x8086, 0x2929, "Intel ICH9M"),
    (0x8086, 0x294d, "Intel ICH9"),
];

/// The controller's registers.
pub struct Ctlr {
    mmio: MmioGrant<Bytes>,
    cap: u32,
}

impl core::fmt::Debug for Ctlr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Ctlr")
            .field("cap", &self.cap)
            .finish_non_exhaustive()
    }
}

enum Data {
    None,
    Read(Mapped<IoBuf, dir::FromDevice>),
    Write(Mapped<IoBuf, dir::ToDevice>),
}

struct InFlight {
    bio: Bio,
    data: Data,
}

impl InFlight {
    fn finish(self) -> Bio {
        let Self { mut bio, data } = self;
        match data {
            Data::None => {}
            Data::Read(d) => bio.buf = d.unmap(),
            Data::Write(d) => bio.buf = d.unmap(),
        }
        bio
    }
}

/// What identify says about a drive.
#[derive(Clone, Debug)]
pub struct Drive {
    model: String,
    sectors: u64,
    sector_size: u32,
    /// NCQ queue depth, if the drive and controller support NCQ.
    ncq: Option<usize>,
}

struct PortState {
    mem: DmaCoherent<u8>,
    slots: Vec<Option<InFlight>>,
    /// Slots issued to the hardware.
    issued: u32,
    drive: Option<Drive>,
}

/// One port, shared by the controller and its channel.
pub struct Port {
    ctlr: Arc<Ctlr>,
    index: usize,
    state: Mutex<PortState>,
    tag: DmaTag,
    disk: SpinLock<Option<Arc<Disk>>>,
}

impl core::fmt::Debug for Port {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Port")
            .field("index", &self.index)
            .finish_non_exhaustive()
    }
}

fn sleep_ms(s: &Sleepable, ms: u64) {
    bus::manager().sleep_ms(s, ms);
}

impl Port {
    fn reg(&self, off: usize) -> usize {
        0x100 + 0x80 * self.index + off
    }

    fn read(&self, off: usize) -> u32 {
        self.ctlr.mmio.read32(self.reg(off))
    }

    fn write(&self, off: usize, v: u32) {
        self.ctlr.mmio.write32(self.reg(off), v);
    }

    /// Polls until `(PxCMD & mask) == 0`, for at most `ms`.
    fn wait_cmd_clear(&self, s: &Sleepable, mask: u32, ms: u64) -> bool {
        for _ in 0..ms {
            if self.read(PX_CMD) & mask == 0 {
                return true;
            }
            sleep_ms(s, 1);
        }
        self.read(PX_CMD) & mask == 0
    }

    /// Stops the command engine and FIS reception.
    fn stop_engine(&self, s: &Sleepable) {
        self.write(PX_CMD, self.read(PX_CMD) & !CMD_ST);
        self.wait_cmd_clear(s, CMD_CR, 500);
    }

    fn stop_all(&self, s: &Sleepable) {
        self.stop_engine(s);
        self.write(PX_CMD, self.read(PX_CMD) & !CMD_FRE);
        self.wait_cmd_clear(s, CMD_FR, 500);
    }

    fn start_engine(&self) {
        self.write(PX_SERR, u32::MAX);
        self.write(PX_IS, u32::MAX);
        self.write(PX_CMD, self.read(PX_CMD) | CMD_ST);
    }

    /// Writes a command into `slot`: its FIS and PRDs; returns the
    /// header's first dword.
    fn build(
        st: &PortState,
        slot: usize,
        fis: &[u8; 20],
        segs: &[vm::dma::Segment],
        write: bool,
    ) -> u32 {
        let table = TABLES + slot * TABLE;
        let m = &st.mem;
        for i in 0..0x80 {
            m.write(table + i, 0);
        }
        for (i, &b) in fis.iter().enumerate() {
            m.write(table + i, b);
        }
        for (i, g) in segs.iter().enumerate() {
            let at = table + 0x80 + 16 * i;
            m.write_at::<u64>(at, g.bus);
            m.write_at::<u32>(at + 8, 0);
            m.write_at::<u32>(at + 12, (g.len as u32 - 1) & 0x3f_ffff);
        }
        let header = LIST + slot * 32;
        // CFL 5 dwords, write, PRD count.
        let dw0 = 5 | u32::from(write) << 6 | (segs.len() as u32) << 16;
        m.write_at::<u32>(header, dw0);
        m.write_at::<u32>(header + 4, 0);
        m.write_at::<u64>(header + 8, m.bus_addr() + table as u64);
        for off in (16..32).step_by(4) {
            m.write_at::<u32>(header + off, 0);
        }
        dw0
    }

    /// An H2D register FIS.
    fn fis(command: u8, lba: u64, count: u16, features: u16, device: u8) -> [u8; 20] {
        let mut f = [0u8; 20];
        f[0] = 0x27;
        f[1] = 0x80;
        f[2] = command;
        f[3] = features as u8;
        f[4] = lba as u8;
        f[5] = (lba >> 8) as u8;
        f[6] = (lba >> 16) as u8;
        f[7] = device;
        f[8] = (lba >> 24) as u8;
        f[9] = (lba >> 32) as u8;
        f[10] = (lba >> 40) as u8;
        f[11] = (features >> 8) as u8;
        f[12] = count as u8;
        f[13] = (count >> 8) as u8;
        f
    }

    /// Brings the port up and identifies its drive, if it has one.
    fn init(&self, s: &Sleepable) -> Result<Option<Drive>, AttachError> {
        self.stop_all(s);
        let (clb, fb) = {
            let st = self.state.lock(s);
            (
                st.mem.bus_addr() + LIST as u64,
                st.mem.bus_addr() + FIS as u64,
            )
        };
        self.write(PX_CLB, clb as u32);
        self.write(PX_CLB + 4, (clb >> 32) as u32);
        self.write(PX_FB, fb as u32);
        self.write(PX_FB + 4, (fb >> 32) as u32);
        self.write(PX_SERR, u32::MAX);
        self.write(PX_IS, u32::MAX);
        let mut cmd = self.read(PX_CMD) | CMD_FRE | CMD_POD;
        if self.ctlr.cap & CAP_SSS != 0 {
            cmd |= CMD_SUD;
        }
        self.write(PX_CMD, cmd);
        // A link: device present and the PHY up. Reset the link once if
        // it is not.
        let linked = |p: &Self| p.read(PX_SSTS) & 0xf == 3;
        if !linked(self) {
            self.write(PX_SCTL, (self.read(PX_SCTL) & !0xf) | 1);
            sleep_ms(s, 2);
            self.write(PX_SCTL, self.read(PX_SCTL) & !0xf);
            for _ in 0..1000 {
                if linked(self) {
                    break;
                }
                sleep_ms(s, 1);
            }
        }
        if !linked(self) {
            return Ok(None);
        }
        for _ in 0..3000 {
            if self.read(PX_TFD) & (TFD_BSY | TFD_DRQ) == 0 {
                break;
            }
            sleep_ms(s, 1);
        }
        if self.read(PX_SIG) != SIG_ATA {
            return Ok(None);
        }
        self.write(PX_SERR, u32::MAX);
        self.write(PX_IE, IE_EVENTS | IS_ERRORS);
        self.start_engine();

        // IDENTIFY DEVICE into a page of its own, polled.
        let buf = self
            .tag
            .coherent::<u8>(512)
            .map_err(|_| AttachError::NoMemory)?;
        {
            let st = self.state.lock(s);
            let seg = vm::dma::Segment {
                bus: buf.bus_addr(),
                len: 512,
            };
            Self::build(&st, 0, &Self::fis(ATA_IDENTIFY, 0, 0, 0, 0), &[seg], false);
        }
        self.write(PX_CI, 1);
        let mut done = false;
        for _ in 0..5000 {
            if self.read(PX_CI) & 1 == 0 {
                done = true;
                break;
            }
            sleep_ms(s, 1);
        }
        if !done || self.read(PX_TFD) & TFD_ERR != 0 {
            return Err(AttachError::Device("IDENTIFY DEVICE failed"));
        }
        let word = |i: usize| u16::from_le_bytes([buf.read(2 * i), buf.read(2 * i + 1)]);
        let mut model = String::new();
        for w in 27..47 {
            let v = word(w);
            for b in [(v >> 8) as u8, v as u8] {
                model.push(char::from(b));
            }
        }
        let model = model.trim().into();
        let lba48 = word(83) & (1 << 10) != 0;
        let sectors = if lba48 {
            (0..4).fold(0u64, |a, i| a | u64::from(word(100 + i)) << (16 * i))
        } else {
            u64::from(word(60)) | u64::from(word(61)) << 16
        };
        let w106 = word(106);
        let sector_size = if w106 & 0xc000 == 0x4000 && w106 & (1 << 12) != 0 {
            (u32::from(word(117)) | u32::from(word(118)) << 16) * 2
        } else {
            512
        };
        let ncq = (self.ctlr.cap & CAP_SNCQ != 0 && word(76) & (1 << 8) != 0 && lba48)
            .then(|| usize::from(word(75) & 0x1f) + 1)
            .map(|d| d.min(SLOTS).min(((self.ctlr.cap >> 8) & 0x1f) as usize + 1));
        if !lba48 || sectors == 0 || !sector_size.is_power_of_two() {
            return Err(AttachError::Device("drive without 48-bit addressing"));
        }
        Ok(Some(Drive {
            model,
            sectors,
            sector_size,
            ncq,
        }))
    }

    /// In the controller's interrupt thread: completes what finished.
    fn service(&self, s: &Sleepable) {
        let mut done = Vec::new();
        {
            let mut st = self.state.lock(s);
            let is = self.read(PX_IS);
            self.write(PX_IS, is);
            if is & IS_ERRORS != 0 && st.issued != 0 {
                // Fail everything outstanding and restart the port.
                for slot in st.slots.iter_mut() {
                    if let Some(f) = slot.take() {
                        done.push((f.finish(), Err(BioError::Io)));
                    }
                }
                st.issued = 0;
                self.stop_engine(s);
                self.start_engine();
            } else {
                let active = self.read(PX_CI) | self.read(PX_SACT);
                let finished = st.issued & !active;
                for slot in 0..SLOTS {
                    if finished & (1 << slot) != 0
                        && let Some(f) = st.slots[slot].take()
                    {
                        done.push((f.finish(), Ok(())));
                    }
                }
                st.issued &= active;
            }
        }
        for (bio, r) in done {
            bio.complete(r);
        }
        let disk = self.disk.lock().clone();
        if let Some(d) = disk {
            d.resume(s, 0);
        }
    }

    fn map(&self, bio: &mut Bio) -> Result<Data, ()> {
        match bio.op {
            BioOp::Flush => Ok(Data::None),
            BioOp::Read => self
                .tag
                .map(core::mem::take(&mut bio.buf))
                .map(Data::Read)
                .map_err(|(buf, _)| bio.buf = buf),
            BioOp::Write => self
                .tag
                .map(core::mem::take(&mut bio.buf))
                .map(Data::Write)
                .map_err(|(buf, _)| bio.buf = buf),
        }
    }
}

impl DiskQueues for Port {
    fn queues(&self) -> usize {
        1
    }

    fn submit(&self, s: &Sleepable, _: usize, bios: &mut Vec<Bio>) {
        let mut st = self.state.lock(s);
        let Some(drive) = st.drive.clone() else {
            return;
        };
        let depth = drive.ncq.unwrap_or(1);
        let shift = drive.sector_size.trailing_zeros();
        let (mut ci, mut sact) = (0u32, 0u32);
        let mut refused = Vec::new();
        for mut bio in core::mem::take(bios) {
            let busy = st.issued | ci;
            let queued = drive.ncq.is_some() && bio.op != BioOp::Flush;
            // A non-queued command waits for the queue to drain, and
            // nothing queues behind it.
            let slot = (0..depth).find(|&i| busy & (1 << i) == 0);
            let blocked = !refused.is_empty()
                || slot.is_none()
                || (!queued && busy != 0)
                || st
                    .slots
                    .iter()
                    .any(|s| matches!(s, Some(f) if f.bio.op == BioOp::Flush))
                || (drive.ncq.is_none() && busy != 0);
            let Some(slot) = slot.filter(|_| !blocked) else {
                refused.push(bio);
                continue;
            };
            let count = (bio.len() >> shift) as u16;
            let lba = bio.offset >> shift;
            let Ok(data) = self.map(&mut bio) else {
                refused.push(bio);
                continue;
            };
            let segs: &[vm::dma::Segment] = match &data {
                Data::None => &[],
                Data::Read(d) => d.segments(),
                Data::Write(d) => d.segments(),
            };
            if segs.len() > PRDS {
                InFlight { bio, data }
                    .finish()
                    .complete(Err(BioError::Invalid));
                continue;
            }
            let write = bio.op == BioOp::Write;
            let fis = match (bio.op, queued) {
                (BioOp::Flush, _) => Self::fis(ATA_FLUSH_EXT, 0, 0, 0, 0x40),
                (BioOp::Read, true) => {
                    Self::fis(ATA_READ_FPDMA, lba, (slot as u16) << 3, count, 0x40)
                }
                (BioOp::Write, true) => {
                    Self::fis(ATA_WRITE_FPDMA, lba, (slot as u16) << 3, count, 0x40)
                }
                (BioOp::Read, false) => Self::fis(ATA_READ_DMA_EXT, lba, count, 0, 0x40),
                (BioOp::Write, false) => Self::fis(ATA_WRITE_DMA_EXT, lba, count, 0, 0x40),
            };
            Self::build(&st, slot, &fis, segs, write);
            st.slots[slot] = Some(InFlight { bio, data });
            ci |= 1 << slot;
            if queued {
                sact |= 1 << slot;
            }
        }
        if ci != 0 {
            st.issued |= ci;
            if sact != 0 {
                self.write(PX_SACT, sact);
            }
            self.write(PX_CI, ci);
        }
        *bios = refused;
    }
}

/// An `ahciN` instance.
pub struct Ahci {
    ctlr: Arc<Ctlr>,
    ports: Vec<Arc<Port>>,
    channels: usize,
    implemented: u32,
    line: SpinLock<Option<IrqLine>>,
    _messages: Option<Messages>,
}

impl core::fmt::Debug for Ahci {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Ahci")
            .field("channels", &self.channels)
            .finish_non_exhaustive()
    }
}

fn ctlr_filter(d: &Ahci, _: usize, _: &IrqCx) -> Filter {
    if d.ctlr.mmio.read32(IS) == 0 {
        Filter::Stray
    } else {
        Filter::Schedule
    }
}

fn ctlr_thread(d: &Ahci, _: usize, s: &Sleepable) {
    let is = d.ctlr.mmio.read32(IS);
    for p in &d.ports {
        if is & (1 << p.index) != 0 {
            p.service(s);
        }
    }
    d.ctlr.mmio.write32(IS, is);
}

impl Driver for Ahci {
    const NAME: &'static str = "ahci";
    const IDS: Ids = Ids::Pci(&[PciId::class(0x01_06_01)]);
    const CONCURRENCY: bus::Concurrency = bus::Concurrency::Concurrent;

    fn describe(ident: &Ident) -> String {
        if let Ident::Pci(i, _, _) = ident
            && let Some((_, _, n)) = KNOWN
                .iter()
                .find(|(v, dev, _)| *v == i.vendor && *dev == i.device)
        {
            return alloc::format!("{n} AHCI SATA controller");
        }
        "AHCI SATA controller".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let child = cx.bus_data_arc::<PciChild>().ok_or(AttachError::Decline)?;
        let s = cx.sleepable();
        // ABAR is BAR 5; resources list memory BARs in order.
        let index = child
            .function
            .bars
            .iter()
            .filter(|b| b.kind != BarKind::Io)
            .position(|b| b.index == 5)
            .ok_or(AttachError::Device("no AHCI base address"))?;
        let mmio = cx.mmio_bytes(index)?;
        child.enable_decoding();
        child.set_bus_master(true);
        // AHCI mode, then a controller reset.
        mmio.write32(GHC, mmio.read32(GHC) | GHC_AE);
        mmio.write32(GHC, mmio.read32(GHC) | GHC_HR);
        for _ in 0..1000 {
            if mmio.read32(GHC) & GHC_HR == 0 {
                break;
            }
            sleep_ms(s, 1);
        }
        if mmio.read32(GHC) & GHC_HR != 0 {
            return Err(AttachError::Device("controller reset timeout"));
        }
        mmio.write32(GHC, mmio.read32(GHC) | GHC_AE);
        let cap = mmio.read32(CAP);
        let implemented = mmio.read32(PI);
        let channels = ((32 - implemented.leading_zeros()) as usize).max((cap & 0x1f) as usize + 1);
        let tag = cx.dma_tag(DmaConstraints {
            addr_bits: if cap & CAP_S64A != 0 { 64 } else { 32 },
            align: 2,
            max_segment: PAGE as u64,
            boundary: Some(PAGE as u64),
            ..DmaConstraints::UNLIMITED
        })?;
        let mem_tag = cx.dma_tag(DmaConstraints {
            addr_bits: if cap & CAP_S64A != 0 { 64 } else { 32 },
            ..DmaConstraints::UNLIMITED
        })?;
        let (line, messages) = match pci::bus::msix_lines(cx, 1) {
            Ok((mut l, m)) if !l.is_empty() => (l.remove(0), Some(m)),
            _ => match pci::bus::msi_line(cx) {
                Ok((l, m)) => (l, Some(m)),
                Err(_) => (cx.irq(0)?, None),
            },
        };
        let ctlr = Arc::new(Ctlr { mmio, cap });
        let mut ports = Vec::new();
        for index in 0..32usize {
            if implemented & (1 << index) == 0 {
                continue;
            }
            let mem = mem_tag
                .coherent::<u8>(TABLES + SLOTS * TABLE)
                .map_err(|_| AttachError::NoMemory)?;
            ports.push(Arc::new(Port {
                ctlr: Arc::clone(&ctlr),
                index,
                state: Mutex::new(
                    &PORT_LOCK,
                    PortState {
                        mem,
                        slots: (0..SLOTS).map(|_| None).collect(),
                        issued: 0,
                        drive: None,
                    },
                ),
                tag,
                disk: SpinLock::new(None),
            }));
        }
        Ok(Self {
            ctlr,
            ports,
            channels,
            implemented,
            line: SpinLock::new(Some(line)),
            _messages: messages,
        })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        // Taken in its own statement: installing may sleep.
        let line = self.line.lock().take();
        if let Some(line) = line {
            cx.interrupt(line, Some(ctlr_filter), Some(ctlr_thread), 0)?;
        }
        let m = &self.ctlr.mmio;
        m.write32(IS, u32::MAX);
        m.write32(GHC, m.read32(GHC) | GHC_IE);
        for ch in 0..self.channels {
            let ident = Ident::Channel {
                class: "ahcich",
                channel: ch as u32,
            };
            match self.ports.iter().find(|p| p.index == ch) {
                Some(p) if self.implemented & (1 << ch) != 0 => {
                    cx.add_child(ident, Some(Arc::clone(p) as _), Vec::<Resource>::new());
                }
                _ => {
                    cx.add_disabled_child(ident);
                }
            }
        }
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {
        let m = &self.ctlr.mmio;
        m.write32(GHC, m.read32(GHC) & !GHC_IE);
    }
}

/// An `ahcichN` instance.
pub struct AhciCh {
    port: Arc<Port>,
}

impl core::fmt::Debug for AhciCh {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AhciCh")
            .field("port", &self.port.index)
            .finish()
    }
}

impl Driver for AhciCh {
    const NAME: &'static str = "ahcich";
    const IDS: Ids = Ids::Nexus(&["ahcich"]);
    const CONCURRENCY: bus::Concurrency = bus::Concurrency::Concurrent;

    fn describe(_: &Ident) -> String {
        "AHCI channel".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let port = cx.bus_data_arc::<Port>().ok_or(AttachError::Decline)?;
        let drive = port.init(cx.sleepable())?;
        port.state.lock(cx.sleepable()).drive = drive;
        Ok(Self { port })
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let s = cx.sleepable();
        let Some(drive) = self.port.state.lock(s).drive.clone() else {
            return Ok(());
        };
        let info = DiskInfo {
            name: "ada",
            unit: storage::alloc_unit("ada"),
            sector_size: drive.sector_size,
            sectors: drive.sectors,
            max_transfer: MAX_TRANSFER,
            read_only: false,
        };
        bus::manager().log(format_args!(
            "ada{}: <{}> {}",
            info.unit,
            drive.model,
            match drive.ncq {
                Some(d) => alloc::format!("NCQ, depth {d}"),
                None => "no NCQ".into(),
            }
        ));
        let disk = storage::publish(s, info, Arc::clone(&self.port) as _);
        *self.port.disk.lock() = Some(disk);
        Ok(())
    }

    fn stop(&self, cx: &mut StopCx<'_>, _: StopReason) {
        let s = cx.sleepable();
        // Taken in its own statement: no spinlock held while withdrawing.
        let disk = self.port.disk.lock().take();
        if let Some(disk) = disk {
            storage::withdraw(s, &disk);
        }
        self.port.stop_all(s);
        self.port.write(PX_IE, 0);
        let held: Vec<InFlight> = {
            let mut st = self.port.state.lock(s);
            st.issued = 0;
            st.drive = None;
            st.slots.iter_mut().filter_map(Option::take).collect()
        };
        for f in held {
            f.finish().complete(Err(BioError::Gone));
        }
    }
}

/// The declarations for the build's driver table.
pub const AHCI: DriverDecl = DriverDecl::of::<Ahci>();
/// See [`AHCI`].
pub const AHCICH: DriverDecl = DriverDecl::of::<AhciCh>();
