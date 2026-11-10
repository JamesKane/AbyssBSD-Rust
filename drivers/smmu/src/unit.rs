// SPDX-License-Identifier: BSD-2-Clause
//! An SMMUv3 running (docs/IOMMU.md §4.7, §8 stage 2): its command and
//! event queues, its stream table, `CMD_SYNC`, events and global errors,
//! and the reset and quiesce sequences, over [`Hw`].
//!
//! - **Queues.** The command queue is filled at PROD under the caller's
//!   lock (the driver's spin lock): an entry is written, a barrier
//!   completes it, then PROD publishes it (the board port's
//!   `1b89d7594db`). A sync's slot is chosen, written, and published the
//!   same way, and the wait spins on it, never sleeps. The queues are at
//!   most [`CMDQ_MAX_LOG2`] and [`EVTQ_MAX_LOG2`] entries (FreeBSD takes
//!   IDR1's largest; IOMMU.md §15).
//! - **Sync.** An MSI write to the sync's own slot on a coherent SMMU
//!   with IDR0.MSI (FreeBSD's `SMMU_OPT_MSIPOLL`, upstream `50e01b6351b`);
//!   otherwise CONS is polled, with SEV completion when the SMMU has it
//!   after errata (MMU-600 before r1 loses it, erratum 1076982). A sync
//!   not done in a second is FreeBSD's `Failed to sync`.
//! - **Stream table.** Linear or two-level, as [`Geometry::for_smmu`]
//!   decides; level-2 tables are allocated when a stream in their span is
//!   first given an STE. Every STE starts invalid: traffic from a stream
//!   nobody claimed aborts with `C_BAD_STE` (confinement). Before the
//!   unit is enabled, entries are written directly (the RMR streams);
//!   after, an STE that is valid is first made invalid, then the rest is
//!   written, then word 0 with V, each step followed by `CMD_CFGI_STE` and
//!   a sync; a new level-1 descriptor is invalidated with the STE
//!   (`CMD_CFGI_STE`, not leaf).
//! - **Translation** (stage 3). A domain's context descriptor (one per
//!   domain, keyed by its ASID, [`Unit::attach_stage1`]) is written whole,
//!   word 0 with V last; the stream's STE then points at it (stage 1, no
//!   substreams), by the same break-before-make, with `CMD_CFGI_CD` beside
//!   the `CMD_CFGI_STE`. [`Unit::invalidate`] issues `CMD_TLBI_NH_VA` (leaf)
//!   per page, or `CMD_TLBI_NH_ASID` beyond [`TLBI_ASID_PAGES`], and one
//!   sync. ASIDs come from a bitmap of IDR0's 8 or 16 bits; 0 is never
//!   given out.
//! - **Errors.** The event queue is drained on demand ([`Unit::drain_events`],
//!   from the driver's event thread), its overflow acknowledged once per
//!   overflow. Global errors are acknowledged (`GERRORN`); a command the
//!   SMMU refused (`CMDQ_ERR`) is replaced with a `CMD_SYNC` so the queue
//!   goes on, also from inside a sync wait, which would otherwise wait
//!   for a stopped queue.
//! - **Reset** ([`Unit::enable`]): FreeBSD's `smmu_reset` order, with the
//!   firmware's `CR0` and `GBPA` recorded first, every control bit cleared
//!   (the bases may only be written with their queues off), and stale
//!   global errors acknowledged. **Quiesce** ([`Unit::quiesce`]) turns
//!   everything off and leaves `GBPA` as firmware had it (IOMMU.md
//!   §4.11).
//!
//! Behavior reference: sys/arm64/iommu/smmu.c (`smmu_reset`, `smmu_sync`,
//! `smmu_cmdq_enqueue_cmd`, `smmu_evtq_dequeue`, `smmu_init_ste_*`,
//! `smmu_init_l1_entry`), and the user's branch orangepi-6-plus of it.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use iommu::cd::{CD_BYTES, CD_WORDS, ContextDescriptor};
use iommu::cmd::{CMD_BYTES, CMD_WORDS, Command, SyncCompletion, TlbiVa};
use iommu::event::{EVENT_BYTES, EVENT_WORDS, Event};
use iommu::queue::{OVERFLOW, Pos, Queue};
use iommu::regs::{
    CmdError, Cr1, Features, Gbpa, QueueBase, StrtabBase, cr0, cr2, gerror, irq_ctrl, off,
};
use iommu::regs::{MemAttrs, StallModel};
use iommu::ste::{Geometry, L1Desc, STE_BYTES, STE_WORDS, Stage1, Ste, SteSlot};
use iommu::{Asid, Iova, PAGE, PhysAddr, StreamId};

use crate::hw::Hw;

/// The largest command queue the driver makes: 4096 entries, 64 KiB.
pub const CMDQ_MAX_LOG2: u8 = 12;
/// The largest event queue: 2048 records, 64 KiB.
pub const EVTQ_MAX_LOG2: u8 = 11;
/// How long a sync, a full queue, or a register acknowledgement is waited
/// for: FreeBSD's `SBT_1S`.
pub const TIMEOUT_NS: u64 = 1_000_000_000;
/// Above this many pages, an invalidation drops the whole ASID rather
/// than issuing a command per page (docs/IOMMU.md §4.7): 2 MiB, past
/// which per-page commands cost more than the refills they save.
pub const TLBI_ASID_PAGES: u64 = 512;

/// How a `CMD_SYNC` signals its completion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncMode {
    /// An MSI write of 0 to the sync's own slot, which the CPU polls.
    Msi,
    /// A WFE wake-up (SEV); CONS is polled.
    Sev,
    /// Nothing; CONS is polled.
    Poll,
}

impl SyncMode {
    /// The form for an SMMU: MSI only when it writes coherently (an MSI
    /// to memory the CPU caches is otherwise not seen), as FreeBSD.
    #[must_use]
    pub const fn for_features(f: &Features) -> Self {
        if f.msi && f.coherent {
            Self::Msi
        } else if f.sev {
            Self::Sev
        } else {
            Self::Poll
        }
    }
}

/// Why the unit could not do something.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// No memory for a queue or table.
    NoMemory,
    /// A control register's write was not acknowledged (its offset).
    NoAck(u32),
    /// A `CMD_SYNC` did not complete.
    SyncTimeout,
    /// The command queue stayed full.
    QueueFull,
    /// The stream is beyond the stream table.
    NoStream(u32),
    /// Every ASID is in use.
    NoAsid,
}

impl Error {
    /// The bus's form of it, for a context request.
    #[must_use]
    pub const fn for_bus(self) -> bus::iommu::IommuError {
        match self {
            Self::NoMemory | Self::NoAsid => bus::iommu::IommuError::NoMemory,
            Self::NoStream(_) => bus::iommu::IommuError::NoStream,
            Self::NoAck(_) | Self::SyncTimeout | Self::QueueFull => {
                bus::iommu::IommuError::Hardware
            }
        }
    }
}

/// Something the unit saw that the driver reports (outside its lock).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notice {
    /// Firmware left the SMMU enabled; it was disabled before being
    /// programmed (FreeBSD's `smmu_reset: Warning: SMMU is enabled`).
    WasEnabled,
    /// The SMMU refused the command at `index`; it was replaced with a
    /// `CMD_SYNC`.
    CommandError {
        /// CMDQ_CONS.ERR.
        error: CmdError,
        /// The command's slot.
        index: u32,
        /// The command as it was.
        words: [u64; CMD_WORDS],
    },
    /// Global errors other than `CMDQ_ERR`, acknowledged (GERROR bits).
    GlobalError(u32),
    /// The event queue overflowed: events were lost.
    EventOverflow,
    /// A `CMD_SYNC` timed out (FreeBSD's `Failed to sync`).
    SyncTimeout,
}

/// Counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Syncs completed.
    pub syncs: u64,
    /// Syncs timed out.
    pub sync_timeouts: u64,
    /// Commands the SMMU refused.
    pub command_errors: u64,
    /// Global errors acknowledged (each bit counts).
    pub global_errors: u64,
    /// Event records drained.
    pub events: u64,
    /// Event queue overflows.
    pub overflows: u64,
}

/// Where the unit is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Programmed nothing yet: structures are written directly.
    Off,
    /// Translating (every stream through its STE).
    Enabled,
    /// Turned off again, `GBPA` as firmware left it.
    Quiesced,
}

/// What firmware left, recorded before the reset changes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Firmware {
    /// `CR0`.
    pub cr0: u32,
    /// `GBPA`, without UPDATE.
    pub gbpa: u32,
}

struct Ring<M> {
    mem: M,
    q: Queue,
    /// The producer index (the command queue's, which the driver owns).
    prod: Pos,
    /// The consumer register as last written (the event queue's, with
    /// its overflow acknowledgement) or read (the command queue's).
    cons: u32,
}

enum Table<M> {
    Linear(M),
    TwoLevel {
        l1: M,
        /// The level-2 tables made so far, by level-1 index.
        l2: BTreeMap<u32, M>,
    },
}

/// An SMMUv3 the driver programs.
pub struct Unit<H: Hw> {
    hw: H,
    features: Features,
    geometry: Geometry,
    sync: SyncMode,
    cmdq: Ring<H::Mem>,
    evtq: Ring<H::Mem>,
    table: Table<H::Mem>,
    state: State,
    firmware: Option<Firmware>,
    notices: Vec<Notice>,
    stats: Stats,
    /// ASIDs in use, one bit each.
    asids: Vec<u64>,
    /// The context descriptors, by their domain's ASID.
    cds: BTreeMap<u16, H::Mem>,
}

impl<H: Hw> core::fmt::Debug for Unit<H> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Unit")
            .field("geometry", &self.geometry)
            .field("sync", &self.sync)
            .field("state", &self.state)
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

/// The bytes of a queue of `2^log2` entries of `entry` bytes.
const fn queue_bytes(log2: u8, entry: u64) -> u64 {
    entry << log2
}

impl<H: Hw> Unit<H> {
    /// Allocates the queues and the stream table for an SMMU with
    /// `features` (which [`Features::check`] accepted). Nothing is written
    /// to the hardware.
    ///
    /// # Errors
    ///
    /// [`Error::NoMemory`].
    pub fn new(hw: H, features: Features) -> Result<Self, Error> {
        let geometry = Geometry::for_smmu(features.sid_bits(), features.two_level);
        let ring = |log2: u8, entry: u64, cons: u32| -> Result<Ring<H::Mem>, Error> {
            let q = Queue::clamped(log2);
            let bytes = queue_bytes(q.log2size(), entry);
            let mem = hw.alloc(bytes, bytes).ok_or(Error::NoMemory)?;
            Ok(Ring {
                mem,
                q,
                prod: q.pos(0),
                cons,
            })
        };
        let cmdq = ring(features.idr1.cmdqs.min(CMDQ_MAX_LOG2), CMD_BYTES, 0)?;
        let evtq = ring(features.idr1.eventqs.min(EVTQ_MAX_LOG2), EVENT_BYTES, 0)?;
        let base = geometry.base_bytes().max(STE_BYTES);
        let mem = hw.alloc(base, base).ok_or(Error::NoMemory)?;
        let table = match geometry {
            Geometry::Linear { .. } => Table::Linear(mem),
            Geometry::TwoLevel { .. } => Table::TwoLevel {
                l1: mem,
                l2: BTreeMap::new(),
            },
        };
        Ok(Self {
            hw,
            features,
            geometry,
            sync: SyncMode::for_features(&features),
            cmdq,
            evtq,
            table,
            state: State::Off,
            firmware: None,
            notices: Vec::new(),
            stats: Stats::default(),
            // ASID 0 is never given out.
            asids: {
                let mut a = alloc::vec![0u64; (1usize << features.asid_bits()) / 64];
                a[0] = 1;
                a
            },
            cds: BTreeMap::new(),
        })
    }

    /// The hardware access.
    pub const fn hw(&self) -> &H {
        &self.hw
    }

    /// What the SMMU supports.
    pub const fn features(&self) -> &Features {
        &self.features
    }

    /// The stream table's form.
    pub const fn geometry(&self) -> Geometry {
        self.geometry
    }

    /// How syncs complete.
    pub const fn sync_mode(&self) -> SyncMode {
        self.sync
    }

    /// Where the unit is.
    pub const fn state(&self) -> State {
        self.state
    }

    /// What firmware left, once [`Unit::enable`] has run.
    pub const fn firmware(&self) -> Option<Firmware> {
        self.firmware
    }

    /// The counters.
    pub const fn stats(&self) -> &Stats {
        &self.stats
    }

    /// The command and event queues' sizes (log2 of their entries).
    pub const fn queue_sizes(&self) -> (u8, u8) {
        (self.cmdq.q.log2size(), self.evtq.q.log2size())
    }

    /// What happened since the last call, for the driver to report.
    pub fn take_notices(&mut self) -> Vec<Notice> {
        core::mem::take(&mut self.notices)
    }

    // ---- The stream table.

    /// Where `sid`'s STE is: its table and the word of its word 0.
    fn ste_at(&self, sid: u32) -> Option<(&H::Mem, usize)> {
        let words = |bytes: u64| usize::try_from(bytes / 8).ok();
        match (self.geometry.slot(sid)?, &self.table) {
            (SteSlot::Linear(o), Table::Linear(m)) => Some((m, words(o)?)),
            (SteSlot::TwoLevel { l1, l2: o, .. }, Table::TwoLevel { l2, .. }) => {
                let m = l2.get(&u32::try_from(l1 / 8).ok()?)?;
                Some((m, words(o)?))
            }
            _ => None,
        }
    }

    /// Makes the level-2 table for `sid`'s span, if it has none: whether
    /// a level-1 descriptor was made valid.
    fn ensure_l2(&mut self, sid: u32) -> Result<bool, Error> {
        let Some(slot) = self.geometry.slot(sid) else {
            return Err(Error::NoStream(sid));
        };
        let (SteSlot::TwoLevel { l1: l1_off, .. }, Geometry::TwoLevel { split, .. }) =
            (slot, self.geometry)
        else {
            return Ok(false);
        };
        let Table::TwoLevel { l1, l2 } = &mut self.table else {
            return Ok(false);
        };
        let index = u32::try_from(l1_off / 8).map_err(|_| Error::NoStream(sid))?;
        if l2.contains_key(&index) {
            return Ok(false);
        }
        let bytes = STE_BYTES << split;
        let table = self.hw.alloc(bytes, bytes).ok_or(Error::NoMemory)?;
        let desc = L1Desc::new(PhysAddr::new(self.hw.pa(&table)), split);
        // The zeroed table before the descriptor that points to it.
        self.hw.write_barrier();
        let word = usize::try_from(l1_off / 8).map_err(|_| Error::NoStream(sid))?;
        self.hw.store(l1, word, desc.encode());
        l2.insert(index, table);
        Ok(true)
    }

    /// `sid`'s STE as the table holds it, and its level-1 descriptor in a
    /// two-level table (for FreeBSD's event dump).
    #[must_use]
    pub fn ste_words(&self, sid: u32) -> (Option<u64>, Option<[u64; STE_WORDS]>) {
        let l1 = match (self.geometry.slot(sid), &self.table) {
            (Some(SteSlot::TwoLevel { l1: o, .. }), Table::TwoLevel { l1, .. }) => {
                usize::try_from(o / 8).ok().map(|w| self.hw.load(l1, w))
            }
            _ => None,
        };
        let ste = self.ste_at(sid).map(|(m, w)| {
            let mut out = [0; STE_WORDS];
            for (i, x) in out.iter_mut().enumerate() {
                *x = self.hw.load(m, w + i);
            }
            out
        });
        (l1, ste)
    }

    /// `sid`'s STE, decoded (invalid if it has none).
    #[must_use]
    pub fn stream(&self, sid: u32) -> Ste {
        match self.ste_words(sid).1 {
            Some(w) => Ste::decode(&w).unwrap_or(Ste::Invalid),
            None => Ste::Invalid,
        }
    }

    /// Gives `sid` the STE `ste`. Before [`Unit::enable`] the table is
    /// written directly; after, by the break-before-make sequence the
    /// module describes, with the invalidations and syncs that make the
    /// SMMU see it. Giving a stream the STE it has does nothing.
    ///
    /// # Errors
    ///
    /// [`Error::NoStream`], [`Error::NoMemory`], or a sync's.
    pub fn set_stream(&mut self, sid: u32, ste: Ste) -> Result<(), Error> {
        self.set_stream_with(sid, ste, None)
    }

    /// [`Unit::set_stream`], with `also` issued beside the last
    /// `CMD_CFGI_STE`, before its sync.
    fn set_stream_with(&mut self, sid: u32, ste: Ste, also: Option<Command>) -> Result<(), Error> {
        let new_l1 = self.ensure_l2(sid)?;
        let words = ste.encode();
        let (cur, live) = {
            let (m, w) = self.ste_at(sid).ok_or(Error::NoStream(sid))?;
            let mut cur = [0; STE_WORDS];
            for (i, x) in cur.iter_mut().enumerate() {
                *x = self.hw.load(m, w + i);
            }
            (cur, self.state == State::Enabled)
        };
        if cur == words && !new_l1 {
            return Ok(());
        }
        let valid = cur[0] & 1 != 0;
        if live && valid {
            // Break: no SMMU may see a half-written entry, so the old one
            // goes first.
            self.store_ste_word(sid, 0, 0)?;
            self.push(Command::CfgiSte {
                sid: StreamId(sid),
                leaf: true,
            })?;
            self.sync()?;
        }
        // Make: the other words, then word 0 (with V) last.
        for (i, &w) in words.iter().enumerate().skip(1) {
            self.store_ste_word(sid, i, w)?;
        }
        self.hw.write_barrier();
        self.store_ste_word(sid, 0, words[0])?;
        if live {
            self.push(Command::CfgiSte {
                sid: StreamId(sid),
                leaf: !new_l1,
            })?;
            if let Some(c) = also {
                self.push(c)?;
            }
            self.sync()?;
        }
        Ok(())
    }

    // ---- Translation.

    /// Takes a free ASID.
    ///
    /// # Errors
    ///
    /// [`Error::NoAsid`].
    pub fn alloc_asid(&mut self) -> Result<Asid, Error> {
        for (i, w) in self.asids.iter_mut().enumerate() {
            if *w != u64::MAX {
                let bit = w.trailing_ones();
                *w |= 1 << bit;
                let n = u32::try_from(i).map_err(|_| Error::NoAsid)? * 64 + bit;
                return u16::try_from(n).map(Asid).map_err(|_| Error::NoAsid);
            }
        }
        Err(Error::NoAsid)
    }

    /// Gives `asid` back, its TLB entries and context descriptor gone
    /// (its streams must no longer point at the descriptor).
    ///
    /// # Errors
    ///
    /// A sync's; the ASID is then kept out of use.
    pub fn free_asid(&mut self, asid: Asid) -> Result<(), Error> {
        self.push(Command::TlbiNhAsid { asid })?;
        self.sync()?;
        self.cds.remove(&asid.0);
        if let Some(w) = self.asids.get_mut(usize::from(asid.0 / 64)) {
            *w &= !(1 << (asid.0 % 64));
        }
        Ok(())
    }

    /// The STE of a stream translated through the CD at `cd`.
    fn stage1(&self, cd: u64) -> Ste {
        Ste::Stage1(Stage1 {
            cd: PhysAddr::new(cd),
            cd_attrs: MemAttrs::for_coherency(self.features.coherent),
            // Stalls are not used (FreeBSD sets S1STALLD unless the SMMU
            // forces stalls).
            stall_disable: self.features.idr0.stall_model == StallModel::StallAndTerminate,
        })
    }

    /// Translates `sid` through the domain whose descriptor is `cd`
    /// (FreeBSD's `smmu_init_cd` and `smmu_init_ste_s1`): the descriptor
    /// is written for its ASID the first time, then the stream's STE
    /// points at it, the stream's cached descriptor invalidated with it.
    ///
    /// # Errors
    ///
    /// [`Error::NoMemory`], [`Error::NoStream`], or a sync's.
    pub fn attach_stage1(&mut self, sid: u32, cd: &ContextDescriptor) -> Result<(), Error> {
        if !self.cds.contains_key(&cd.asid.0) {
            let mem = self.hw.alloc(CD_BYTES, CD_BYTES).ok_or(Error::NoMemory)?;
            let words = cd.encode();
            for (i, &w) in words.iter().enumerate().skip(1) {
                self.hw.store(&mem, i, w);
            }
            // The rest of it before V.
            self.hw.write_barrier();
            self.hw.store(&mem, 0, words[0]);
            debug_assert_eq!(words.len(), CD_WORDS);
            self.cds.insert(cd.asid.0, mem);
        }
        let pa = self
            .cds
            .get(&cd.asid.0)
            .map(|m| self.hw.pa(m))
            .ok_or(Error::NoMemory)?;
        let ste = self.stage1(pa);
        self.set_stream_with(
            sid,
            ste,
            Some(Command::CfgiCd {
                sid: StreamId(sid),
                ssid: 0,
                leaf: true,
            }),
        )
    }

    /// Invalidates `asid`'s cached leaf translations of `[iova, iova +
    /// len)` and waits (FreeBSD's `smmu_tlbi_va` per page, then
    /// `smmu_sync`): one command per page, or one for the whole ASID past
    /// [`TLBI_ASID_PAGES`].
    ///
    /// # Errors
    ///
    /// [`Error::SyncTimeout`], [`Error::QueueFull`].
    pub fn invalidate(&mut self, asid: Asid, iova: u64, len: u64) -> Result<(), Error> {
        let pages = len.div_ceil(PAGE);
        if pages > TLBI_ASID_PAGES {
            self.push(Command::TlbiNhAsid { asid })?;
        } else {
            let first = iova & !(PAGE - 1);
            for p in 0..pages {
                self.push(Command::TlbiNhVa {
                    asid,
                    va: TlbiVa::page(Iova::new(first + p * PAGE), true),
                })?;
            }
        }
        self.sync()
    }

    fn store_ste_word(&self, sid: u32, i: usize, v: u64) -> Result<(), Error> {
        let (m, w) = self.ste_at(sid).ok_or(Error::NoStream(sid))?;
        self.hw.store(m, w + i, v);
        Ok(())
    }

    // ---- The command queue.

    /// Writes `words` at PROD and moves PROD on, without publishing.
    fn push_words(&mut self, words: [u64; CMD_WORDS]) -> Result<Pos, Error> {
        self.wait_space()?;
        let q = self.cmdq.q;
        let slot = self.cmdq.prod;
        let first = usize::try_from(q.offset(slot, CMD_BYTES) / 8).unwrap_or(0);
        for (i, &w) in words.iter().enumerate() {
            self.hw.store(&self.cmdq.mem, first + i, w);
        }
        self.cmdq.prod = q.next(slot);
        Ok(slot)
    }

    /// Queues `c` (published with the next sync).
    fn push(&mut self, c: Command) -> Result<(), Error> {
        self.push_words(c.encode()).map(|_| ())
    }

    /// Publishes what was pushed: the commands complete in memory before
    /// PROD shows them.
    fn publish(&self) {
        self.hw.doorbell_barrier();
        self.hw.write32(off::CMDQ_PROD, self.cmdq.prod.get());
    }

    /// Waits until the command queue has room for one more entry and the
    /// sync that may follow it.
    fn wait_space(&mut self) -> Result<(), Error> {
        let q = self.cmdq.q;
        // One slot is kept for the sync a batch ends with, so a batch
        // never fills the queue it then waits on.
        let need = if q.entries() > 2 { 2 } else { 1 };
        let start = self.hw.now_ns();
        loop {
            self.cmdq.cons = self.hw.read32(off::CMDQ_CONS);
            if q.space(self.cmdq.prod, q.pos(self.cmdq.cons)) >= need {
                return Ok(());
            }
            // The queue is full: publish what is pending so the SMMU can
            // consume it, and see to a refused command.
            self.publish();
            self.check_cmdq_error();
            if self.hw.now_ns().wrapping_sub(start) > TIMEOUT_NS {
                return Err(Error::QueueFull);
            }
            self.hw.relax();
        }
    }

    /// Issues a `CMD_SYNC` after what was pushed, publishes them, and waits
    /// for it: then the SMMU has completed every command before it.
    ///
    /// # Errors
    ///
    /// [`Error::SyncTimeout`] after a second (also a [`Notice`]), or
    /// [`Error::QueueFull`].
    pub fn sync(&mut self) -> Result<(), Error> {
        self.wait_space()?;
        let q = self.cmdq.q;
        let slot = self.cmdq.prod;
        let at = q.offset(slot, CMD_BYTES);
        let completion = match self.sync {
            SyncMode::Msi => SyncCompletion::Msi {
                addr: PhysAddr::new(self.hw.pa(&self.cmdq.mem) + at),
                data: 0,
            },
            SyncMode::Sev => SyncCompletion::Sev,
            SyncMode::Poll => SyncCompletion::None,
        };
        self.push_words(Command::Sync(completion).encode())?;
        let end = self.cmdq.prod;
        self.publish();
        let start = self.hw.now_ns();
        let at = usize::try_from(at).unwrap_or(0);
        loop {
            let done = match self.sync {
                // The MSI clears the slot's first word (the opcode, never
                // 0).
                SyncMode::Msi => self.hw.load32(&self.cmdq.mem, at) == 0,
                SyncMode::Sev | SyncMode::Poll => {
                    self.cmdq.cons = self.hw.read32(off::CMDQ_CONS);
                    q.consumed(slot, end, q.pos(self.cmdq.cons))
                }
            };
            if done {
                self.stats.syncs += 1;
                return Ok(());
            }
            // A refused command stops the queue until it is fixed.
            self.check_cmdq_error();
            if self.hw.now_ns().wrapping_sub(start) > TIMEOUT_NS {
                self.stats.sync_timeouts += 1;
                self.notices.push(Notice::SyncTimeout);
                return Err(Error::SyncTimeout);
            }
            self.hw.relax();
        }
    }

    /// If the SMMU stopped the command queue on a command it refused,
    /// replaces that command with a `CMD_SYNC` (no signal) and acknowledges
    /// the error, so the queue goes on (IOMMU.md §4.10).
    fn check_cmdq_error(&mut self) {
        let (g, n) = (self.hw.read32(off::GERROR), self.hw.read32(off::GERRORN));
        if gerror::active(g, n) & gerror::CMDQ_ERR == 0 {
            return;
        }
        let q = self.cmdq.q;
        let cons_reg = self.hw.read32(off::CMDQ_CONS);
        let at = q.pos(cons_reg);
        let first = usize::try_from(q.offset(at, CMD_BYTES) / 8).unwrap_or(0);
        let mut words = [0; CMD_WORDS];
        for (i, w) in words.iter_mut().enumerate() {
            *w = self.hw.load(&self.cmdq.mem, first + i);
        }
        for (i, &w) in Command::Sync(SyncCompletion::None)
            .encode()
            .iter()
            .enumerate()
        {
            self.hw.store(&self.cmdq.mem, first + i, w);
        }
        self.hw.doorbell_barrier();
        self.hw.write32(off::GERRORN, n ^ gerror::CMDQ_ERR);
        self.stats.command_errors += 1;
        self.notices.push(Notice::CommandError {
            error: CmdError::from_cons(cons_reg),
            index: q.index(at),
            words,
        });
    }

    // ---- Errors and events.

    /// Acknowledges the active global errors: a refused command is
    /// skipped ([`Notice::CommandError`]), the others are reported
    /// ([`Notice::GlobalError`]). The gerror interrupt's work.
    pub fn handle_gerror(&mut self) {
        self.check_cmdq_error();
        let (g, n) = (self.hw.read32(off::GERROR), self.hw.read32(off::GERRORN));
        let active = gerror::active(g, n) & !gerror::CMDQ_ERR;
        if active == 0 {
            return;
        }
        self.hw.write32(off::GERRORN, n ^ active);
        self.stats.global_errors += u64::from(active.count_ones());
        self.notices.push(Notice::GlobalError(active));
    }

    /// Drains the event queue: `f` gets each record, decoded and raw. An
    /// overflow is acknowledged and noted once. At most one queue's worth
    /// per call, so a storm cannot hold the caller.
    pub fn drain_events(&mut self, mut f: impl FnMut(Event, [u64; EVENT_WORDS])) {
        let q = self.evtq.q;
        let mut budget = q.entries();
        loop {
            let prod_reg = self.hw.read32(off::EVENTQ_PROD);
            // The records PROD covers, read after it.
            self.hw.read_barrier();
            if (prod_reg ^ self.evtq.cons) & OVERFLOW != 0 {
                self.stats.overflows += 1;
                self.notices.push(Notice::EventOverflow);
            }
            let prod = q.pos(prod_reg);
            let mut cons = q.pos(self.evtq.cons);
            if q.is_empty(prod, cons) || budget == 0 {
                // Acknowledge an overflow even with nothing to read.
                let ack = cons.get() | (prod_reg & OVERFLOW);
                if ack != self.evtq.cons {
                    self.evtq.cons = ack;
                    self.hw.write32(off::EVENTQ_CONS, ack);
                }
                return;
            }
            while !q.is_empty(prod, cons) && budget > 0 {
                let first = usize::try_from(q.offset(cons, EVENT_BYTES) / 8).unwrap_or(0);
                let mut w = [0; EVENT_WORDS];
                for (i, x) in w.iter_mut().enumerate() {
                    *x = self.hw.load(&self.evtq.mem, first + i);
                }
                self.stats.events += 1;
                f(Event::decode(&w), w);
                cons = q.next(cons);
                budget -= 1;
            }
            // Free the slots, acknowledging any overflow PROD showed.
            self.evtq.cons = cons.get() | (prod_reg & OVERFLOW);
            self.hw.write32(off::EVENTQ_CONS, self.evtq.cons);
        }
    }

    // ---- Reset and quiesce.

    /// Writes `v` to the control register at `reg` and waits for its
    /// acknowledgement at `ack` (FreeBSD's `smmu_write_ack`).
    fn write_ack(&self, reg: u32, ack: u32, v: u32) -> Result<(), Error> {
        self.hw.write32(reg, v);
        let start = self.hw.now_ns();
        while self.hw.read32(ack) != v {
            if self.hw.now_ns().wrapping_sub(start) > TIMEOUT_NS {
                return Err(Error::NoAck(reg));
            }
            self.hw.relax();
        }
        Ok(())
    }

    /// Resets and enables the SMMU: firmware's state recorded, translation
    /// off, interrupts, the stream table and command queue programmed,
    /// cached configuration and TLBs invalidated, the event queue
    /// programmed, then translation on. Every stream is as the table says:
    /// invalid but for those [`Unit::set_stream`] gave an entry.
    ///
    /// # Errors
    ///
    /// The first step that failed; the SMMU is then left disabled.
    pub fn enable(&mut self) -> Result<(), Error> {
        let r = self.reset();
        if r.is_err() {
            // Whatever was programmed, nothing translates: as FreeBSD's
            // failed attach leaves it, but disabled.
            let _ = self.write_ack(off::CR0, off::CR0ACK, 0);
        }
        r
    }

    fn reset(&mut self) -> Result<(), Error> {
        let h = &self.hw;
        let cr0_fw = h.read32(off::CR0);
        self.firmware = Some(Firmware {
            cr0: cr0_fw,
            gbpa: h.read32(off::GBPA) & !Gbpa::UPDATE,
        });
        if cr0_fw & cr0::SMMUEN != 0 {
            self.notices.push(Notice::WasEnabled);
        }
        // Everything off: the queues too, whose bases are written next.
        self.write_ack(off::CR0, off::CR0ACK, 0)?;
        let h = &self.hw;
        // Wired interrupts: no MSIs.
        for cfg0 in [off::GERROR_IRQ_CFG0, off::EVENTQ_IRQ_CFG0] {
            h.write64(cfg0, 0);
            h.write32(cfg0 + 8, 0);
            h.write32(cfg0 + 12, 0);
        }
        self.write_ack(off::IRQ_CTRL, off::IRQ_CTRLACK, 0)?;
        self.write_ack(
            off::IRQ_CTRL,
            off::IRQ_CTRLACK,
            irq_ctrl::EVENTQ_IRQEN | irq_ctrl::GERROR_IRQEN,
        )?;
        let h = &self.hw;
        // Errors firmware left unacknowledged would stop the queue.
        let (g, n) = (h.read32(off::GERROR), h.read32(off::GERRORN));
        if gerror::active(g, n) != 0 {
            h.write32(off::GERRORN, n ^ gerror::active(g, n));
        }
        h.write32(
            off::CR1,
            Cr1::for_coherency(self.features.coherent).encode(),
        );
        // E2H only where EL2 contexts exist; it is RES0 otherwise.
        let e2h = if self.features.idr0.hyp { cr2::E2H } else { 0 };
        h.write32(off::CR2, cr2::PTM | cr2::RECINVSID | e2h);
        let table = match &self.table {
            Table::Linear(m) | Table::TwoLevel { l1: m, .. } => h.pa(m),
        };
        h.write64(
            off::STRTAB_BASE,
            StrtabBase {
                addr: PhysAddr::new(table),
                read_alloc: true,
            }
            .encode(),
        );
        h.write32(off::STRTAB_BASE_CFG, self.geometry.cfg().encode());
        h.write64(
            off::CMDQ_BASE,
            QueueBase {
                addr: PhysAddr::new(h.pa(&self.cmdq.mem)),
                log2size: self.cmdq.q.log2size(),
                alloc_hint: true,
            }
            .encode(),
        );
        self.cmdq.prod = self.cmdq.q.pos(0);
        self.cmdq.cons = 0;
        h.write32(off::CMDQ_PROD, 0);
        h.write32(off::CMDQ_CONS, 0);
        self.write_ack(off::CR0, off::CR0ACK, cr0::CMDQEN)?;
        // Nothing firmware or an earlier kernel left cached survives.
        self.push(Command::cfgi_all())?;
        self.sync()?;
        if self.features.idr0.hyp {
            self.push(Command::TlbiEl2All)?;
        }
        self.push(Command::TlbiNsnhAll)?;
        self.sync()?;
        let h = &self.hw;
        h.write64(
            off::EVENTQ_BASE,
            QueueBase {
                addr: PhysAddr::new(h.pa(&self.evtq.mem)),
                log2size: self.evtq.q.log2size(),
                alloc_hint: true,
            }
            .encode(),
        );
        self.evtq.cons = 0;
        h.write32(off::EVENTQ_PROD, 0);
        h.write32(off::EVENTQ_CONS, 0);
        let mut cr0 = cr0::CMDQEN | cr0::EVENTQEN;
        self.write_ack(off::CR0, off::CR0ACK, cr0)?;
        if self.features.idr0.ats {
            // Translated (ATS) traffic is checked against the STE, whose
            // EATS is 0: no device skips the SMMU that way.
            cr0 |= cr0::ATSCHK;
            self.write_ack(off::CR0, off::CR0ACK, cr0)?;
        }
        self.write_ack(off::CR0, off::CR0ACK, cr0 | cr0::SMMUEN)?;
        self.state = State::Enabled;
        Ok(())
    }

    /// Turns the SMMU off for whatever runs next (shutdown, a detach):
    /// translation, the queues, and the interrupts off, and `GBPA` as
    /// firmware left it, so traffic goes where firmware had it go
    /// (IOMMU.md §4.11). A unit never enabled is left alone.
    ///
    /// # Errors
    ///
    /// [`Error::NoAck`] if the SMMU did not acknowledge; the rest is still
    /// done.
    pub fn quiesce(&mut self) -> Result<(), Error> {
        let Some(fw) = self.firmware else {
            return Ok(());
        };
        let off_ok = self.write_ack(off::CR0, off::CR0ACK, 0);
        let irq_ok = self.write_ack(off::IRQ_CTRL, off::IRQ_CTRLACK, 0);
        let h = &self.hw;
        let mut gbpa_ok = Ok(());
        if h.read32(off::GBPA) & !Gbpa::UPDATE != fw.gbpa {
            h.write32(off::GBPA, Gbpa(fw.gbpa).update());
            let start = h.now_ns();
            while Gbpa(h.read32(off::GBPA)).updating() {
                if h.now_ns().wrapping_sub(start) > TIMEOUT_NS {
                    gbpa_ok = Err(Error::NoAck(off::GBPA));
                    break;
                }
                h.relax();
            }
        }
        self.state = State::Quiesced;
        off_ok.and(irq_ok).and(gbpa_ok)
    }

    /// Pushes raw command words and syncs: a test's way to issue what the
    /// driver never would.
    #[cfg(test)]
    pub(crate) fn push_raw_and_sync(&mut self, words: [u64; CMD_WORDS]) -> Result<(), Error> {
        self.push_words(words)?;
        self.sync()
    }
}
