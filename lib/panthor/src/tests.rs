// SPDX-License-Identifier: BSD-2-Clause

use crate::control::{self, Flush};
use crate::id::{self, GpuId, Power};
use crate::model::{Model, Ticker};
use crate::regs::{self, Regs};
use crate::{Error, sky1};

#[test]
fn the_board_gpu_is_identified_as_freebsd_named_it() {
    let m = Model::sky1();
    let info = id::probe(&m).unwrap();
    let g = info.gpu_id;
    // FreeBSD prints `id` as GPU_ID's upper half: 0xc870.
    assert_eq!(g.0 >> 16, 0xC870);
    assert_eq!(
        (
            g.arch_major(),
            g.arch_minor(),
            g.arch_rev(),
            g.product_major()
        ),
        (12, 8, 7, 0)
    );
    assert_eq!(
        (g.version_major(), g.version_minor(), g.version_status()),
        (0, 0, 8)
    );
    assert_eq!(info.power(), Power::Legacy);
    assert_eq!(info.shader_cores(), 10);
    assert_eq!(info.product(), "Mali-G720-Immortalis");
    assert_eq!((info.va_bits(), info.pa_bits()), (48, 40));
    assert_eq!(info.l2_line_log2(), 6);
    assert_eq!(info.as_present, 0xFF);
}

#[test]
fn product_names_follow_cores_and_ray_tracing() {
    let m = Model::sky1();
    let mut info = id::probe(&m).unwrap();
    info.gpu_features = 0;
    assert_eq!(info.product(), "Mali-G720");
    info.shader_present = 0b1111;
    assert_eq!(info.product(), "Mali-G620");
    info.gpu_id = GpuId(0xA007_0000);
    assert_eq!(info.product(), "Mali-G610");
}

#[test]
fn an_absent_or_unknown_gpu_is_refused() {
    let m = Model::sky1();
    m.set(regs::GPU_ID, 0);
    assert_eq!(id::probe(&m), Err(Error::NoGpu));
    m.set(regs::GPU_ID, 0x9000_0000);
    assert_eq!(id::probe(&m), Err(Error::UnsupportedArch(9)));
    // Architecture 14's present masks are in the PWR block, which is not built.
    m.set(regs::GPU_ID, 0xE000_0000);
    assert_eq!(id::probe(&m), Err(Error::UnsupportedArch(14)));
    m.set(regs::GPU_ID, 0xC870_0008);
    m.set(regs::MMU_FEATURES, 0);
    assert!(matches!(id::probe(&m), Err(Error::Bad(_))));
}

#[test]
fn counters_are_read_high_low_high() {
    // A carry between the reads of the halves: the model changes the counter on the first read
    // of its high word.
    struct Carry(core::cell::Cell<u32>);
    impl Regs for Carry {
        fn read32(&self, off: usize) -> u32 {
            let n = self.0.get();
            self.0.set(n + 1);
            match (off, n) {
                (0x94, 0) => 1, // high, before the carry
                (0x90, 1) => 0, // low, after the carry
                (0x94, 2) => 2, // high, after: differs, so again
                (0x94, _) => 2,
                (0x90, _) => 5,
                _ => 0,
            }
        }
        fn write32(&self, _: usize, _: u32) {}
    }
    let c = Carry(core::cell::Cell::new(0));
    assert_eq!(c.read_counter(regs::CYCLE_COUNT), 2 << 32 | 5);
}

#[test]
fn soft_reset_clears_a_stale_completion_first_and_waits() {
    let m = Model::sky1();
    // A completion left from before: it must not end the wait.
    m.set(regs::GPU_INT + regs::INT_RAWSTAT, regs::INT_RESET_COMPLETED);
    control::soft_reset(&m, &mut Ticker(&m)).unwrap();
    let w = m.writes.borrow();
    assert_eq!(
        w[0],
        (regs::GPU_INT + regs::INT_CLEAR, regs::INT_RESET_COMPLETED)
    );
    assert_eq!(w[1], (regs::GPU_CMD, regs::CMD_SOFT_RESET));
    assert!(m.now() >= m.latency, "returned before the reset completed");
    // Every block is masked afterwards.
    for b in [regs::GPU_INT, regs::JOB_INT, regs::MMU_INT] {
        assert_eq!(m.get(b + regs::INT_MASK), 0);
    }
    assert_eq!(m.get(regs::GPU_INT + regs::INT_RAWSTAT), 0);
}

#[test]
fn a_reset_that_never_completes_times_out() {
    let m = Model::sky1();
    m.dead.set(true);
    assert_eq!(
        control::soft_reset(&m, &mut Ticker(&m)),
        Err(Error::Timeout("soft reset"))
    );
    assert!(m.now() >= 100_000);
}

#[test]
fn the_l2_powers_up_after_the_coherency_protocol_is_set() {
    let m = Model::sky1();
    let info = id::probe(&m).unwrap();
    let proto = sky1::coherency_protocol(&info);
    // COHERENCY_FEATURES 0x21 has the shareable-cache bit: ACE-Lite with bit 5.
    assert_eq!(proto, 0x20);
    control::l2_power_up(&m, &mut Ticker(&m), proto).unwrap();
    let w = m.writes.borrow();
    let at = |off| w.iter().position(|&(o, _)| o == off).unwrap();
    assert!(at(regs::COHERENCY_PROTOCOL) < at(regs::L2_PWRON));
    assert_eq!(m.get(regs::COHERENCY_PROTOCOL), 0x20);
    assert_eq!(m.get(regs::L2_READY) & 1, 1);
}

#[test]
fn the_l2_power_up_waits_out_a_transition_already_under_way() {
    let m = Model::sky1();
    m.set(regs::L2_PWRTRANS, 1);
    m.dead.set(true);
    assert_eq!(
        control::l2_power_up(&m, &mut Ticker(&m), 0),
        Err(Error::Timeout("L2 transition before power-up"))
    );
    assert!(!m.writes.borrow().iter().any(|&(o, _)| o == regs::L2_PWRON));
}

#[test]
fn l2_power_down_takes_every_slice() {
    let m = Model::sky1();
    control::l2_power_up(&m, &mut Ticker(&m), 0).unwrap();
    control::l2_power_down(&m, &mut Ticker(&m)).unwrap();
    assert!(m.writes.borrow().contains(&(regs::L2_PWROFF, 1)));
    assert_eq!(m.get(regs::L2_READY), 0);
}

#[test]
fn flush_commands_are_encoded_as_the_specification_shows() {
    // 02 §2.3.3's example: clean and invalidate L2 and LSC, others alone.
    let f = Flush {
        l2: 3,
        lsc: 3,
        other: 0,
    };
    assert_eq!(f.command(), 0x3304);
    // 03 §3.6.2.
    assert_eq!(Flush::PAGE_TABLES.command(), 0x0002_3304);
    assert_eq!(Flush::SUSPEND.command(), 0x0000_1104);
}

#[test]
fn a_flush_waits_for_completion_and_refuses_a_pending_one() {
    let m = Model::sky1();
    control::flush(&m, &mut Ticker(&m), Flush::PAGE_TABLES).unwrap();
    assert_eq!(m.get(regs::GPU_INT + regs::INT_RAWSTAT), 0);
    m.set(
        regs::GPU_INT + regs::INT_RAWSTAT,
        regs::INT_CLEAN_CACHES_COMPLETED,
    );
    assert_eq!(
        control::flush(&m, &mut Ticker(&m), Flush::SUSPEND),
        Err(Error::FlushPending)
    );
}

#[test]
fn unmasking_drops_what_was_latched_first() {
    let m = Model::sky1();
    m.set(regs::GPU_INT + regs::INT_RAWSTAT, regs::INT_FAULT);
    control::unmask_block(&m, regs::GPU_INT, regs::GPU_EVENTS);
    let w = m.writes.borrow();
    assert_eq!(w[0], (regs::GPU_INT + regs::INT_CLEAR, regs::GPU_EVENTS));
    assert_eq!(w[1], (regs::GPU_INT + regs::INT_MASK, regs::GPU_EVENTS));
    assert_eq!(m.read32(regs::GPU_INT + regs::INT_STAT), 0);
}

#[test]
fn harvesting_removes_fused_cores() {
    // No fuses: all ten cores.
    assert_eq!(sky1::harvested(0x0055_0555, 0), 0x0055_0555);
    // A fuse bit at position 2 (field bit 6 in the register) removes core 2.
    assert_eq!(sky1::harvested(0x0055_0555, 1 << 6), 0x0055_0551);
    // Bits outside [27:4] are ignored.
    assert_eq!(sky1::harvested(0x0055_0555, 0xF000_000F), 0x0055_0555);
    // A present mask wider than the full part is cut to it.
    assert_eq!(sky1::harvested(0xFFFF_FFFF, 0), 0x0055_0555);
}

#[test]
fn the_firmware_pairs_pack_into_the_specified_alloc_words() {
    // The board's DSDT `int_id_override` (acpidump-dt.txt, the GPU's `IIOR`).
    let pairs = [
        (0x02, 0x23),
        (0x04, 0x23),
        (0x10, 0x22),
        (0x11, 0x32),
        (0x12, 0x52),
        (0x15, 0x32),
        (0x16, 0x52),
        (0x18, 0x22),
        (0x1C, 0x32),
    ];
    assert_eq!(sky1::sysc_alloc_from_pairs(&pairs), Some(sky1::SYSC_ALLOC));
    assert_eq!(sky1::sysc_alloc_from_pairs(&[(32, 1)]), None);
}

#[test]
fn the_system_cache_registers_are_written() {
    let m = Model::sky1();
    sky1::program_system_cache(&m);
    assert_eq!(m.get(regs::SYSC_PBHA_OVERRIDE + 12), sky1::PBHA_OVERRIDE3);
    for (n, v) in sky1::SYSC_ALLOC.iter().enumerate() {
        assert_eq!(m.get(regs::SYSC_ALLOC + 4 * n), *v);
    }
}
