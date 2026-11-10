// SPDX-License-Identifier: BSD-2-Clause
//! The model against IHI 0070's behavior, driven the way the driver will
//! drive it: structures written to memory, commands through the queue.

extern crate std;

use std::collections::BTreeMap;
use std::vec::Vec;

use pt::smmu::{Smmu, dma};
use pt::{Cache, PageTable, VirtAddr};

use super::*;
use crate::cd::CD_BYTES;
use crate::cmd::{TlbiRange, TlbiVa};
use crate::event::code;
use crate::regs::{Cr1, MemAttrs};
use crate::ste::Stage1;
use crate::testutil::Rng;

const CMDQ_LOG2: u8 = 6;
const EVQ_LOG2: u8 = 3;
const RW: Attrs = dma(true, Cache::WriteBack);
const RO: Attrs = dma(false, Cache::WriteBack);

/// A minimal driver: tables, queues, and commands, as stage 2 will do it.
struct Rig {
    m: Model<SparseMemory>,
    cmdq: u64,
    evq: u64,
    strtab: u64,
    geometry: Geometry,
    sync_word: u64,
    syncs: u32,
}

fn sid(n: u32) -> StreamId {
    StreamId(n)
}

fn pa(v: u64) -> pt::PhysAddr {
    pt::PhysAddr::new(v)
}

impl Rig {
    fn new(cfg: Config) -> Self {
        let geometry =
            Geometry::choose(cfg.idr1.sidsize, cfg.idr0.st_level == StLevel::TwoLevel, 16);
        let mut mem = SparseMemory::new(0x4000_0000);
        let cmdq = mem.alloc(CMD_BYTES << CMDQ_LOG2, CMD_BYTES << CMDQ_LOG2);
        let evq = mem.alloc(EVENT_BYTES << EVQ_LOG2, EVENT_BYTES << EVQ_LOG2);
        let strtab = mem.alloc(geometry.base_bytes(), geometry.base_bytes());
        let sync_word = mem.alloc(8, 8);
        let mut r = Self {
            m: Model::new(cfg, mem),
            cmdq,
            evq,
            strtab,
            geometry,
            sync_word,
            syncs: 0,
        };
        let m = &mut r.m;
        m.write64(
            off::STRTAB_BASE,
            StrtabBase {
                addr: PhysAddr::new(strtab),
                read_alloc: true,
            }
            .encode(),
        );
        m.write32(off::STRTAB_BASE_CFG, geometry.cfg().encode());
        m.write64(
            off::CMDQ_BASE,
            QueueBase {
                addr: PhysAddr::new(cmdq),
                log2size: CMDQ_LOG2,
                alloc_hint: true,
            }
            .encode(),
        );
        m.write32(off::CMDQ_PROD, 0);
        m.write32(off::CMDQ_CONS, 0);
        m.write64(
            off::EVENTQ_BASE,
            QueueBase {
                addr: PhysAddr::new(evq),
                log2size: EVQ_LOG2,
                alloc_hint: true,
            }
            .encode(),
        );
        m.write32(off::EVENTQ_PROD, 0);
        m.write32(off::EVENTQ_CONS, 0);
        m.write32(
            off::IRQ_CTRL,
            irq_ctrl::EVENTQ_IRQEN | irq_ctrl::GERROR_IRQEN,
        );
        m.write32(off::CR1, Cr1::for_coherency(true).encode());
        m.write32(off::CR0, cr0::CMDQEN | cr0::EVENTQEN);
        r.cmd(Command::cfgi_all());
        r.cmd(Command::TlbiNsnhAll);
        r.sync();
        r.m.write32(off::CR0, cr0::CMDQEN | cr0::EVENTQEN | cr0::SMMUEN);
        assert_eq!(r.m.read32(off::CR0ACK), r.m.read32(off::CR0));
        r
    }

    fn cmdq(&self) -> Queue {
        Queue::clamped(CMDQ_LOG2)
    }

    /// Writes a command at PROD and publishes it.
    fn cmd(&mut self, c: Command) {
        let q = self.cmdq();
        let prod = q.pos(self.m.read32(off::CMDQ_PROD));
        let at = self.cmdq + q.offset(prod, CMD_BYTES);
        for (i, w) in c.encode().iter().enumerate() {
            assert!(self.m.mem_mut().write_u64(at + 8 * i as u64, *w));
        }
        self.m.write32(off::CMDQ_PROD, q.next(prod).get());
    }

    /// A sync with MSI completion, checked.
    fn sync(&mut self) {
        self.syncs += 1;
        let data = self.syncs;
        self.cmd(Command::Sync(SyncCompletion::Msi {
            addr: PhysAddr::new(self.sync_word),
            data,
        }));
        let mut b = [0; 4];
        assert!(self.m.mem().read(self.sync_word, &mut b));
        assert_eq!(u32::from_le_bytes(b), data, "sync did not complete");
    }

    /// The STE's address, creating its level-2 table if needed.
    fn ste_addr(&mut self, s: StreamId) -> u64 {
        match self.geometry.slot(s.0).unwrap() {
            SteSlot::Linear(o) => self.strtab + o,
            SteSlot::TwoLevel { l1, l2, .. } => {
                let at = self.strtab + l1;
                let d = L1Desc::decode(self.m.mem().read_u64(at).unwrap());
                if d.span != 0 {
                    return d.l2.get() + l2;
                }
                let bytes = 64 << Geometry::SPLIT;
                let table = self.m.mem_mut().alloc(bytes, bytes);
                let d = L1Desc::new(PhysAddr::new(table), Geometry::SPLIT);
                self.m.mem_mut().write_u64(at, d.encode());
                self.cmd(Command::CfgiSte {
                    sid: s,
                    leaf: false,
                });
                self.sync();
                table + l2
            }
        }
    }

    /// Writes an STE (word 0 last) without invalidating.
    fn write_ste(&mut self, s: StreamId, ste: Ste) {
        let at = self.ste_addr(s);
        let w = ste.encode();
        for i in (0..8).rev() {
            self.m.mem_mut().write_u64(at + 8 * i as u64, w[i]);
        }
    }

    /// Installs an STE as the driver does: written, invalidated, synced.
    fn set_ste(&mut self, s: StreamId, ste: Ste) {
        self.write_ste(s, ste);
        self.cmd(Command::CfgiSte { sid: s, leaf: true });
        self.sync();
    }

    /// A domain: tables and a CD for `asid`, installed for `s`.
    fn domain(&mut self, s: StreamId, asid: u16) -> (u64, PageTable<Smmu>) {
        let pt = PageTable::<Smmu>::new(self.m.mem_mut(), PageSize::Size1G).unwrap();
        let cd_pa = self.m.mem_mut().alloc(CD_BYTES, CD_BYTES);
        self.write_cd(
            cd_pa,
            ContextDescriptor::new(
                Asid(asid),
                PhysAddr::new(pt.root().get()),
                Oas::Bits48,
                true,
            ),
        );
        self.set_ste(
            s,
            Ste::Stage1(Stage1 {
                cd: PhysAddr::new(cd_pa),
                cd_attrs: MemAttrs::COHERENT,
                stall_disable: false,
            }),
        );
        self.cmd(Command::CfgiCd {
            sid: s,
            ssid: 0,
            leaf: true,
        });
        self.sync();
        (cd_pa, pt)
    }

    fn write_cd(&mut self, at: u64, cd: ContextDescriptor) {
        let w = cd.encode();
        for i in (0..8).rev() {
            self.m.mem_mut().write_u64(at + 8 * i as u64, w[i]);
        }
    }

    fn map(&mut self, t: &mut PageTable<Smmu>, va: u64, p: u64, len: u64, a: Attrs) {
        t.map(self.m.mem_mut(), VirtAddr::new(va), pa(p), len, a)
            .unwrap();
    }

    fn unmap(&mut self, t: &mut PageTable<Smmu>, va: u64, len: u64) {
        t.unmap(self.m.mem_mut(), VirtAddr::new(va), len, |_| {})
            .unwrap();
    }

    fn tlbi(&mut self, asid: u16, va: u64) {
        self.cmd(Command::TlbiNhVa {
            asid: Asid(asid),
            va: TlbiVa::page(Iova::new(va), true),
        });
        self.sync();
    }

    /// Drains the event queue.
    fn events(&mut self) -> Vec<Event> {
        let q = Queue::clamped(EVQ_LOG2);
        let mut out = Vec::new();
        loop {
            let both = self.m.read64(off::EVENTQ_PROD);
            let (prod_reg, cons_reg) = (lo32(both), lo32(both >> 32));
            let (prod, cons) = (q.pos(prod_reg), q.pos(cons_reg));
            if q.is_empty(prod, cons) {
                break;
            }
            let at = self.evq + q.offset(cons, EVENT_BYTES);
            let mut w = [0u64; 4];
            for (i, x) in w.iter_mut().enumerate() {
                *x = self.m.mem().read_u64(at + 8 * i as u64).unwrap();
            }
            out.push(Event::decode(&w));
            self.m
                .write32(off::EVENTQ_CONS, (cons_reg & OVERFLOW) | q.next(cons).get());
        }
        out
    }

    fn read(&mut self, s: StreamId, va: u64, n: usize) -> Result<Vec<u8>, Termination> {
        let mut b = std::vec![0; n];
        self.m.dma_read(s, Iova::new(va), &mut b)?;
        Ok(b)
    }

    fn translation_fault(&mut self, s: StreamId, va: u64) {
        assert_eq!(
            self.m.translate(s, Iova::new(va), Access::READ),
            Err(Termination::Fault(Record::Translation(Fault::walk(
                Iova::new(va),
                Access::READ
            ))))
        );
    }
}

fn single(events: &[Event]) -> Event {
    assert_eq!(events.len(), 1, "{events:?}");
    events[0]
}

#[test]
fn gbpa_while_disabled() {
    let mut m = Model::new(Config::board(), SparseMemory::new(0));
    assert_eq!(
        m.translate(sid(1), Iova::new(0x1000), Access::READ),
        Err(Termination::GlobalAbort)
    );
    // GBPA takes effect only with UPDATE.
    m.write32(off::GBPA, 0);
    assert_eq!(m.violations(), [Violation::GbpaWithoutUpdate]);
    m.write32(off::GBPA, Gbpa(0).update());
    assert!(!Gbpa(m.read32(off::GBPA)).updating());
    assert_eq!(
        m.translate(sid(1), Iova::new(0x1234), Access::WRITE),
        Ok(Translation::Bypass(0x1234))
    );
    m.write32(off::GBPA, Gbpa(0).with_abort(true).update());
    assert_eq!(
        m.translate(sid(1), Iova::new(0x1000), Access::READ),
        Err(Termination::GlobalAbort)
    );
}

#[test]
fn ste_configurations_linear() {
    let mut r = Rig::new(Config::qemu());
    assert!(matches!(r.geometry, Geometry::Linear { log2size: 16 }));
    r.m.mem_mut().back(0x9000_0000, 0x2000);
    r.m.mem_mut().write(0x9000_0010, b"hello");
    // Invalid (the initial state): C_BAD_STE, recorded.
    assert_eq!(
        r.m.translate(sid(8), Iova::new(0x9000_0000), Access::READ),
        Err(Termination::Fault(Record::BadSte))
    );
    let e = single(&r.events());
    assert_eq!((e.sid, e.record, e.ssid), (sid(8), Record::BadSte, None));
    assert_eq!(r.m.take_irqs().event, 1);
    // Abort: terminated, silently.
    r.set_ste(sid(8), Ste::Abort);
    assert_eq!(
        r.m.translate(sid(8), Iova::new(0x9000_0000), Access::READ),
        Err(Termination::SteAbort)
    );
    assert!(r.events().is_empty());
    // Bypass.
    r.set_ste(sid(8), Ste::Bypass);
    assert_eq!(r.read(sid(8), 0x9000_0010, 5).unwrap(), b"hello");
    // Out of the table.
    assert_eq!(
        r.m.translate(sid(0x1_0000), Iova::ZERO, Access::READ),
        Err(Termination::Fault(Record::BadStreamId))
    );
    assert_eq!(single(&r.events()).record, Record::BadStreamId);
    // Illegal STEs are C_BAD_STE.
    let at = r.ste_addr(sid(9));
    r.m.mem_mut().write_u64(at, 1 | 0b110 << 1);
    r.cmd(Command::CfgiSte {
        sid: sid(9),
        leaf: true,
    });
    r.sync();
    assert_eq!(
        r.m.translate(sid(9), Iova::ZERO, Access::READ),
        Err(Termination::Fault(Record::BadSte))
    );
    assert!(r.m.violations().is_empty());
}

#[test]
fn two_level_stream_table() {
    let mut r = Rig::new(Config::board());
    assert!(matches!(
        r.geometry,
        Geometry::TwoLevel {
            log2size: 32,
            split: 8
        }
    ));
    // No level-2 table yet: C_BAD_STREAMID.
    assert_eq!(
        r.m.translate(sid(0xc100), Iova::ZERO, Access::READ),
        Err(Termination::Fault(Record::BadStreamId))
    );
    r.m.mem_mut().back(0x1000, 0x1000);
    r.set_ste(sid(0xc100), Ste::Bypass);
    assert_eq!(r.read(sid(0xc100), 0x1000, 1).unwrap(), [0]);
    // Its neighbor shares the level-2 table and is invalid.
    assert_eq!(
        r.m.translate(sid(0xc101), Iova::ZERO, Access::READ),
        Err(Termination::Fault(Record::BadSte))
    );
    // The top StreamID.
    r.set_ste(sid(u32::MAX), Ste::Abort);
    assert_eq!(
        r.m.translate(sid(u32::MAX), Iova::ZERO, Access::READ),
        Err(Termination::SteAbort)
    );
    // A level-2 table that is not there: F_STE_FETCH.
    let at = r.strtab + 0x77 * 8;
    r.m.mem_mut()
        .write_u64(at, L1Desc::new(PhysAddr::new(0x7_0000_0000), 8).encode());
    r.cmd(Command::CfgiSte {
        sid: sid(0x7700),
        leaf: false,
    });
    r.sync();
    r.events();
    assert_eq!(
        r.m.translate(sid(0x7705), Iova::ZERO, Access::READ),
        Err(Termination::Fault(Record::SteFetch {
            fetch: PhysAddr::new(0x7_0000_0000 + 5 * 64)
        }))
    );
    // A span too small for the index: C_BAD_STREAMID.
    let l2 = r.m.mem_mut().alloc(64, 64);
    r.m.mem_mut().write_u64(
        r.strtab + 0x78 * 8,
        L1Desc {
            l2: PhysAddr::new(l2),
            span: 1,
        }
        .encode(),
    );
    r.cmd(Command::CfgiSte {
        sid: sid(0x7800),
        leaf: false,
    });
    r.sync();
    r.write_ste(sid(0x7800), Ste::Bypass);
    assert_eq!(
        r.m.translate(sid(0x7801), Iova::ZERO, Access::READ),
        Err(Termination::Fault(Record::BadStreamId))
    );
}

#[test]
fn stage1_translation_and_faults() {
    let mut r = Rig::new(Config::board());
    let s = sid(0xc100);
    let (_, mut t) = r.domain(s, 5);
    let buf = r.m.mem_mut().alloc(0x4000, 0x1000);
    r.m.mem_mut().write(buf + 0x1ff8, b"crossing page");
    // Two scattered physical pages at one IOVA range.
    r.map(&mut t, 0x10_0000, buf + 0x1000, 0x1000, RW);
    r.map(&mut t, 0x10_1000, buf + 0x3000, 0x1000, RO);
    r.m.mem_mut().write(buf + 0x3000, b"second");
    assert_eq!(&r.read(s, 0x10_0ff8, 14).unwrap()[8..], b"second");
    r.m.dma_write(s, Iova::new(0x10_0000), b"device").unwrap();
    let mut b = [0; 6];
    r.m.mem().read(buf + 0x1000, &mut b);
    assert_eq!(&b, b"device");
    assert_eq!(
        r.m.translate(s, Iova::new(0x10_0123), Access::READ),
        Ok(Translation::Mapped {
            pa: buf + 0x1123,
            size: PageSize::Size4K,
            attrs: RW
        })
    );
    // Writing the read-only page: F_PERMISSION, recorded with the access.
    assert_eq!(
        r.m.dma_write(s, Iova::new(0x10_1000), b"x"),
        Err(Termination::Fault(Record::Permission(Fault::walk(
            Iova::new(0x10_1000),
            Access::WRITE
        ))))
    );
    // An unmapped page, a guard: F_TRANSLATION.
    assert!(r.read(s, 0x10_2000, 4).is_err());
    let ev = r.events();
    assert_eq!(ev.len(), 2);
    assert_eq!(ev[0].sid, s);
    assert_eq!(ev[0].record.code(), code::F_PERMISSION);
    assert_eq!(
        ev[1].record,
        Record::Translation(Fault::walk(Iova::new(0x10_2000), Access::READ))
    );
    // Blocks at 2 MiB and 1 GiB.
    r.map(&mut t, 0x4000_0000, 0x8_0000_0000, 1 << 30, RW);
    r.map(&mut t, 0x20_0000, 0x8_4020_0000, 2 << 20, RW);
    for (va, pa, size) in [
        (0x4123_4567, 0x8_0123_4567, PageSize::Size1G),
        (0x3f_ffff, 0x8_403f_ffff, PageSize::Size2M),
    ] {
        assert_eq!(
            r.m.translate(s, Iova::new(va), Access::WRITE),
            Ok(Translation::Mapped {
                pa,
                size,
                attrs: RW
            })
        );
    }
    // Beyond the 48-bit input range.
    assert_eq!(
        r.m.translate(s, Iova::new(1 << 48), Access::READ),
        Err(Termination::Fault(Record::Translation(Fault {
            class: FaultClass::In,
            ..Fault::walk(Iova::new(1 << 48), Access::READ)
        })))
    );
    // Output beyond the 40-bit OAS: F_ADDR_SIZE.
    r.map(&mut t, 0x50_0000, 1 << 40, 0x1000, RW);
    assert!(matches!(
        r.m.translate(s, Iova::new(0x50_0000), Access::READ),
        Err(Termination::Fault(Record::AddrSize(_)))
    ));
    // Memory behind the mapping that is not there.
    r.map(&mut t, 0x51_0000, 0x7_0000_0000, 0x1000, RW);
    assert_eq!(
        r.read(s, 0x51_0000, 1),
        Err(Termination::ExternalAbort(0x7_0000_0000))
    );
    assert!(r.m.violations().is_empty());
}

#[test]
fn a_missing_tlbi_is_a_stale_translation() {
    let mut r = Rig::new(Config::board());
    let s = sid(3);
    let (_, mut t) = r.domain(s, 9);
    let a = r.m.mem_mut().alloc(0x1000, 0x1000);
    r.map(&mut t, 0x5000, a, 0x1000, RW);
    assert!(r.read(s, 0x5000, 8).is_ok());
    assert_eq!(r.m.tlb_len(), 1);
    // Unmapped, not invalidated: the device still reaches the page.
    r.unmap(&mut t, 0x5000, 0x1000);
    assert!(r.read(s, 0x5000, 8).is_ok());
    // Invalidated: it faults.
    r.tlbi(9, 0x5000);
    r.translation_fault(s, 0x5000);
    // Another ASID's TLBI does not help.
    r.map(&mut t, 0x6000, a, 0x1000, RW);
    assert!(r.read(s, 0x6000, 8).is_ok());
    r.unmap(&mut t, 0x6000, 0x1000);
    r.tlbi(10, 0x6000);
    assert!(r.read(s, 0x6000, 8).is_ok());
    r.cmd(Command::TlbiNhAsid { asid: Asid(9) });
    r.sync();
    r.translation_fault(s, 0x6000);
}

#[test]
fn range_and_vaa_invalidation() {
    let mut r = Rig::new(Config::board());
    let s = sid(3);
    let (_, mut t) = r.domain(s, 9);
    let a = r.m.mem_mut().alloc(0x8000, 0x1000);
    r.map(&mut t, 0x10_0000, a, 0x8000, RW);
    for p in 0..8 {
        assert!(r.read(s, 0x10_0000 + p * 0x1000, 1).is_ok());
    }
    r.unmap(&mut t, 0x10_0000, 0x8000);
    // NUM 3, SCALE 1: 8 pages.
    r.cmd(Command::TlbiNhVa {
        asid: Asid(9),
        va: TlbiVa {
            addr: Iova::new(0x10_0000),
            leaf: true,
            ttl: 3,
            range: Some(TlbiRange { num: 3, scale: 1 }),
        },
    });
    r.sync();
    for p in 0..8 {
        r.translation_fault(s, 0x10_0000 + p * 0x1000);
    }
    // VAA: every ASID.
    r.map(&mut t, 0x20_0000, a, 0x1000, RW);
    assert!(r.read(s, 0x20_0000, 1).is_ok());
    r.unmap(&mut t, 0x20_0000, 0x1000);
    r.cmd(Command::TlbiNhVaa {
        va: TlbiVa::page(Iova::new(0x20_0000), true),
    });
    r.sync();
    r.translation_fault(s, 0x20_0000);
}

#[test]
fn global_entries_survive_asid_invalidation() {
    let mut r = Rig::new(Config::board());
    let s = sid(3);
    let (_, mut t) = r.domain(s, 9);
    let a = r.m.mem_mut().alloc(0x1000, 0x1000);
    r.map(&mut t, 0x5000, a, 0x1000, RW);
    // Make the leaf global (nG clear), as pt::smmu never does.
    let root = t.root().get();
    let leaf_at = {
        let mut table = root;
        for lvl in (2..=4).rev() {
            let idx = (0x5000u64 >> span_shift(lvl)) & 0x1ff;
            table = r.m.mem().read_u64(table + idx * 8).unwrap() & 0xffff_ffff_f000;
        }
        table + ((0x5000 >> 12) & 0x1ff) * 8
    };
    let e = r.m.mem().read_u64(leaf_at).unwrap();
    r.m.mem_mut().write_u64(leaf_at, e & !(1 << 11));
    assert!(r.read(s, 0x5000, 1).is_ok());
    r.unmap(&mut t, 0x5000, 0x1000);
    r.cmd(Command::TlbiNhAsid { asid: Asid(9) });
    r.sync();
    assert!(
        r.read(s, 0x5000, 1).is_ok(),
        "a global entry outlives TLBI by ASID"
    );
    r.tlbi(9, 0x5000);
    r.translation_fault(s, 0x5000);
}

#[test]
fn walk_cache_needs_non_leaf_invalidation() {
    let mut r = Rig::new(Config::board());
    let s = sid(3);
    let (_, mut t) = r.domain(s, 9);
    let a = r.m.mem_mut().alloc(0x2000, 0x1000);
    r.map(&mut t, 0x5000, a, 0x1000, RW);
    assert!(r.read(s, 0x5000, 1).is_ok());
    // Free the tables (their frames are zeroed) and build new ones for
    // another page in the same 2 MiB.
    r.unmap(&mut t, 0x5000, 0x1000);
    t.free_tables(r.m.mem_mut(), VirtAddr::ZERO, 1 << 39);
    r.map(&mut t, 0x6000, a + 0x1000, 0x1000, RW);
    // Leaf-only invalidation leaves the cached walk to the freed table.
    r.tlbi(9, 0x5000);
    r.translation_fault(s, 0x6000);
    // A non-leaf one clears it.
    r.cmd(Command::TlbiNhVa {
        asid: Asid(9),
        va: TlbiVa::page(Iova::new(0x6000), false),
    });
    r.sync();
    assert_eq!(
        r.m.translate(s, Iova::new(0x6000), Access::READ),
        Ok(Translation::Mapped {
            pa: a + 0x1000,
            size: PageSize::Size4K,
            attrs: RW
        })
    );
}

#[test]
fn faults_are_not_cached_unless_configured() {
    for cache_faults in [false, true] {
        let mut r = Rig::new(Config {
            cache_faults,
            ..Config::board()
        });
        let s = sid(3);
        let (_, mut t) = r.domain(s, 9);
        let a = r.m.mem_mut().alloc(0x1000, 0x1000);
        r.translation_fault(s, 0x5000);
        r.map(&mut t, 0x5000, a, 0x1000, RW);
        // Without a TLBI after the map.
        assert_eq!(r.read(s, 0x5000, 1).is_ok(), !cache_faults);
        r.tlbi(9, 0x5000);
        assert!(r.read(s, 0x5000, 1).is_ok());
    }
}

#[test]
fn a_missing_cfgi_is_stale_configuration() {
    let mut r = Rig::new(Config::board());
    let s = sid(0xc100);
    r.m.mem_mut().back(0, 0x1000);
    r.set_ste(s, Ste::Bypass);
    assert!(r.read(s, 0, 1).is_ok());
    // Rewritten without CFGI_STE: still bypassing.
    r.write_ste(s, Ste::Abort);
    assert!(r.read(s, 0, 1).is_ok());
    r.cmd(Command::CfgiSte { sid: s, leaf: true });
    r.sync();
    assert_eq!(r.read(s, 0, 1), Err(Termination::SteAbort));

    // A prefetched STE is just as stale.
    r.write_ste(s, Ste::Bypass);
    r.cmd(Command::CfgiSte { sid: s, leaf: true });
    r.cmd(Command::PrefetchConfig { sid: s });
    r.sync();
    r.write_ste(s, Ste::Abort);
    assert!(r.read(s, 0, 1).is_ok());

    // A CD changed without CFGI_CD keeps its old tables.
    let s = sid(4);
    let (cd_pa, mut t1) = r.domain(s, 1);
    let mut t2 = PageTable::<Smmu>::new(r.m.mem_mut(), PageSize::Size4K).unwrap();
    let a = r.m.mem_mut().alloc(0x2000, 0x1000);
    r.map(&mut t1, 0x5000, a, 0x1000, RW);
    r.map(&mut t2, 0x5000, a + 0x1000, 0x1000, RW);
    let pa_of = |r: &mut Rig| match r.m.translate(s, Iova::new(0x5000), Access::READ) {
        Ok(Translation::Mapped { pa, .. }) => pa,
        other => panic!("{other:?}"),
    };
    assert_eq!(pa_of(&mut r), a);
    r.write_cd(
        cd_pa,
        ContextDescriptor::new(Asid(2), PhysAddr::new(t2.root().get()), Oas::Bits48, true),
    );
    r.cmd(Command::CfgiSte { sid: s, leaf: true });
    r.sync();
    assert_eq!(pa_of(&mut r), a, "CFGI_STE does not invalidate the CD");
    r.cmd(Command::CfgiCdAll { sid: s });
    r.sync();
    assert_eq!(pa_of(&mut r), a + 0x1000);

    // CFGI_ALL clears everything; a range only its StreamIDs.
    r.write_ste(sid(4), Ste::Abort);
    r.write_ste(sid(0xc100), Ste::Bypass);
    r.cmd(Command::CfgiSteRange {
        sid: sid(0xc100),
        range: 7,
    });
    r.sync();
    assert!(r.read(sid(0xc100), 0, 1).is_ok());
    assert!(
        r.m.translate(sid(4), Iova::new(0x5000), Access::READ)
            .is_ok()
    );
    r.cmd(Command::cfgi_all());
    r.sync();
    assert_eq!(
        r.m.translate(sid(4), Iova::new(0x5000), Access::READ),
        Err(Termination::SteAbort)
    );
}

#[test]
fn invalid_configuration_caching_knob() {
    for strict in [true, false] {
        let mut r = Rig::new(Config {
            cache_invalid_config: strict,
            ..Config::board()
        });
        r.m.mem_mut().back(0, 0x1000);
        let s = sid(0x40);
        r.ste_addr(s);
        assert!(r.read(s, 0, 1).is_err());
        r.write_ste(s, Ste::Bypass);
        // Without a CFGI after making it valid.
        assert_eq!(r.read(s, 0, 1).is_ok(), !strict);
    }
}

#[test]
fn cd_flags_and_errors() {
    let mut r = Rig::new(Config::board());
    let s = sid(3);
    let (cd_pa, mut t) = r.domain(s, 9);
    let root = t.root().get();
    // A = 0: a fault reads zero and drops writes, still recorded (R).
    let quiet = ContextDescriptor {
        abort: false,
        ..ContextDescriptor::new(Asid(9), PhysAddr::new(root), Oas::Bits48, true)
    };
    r.write_cd(cd_pa, quiet);
    r.cmd(Command::CfgiCd {
        sid: s,
        ssid: 0,
        leaf: true,
    });
    r.sync();
    r.events();
    assert_eq!(r.read(s, 0x9000, 4), Ok(std::vec![0; 4]));
    assert_eq!(r.m.dma_write(s, Iova::new(0x9000), b"lost"), Ok(()));
    assert_eq!(r.events().len(), 2);
    // R = 0: nothing recorded.
    r.write_cd(
        cd_pa,
        ContextDescriptor {
            record: false,
            ..quiet
        },
    );
    r.cmd(Command::CfgiCdAll { sid: s });
    r.sync();
    assert_eq!(r.read(s, 0x9000, 4), Ok(std::vec![0; 4]));
    assert!(r.events().is_empty());
    // An invalid CD: C_BAD_CD.
    r.m.mem_mut().write_u64(cd_pa, 0);
    r.cmd(Command::CfgiCdAll { sid: s });
    r.sync();
    assert_eq!(r.read(s, 0x9000, 1), Err(Termination::Fault(Record::BadCd)));
    // A CD that is not there: F_CD_FETCH.
    r.set_ste(
        s,
        Ste::Stage1(Stage1 {
            cd: PhysAddr::new(0x7_0000_0040),
            cd_attrs: MemAttrs::COHERENT,
            stall_disable: false,
        }),
    );
    r.cmd(Command::CfgiCdAll { sid: s });
    r.sync();
    r.events();
    assert_eq!(
        r.read(s, 0x9000, 1),
        Err(Termination::Fault(Record::CdFetch {
            fetch: PhysAddr::new(0x7_0000_0040)
        }))
    );
    assert_eq!(single(&r.events()).record.code(), code::F_CD_FETCH);
    // Tables that are not there: F_WALK_EABT at the root's entry.
    let s = sid(5);
    let cd2 = r.m.mem_mut().alloc(64, 64);
    r.write_cd(
        cd2,
        ContextDescriptor::new(Asid(11), PhysAddr::new(0x7_0000_0000), Oas::Bits48, true),
    );
    r.set_ste(
        s,
        Ste::Stage1(Stage1 {
            cd: PhysAddr::new(cd2),
            cd_attrs: MemAttrs::COHERENT,
            stall_disable: false,
        }),
    );
    assert!(matches!(
        r.read(s, 0x80_0000_0000, 1),
        Err(Termination::Fault(Record::WalkEabt { fetch, .. })) if fetch.get() == 0x7_0000_0008
    ));
    let _ = &mut t;
}

#[test]
fn access_flag_and_privilege() {
    let mut r = Rig::new(Config::board());
    let s = sid(3);
    let (cd_pa, mut t) = r.domain(s, 9);
    let a = r.m.mem_mut().alloc(0x1000, 0x1000);
    r.map(&mut t, 0x5000, a, 0x1000, RW);
    let leaf_at = |r: &Rig| {
        let mut table = t.root().get();
        for lvl in (2..=4).rev() {
            let idx = (0x5000u64 >> span_shift(lvl)) & 0x1ff;
            table = r.m.mem().read_u64(table + idx * 8).unwrap() & 0xffff_ffff_f000;
        }
        table + 5 * 8
    };
    let at = leaf_at(&r);
    let e = r.m.mem().read_u64(at).unwrap();
    // AF clear: F_ACCESS, and not cached.
    r.m.mem_mut().write_u64(at, e & !(1 << 10));
    assert!(matches!(
        r.read(s, 0x5000, 1),
        Err(Termination::Fault(Record::Access(_)))
    ));
    assert_eq!(r.m.tlb_len(), 0);
    // AFFD: no access flag faults.
    r.write_cd(
        cd_pa,
        ContextDescriptor {
            affd: true,
            ..ContextDescriptor::new(Asid(9), PhysAddr::new(t.root().get()), Oas::Bits48, true)
        },
    );
    r.cmd(Command::CfgiCdAll { sid: s });
    r.sync();
    assert!(r.read(s, 0x5000, 1).is_ok());
    // EL1-only (AP[1] clear): an unprivileged device access faults; a
    // privileged one does not.
    r.m.mem_mut().write_u64(at, e & !(1 << 6));
    r.tlbi(9, 0x5000);
    assert!(matches!(
        r.read(s, 0x5000, 1),
        Err(Termination::Fault(Record::Permission(_)))
    ));
    let privileged = Access {
        privileged: true,
        ..Access::WRITE
    };
    assert!(r.m.translate(s, Iova::new(0x5000), privileged).is_ok());
    // Instruction fetches from execute-never pages.
    r.m.mem_mut().write_u64(at, e);
    r.tlbi(9, 0x5000);
    let fetch = Access {
        instruction: true,
        ..Access::READ
    };
    assert!(matches!(
        r.m.translate(s, Iova::new(0x5000), fetch),
        Err(Termination::Fault(Record::Permission(_)))
    ));
}

#[test]
fn three_level_walk() {
    // T0SZ 25: a 39-bit space whose first table is the 48-bit tables'
    // level-1 table under root entry 0.
    let mut r = Rig::new(Config::board());
    let s = sid(3);
    let (cd_pa, mut t) = r.domain(s, 9);
    let a = r.m.mem_mut().alloc(0x1000, 0x1000);
    r.map(&mut t, 0x12_3456_7000, a, 0x1000, RW);
    let l1 = r.m.mem().read_u64(t.root().get()).unwrap() & 0xffff_ffff_f000;
    r.write_cd(
        cd_pa,
        ContextDescriptor {
            t0sz: 25,
            ..ContextDescriptor::new(Asid(9), PhysAddr::new(l1), Oas::Bits48, true)
        },
    );
    r.cmd(Command::CfgiCdAll { sid: s });
    r.cmd(Command::TlbiNhAsid { asid: Asid(9) });
    r.sync();
    assert!(matches!(
        r.m.translate(s, Iova::new(0x12_3456_7abc), Access::READ),
        Ok(Translation::Mapped { pa, .. }) if pa == a + 0xabc
    ));
    assert!(matches!(
        r.m.translate(s, Iova::new(1 << 39), Access::READ),
        Err(Termination::Fault(Record::Translation(f))) if f.class == FaultClass::In
    ));
}

#[test]
fn command_errors_stop_the_queue() {
    let mut r = Rig::new(Config::board());
    r.m.take_irqs();
    let q = r.cmdq();
    // An illegal opcode, then a sync behind it.
    let bad_at = q.pos(r.m.read32(off::CMDQ_PROD));
    let at = r.cmdq + q.offset(bad_at, CMD_BYTES);
    r.m.mem_mut().write_u64(at, 0x7f);
    r.m.mem_mut().write_u64(at + 8, 0);
    r.m.write32(off::CMDQ_PROD, q.next(bad_at).get());
    let syncs = r.m.stats().syncs;
    r.cmd(Command::Sync(SyncCompletion::Sev));
    let cons = r.m.read32(off::CMDQ_CONS);
    assert_eq!(CmdError::from_cons(cons), CmdError::Illegal);
    assert_eq!(q.pos(cons), bad_at);
    assert_eq!(
        gerror::active(r.m.read32(off::GERROR), r.m.read32(off::GERRORN)),
        gerror::CMDQ_ERR
    );
    assert_eq!(r.m.take_irqs().gerror, 1);
    assert_eq!(r.m.stats().syncs, syncs);
    // The design's recovery: a CMD_SYNC in its place, then acknowledge.
    let nop = Command::Sync(SyncCompletion::None).encode();
    r.m.mem_mut().write_u64(at, nop[0]);
    r.m.mem_mut().write_u64(at + 8, nop[1]);
    r.m.write32(off::GERRORN, r.m.read32(off::GERROR));
    assert_eq!(r.m.stats().syncs, syncs + 2);
    assert!(q.is_empty(
        q.pos(r.m.read32(off::CMDQ_PROD)),
        q.pos(r.m.read32(off::CMDQ_CONS))
    ));
    // A command the model does not implement is illegal too, and noted.
    r.cmd(Command::TlbiNhAll);
    let bad_at = q.pos(r.m.read32(off::CMDQ_PROD));
    let at = r.cmdq + q.offset(bad_at, CMD_BYTES);
    r.m.mem_mut()
        .write_u64(at, u64::from(crate::cmd::op::ATC_INV));
    r.m.write32(off::CMDQ_PROD, q.next(bad_at).get());
    assert_eq!(
        CmdError::from_cons(r.m.read32(off::CMDQ_CONS)),
        CmdError::Illegal
    );
    assert_eq!(
        r.m.violations(),
        [Violation::UnsupportedCommand(crate::cmd::op::ATC_INV)]
    );
}

#[test]
fn command_fetch_abort_and_msi_abort() {
    let mut r = Rig::new(Config::board());
    // The sync's MSI address is not memory.
    r.cmd(Command::Sync(SyncCompletion::Msi {
        addr: PhysAddr::new(0x7_0000_0000),
        data: 1,
    }));
    assert_eq!(
        gerror::active(r.m.read32(off::GERROR), r.m.read32(off::GERRORN)),
        gerror::MSI_CMDQ_ABT_ERR
    );
    r.m.write32(off::GERRORN, r.m.read32(off::GERROR));
    // The queue's memory goes away: CERROR_ABT.
    let base = r.cmdq;
    r.m.mem_mut().unback(base, CMD_BYTES << CMDQ_LOG2);
    let q = r.cmdq();
    let prod = q.pos(r.m.read32(off::CMDQ_PROD));
    r.m.write32(off::CMDQ_PROD, q.next(prod).get());
    assert_eq!(
        CmdError::from_cons(r.m.read32(off::CMDQ_CONS)),
        CmdError::Abort
    );
}

#[test]
fn queue_wraps_and_sync_forms() {
    let mut r = Rig::new(Config::board());
    r.m.clear_executed();
    let sevs = r.m.stats().sevs;
    // Five times around a 64-entry queue.
    for i in 0..320u32 {
        if i % 3 == 0 {
            r.cmd(Command::Sync(SyncCompletion::Sev));
        } else {
            r.cmd(Command::CfgiSte {
                sid: sid(i),
                leaf: true,
            });
        }
    }
    r.sync();
    assert_eq!(r.m.executed().len(), 321);
    assert_eq!(
        r.m.executed()[1],
        Command::CfgiSte {
            sid: sid(1),
            leaf: true
        }
    );
    assert_eq!(r.m.stats().sevs - sevs, 107);
    let cons = r.m.read32(off::CMDQ_CONS);
    assert_eq!(cons, r.m.read32(off::CMDQ_PROD));
    // 321 + the three of Rig::new: 324 = 5 * 64 + 4, wrap bit set.
    assert_eq!(cons, (1 << CMDQ_LOG2) | 4);
}

#[test]
fn deferred_consumption() {
    let mut r = Rig::new(Config::board());
    r.m.cfg.auto_consume = false;
    let before = r.m.stats().commands;
    r.cmd(Command::TlbiNhAll);
    r.cmd(Command::Sync(SyncCompletion::None));
    assert_eq!(r.m.stats().commands, before);
    let q = r.cmdq();
    let sync = q.advance(q.pos(r.m.read32(off::CMDQ_PROD)), (1 << CMDQ_LOG2) * 2 - 1);
    let prod = q.pos(r.m.read32(off::CMDQ_PROD));
    assert!(!q.consumed(sync, prod, q.pos(r.m.read32(off::CMDQ_CONS))));
    r.m.consume();
    assert_eq!(r.m.stats().commands, before + 2);
    assert!(q.consumed(sync, prod, q.pos(r.m.read32(off::CMDQ_CONS))));
}

#[test]
fn event_queue_overflow() {
    let mut r = Rig::new(Config::board());
    r.m.take_irqs();
    // Ten faults into eight slots.
    for i in 0..10 {
        let _ = r.m.translate(sid(0x100 + i), Iova::ZERO, Access::READ);
    }
    assert_eq!(r.m.take_irqs().event, 8);
    assert_eq!(r.m.stats().events_lost, 2);
    let prod = r.m.read32(off::EVENTQ_PROD);
    assert_ne!(prod & OVERFLOW, 0, "overflow flagged");
    let ev = r.events();
    assert_eq!(ev.len(), 8);
    assert_eq!(ev[0].sid, sid(0x100));
    assert_eq!(ev[7].sid, sid(0x107));
    // Acknowledge: CONS.OVACKFLG follows PROD.OVFLG.
    let cons = r.m.read32(off::EVENTQ_CONS);
    r.m.write32(off::EVENTQ_CONS, (cons & !OVERFLOW) | (prod & OVERFLOW));
    let _ = r.m.translate(sid(0x200), Iova::ZERO, Access::READ);
    let ev = r.events();
    assert_eq!(single(&ev).sid, sid(0x200));
    assert_eq!(
        (r.m.read32(off::EVENTQ_PROD) ^ r.m.read32(off::EVENTQ_CONS)) & OVERFLOW,
        0
    );
    // No interrupt when disabled; no event when the queue is off.
    r.m.write32(off::IRQ_CTRL, irq_ctrl::GERROR_IRQEN);
    r.m.take_irqs();
    let _ = r.m.translate(sid(0x201), Iova::ZERO, Access::READ);
    assert_eq!(r.m.take_irqs().event, 0);
    assert_eq!(r.events().len(), 1);
    r.m.write32(off::CR0, cr0::CMDQEN | cr0::SMMUEN);
    let lost = r.m.stats().events_lost;
    let _ = r.m.translate(sid(0x202), Iova::ZERO, Access::READ);
    assert_eq!(r.m.stats().events_lost, lost + 1);
}

#[test]
fn forbidden_register_writes() {
    let mut r = Rig::new(Config::board());
    let base = r.m.read64(off::CMDQ_BASE);
    r.m.write64(off::CMDQ_BASE, 0);
    r.m.write64(off::STRTAB_BASE, 0);
    r.m.write32(off::STRTAB_BASE_CFG, 0);
    r.m.write32(off::EVENTQ_PROD, 0);
    r.m.write32(off::IDR0, 0);
    r.m.write32(off::GERROR, 1);
    assert_eq!(r.m.read64(off::CMDQ_BASE), base);
    assert_eq!(
        r.m.violations(),
        [
            Violation::WhileEnabled(off::CMDQ_BASE),
            Violation::WhileEnabled(off::CMDQ_BASE + 4),
            Violation::WhileEnabled(off::STRTAB_BASE),
            Violation::WhileEnabled(off::STRTAB_BASE + 4),
            Violation::WhileEnabled(off::STRTAB_BASE_CFG),
            Violation::WhileEnabled(off::EVENTQ_PROD),
            Violation::ReadOnly(off::IDR0),
            Violation::ReadOnly(off::GERROR),
        ]
    );
    // A two-level table on an SMMU without them.
    let mut m = Model::new(Config::qemu(), SparseMemory::new(0));
    let mut cfg = Config::qemu();
    cfg.idr0.st_level = StLevel::Linear;
    m.cfg = cfg;
    m.write32(
        off::STRTAB_BASE_CFG,
        StrtabCfg::TwoLevel {
            log2size: 16,
            split: 8,
        }
        .encode(),
    );
    m.write32(off::CR0, cr0::SMMUEN);
    assert_eq!(m.violations(), [Violation::BadStrtabCfg]);
    // A queue larger than IDR1 allows.
    let mut m = Model::new(Config::qemu(), SparseMemory::new(0));
    m.write64(
        off::CMDQ_BASE,
        QueueBase {
            addr: PhysAddr::new(0),
            log2size: 12,
            alloc_hint: false,
        }
        .encode(),
    );
    m.write32(off::CMDQ_PROD, 1);
    m.write32(off::CR0, cr0::CMDQEN);
    assert_eq!(m.violations()[0], Violation::QueueTooLarge(off::CMDQ_BASE));
    // The ID registers as configured.
    assert_eq!(Idr1::decode(r.m.read32(off::IDR1)).sidsize, 32);
    assert_eq!(Idr5::decode(r.m.read32(off::IDR5)).oas, Oas::Bits40);
    assert_eq!(
        crate::regs::Features::decode(
            r.m.read32(off::IDR0),
            r.m.read32(off::IDR1),
            r.m.read32(off::IDR3),
            r.m.read32(off::IDR5),
            r.m.read32(off::IIDR),
            false
        )
        .check(),
        Ok(())
    );
}

/// Random maps and unmaps with the strict-unmap discipline (clear, TLBI,
/// sync before reuse) against a reference map: every probe agrees, so no
/// unmapped page is reachable.
#[test]
fn random_strict_unmap_against_reference() {
    let mut rng = Rng(0xfeed_face);
    let mut r = Rig::new(Config::board());
    let streams = [(sid(0xc100), 1u16), (sid(0xc200), 2), (sid(0x30), 3)];
    type Domain = (StreamId, u16, PageTable<Smmu>, BTreeMap<u64, u64>);
    let mut domains: Vec<Domain> = streams
        .iter()
        .map(|&(s, asid)| {
            let (_, t) = r.domain(s, asid);
            (s, asid, t, BTreeMap::new())
        })
        .collect();
    let frames = r.m.mem_mut().alloc(64 * PAGE, PAGE);
    for _ in 0..3000 {
        let d = usize::try_from(rng.below(3)).unwrap();
        let page = rng.below(256) | (rng.below(4) << 18);
        let va = page * PAGE;
        let (s, asid) = (domains[d].0, domains[d].1);
        if rng.chance(3) {
            if domains[d].3.remove(&page).is_some() {
                let mut t = core::mem::replace(&mut domains[d].2, PageTable::from_root(pa(0)));
                r.unmap(&mut t, va, PAGE);
                domains[d].2 = t;
                r.tlbi(asid, va);
            }
        } else if !domains[d].3.contains_key(&page) {
            let f = frames + rng.below(64) * PAGE;
            let mut t = core::mem::replace(&mut domains[d].2, PageTable::from_root(pa(0)));
            r.map(&mut t, va, f, PAGE, RW);
            domains[d].2 = t;
            domains[d].3.insert(page, f);
        }
        // Probe a random page of a random domain.
        let d = usize::try_from(rng.below(3)).unwrap();
        let page = rng.below(256) | (rng.below(4) << 18);
        let got =
            r.m.translate(domains[d].0, Iova::new(page * PAGE + 8), Access::WRITE);
        match domains[d].3.get(&page) {
            Some(&f) => assert_eq!(
                got,
                Ok(Translation::Mapped {
                    pa: f + 8,
                    size: PageSize::Size4K,
                    attrs: RW
                })
            ),
            None => assert!(matches!(
                got,
                Err(Termination::Fault(Record::Translation(_)))
            )),
        }
        let _ = s;
    }
    assert!(r.m.stats().tlb_hits > 100);
    assert!(r.m.violations().is_empty());
}
