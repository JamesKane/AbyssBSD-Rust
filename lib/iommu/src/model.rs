// SPDX-License-Identifier: BSD-2-Clause
//! A host model of an SMMUv3: the parts of Arm IHI 0070 the driver and the
//! framework use, for their host tests (docs/IOMMU.md §7).
//!
//! [`Model`] owns a [`PhysMemory`] (the stream table, CDs, queues, page
//! tables, and DMA buffers all live there) and exposes the register frame
//! through [`Model::read32`] and [`Model::write32`] (and 64-bit forms), so
//! a driver's register trait can be implemented over it. Devices act
//! through [`Model::dma_read`], [`Model::dma_write`], and
//! [`Model::translate`].
//!
//! What it does:
//!
//! - **Registers.** CR0 with an immediate CR0ACK, IRQ_CTRL/ACK, GBPA with
//!   its update handshake, GERROR/GERRORN, the stream table and queue
//!   bases, and the queues' PROD and CONS. Writes the architecture forbids
//!   (a base register while its queue or the SMMU is enabled, a read-only
//!   register) are ignored and recorded as [`Violation`]s.
//! - **Commands.** A write to CMDQ_PROD consumes the queue at once (or
//!   only on [`Model::consume`] when [`Config::auto_consume`] is off):
//!   the CFGI and TLBI commands of [`crate::cmd`], PREFETCH_CONFIG, and
//!   CMD_SYNC with MSI-write or SEV completion. A command it cannot
//!   execute stops the queue with CMDQ_CONS.ERR and GERROR.CMDQ_ERR, until
//!   GERRORN acknowledges it.
//! - **Translation.** Linear and two-level stream tables, the STE and CD
//!   decoders of [`crate::ste`] and [`crate::cd`], and a stage 1 walk of
//!   the `pt::smmu` format from the CD's TTB0 with any T0SZ in range: the
//!   access flag, permissions, and the output size are checked. GBPA
//!   governs traffic while SMMUEN is clear.
//! - **Caches.** Level-1 descriptors, STEs, and CDs are cached per
//!   StreamID until a CFGI removes them; leaf translations per ASID and
//!   intermediate table walks are cached until a TLBI removes them. So a
//!   driver that changes a structure without invalidating it sees the
//!   stale one, as hardware may. Faulting walks are not cached unless
//!   [`Config::cache_faults`] (the Arm rule; the knob lets a test show a
//!   design does not depend on it), and invalid configuration is cached
//!   unless [`Config::cache_invalid_config`] is off.
//! - **Events.** Configuration errors and faults go to the event queue at
//!   PROD, with the overflow flag when full, and raise the event
//!   interrupt when enabled ([`Model::take_irqs`]). Translation faults
//!   are recorded when the CD's R is set, and complete as read-as-zero,
//!   write-ignored when its A is clear.
//!
//! What it leaves out: stage 2, substreams, ATS, PRI, stalls, the secure
//! side, the SMMU's own MSIs, and register-level timing.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use pt::smmu::Smmu;
use pt::{Attrs, Format, Frame, Level, PageSize};

use crate::cd::{CD_WORDS, CdError, ContextDescriptor};
use crate::cmd::{
    CMD_BYTES, CMD_WORDS, CmdDecodeError, Command, RANGE_ALL, SyncCompletion, TlbiVa,
};
use crate::event::{Access, EVENT_BYTES, Event, Fault, FaultClass, Input, Record};
use crate::queue::{OVERFLOW, Queue};
use crate::regs::{
    CmdError, Gbpa, Httu, Idr0, Idr1, Idr5, Iidr, Oas, QueueBase, StLevel, StallModel, StrtabBase,
    StrtabCfg, Ttf, cr0, gerror, irq_ctrl, off,
};
use crate::ste::{Geometry, L1Desc, STE_WORDS, Ste, SteError, SteSlot};
use crate::{Asid, Iova, PAGE, PhysAddr, StreamId, lo32};

/// Physical memory as the SMMU sees it.
pub trait PhysMemory {
    /// Reads `buf.len()` bytes at `pa`; `false` if any of it is not
    /// backed (an external abort).
    fn read(&self, pa: u64, buf: &mut [u8]) -> bool;
    /// Writes `data` at `pa`; `false` if any of it is not backed.
    fn write(&mut self, pa: u64, data: &[u8]) -> bool;

    /// Reads a little-endian 64-bit word.
    fn read_u64(&self, pa: u64) -> Option<u64> {
        let mut b = [0; 8];
        self.read(pa, &mut b).then(|| u64::from_le_bytes(b))
    }

    /// Writes a little-endian 64-bit word.
    fn write_u64(&mut self, pa: u64, v: u64) -> bool {
        self.write(pa, &v.to_le_bytes())
    }

    /// Writes a little-endian 32-bit word.
    fn write_u32(&mut self, pa: u64, v: u32) -> bool {
        self.write(pa, &v.to_le_bytes())
    }
}

/// Memory for tests: 4 KiB pages that exist once backed, with a bump
/// allocator for tables and queues. It is also a [`pt::Memory`], so the
/// page tables a test builds with `pt::PageTable<Smmu>` are the ones the
/// model walks.
#[derive(Debug, Default)]
pub struct SparseMemory {
    pages: BTreeMap<u64, Box<Frame>>,
    next: u64,
}

impl SparseMemory {
    /// An empty memory whose allocations start at `alloc_base`.
    #[must_use]
    pub fn new(alloc_base: u64) -> Self {
        Self {
            pages: BTreeMap::new(),
            next: alloc_base,
        }
    }

    /// Backs `[pa, pa + len)` with zeroed pages (those already backed keep
    /// their contents).
    pub fn back(&mut self, pa: u64, len: u64) {
        let mut p = pa & !(PAGE - 1);
        while p < pa + len {
            self.pages.entry(p).or_insert_with(|| Box::new([0; 512]));
            p += PAGE;
        }
    }

    /// Removes the backing of `[pa, pa + len)`: accesses there abort.
    pub fn unback(&mut self, pa: u64, len: u64) {
        let mut p = pa & !(PAGE - 1);
        while p < pa + len {
            self.pages.remove(&p);
            p += PAGE;
        }
    }

    /// Allocates `bytes` of zeroed, backed memory aligned to `align` (a
    /// power of two, at least a page is used).
    pub fn alloc(&mut self, bytes: u64, align: u64) -> u64 {
        let align = align.max(PAGE);
        let pa = self.next.next_multiple_of(align);
        let len = bytes.max(1).next_multiple_of(PAGE);
        self.next = pa + len;
        self.back(pa, len);
        pa
    }

    /// Whether the page holding `pa` is backed.
    #[must_use]
    pub fn is_backed(&self, pa: u64) -> bool {
        self.pages.contains_key(&(pa & !(PAGE - 1)))
    }

    fn byte(&self, pa: u64) -> Option<u8> {
        let f = self.pages.get(&(pa & !(PAGE - 1)))?;
        let i = usize::try_from((pa & (PAGE - 1)) / 8).ok()?;
        Some(f[i].to_le_bytes()[usize::try_from(pa & 7).ok()?])
    }

    fn set_byte(&mut self, pa: u64, v: u8) -> bool {
        let Some(f) = self.pages.get_mut(&(pa & !(PAGE - 1))) else {
            return false;
        };
        let (Ok(i), Ok(b)) = (
            usize::try_from((pa & (PAGE - 1)) / 8),
            usize::try_from(pa & 7),
        ) else {
            return false;
        };
        let mut bytes = f[i].to_le_bytes();
        bytes[b] = v;
        f[i] = u64::from_le_bytes(bytes);
        true
    }
}

impl PhysMemory for SparseMemory {
    fn read(&self, pa: u64, buf: &mut [u8]) -> bool {
        // Check everything first: an abort reads nothing.
        let n = buf.len() as u64;
        if n == 0 {
            return true;
        }
        let Some(last) = pa.checked_add(n - 1) else {
            return false;
        };
        let mut p = pa & !(PAGE - 1);
        while p <= last {
            if !self.pages.contains_key(&p) {
                return false;
            }
            p += PAGE;
        }
        for (i, b) in buf.iter_mut().enumerate() {
            *b = self.byte(pa + i as u64).unwrap_or(0);
        }
        true
    }

    fn write(&mut self, pa: u64, data: &[u8]) -> bool {
        let n = data.len() as u64;
        if n == 0 {
            return true;
        }
        let Some(last) = pa.checked_add(n - 1) else {
            return false;
        };
        let mut p = pa & !(PAGE - 1);
        while p <= last {
            if !self.pages.contains_key(&p) {
                return false;
            }
            p += PAGE;
        }
        for (i, &b) in data.iter().enumerate() {
            self.set_byte(pa + i as u64, b);
        }
        true
    }
}

impl pt::Memory for SparseMemory {
    fn alloc_frame(&mut self) -> Option<pt::PhysAddr> {
        Some(pt::PhysAddr::new(self.alloc(PAGE, PAGE)))
    }

    fn frame(&mut self, pa: pt::PhysAddr) -> &mut Frame {
        self.pages
            .entry(pa.get() & !(PAGE - 1))
            .or_insert_with(|| Box::new([0; 512]))
    }

    fn free_frame(&mut self, pa: pt::PhysAddr) {
        if let Some(f) = self.pages.get_mut(&(pa.get() & !(PAGE - 1))) {
            f.fill(0);
        }
    }
}

/// The model's identity and behavior.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools, reason = "independent knobs")]
pub struct Config {
    /// IDR0.
    pub idr0: Idr0,
    /// IDR1.
    pub idr1: Idr1,
    /// IDR3.
    pub idr3: u32,
    /// IDR5.
    pub idr5: Idr5,
    /// IIDR.
    pub iidr: Iidr,
    /// Cache invalid STEs, level-1 descriptors, and CDs too (a stricter
    /// reading: a driver must invalidate after making one valid).
    pub cache_invalid_config: bool,
    /// Cache faulting translations (the Arm rule is not to; with this on
    /// a map that skips its TLBI after a faulting access stays faulting).
    pub cache_faults: bool,
    /// Consume commands as soon as PROD is written.
    pub auto_consume: bool,
}

impl Config {
    /// An SMMU like the Orange Pi 6 Plus's (docs/IOMMU.md §3): two-level
    /// stream tables, 32 StreamID bits, coherent, MSI and SEV, 16-bit
    /// ASIDs, 40-bit output, no stage 2, ATS, or PRI.
    #[must_use]
    pub fn board() -> Self {
        Self {
            idr0: Idr0 {
                st_level: StLevel::TwoLevel,
                term_model: true,
                stall_model: StallModel::Terminate,
                ttendian: 2,
                sev: true,
                msi: true,
                asid16: true,
                httu: Httu::None,
                cohacc: true,
                ttf: Ttf::Aarch64,
                s1p: true,
                ..Idr0::decode(0)
            },
            idr1: Idr1 {
                cmdqs: 19,
                eventqs: 19,
                sidsize: 32,
                ..Idr1::decode(0)
            },
            idr3: 0,
            idr5: Idr5 {
                stall_max: 0,
                vax: 0,
                gran64k: true,
                gran16k: false,
                gran4k: true,
                oas: Oas::Bits40,
            },
            iidr: Iidr {
                product_id: crate::regs::PRODUCT_MMU_600,
                variant: 1,
                revision: 0,
                implementer: crate::regs::IMPLEMENTER_ARM,
            },
            cache_invalid_config: true,
            cache_faults: false,
            auto_consume: true,
        }
    }

    /// An SMMU like QEMU's `virt,iommu=smmuv3`: 16 StreamID bits (a
    /// linear table fits), 44-bit output, no MSI or SEV, not coherent.
    #[must_use]
    pub fn qemu() -> Self {
        let b = Self::board();
        Self {
            idr0: Idr0 {
                sev: false,
                msi: false,
                cohacc: false,
                ..b.idr0
            },
            idr1: Idr1 {
                cmdqs: 8,
                eventqs: 8,
                sidsize: 16,
                ..b.idr1
            },
            idr5: Idr5 {
                oas: Oas::Bits44,
                ..b.idr5
            },
            iidr: Iidr::decode(0),
            ..b
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::board()
    }
}

/// Something the driver did that the architecture does not allow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Violation {
    /// A base or PROD/CONS register written while its queue or the SMMU
    /// was enabled (the write is ignored).
    WhileEnabled(u32),
    /// A read-only register written.
    ReadOnly(u32),
    /// A queue larger than IDR1 allows (it is clamped).
    QueueTooLarge(u32),
    /// A stream table format the SMMU lacks or a reserved one.
    BadStrtabCfg,
    /// A command the model does not implement (reported as illegal).
    UnsupportedCommand(u8),
    /// A GBPA write without UPDATE (ignored).
    GbpaWithoutUpdate,
}

/// Counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Commands executed.
    pub commands: u64,
    /// CMD_SYNCs completed.
    pub syncs: u64,
    /// SEVs sent.
    pub sevs: u64,
    /// Sync MSI writes.
    pub msi_writes: u64,
    /// Events produced.
    pub events: u64,
    /// Events lost: queue disabled or full.
    pub events_lost: u64,
    /// Translations from the TLB.
    pub tlb_hits: u64,
    /// Table walks.
    pub walks: u64,
}

/// Interrupts raised since the last [`Model::take_irqs`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Irqs {
    /// Event queue interrupts.
    pub event: u32,
    /// Global error interrupts.
    pub gerror: u32,
}

/// Why a transaction did not reach memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Termination {
    /// SMMUEN clear and GBPA.ABORT set.
    GlobalAbort,
    /// The STE's configuration is abort.
    SteAbort,
    /// A configuration error or a fault (recorded as an event if the
    /// configuration says so).
    Fault(Record),
    /// The memory behind the translated address is not there.
    ExternalAbort(u64),
}

/// How a transaction was handled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Translation {
    /// Untranslated: SMMUEN clear with GBPA bypass, or a bypass STE.
    Bypass(u64),
    /// Translated through a leaf.
    Mapped {
        /// The output address.
        pa: u64,
        /// The leaf's size.
        size: PageSize,
        /// The leaf's attributes.
        attrs: Attrs,
    },
    /// Faulted under a CD with A clear: reads return zero, writes vanish.
    RazWi,
}

/// What a StreamID's configuration resolves to.
#[derive(Clone, Copy, Debug)]
enum StreamCfg {
    Abort,
    Bypass,
    Stage1(ContextDescriptor),
}

const GLOBAL: u32 = 1 << 16;

#[derive(Clone, Copy, Debug)]
struct TlbEntry {
    /// The leaf descriptor; 0 for a cached fault.
    desc: u64,
    pa: u64,
    size: PageSize,
}

/// A walk's leaf.
#[derive(Clone, Copy, Debug)]
struct Leaf {
    desc: u64,
    pa: u64,
    size: PageSize,
}

/// The SMMU model.
#[derive(Debug)]
pub struct Model<M> {
    mem: M,
    cfg: Config,
    regs: BTreeMap<u32, u32>,
    l1_cache: BTreeMap<u32, u64>,
    ste_cache: BTreeMap<u32, Result<Ste, SteError>>,
    cd_cache: BTreeMap<u32, Result<ContextDescriptor, CdError>>,
    /// (ASID or [`GLOBAL`], block-aligned address).
    tlb: BTreeMap<(u32, u64), TlbEntry>,
    /// (ASID, table level as `pt` counts, address >> the span above):
    /// the table's address.
    walk_cache: BTreeMap<(u16, u8, u64), u64>,
    stats: Stats,
    irqs: Irqs,
    violations: Vec<Violation>,
    executed: Vec<Command>,
}

const fn level(n: u8) -> Level {
    match n {
        1 => PageSize::Size4K.level(),
        2 => PageSize::Size2M.level(),
        3 => PageSize::Size1G.level(),
        _ => Level::ROOT,
    }
}

const fn span_shift(pt_level: u8) -> u32 {
    12 + 9 * (pt_level as u32 - 1)
}

impl<M: PhysMemory> Model<M> {
    /// A model in reset state: disabled, GBPA aborting (as firmware may
    /// leave it; tests set GBPA to model other firmware), everything else
    /// zero.
    pub fn new(cfg: Config, mem: M) -> Self {
        let mut regs = BTreeMap::new();
        regs.insert(off::GBPA, Gbpa::ABORT);
        Self {
            mem,
            cfg,
            regs,
            l1_cache: BTreeMap::new(),
            ste_cache: BTreeMap::new(),
            cd_cache: BTreeMap::new(),
            tlb: BTreeMap::new(),
            walk_cache: BTreeMap::new(),
            stats: Stats::default(),
            irqs: Irqs::default(),
            violations: Vec::new(),
            executed: Vec::new(),
        }
    }

    /// The memory.
    pub const fn mem(&self) -> &M {
        &self.mem
    }

    /// The memory, for the driver's writes to it.
    pub const fn mem_mut(&mut self) -> &mut M {
        &mut self.mem
    }

    /// The configuration.
    pub const fn config(&self) -> &Config {
        &self.cfg
    }

    /// Counters.
    pub const fn stats(&self) -> &Stats {
        &self.stats
    }

    /// The architecture violations seen.
    pub fn violations(&self) -> &[Violation] {
        &self.violations
    }

    /// The commands executed, in order.
    pub fn executed(&self) -> &[Command] {
        &self.executed
    }

    /// Forgets the executed-command log.
    pub fn clear_executed(&mut self) {
        self.executed.clear();
    }

    /// Interrupts raised since the last call.
    pub fn take_irqs(&mut self) -> Irqs {
        core::mem::take(&mut self.irqs)
    }

    /// Cached leaf translations.
    pub fn tlb_len(&self) -> usize {
        self.tlb.len()
    }

    /// Cached STEs.
    pub fn ste_cache_len(&self) -> usize {
        self.ste_cache.len()
    }

    fn reg(&self, o: u32) -> u32 {
        self.regs.get(&o).copied().unwrap_or(0)
    }

    fn reg64(&self, o: u32) -> u64 {
        u64::from(self.reg(o)) | u64::from(self.reg(o + 4)) << 32
    }

    fn cr0(&self) -> u32 {
        self.reg(off::CR0)
    }

    fn enabled(&self, bit: u32) -> bool {
        self.cr0() & bit != 0
    }

    /// Reads a 32-bit register.
    pub fn read32(&self, o: u32) -> u32 {
        match o {
            off::IDR0 => self.cfg.idr0.encode(),
            off::IDR1 => self.cfg.idr1.encode(),
            off::IDR3 => self.cfg.idr3,
            off::IDR5 => self.cfg.idr5.encode(),
            off::IIDR => self.cfg.iidr.encode(),
            off::CR0ACK => self.reg(off::CR0),
            off::IRQ_CTRLACK => self.reg(off::IRQ_CTRL),
            _ => self.reg(o),
        }
    }

    /// Reads a 64-bit register, or two adjacent 32-bit ones (FreeBSD reads
    /// EVENTQ_PROD and CONS together).
    pub fn read64(&self, o: u32) -> u64 {
        u64::from(self.read32(o)) | u64::from(self.read32(o + 4)) << 32
    }

    /// Writes a 64-bit register (low half first).
    pub fn write64(&mut self, o: u32, v: u64) {
        self.write32(o, lo32(v));
        self.write32(o + 4, lo32(v >> 32));
    }

    fn violation(&mut self, v: Violation) {
        self.violations.push(v);
    }

    /// Writes a 32-bit register.
    pub fn write32(&mut self, o: u32, v: u32) {
        let guarded = |o: u32| match o {
            off::STRTAB_BASE | 0x84 | off::STRTAB_BASE_CFG => Some(cr0::SMMUEN),
            off::CMDQ_BASE | 0x94 | off::CMDQ_CONS => Some(cr0::CMDQEN),
            off::EVENTQ_BASE | 0xa4 | off::EVENTQ_PROD => Some(cr0::EVENTQEN),
            _ => None,
        };
        if let Some(bit) = guarded(o)
            && self.enabled(bit)
        {
            self.violation(Violation::WhileEnabled(o));
            return;
        }
        match o {
            off::IDR0..=off::AIDR | off::CR0ACK | off::IRQ_CTRLACK | off::GERROR | off::STATUSR => {
                self.violation(Violation::ReadOnly(o));
            }
            off::CR0 => {
                let was = self.cr0();
                self.regs.insert(o, v);
                if v & cr0::SMMUEN != 0 && was & cr0::SMMUEN == 0 {
                    self.check_strtab();
                }
                if self.cfg.auto_consume {
                    self.consume();
                }
            }
            off::GBPA => {
                if v & Gbpa::UPDATE == 0 {
                    self.violation(Violation::GbpaWithoutUpdate);
                } else {
                    self.regs.insert(o, v & !Gbpa::UPDATE);
                }
            }
            off::GERRORN => {
                self.regs.insert(o, v);
                if self.cfg.auto_consume {
                    self.consume();
                }
            }
            off::CMDQ_PROD => {
                self.regs.insert(o, v & 0x000f_ffff);
                if self.cfg.auto_consume {
                    self.consume();
                }
            }
            _ => {
                self.regs.insert(o, v);
            }
        }
    }

    fn check_strtab(&mut self) {
        match StrtabCfg::decode(self.reg(off::STRTAB_BASE_CFG)) {
            StrtabCfg::Reserved(_) => self.violation(Violation::BadStrtabCfg),
            StrtabCfg::TwoLevel { .. } if self.cfg.idr0.st_level != StLevel::TwoLevel => {
                self.violation(Violation::BadStrtabCfg);
            }
            _ => {}
        }
    }

    /// A queue's geometry from its base register, clamped to IDR1.
    fn queue(&mut self, base_off: u32, max: u8) -> (QueueBase, Queue) {
        let mut b = QueueBase::decode(self.reg64(base_off));
        if b.log2size > max {
            self.violation(Violation::QueueTooLarge(base_off));
            b.log2size = max;
        }
        (b, Queue::clamped(b.log2size))
    }

    fn raise_gerror(&mut self, bit: u32) {
        if gerror::active(self.reg(off::GERROR), self.reg(off::GERRORN)) & bit != 0 {
            return;
        }
        let g = self.reg(off::GERROR) ^ bit;
        self.regs.insert(off::GERROR, g);
        if self.reg(off::IRQ_CTRL) & irq_ctrl::GERROR_IRQEN != 0 {
            self.irqs.gerror += 1;
        }
    }

    /// Consumes the command queue up to PROD, stopping at an error.
    pub fn consume(&mut self) {
        if !self.enabled(cr0::CMDQEN) {
            return;
        }
        let (base, q) = self.queue(off::CMDQ_BASE, self.cfg.idr1.cmdqs);
        loop {
            if gerror::active(self.reg(off::GERROR), self.reg(off::GERRORN)) & gerror::CMDQ_ERR != 0
            {
                return;
            }
            let cons_reg = self.reg(off::CMDQ_CONS);
            let (prod, cons) = (q.pos(self.reg(off::CMDQ_PROD)), q.pos(cons_reg));
            if q.is_empty(prod, cons) {
                return;
            }
            let at = base.addr.get() + q.offset(cons, CMD_BYTES);
            let mut w = [0u64; CMD_WORDS];
            let mut fetched = true;
            for (i, word) in w.iter_mut().enumerate() {
                match self.mem.read_u64(at + 8 * i as u64) {
                    Some(v) => *word = v,
                    None => fetched = false,
                }
            }
            let result = if fetched {
                Command::decode(&w).map_err(|e| match e {
                    CmdDecodeError::Illegal(_) => CmdError::Illegal,
                    CmdDecodeError::Unsupported(opc) => {
                        self.violation(Violation::UnsupportedCommand(opc));
                        CmdError::Illegal
                    }
                })
            } else {
                Err(CmdError::Abort)
            };
            match result {
                Ok(c) => {
                    self.execute(c);
                    // A successful command clears nothing in ERR, which
                    // keeps the last error until the next one.
                    let next = q.next(cons).get() | (cons_reg & 0x7f00_0000);
                    self.regs.insert(off::CMDQ_CONS, next);
                }
                Err(e) => {
                    let v = (cons_reg & !0x7f00_0000) | e.encoding() << 24;
                    self.regs.insert(off::CMDQ_CONS, v);
                    self.raise_gerror(gerror::CMDQ_ERR);
                    return;
                }
            }
        }
    }

    fn execute(&mut self, c: Command) {
        self.stats.commands += 1;
        self.executed.push(c);
        match c {
            Command::PrefetchConfig { sid } => {
                let _ = self.stream_config(sid, false);
            }
            Command::CfgiSte { sid, leaf } => {
                self.ste_cache.remove(&sid.0);
                if !leaf {
                    self.drop_l1(sid.0);
                }
            }
            Command::CfgiSteRange { sid, range } => {
                if range >= RANGE_ALL {
                    self.ste_cache.clear();
                    self.l1_cache.clear();
                    self.cd_cache.clear();
                } else {
                    let n = 1u64 << (range + 1);
                    let first = u64::from(sid.0) & !(n - 1);
                    self.ste_cache
                        .retain(|&s, _| !(first..first + n).contains(&u64::from(s)));
                    let split = self.split();
                    self.l1_cache.retain(|&i, _| {
                        let lo = u64::from(i) << split;
                        let hi = lo + (1 << split);
                        hi <= first || lo >= first + n
                    });
                }
            }
            Command::CfgiCd { sid, .. } | Command::CfgiCdAll { sid } => {
                self.cd_cache.remove(&sid.0);
            }
            Command::TlbiNhAll | Command::TlbiNsnhAll => {
                self.tlb.clear();
                self.walk_cache.clear();
            }
            Command::TlbiEl2All => {}
            Command::TlbiNhAsid { asid } => {
                let a = u32::from(asid.0);
                self.tlb.retain(|&(k, _), _| k != a);
                self.walk_cache.retain(|&(k, _, _), _| k != asid.0);
            }
            Command::TlbiNhVa { asid, va } => self.tlbi_va(Some(asid), &va),
            Command::TlbiNhVaa { va } => self.tlbi_va(None, &va),
            Command::Sync(completion) => {
                // Everything before it has completed: the model executes in
                // order and at once.
                self.stats.syncs += 1;
                match completion {
                    SyncCompletion::None => {}
                    SyncCompletion::Sev => self.stats.sevs += 1,
                    SyncCompletion::Msi { addr, data } => {
                        if self.mem.write_u32(addr.get(), data) {
                            self.stats.msi_writes += 1;
                        } else {
                            self.raise_gerror(gerror::MSI_CMDQ_ABT_ERR);
                        }
                    }
                }
            }
        }
    }

    fn split(&self) -> u32 {
        match StrtabCfg::decode(self.reg(off::STRTAB_BASE_CFG)) {
            StrtabCfg::TwoLevel { split, .. } => u32::from(split),
            _ => 0,
        }
    }

    fn drop_l1(&mut self, sid: u32) {
        let split = self.split();
        self.l1_cache.remove(&(sid >> split));
    }

    /// Removes the TLB entries (and, unless leaf, walk-cache entries) for
    /// `va`'s range in `asid` (global entries included), or in every ASID.
    fn tlbi_va(&mut self, asid: Option<Asid>, va: &TlbiVa) {
        let lo = va.addr.get();
        let hi = lo.saturating_add(va.pages().saturating_mul(PAGE));
        let in_asid = |k: u32| asid.is_none_or(|a| k == u32::from(a.0) || k == GLOBAL);
        self.tlb.retain(|&(k, base), e| {
            let end = base + e.size.bytes();
            !(in_asid(k) && base < hi && end > lo)
        });
        if !va.leaf {
            self.walk_cache.retain(|&(k, lvl, prefix), _| {
                let shift = span_shift(lvl + 1);
                let base = prefix << shift;
                let end = base.saturating_add(1 << shift);
                !(asid.is_none_or(|a| a.0 == k) && base < hi && end > lo)
            });
        }
    }

    /// Records an event, if the queue takes it.
    fn record(&mut self, sid: StreamId, record: Record) {
        self.stats.events += 1;
        if !self.enabled(cr0::EVENTQEN) {
            self.stats.events_lost += 1;
            return;
        }
        let (base, q) = self.queue(off::EVENTQ_BASE, self.cfg.idr1.eventqs);
        let (prod_reg, cons_reg) = (self.reg(off::EVENTQ_PROD), self.reg(off::EVENTQ_CONS));
        let (prod, cons) = (q.pos(prod_reg), q.pos(cons_reg));
        if q.is_full(prod, cons) {
            self.stats.events_lost += 1;
            // A new overflow: PROD.OVFLG made to differ from CONS.OVACKFLG.
            if (prod_reg ^ cons_reg) & OVERFLOW == 0 {
                self.regs.insert(off::EVENTQ_PROD, prod_reg ^ OVERFLOW);
            }
            return;
        }
        let words = Event {
            sid,
            ssid: None,
            record,
        }
        .encode();
        let at = base.addr.get() + q.offset(prod, EVENT_BYTES);
        let mut ok = true;
        for (i, w) in words.iter().enumerate() {
            ok &= self.mem.write_u64(at + 8 * i as u64, *w);
        }
        if !ok {
            self.stats.events_lost += 1;
            self.raise_gerror(gerror::EVTQ_ABT_ERR);
            return;
        }
        self.regs
            .insert(off::EVENTQ_PROD, (prod_reg & OVERFLOW) | q.next(prod).get());
        if self.reg(off::IRQ_CTRL) & irq_ctrl::EVENTQ_IRQEN != 0 {
            self.irqs.event += 1;
        }
    }

    /// Terminates with a configuration error, recording it if `report`.
    fn config_error(&mut self, sid: StreamId, r: Record, report: bool) -> Termination {
        if report {
            self.record(sid, r);
        }
        Termination::Fault(r)
    }

    fn fetch<const N: usize>(&self, pa: u64) -> Option<[u64; N]> {
        let mut w = [0; N];
        for (i, x) in w.iter_mut().enumerate() {
            *x = self.mem.read_u64(pa + 8 * i as u64)?;
        }
        Some(w)
    }

    /// The STE for `sid`, from the cache or the table.
    fn ste(&mut self, sid: StreamId, report: bool) -> Result<Ste, Termination> {
        let bad_sid = Record::BadStreamId;
        if let Some(&cached) = self.ste_cache.get(&sid.0) {
            return match cached {
                Ok(Ste::Invalid) | Err(_) => Err(self.config_error(sid, Record::BadSte, report)),
                Ok(s) => Ok(s),
            };
        }
        let sidsize = u32::from(self.cfg.idr1.sidsize);
        if sidsize < 32 && sid.0 >> sidsize != 0 {
            return Err(self.config_error(sid, bad_sid, report));
        }
        let base = StrtabBase::decode(self.reg64(off::STRTAB_BASE)).addr.get();
        let geometry = match StrtabCfg::decode(self.reg(off::STRTAB_BASE_CFG)) {
            StrtabCfg::Linear { log2size } => Geometry::Linear { log2size },
            StrtabCfg::TwoLevel { log2size, split } => Geometry::TwoLevel { log2size, split },
            StrtabCfg::Reserved(_) => return Err(self.config_error(sid, bad_sid, report)),
        };
        let at = match geometry.slot(sid.0) {
            None => return Err(self.config_error(sid, bad_sid, report)),
            Some(SteSlot::Linear(o)) => base + o,
            Some(SteSlot::TwoLevel { l1, l2, index }) => {
                let key = lo32(l1 / 8);
                let desc = match self.l1_cache.get(&key) {
                    Some(&d) => d,
                    None => {
                        let Some(d) = self.mem.read_u64(base + l1) else {
                            let fetch = PhysAddr::new(base + l1);
                            return Err(self.config_error(sid, Record::SteFetch { fetch }, report));
                        };
                        if self.cfg.cache_invalid_config || L1Desc::decode(d).span != 0 {
                            self.l1_cache.insert(key, d);
                        }
                        d
                    }
                };
                let d = L1Desc::decode(desc);
                if index >= d.ste_count() {
                    return Err(self.config_error(sid, bad_sid, report));
                }
                d.l2.get() + l2
            }
        };
        let Some(words) = self.fetch::<STE_WORDS>(at) else {
            let fetch = PhysAddr::new(at);
            return Err(self.config_error(sid, Record::SteFetch { fetch }, report));
        };
        let ste = Ste::decode(&words);
        let valid = matches!(ste, Ok(s) if !matches!(s, Ste::Invalid));
        if valid || self.cfg.cache_invalid_config {
            self.ste_cache.insert(sid.0, ste);
        }
        match ste {
            Ok(Ste::Invalid) | Err(_) => Err(self.config_error(sid, Record::BadSte, report)),
            Ok(s) => Ok(s),
        }
    }

    /// The CD at `pa` for `sid`, from the cache or memory.
    fn cd(
        &mut self,
        sid: StreamId,
        pa: u64,
        report: bool,
    ) -> Result<ContextDescriptor, Termination> {
        let cd = match self.cd_cache.get(&sid.0) {
            Some(&c) => c,
            None => {
                let Some(words) = self.fetch::<CD_WORDS>(pa) else {
                    let fetch = PhysAddr::new(pa);
                    return Err(self.config_error(sid, Record::CdFetch { fetch }, report));
                };
                let c = ContextDescriptor::decode(&words);
                if c.is_ok() || self.cfg.cache_invalid_config {
                    self.cd_cache.insert(sid.0, c);
                }
                c
            }
        };
        cd.map_err(|_| self.config_error(sid, Record::BadCd, report))
    }

    fn stream_config(&mut self, sid: StreamId, report: bool) -> Result<StreamCfg, Termination> {
        match self.ste(sid, report)? {
            Ste::Invalid => Err(self.config_error(sid, Record::BadSte, report)),
            Ste::Abort => Ok(StreamCfg::Abort),
            Ste::Bypass => Ok(StreamCfg::Bypass),
            Ste::Stage1(s) => Ok(StreamCfg::Stage1(self.cd(sid, s.cd.get(), report)?)),
        }
    }

    /// The output address size in bits.
    fn oas(&self, cd: &ContextDescriptor) -> u32 {
        cd.ips.bits().min(self.cfg.idr5.oas.bits())
    }

    fn tlb_lookup(&self, asid: Asid, va: u64) -> Option<TlbEntry> {
        for size in [PageSize::Size4K, PageSize::Size2M, PageSize::Size1G] {
            let base = va & !(size.bytes() - 1);
            for k in [u32::from(asid.0), GLOBAL] {
                if let Some(e) = self.tlb.get(&(k, base))
                    && e.size == size
                {
                    return Some(*e);
                }
            }
        }
        None
    }

    /// Walks the stage 1 tables for `va`.
    fn walk(&mut self, cd: &ContextDescriptor, va: u64, access: Access) -> Result<Leaf, Record> {
        self.stats.walks += 1;
        let input = Input {
            addr: Iova::new(va),
            access,
        };
        let fault = |class| Fault {
            class,
            ..Fault::walk(Iova::new(va), access)
        };
        let ib = cd.input_bits();
        if ib < 64 && va >> ib != 0 {
            return Err(Record::Translation(fault(FaultClass::In)));
        }
        let oas = self.oas(cd);
        let too_big = |pa: u64| oas < 64 && pa >> oas != 0;
        // The levels a T0SZ needs: 9 bits each above the 12-bit page.
        let root = u8::try_from((ib - 12).div_ceil(9)).unwrap_or(4).clamp(1, 4);
        let (mut lvl, mut table) = (root, cd.ttb0.get());
        // Resume from the deepest cached table.
        for k in 1..root {
            if let Some(&t) = self
                .walk_cache
                .get(&(cd.asid.0, k, va >> span_shift(k + 1)))
            {
                (lvl, table) = (k, t);
                break;
            }
        }
        if too_big(table) {
            return Err(Record::AddrSize(fault(FaultClass::Tt)));
        }
        loop {
            let idx = (va >> span_shift(lvl)) & 0x1ff;
            let at = table + idx * 8;
            let Some(desc) = self.mem.read_u64(at) else {
                return Err(Record::WalkEabt {
                    input,
                    class: FaultClass::Tt,
                    fetch: PhysAddr::new(at),
                });
            };
            if !Smmu::is_valid(desc) {
                return Err(Record::Translation(fault(FaultClass::Tt)));
            }
            if lvl > 1 && Smmu::is_table(desc, level(lvl)) {
                let next = Smmu::address(desc, level(lvl)).get();
                if too_big(next) {
                    return Err(Record::AddrSize(fault(FaultClass::Tt)));
                }
                self.walk_cache
                    .insert((cd.asid.0, lvl - 1, va >> span_shift(lvl)), next);
                (lvl, table) = (lvl - 1, next);
                continue;
            }
            // A block at level 0, or the reserved encoding at level 3.
            let size = match lvl {
                1 if pt::smmu::is_page(desc) => PageSize::Size4K,
                2 => PageSize::Size2M,
                3 => PageSize::Size1G,
                _ => return Err(Record::Translation(fault(FaultClass::Tt))),
            };
            let pa = Smmu::address(desc, level(lvl)).get();
            if too_big(pa) {
                return Err(Record::AddrSize(fault(FaultClass::Tt)));
            }
            return Ok(Leaf { desc, pa, size });
        }
    }

    fn translate_s1(
        &mut self,
        cd: &ContextDescriptor,
        va: u64,
        access: Access,
    ) -> Result<(u64, PageSize, Attrs), Record> {
        let leaf = match self.tlb_lookup(cd.asid, va) {
            Some(e) => {
                self.stats.tlb_hits += 1;
                if e.desc == 0 {
                    return Err(Record::Translation(Fault::walk(Iova::new(va), access)));
                }
                Leaf {
                    desc: e.desc,
                    pa: e.pa,
                    size: e.size,
                }
            }
            None => match self.walk(cd, va, access) {
                Ok(l) => {
                    if pt::smmu::access_flag(l.desc) || cd.affd {
                        let k = if l.desc & (1 << 11) != 0 {
                            u32::from(cd.asid.0)
                        } else {
                            GLOBAL
                        };
                        let base = va & !(l.size.bytes() - 1);
                        self.tlb.insert(
                            (k, base),
                            TlbEntry {
                                desc: l.desc,
                                pa: l.pa,
                                size: l.size,
                            },
                        );
                    }
                    l
                }
                Err(r) => {
                    if self.cfg.cache_faults && matches!(r, Record::Translation(_)) {
                        self.tlb.insert(
                            (u32::from(cd.asid.0), va & !(PAGE - 1)),
                            TlbEntry {
                                desc: 0,
                                pa: 0,
                                size: PageSize::Size4K,
                            },
                        );
                    }
                    return Err(r);
                }
            },
        };
        let f = Fault::walk(Iova::new(va), access);
        if !pt::smmu::access_flag(leaf.desc) && !cd.affd {
            return Err(Record::Access(f));
        }
        let lvl = leaf.size.level();
        let attrs = Smmu::attrs(leaf.desc, lvl);
        let permitted = (access.privileged || pt::smmu::unprivileged(leaf.desc))
            && (access.read || !pt::smmu::read_only(leaf.desc))
            && (!access.instruction || attrs.exec);
        if !permitted {
            return Err(Record::Permission(f));
        }
        let pa = leaf.pa + (va & (leaf.size.bytes() - 1));
        Ok((pa, leaf.size, attrs))
    }

    /// Handles one transaction of `sid` at `iova`, as the SMMU would.
    pub fn translate(
        &mut self,
        sid: StreamId,
        iova: Iova,
        access: Access,
    ) -> Result<Translation, Termination> {
        if !self.enabled(cr0::SMMUEN) {
            return if Gbpa(self.reg(off::GBPA)).aborts() {
                Err(Termination::GlobalAbort)
            } else {
                Ok(Translation::Bypass(iova.get()))
            };
        }
        match self.stream_config(sid, true)? {
            StreamCfg::Abort => Err(Termination::SteAbort),
            StreamCfg::Bypass => Ok(Translation::Bypass(iova.get())),
            StreamCfg::Stage1(cd) => match self.translate_s1(&cd, iova.get(), access) {
                Ok((pa, size, attrs)) => Ok(Translation::Mapped { pa, size, attrs }),
                Err(r) => {
                    if cd.record {
                        self.record(sid, r);
                    }
                    if cd.abort {
                        Err(Termination::Fault(r))
                    } else {
                        Ok(Translation::RazWi)
                    }
                }
            },
        }
    }

    /// A device read of `buf.len()` bytes at `iova`, page by page.
    pub fn dma_read(
        &mut self,
        sid: StreamId,
        iova: Iova,
        buf: &mut [u8],
    ) -> Result<(), Termination> {
        let mut done = 0;
        while done < buf.len() {
            let va = iova.get() + done as u64;
            let n = usize::try_from((PAGE - (va & (PAGE - 1))).min((buf.len() - done) as u64))
                .unwrap_or(1);
            let chunk = &mut buf[done..done + n];
            match self.translate(sid, Iova::new(va), Access::READ)? {
                Translation::Bypass(pa) | Translation::Mapped { pa, .. } => {
                    if !self.mem.read(pa, chunk) {
                        return Err(Termination::ExternalAbort(pa));
                    }
                }
                Translation::RazWi => chunk.fill(0),
            }
            done += n;
        }
        Ok(())
    }

    /// A device write of `data` at `iova`, page by page.
    pub fn dma_write(&mut self, sid: StreamId, iova: Iova, data: &[u8]) -> Result<(), Termination> {
        let mut done = 0;
        while done < data.len() {
            let va = iova.get() + done as u64;
            let n = usize::try_from((PAGE - (va & (PAGE - 1))).min((data.len() - done) as u64))
                .unwrap_or(1);
            match self.translate(sid, Iova::new(va), Access::WRITE)? {
                Translation::Bypass(pa) | Translation::Mapped { pa, .. } => {
                    if !self.mem.write(pa, &data[done..done + n]) {
                        return Err(Termination::ExternalAbort(pa));
                    }
                }
                Translation::RazWi => {}
            }
            done += n;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
