// SPDX-License-Identifier: BSD-2-Clause
//! The engine sequences against a model that logs every register access:
//! the write order of specification 02, sections 3.1 to 3.4.

use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;
use std::collections::BTreeMap;

use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ev {
    Read(usize),
    Write(usize, u32),
    Delay(u32),
}

/// A GSP Falcon, as far as the sequences ask: it needs a few polls to
/// scrub, shows idle DMA at once, and halts when started.
#[derive(Default)]
struct Model {
    regs: RefCell<BTreeMap<usize, u32>>,
    log: RefCell<Vec<Ev>>,
    scrub_polls: RefCell<u32>,
    started: RefCell<bool>,
    /// What `MAILBOX0` reads back after the halt.
    status: u32,
    /// The core starts as the RISC-V one.
    riscv_core: bool,
    never_scrubs: bool,
    never_halts: bool,
}

impl Registers for Model {
    fn read32(&self, off: usize) -> u32 {
        self.log.borrow_mut().push(Ev::Read(off));
        let e = GSP;
        let r = self.regs.borrow();
        match off {
            o if o == e.base + HWCFG2 => {
                let scrubbing = self.never_scrubs || *self.scrub_polls.borrow() > 0;
                if *self.scrub_polls.borrow() > 0 {
                    *self.scrub_polls.borrow_mut() -= 1;
                }
                1 << 31 | u32::from(scrubbing) << 12
            }
            o if o == e.base + DMATRFCMD => 1 << 1,
            o if o == e.base + CPUCTL => {
                let base = r.get(&o).copied().unwrap_or(0) & !2;
                if *self.started.borrow() && !self.never_halts {
                    base | 1 << 4
                } else {
                    base
                }
            }
            o if o == e.base + MAILBOX0 && *self.started.borrow() => self.status,
            o if o == e.base2 + BCR_CTRL => {
                let v = r.get(&o).copied();
                match v {
                    Some(v) => v | 1,
                    None if self.riscv_core => 1 << 4 | 1,
                    None => 1,
                }
            }
            o => r.get(&o).copied().unwrap_or(0),
        }
    }

    fn write32(&self, off: usize, v: u32) {
        self.log.borrow_mut().push(Ev::Write(off, v));
        if off == GSP.base + ENGINE && v & 1 == 0 {
            *self.scrub_polls.borrow_mut() = 3;
        }
        if (off == GSP.base + CPUCTL || off == GSP.base + CPUCTL_ALIAS) && v & 2 != 0 {
            *self.started.borrow_mut() = true;
        }
        self.regs.borrow_mut().insert(off, v);
    }

    fn delay_us(&self, us: u32) {
        self.log.borrow_mut().push(Ev::Delay(us));
    }
}

impl Model {
    fn writes(&self) -> Vec<(usize, u32)> {
        self.log
            .borrow()
            .iter()
            .filter_map(|e| match *e {
                Ev::Write(o, v) => Some((o, v)),
                _ => None,
            })
            .collect()
    }
}

fn at(off: usize) -> usize {
    GSP.base + off
}

#[test]
fn reset_pulses_the_engine_waits_for_the_scrub_and_records_the_chip() {
    let m = Model::default();
    reset(&m, GSP, 0x1940_00a1).unwrap();
    assert_eq!(
        m.writes(),
        [(at(ENGINE), 1), (at(ENGINE), 0), (at(RM), 0x1940_00a1)]
    );
    // The scrub was polled until it finished, then once more after the
    // core selection step.
    assert_eq!(*m.scrub_polls.borrow(), 0);
}

#[test]
fn reset_selects_the_falcon_core_and_waits_for_it() {
    let m = Model {
        riscv_core: true,
        ..Model::default()
    };
    reset(&m, GSP, 0).unwrap();
    let w = m.writes();
    assert!(w.contains(&(GSP.base2 + BCR_CTRL, 0)), "{w:x?}");
    // The core switch came after the engine reset and before the record.
    let pos = |t| w.iter().position(|x| *x == t).unwrap();
    assert!(pos((at(ENGINE), 0)) < pos((GSP.base2 + BCR_CTRL, 0)));
    assert!(pos((GSP.base2 + BCR_CTRL, 0)) < pos((at(RM), 0)));
}

#[test]
fn a_scrub_that_never_ends_is_an_error_not_a_hang() {
    let m = Model {
        never_scrubs: true,
        ..Model::default()
    };
    assert_eq!(
        reset(&m, GSP, 0),
        Err(FalconError::Timeout("memory scrubbing"))
    );
    // Bounded: at most 20 ms of waiting after the pulse.
    let waited: u32 = m
        .log
        .borrow()
        .iter()
        .map(|e| if let Ev::Delay(us) = e { *us } else { 0 })
        .sum();
    assert!(waited <= 20_000 + 1_000, "{waited} us");
}

fn rom() -> Rom {
    Rom {
        pkc_data_offset: 0xb24,
        engine_id_mask: 0x400,
        ucode_id: 9,
    }
}

fn imem() -> Segment {
    Segment {
        src: 0,
        dst: 0,
        len: 512,
    }
}

fn dmem() -> Segment {
    Segment {
        src: 512,
        dst: 0,
        len: 300,
    }
}

#[test]
fn a_signed_microcode_loads_and_runs_in_the_order_of_the_specification() {
    let m = Model::default();
    run_signed(&m, GSP, 0x1_2345_6700, imem(), dmem(), rom(), 0).unwrap();
    let w = m.writes();
    let expect = [
        // Physical DMA, no context; aperture 0 is coherent system memory.
        (at(FBIF_CTL), 1 << 7),
        (at(DMACTL), 0),
        (at(FBIF_TRANSCFG0), 0b101),
        // Instruction memory: the buffer itself, source offset as the
        // framebuffer offset (it supplies the virtual tag); two blocks.
        (at(DMATRFBASE), 0x0123_4567),
        (at(DMATRFBASE1), 0),
        (at(DMATRFMOFFS), 0),
        (at(DMATRFFBOFFS), 0),
        (at(DMATRFCMD), 6 << 8 | 1 << 4 | 1 << 2),
        (at(DMATRFMOFFS), 256),
        (at(DMATRFFBOFFS), 256),
        (at(DMATRFCMD), 6 << 8 | 1 << 4 | 1 << 2),
        // Data memory: the base moves by the source offset (512), the
        // framebuffer offset is 0; two blocks (300 bytes).
        (at(DMATRFBASE), 0x0123_4569),
        (at(DMATRFBASE1), 0),
        (at(DMATRFMOFFS), 0),
        (at(DMATRFFBOFFS), 0),
        (at(DMATRFCMD), 6 << 8),
        (at(DMATRFMOFFS), 256),
        (at(DMATRFFBOFFS), 256),
        (at(DMATRFCMD), 6 << 8),
        // The boot ROM, then the start.
        (GSP.base2 + BROM_PARAADDR0, 0xb24),
        (GSP.base2 + BROM_ENGIDMASK, 0x400),
        (GSP.base2 + BROM_CURR_UCODE_ID, 9),
        (GSP.base2 + MOD_SEL, 1),
        (at(BOOTVEC), 0),
        (at(MAILBOX0), 0),
        (at(MAILBOX1), 0),
        (at(CPUCTL), 2),
    ];
    assert_eq!(w, expect);
}

#[test]
fn the_address_bits_above_forty_go_to_the_second_base_register() {
    let m = Model::default();
    run_signed(&m, GSP, 1 << 41, imem(), dmem(), rom(), 0).unwrap();
    assert!(m.writes().contains(&(at(DMATRFBASE1), 2)));
}

#[test]
fn a_microcode_that_fails_reports_its_status() {
    let m = Model {
        status: 0x1234,
        ..Model::default()
    };
    assert_eq!(
        run_signed(&m, GSP, 0x1000, imem(), dmem(), rom(), 0),
        Err(FalconError::Failed { mailbox0: 0x1234 })
    );
}

#[test]
fn a_microcode_that_never_halts_times_out() {
    let m = Model {
        never_halts: true,
        ..Model::default()
    };
    assert_eq!(
        run_signed(&m, GSP, 0x1000, imem(), dmem(), rom(), 0),
        Err(FalconError::Timeout("microcode halt"))
    );
}

#[test]
fn misaligned_buffers_are_refused_before_anything_is_written() {
    let m = Model::default();
    assert!(matches!(
        run_signed(&m, GSP, 0x1080, imem(), dmem(), rom(), 0),
        Err(FalconError::BadArgument(_))
    ));
    assert!(m.writes().is_empty());
    // A data segment that moves the base off alignment.
    let odd = Segment {
        src: 513,
        dst: 0,
        len: 16,
    };
    let m = Model::default();
    assert!(matches!(
        run_signed(&m, GSP, 0x1000, imem(), odd, rom(), 0),
        Err(FalconError::BadArgument(_))
    ));
}

#[test]
fn the_alias_register_starts_a_cpu_that_asks_for_it() {
    let m = Model::default();
    m.regs.borrow_mut().insert(at(CPUCTL), 1 << 6);
    run_signed(&m, GSP, 0x1000, imem(), dmem(), rom(), 0).unwrap();
    let w = m.writes();
    assert!(w.contains(&(at(CPUCTL_ALIAS), 2)));
    assert!(!w.contains(&(at(CPUCTL), 2)));
}

#[test]
fn state_reads_only() {
    let m = Model::default();
    let s = state(&m, GSP);
    assert_eq!(s.hwcfg2 >> 31, 1);
    assert!(m.writes().is_empty());
    assert_eq!(pad_to_block(vec![1; 257]).len(), 512);
    assert_eq!(pad_to_block(vec![]).len(), 0);
}
