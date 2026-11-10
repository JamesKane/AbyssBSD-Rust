// SPDX-License-Identifier: BSD-2-Clause
//! The driver's unit against the SMMU model (`iommu::model`, docs/IOMMU.md
//! §7): the reset order, the invalidations that make new STEs and level-1
//! descriptors visible, both sync forms, events and global errors, and
//! quiesce. Every test ends with the model having seen nothing the
//! architecture forbids.

extern crate std;

use alloc::vec::Vec;
use core::cell::{Cell, RefCell};

use acpi::iort::{NamedStreams, ReservedStreams, SmmuNode};
use acpi_tables::iort::SmmuV3;
use iommu::cmd::{Command, SyncCompletion};
use iommu::event::{Access, Event, Record};
use iommu::model::{Config, Model, PhysMemory, SparseMemory, Termination, Translation};
use iommu::regs::{Features, Gbpa, Idr0, Missing, StLevel, StrtabCfg, cr0, gerror, irq_ctrl, off};
use iommu::ste::{Geometry, Ste};
use iommu::{Iova, StreamId};

use crate::hw::Hw;
use crate::unit::{Error, Notice, State, SyncMode, TIMEOUT_NS, Unit};
use crate::{Idr, bypassed_streams, set_tunables, sysctl_init};

/// The model behind the driver's register and memory trait, as stage 1's
/// notes describe: a `RefCell` around it, and a clock that moves on every
/// read of it so a wait that never ends times out.
struct ModelHw {
    m: RefCell<Model<SparseMemory>>,
    clock: Cell<u64>,
    /// Whether a spin wait lets the model consume commands (when it does
    /// not consume them on the PROD write).
    consume_on_relax: Cell<bool>,
    /// Allocations fail.
    no_memory: Cell<bool>,
}

impl ModelHw {
    fn new(cfg: Config) -> Self {
        Self {
            m: RefCell::new(Model::new(cfg, SparseMemory::new(0x4000_0000))),
            clock: Cell::new(0),
            consume_on_relax: Cell::new(true),
            no_memory: Cell::new(false),
        }
    }
}

impl Hw for ModelHw {
    type Mem = u64;

    fn read32(&self, off: u32) -> u32 {
        self.m.borrow().read32(off)
    }

    fn write32(&self, off: u32, v: u32) {
        self.m.borrow_mut().write32(off, v);
    }

    fn alloc(&self, bytes: u64, align: u64) -> Option<u64> {
        if self.no_memory.get() {
            return None;
        }
        Some(self.m.borrow_mut().mem_mut().alloc(bytes, align))
    }

    fn pa(&self, m: &u64) -> u64 {
        *m
    }

    fn load(&self, m: &u64, word: usize) -> u64 {
        self.m
            .borrow()
            .mem()
            .read_u64(m + 8 * word as u64)
            .expect("backed")
    }

    fn store(&self, m: &u64, word: usize, v: u64) {
        assert!(
            self.m
                .borrow_mut()
                .mem_mut()
                .write_u64(m + 8 * word as u64, v)
        );
    }

    fn load32(&self, m: &u64, off: usize) -> u32 {
        let mut b = [0; 4];
        assert!(self.m.borrow().mem().read(m + off as u64, &mut b));
        u32::from_le_bytes(b)
    }

    fn write_barrier(&self) {}
    fn doorbell_barrier(&self) {}
    fn read_barrier(&self) {}

    fn now_ns(&self) -> u64 {
        let t = self.clock.get() + 100;
        self.clock.set(t);
        t
    }

    fn relax(&self) {
        self.clock.set(self.clock.get() + 1_000);
        if self.consume_on_relax.get() {
            self.m.borrow_mut().consume();
        }
    }
}

fn features(cfg: &Config) -> Features {
    Features::decode(
        cfg.idr0.encode(),
        cfg.idr1.encode(),
        cfg.idr3,
        cfg.idr5.encode(),
        cfg.iidr.encode(),
        false,
    )
}

fn unit(cfg: Config) -> Unit<ModelHw> {
    let f = features(&cfg);
    assert_eq!(f.check(), Ok(()));
    Unit::new(ModelHw::new(cfg), f).expect("memory")
}

/// QEMU's SMMU with its real coherency (IDR0.COHACC; the model's preset
/// is not coherent), no MSI: syncs poll.
fn qemu() -> Config {
    let mut c = Config::qemu();
    c.idr0.cohacc = true;
    c
}

/// An SMMU with a linear table: 8 SID bits (two levels offered but not
/// worth it, as FreeBSD decides).
fn linear() -> Config {
    let mut c = qemu();
    c.idr1.sidsize = 8;
    c
}

fn model(u: &Unit<ModelHw>) -> core::cell::RefMut<'_, Model<SparseMemory>> {
    u.hw().m.borrow_mut()
}

fn assert_clean(u: &Unit<ModelHw>) {
    assert_eq!(model(u).violations(), &[], "architecture violations");
}

fn translate(u: &Unit<ModelHw>, sid: u32) -> Result<Translation, Termination> {
    model(u).translate(StreamId(sid), Iova::new(0x1234_5000), Access::WRITE)
}

/// The commands the model executed since the last call, without syncs.
fn commands(u: &Unit<ModelHw>) -> Vec<Command> {
    let mut m = model(u);
    let c = m
        .executed()
        .iter()
        .copied()
        .filter(|c| !matches!(c, Command::Sync(_)))
        .collect();
    m.clear_executed();
    c
}

#[test]
fn reset_follows_freebsds_order() {
    for cfg in [Config::board(), qemu(), linear()] {
        let mut u = unit(cfg);
        assert_eq!(u.state(), State::Off);
        u.enable().unwrap();
        assert_eq!(u.state(), State::Enabled);
        let m = model(&u);
        // Configuration caches, then TLBs, each with its sync.
        let syncs = |c: &Command| matches!(c, Command::Sync(_));
        let ex = m.executed();
        assert_eq!(ex.len(), 4, "{ex:?}");
        assert_eq!(ex[0], Command::cfgi_all());
        assert!(syncs(&ex[1]));
        assert_eq!(ex[2], Command::TlbiNsnhAll);
        assert!(syncs(&ex[3]));
        assert_eq!(
            m.read32(off::CR0),
            cr0::CMDQEN | cr0::EVENTQEN | cr0::SMMUEN
        );
        assert_eq!(
            m.read32(off::IRQ_CTRL),
            irq_ctrl::EVENTQ_IRQEN | irq_ctrl::GERROR_IRQEN
        );
        let cfg_reg = StrtabCfg::decode(m.read32(off::STRTAB_BASE_CFG));
        assert_eq!(cfg_reg, u.geometry().cfg());
        drop(m);
        assert_clean(&u);
    }
}

#[test]
fn table_forms() {
    // The board: two levels, 25 SID bits covered; QEMU: two levels, 16;
    // 8 SID bits: linear.
    assert_eq!(
        unit(Config::board()).geometry(),
        Geometry::TwoLevel {
            log2size: 25,
            split: 8
        }
    );
    assert_eq!(
        unit(qemu()).geometry(),
        Geometry::TwoLevel {
            log2size: 16,
            split: 8
        }
    );
    assert_eq!(unit(linear()).geometry(), Geometry::Linear { log2size: 8 });
    let mut only_linear = qemu();
    only_linear.idr0.st_level = StLevel::Linear;
    assert_eq!(
        unit(only_linear).geometry(),
        Geometry::Linear { log2size: 16 }
    );
    // Queues: FreeBSD's sizes, capped.
    assert_eq!(unit(Config::board()).queue_sizes(), (12, 11));
    assert_eq!(unit(qemu()).queue_sizes(), (8, 8));
}

#[test]
fn every_stream_starts_invalid() {
    for cfg in [Config::board(), qemu(), linear()] {
        let mut u = unit(cfg);
        u.enable().unwrap();
        // In a two-level table, a span without a level-2 table is
        // C_BAD_STREAMID (IHI 0070, and QEMU); its streams are C_BAD_STE
        // once a neighbor has an entry.
        let two_level = matches!(u.geometry(), Geometry::TwoLevel { .. });
        let none = if two_level {
            Record::BadStreamId
        } else {
            Record::BadSte
        };
        for sid in [0, 1, 0x7f, 0xff] {
            assert_eq!(
                translate(&u, sid),
                Err(Termination::Fault(none)),
                "{sid:#x}"
            );
        }
        u.set_stream(0x80, Ste::Bypass).unwrap();
        for sid in [0, 1, 0x7f, 0xff] {
            assert_eq!(
                translate(&u, sid),
                Err(Termination::Fault(Record::BadSte)),
                "{sid:#x}"
            );
        }
        // Beyond a linear table, or an L1 descriptor never made: the
        // architecture's C_BAD_STREAMID or C_BAD_STE, but never memory.
        assert!(translate(&u, 0x1_0000).is_err());
        assert_clean(&u);
    }
}

#[test]
fn reserved_streams_bypass_from_the_start() {
    // As the board's smmu1: RMR streams written before enable, two of
    // them sharing a level-2 table, one in a second.
    for cfg in [Config::board(), linear()] {
        let mut u = unit(cfg);
        for sid in [0x0, 0x1, 0x18] {
            u.set_stream(sid, Ste::Bypass).unwrap();
        }
        // Nothing was issued: the SMMU is off, the reset invalidates.
        assert_eq!(model(&u).executed(), &[]);
        u.enable().unwrap();
        for sid in [0x0, 0x1, 0x18] {
            assert_eq!(translate(&u, sid), Ok(Translation::Bypass(0x1234_5000)));
        }
        assert_eq!(translate(&u, 0x2), Err(Termination::Fault(Record::BadSte)));
        assert_clean(&u);
    }
}

#[test]
fn new_entries_are_invalidated() {
    // The model caches invalid configuration: an STE or a level-1
    // descriptor made valid without CFGI_STE stays unseen.
    for cfg in [Config::board(), qemu(), linear()] {
        assert!(cfg.cache_invalid_config);
        let mut u = unit(cfg);
        u.enable().unwrap();
        commands(&u);
        let two_level = matches!(u.geometry(), Geometry::TwoLevel { .. });
        // The third in a span of its own when there are spans.
        let third = if two_level { 0x1c1 } else { 0xc1 };
        for sid in [0x10, 0x11, third] {
            // Seen invalid first: cached so.
            assert!(translate(&u, sid).is_err());
            u.set_stream(sid, Ste::Bypass).unwrap();
            assert_eq!(
                translate(&u, sid),
                Ok(Translation::Bypass(0x1234_5000)),
                "{sid:#x}"
            );
        }
        let c = commands(&u);
        // A new level-2 table's descriptor is invalidated with its first
        // STE (not leaf); later STEs of its span, leaf only.
        assert_eq!(
            c,
            [
                Command::CfgiSte {
                    sid: StreamId(0x10),
                    leaf: !two_level
                },
                Command::CfgiSte {
                    sid: StreamId(0x11),
                    leaf: true
                },
                Command::CfgiSte {
                    sid: StreamId(third),
                    leaf: !two_level
                },
            ]
        );
        // Giving a stream what it has does nothing.
        u.set_stream(0x10, Ste::Bypass).unwrap();
        assert_eq!(commands(&u), []);
        assert_clean(&u);
    }
}

#[test]
fn a_valid_entry_breaks_before_it_is_remade() {
    let mut u = unit(Config::board());
    u.enable().unwrap();
    u.set_stream(0x40, Ste::Bypass).unwrap();
    commands(&u);
    model(&u).clear_executed();
    u.set_stream(0x40, Ste::Abort).unwrap();
    let m = model(&u);
    let ex: Vec<Command> = m.executed().to_vec();
    drop(m);
    // Invalid, invalidated, synced; then complete, invalidated, synced.
    let cfgi = Command::CfgiSte {
        sid: StreamId(0x40),
        leaf: true,
    };
    assert_eq!(ex.len(), 4, "{ex:?}");
    assert_eq!((ex[0], ex[2]), (cfgi, cfgi));
    assert!(matches!(ex[1], Command::Sync(_)) && matches!(ex[3], Command::Sync(_)));
    // Config abort: terminated without an event.
    assert_eq!(translate(&u, 0x40), Err(Termination::SteAbort));
    let events = model(&u).stats().events;
    u.set_stream(0x40, Ste::Invalid).unwrap();
    assert_eq!(u.stream(0x40), Ste::Invalid);
    assert_eq!(translate(&u, 0x40), Err(Termination::Fault(Record::BadSte)));
    assert_eq!(model(&u).stats().events, events + 1);
    assert_clean(&u);
}

#[test]
fn streams_beyond_the_table_are_refused() {
    let mut u = unit(linear());
    assert_eq!(
        u.set_stream(0x100, Ste::Bypass),
        Err(Error::NoStream(0x100))
    );
    let mut u = unit(Config::board());
    assert_eq!(
        u.set_stream(1 << 25, Ste::Bypass),
        Err(Error::NoStream(1 << 25))
    );
    assert_eq!(u.set_stream((1 << 25) - 1, Ste::Bypass), Ok(()));
}

#[test]
fn sync_forms() {
    // Coherent with MSI: the MSI clears the sync's own slot.
    let mut u = unit(Config::board());
    assert_eq!(u.sync_mode(), SyncMode::Msi);
    u.enable().unwrap();
    let m = model(&u);
    assert!(m.stats().msi_writes >= 2);
    assert!(m.executed().iter().all(|c| match c {
        Command::Sync(SyncCompletion::Msi { data, .. }) => *data == 0,
        Command::Sync(_) => false,
        _ => true,
    }));
    drop(m);
    // MSI but not coherent: polled, with SEV as the board's SMMU has it.
    let mut nc = Config::board();
    nc.idr0.cohacc = false;
    let mut u = unit(nc);
    assert_eq!(u.sync_mode(), SyncMode::Sev);
    u.enable().unwrap();
    assert!(model(&u).stats().sevs >= 2);
    // An MMU-600 r0 loses SEV (erratum 1076982): polled, no signal.
    let mut r0 = nc;
    r0.iidr.variant = 0;
    let mut u = unit(r0);
    assert_eq!(u.sync_mode(), SyncMode::Poll);
    u.enable().unwrap();
    assert!(
        model(&u)
            .executed()
            .contains(&Command::Sync(SyncCompletion::None))
    );
    // QEMU: no MSI, no SEV.
    assert_eq!(unit(qemu()).sync_mode(), SyncMode::Poll);
    assert_clean(&u);
}

#[test]
fn syncs_wait_for_a_slow_smmu() {
    // Commands consumed only while the CPU spins: both forms wait for it.
    for cfg in [Config::board(), qemu()] {
        let mut cfg = cfg;
        cfg.auto_consume = false;
        let mut u = unit(cfg);
        u.enable().unwrap();
        u.set_stream(0x20, Ste::Bypass).unwrap();
        assert_eq!(translate(&u, 0x20), Ok(Translation::Bypass(0x1234_5000)));
        assert_eq!(u.stats().sync_timeouts, 0);
        assert_clean(&u);
    }
}

#[test]
fn a_sync_that_never_completes_times_out() {
    for cfg in [Config::board(), qemu()] {
        let mut cfg = cfg;
        cfg.auto_consume = false;
        let mut u = unit(cfg);
        u.enable().unwrap();
        u.hw().consume_on_relax.set(false);
        let t0 = u.hw().clock.get();
        assert_eq!(u.set_stream(0x20, Ste::Bypass), Err(Error::SyncTimeout));
        assert!(u.hw().clock.get() - t0 > TIMEOUT_NS);
        assert_eq!(u.take_notices(), [Notice::SyncTimeout]);
        assert_eq!(u.stats().sync_timeouts, 1);
        // The SMMU catches up; the next sync covers the change.
        u.hw().consume_on_relax.set(true);
        u.sync().unwrap();
        assert_eq!(translate(&u, 0x20), Ok(Translation::Bypass(0x1234_5000)));
        assert_clean(&u);
    }
}

#[test]
fn events_are_decoded() {
    let mut u = unit(qemu());
    u.enable().unwrap();
    // Their span has a level-2 table: C_BAD_STE.
    u.set_stream(0, Ste::Bypass).unwrap();
    for sid in [3, 4, 5] {
        assert!(translate(&u, sid).is_err());
    }
    assert_eq!(model(&u).take_irqs().event, 3);
    let mut got = Vec::new();
    u.drain_events(|e, w| got.push((e, w)));
    let sids: Vec<u32> = got.iter().map(|(e, _)| e.sid.0).collect();
    assert_eq!(sids, [3, 4, 5]);
    for (e, w) in &got {
        assert_eq!(e.record, Record::BadSte);
        assert_eq!(w[0] & 0xff, 0x04);
        assert_eq!(Event::decode(w), *e);
    }
    // Drained: nothing more, CONS caught up with PROD.
    let mut more = 0;
    u.drain_events(|_, _| more += 1);
    assert_eq!(more, 0);
    let m = model(&u);
    assert_eq!(m.read32(off::EVENTQ_CONS), m.read32(off::EVENTQ_PROD));
    drop(m);
    assert_eq!(u.stats().events, 3);
    assert_eq!(u.take_notices(), []);
    assert_clean(&u);
}

#[test]
fn an_event_queue_overflow_is_acknowledged_once() {
    let mut cfg = qemu();
    cfg.idr1.eventqs = 2;
    let mut u = unit(cfg);
    u.enable().unwrap();
    // Four fit; the next two are lost, and PROD's overflow flag flips.
    for sid in 0..6 {
        assert!(translate(&u, sid).is_err());
    }
    assert_eq!(model(&u).stats().events_lost, 2);
    let mut got = Vec::new();
    u.drain_events(|e, _| got.push(e.sid.0));
    assert_eq!(got, [0, 1, 2, 3]);
    assert_eq!(u.take_notices(), [Notice::EventOverflow]);
    assert_eq!(u.stats().overflows, 1);
    // Acknowledged: the next event is no overflow.
    assert!(translate(&u, 9).is_err());
    let mut got = Vec::new();
    u.drain_events(|e, _| got.push(e.sid.0));
    assert_eq!(got, [9]);
    assert_eq!(u.take_notices(), []);
    let m = model(&u);
    assert_eq!(
        m.read32(off::EVENTQ_CONS),
        m.read32(off::EVENTQ_PROD),
        "index and overflow flag acknowledged"
    );
    drop(m);
    assert_clean(&u);
}

#[test]
fn a_refused_command_is_skipped() {
    for cfg in [Config::board(), qemu()] {
        let mut u = unit(cfg);
        u.enable().unwrap();
        // An opcode the architecture does not define: CERROR_ILL. The sync
        // after it completes all the same.
        u.push_raw_and_sync([0x7f, 0]).unwrap();
        let n = u.take_notices();
        assert_eq!(n.len(), 1, "{n:?}");
        let Notice::CommandError { error, words, .. } = n[0] else {
            panic!("{n:?}");
        };
        assert_eq!(error, iommu::regs::CmdError::Illegal);
        assert_eq!(words, [0x7f, 0]);
        let mut m = model(&u);
        assert_eq!(
            gerror::active(m.read32(off::GERROR), m.read32(off::GERRORN)),
            0
        );
        assert_eq!(m.take_irqs().gerror, 1);
        drop(m);
        // The queue goes on.
        u.set_stream(0x30, Ste::Bypass).unwrap();
        assert_eq!(translate(&u, 0x30), Ok(Translation::Bypass(0x1234_5000)));
        assert_eq!(u.stats().command_errors, 1);
        assert_clean(&u);
    }
}

#[test]
fn global_errors_are_acknowledged() {
    let mut u = unit(qemu());
    u.enable().unwrap();
    // The event queue's memory gone: the SMMU cannot write a record.
    let evq = {
        let m = model(&u);
        iommu::regs::QueueBase::decode(m.read64(off::EVENTQ_BASE))
    };
    model(&u)
        .mem_mut()
        .unback(evq.addr.get(), 32 << evq.log2size);
    assert!(translate(&u, 1).is_err());
    assert_eq!(model(&u).take_irqs().gerror, 1);
    u.handle_gerror();
    assert_eq!(
        u.take_notices(),
        [Notice::GlobalError(gerror::EVTQ_ABT_ERR)]
    );
    let m = model(&u);
    assert_eq!(
        gerror::active(m.read32(off::GERROR), m.read32(off::GERRORN)),
        0
    );
    drop(m);
    // Nothing active: nothing to do.
    u.handle_gerror();
    assert_eq!(u.take_notices(), []);
    assert_eq!(u.stats().global_errors, 1);
    assert_clean(&u);
}

#[test]
fn quiesce_leaves_gbpa_as_firmware_had_it() {
    let mut u = unit(qemu());
    // Firmware left bypass (QEMU's reset value), the model's preset aborts.
    let bypass = Gbpa(0x1000);
    model(&u).write32(off::GBPA, bypass.update());
    // Never enabled: nothing to undo.
    u.quiesce().unwrap();
    assert_eq!(model(&u).read32(off::CR0), 0);
    u.enable().unwrap();
    assert_eq!(u.firmware().unwrap().gbpa, 0x1000);
    // Something changed it meanwhile.
    model(&u).write32(off::GBPA, bypass.with_abort(true).update());
    u.set_stream(0x8, Ste::Bypass).unwrap();
    u.quiesce().unwrap();
    assert_eq!(u.state(), State::Quiesced);
    let m = model(&u);
    assert_eq!(m.read32(off::CR0), 0);
    assert_eq!(m.read32(off::IRQ_CTRL), 0);
    assert_eq!(m.read32(off::GBPA), 0x1000);
    drop(m);
    // Traffic now goes where GBPA says, whatever the streams were.
    for sid in [0x8, 0x9] {
        assert_eq!(translate(&u, sid), Ok(Translation::Bypass(0x1234_5000)));
    }
    assert_clean(&u);
}

#[test]
fn a_unit_firmware_left_enabled_is_disabled_first() {
    // An earlier kernel (or a panic) left it running with its own tables.
    let mut old = unit(qemu());
    old.enable().unwrap();
    old.set_stream(0x5, Ste::Bypass).unwrap();
    let hw = ModelHw {
        m: RefCell::new(old.hw().m.replace(Model::new(qemu(), SparseMemory::new(0)))),
        clock: Cell::new(0),
        consume_on_relax: Cell::new(true),
        no_memory: Cell::new(false),
    };
    // (The new kernel's memory comes after the old one's: the same bump
    // allocator.)
    hw.m.borrow_mut().clear_executed();
    let mut u = Unit::new(hw, features(&qemu())).unwrap();
    u.enable().unwrap();
    assert_eq!(u.take_notices(), [Notice::WasEnabled]);
    assert_ne!(u.firmware().unwrap().cr0 & cr0::SMMUEN, 0);
    // The old table is gone (no level-2 table for the span in the new
    // one); the old caches with it.
    assert_eq!(
        translate(&u, 0x5),
        Err(Termination::Fault(Record::BadStreamId))
    );
    assert_eq!(model(&u).executed()[0], Command::cfgi_all());
    assert_clean(&u);
}

#[test]
fn no_memory_is_an_error() {
    let f = features(&qemu());
    let hw = ModelHw::new(qemu());
    hw.no_memory.set(true);
    assert!(matches!(Unit::new(hw, f), Err(Error::NoMemory)));
    let mut u = unit(Config::board());
    u.enable().unwrap();
    u.hw().no_memory.set(true);
    // A stream whose level-2 table does not exist yet.
    assert_eq!(u.set_stream(0x4000, Ste::Bypass), Err(Error::NoMemory));
    assert_clean(&u);
}

fn node(reserved: Vec<ReservedStreams>, named: Vec<NamedStreams>) -> SmmuNode {
    SmmuNode {
        node: 0x30,
        smmu: SmmuV3 {
            base: 0xb1b_0000,
            flags: 1,
            model: 0,
            event_gsiv: 141,
            pri_gsiv: 150,
            gerr_gsiv: 146,
            sync_gsiv: 142,
            id_mapping_index: None,
        },
        reserved,
        named,
    }
}

#[test]
fn the_bypassed_streams() {
    // The board's smmu1: one RMR with ten streams, and named components
    // that share some of them.
    let rmr = ReservedStreams {
        streams: [0..=1, 6..=7, 0xc..=0xd, 0x12..=0x13, 0x18..=0x19].into(),
        ranges: [(0x8480_0000, 8 << 20)].into(),
        remap: true,
    };
    let dpu0 = NamedStreams {
        name: "\\_SB.DPU0".into(),
        streams: alloc::vec![0..=4],
    };
    let n = node([rmr].into(), [dpu0].into());
    let off = bypassed_streams(&n, false);
    assert_eq!(off.len(), 10, "FreeBSD's `10 streams bypassed`");
    assert_eq!(off[0].1, "stream 0x0: reserved 0x84800000-0x84ffffff");
    let on = bypassed_streams(&n, true);
    let sids: Vec<u32> = on.iter().map(|(s, _)| *s).collect();
    assert_eq!(
        sids,
        [0, 1, 6, 7, 0xc, 0xd, 0x12, 0x13, 0x18, 0x19, 2, 3, 4]
    );
    assert_eq!(on[10].1, "stream 0x2: \\_SB.DPU0");
    let fixed = ReservedStreams {
        streams: alloc::vec![7..=7],
        ranges: [(0x1000, 0x1000)].into(),
        remap: false,
    };
    let n = node([fixed].into(), Vec::new());
    assert_eq!(
        bypassed_streams(&n, true)[0].1,
        "stream 0x7: reserved 0x1000-0x1fff (no remapping)"
    );
}

#[test]
fn identifies_and_takes_coherency_from_either_source() {
    let cfg = Config::qemu();
    let hw = ModelHw::new(cfg);
    let idr = Idr::read(|o| hw.read32(o));
    let n = node(Vec::new(), Vec::new());
    assert!(idr.features(&n.smmu).coherent, "the IORT's override");
    let plain = SmmuV3 { flags: 0, ..n.smmu };
    assert!(!idr.features(&plain).coherent);
    let mut lines = Vec::new();
    idr.verbose(|l| lines.push(alloc::format!("{l}")));
    assert_eq!(
        lines,
        [alloc::format!(
            "IDR0 {:#x} IDR1 {:#x} IDR3 0x0 IDR5 {:#x} IIDR 0x0",
            cfg.idr0.encode(),
            cfg.idr1.encode(),
            cfg.idr5.encode()
        )]
    );
    let mut no_s1 = cfg;
    no_s1.idr0 = Idr0 {
        s1p: false,
        ..cfg.idr0
    };
    let hw = ModelHw::new(no_s1);
    assert_eq!(
        Idr::read(|o| hw.read32(o)).features(&plain).check(),
        Err(Missing::Stage1)
    );
}

#[test]
fn tunables_and_sysctls() {
    set_tunables(&|n| (n == "hw.smmu.bypass_named").then_some(1));
    assert!(crate::bypass_named());
    set_tunables(&|n| (n == "hw.smmu.bypass_named").then_some(0));
    assert!(!crate::bypass_named());
    let mut t = sysctl::Tree::new();
    sysctl_init(&mut t);
    assert!(t.name2oid("hw.smmu.bypass_named").is_some());
}

#[test]
fn model_memory_is_what_the_unit_wrote() {
    // The trait's 32-bit read sees the 64-bit stores' halves.
    let hw = ModelHw::new(qemu());
    let m = hw.alloc(64, 64).unwrap();
    hw.store(&m, 1, 0x1122_3344_5566_7788);
    assert_eq!(hw.load32(&m, 8), 0x5566_7788);
    assert_eq!(hw.load32(&m, 12), 0x1122_3344);
    assert!(hw.m.borrow().mem().read_u64(m + 8).is_some());
}
