// SPDX-License-Identifier: BSD-2-Clause
//! GPU-control sequences for architectures 10 to 13 (specification 03): soft reset, L2 power,
//! cache flushes, and the interrupt blocks. They poll `INT_RAWSTAT` rather than wait for the
//! interrupt, which 03 §3.2 allows; a driver that takes the interrupt can still use them at
//! attach, before its handler is installed.

use crate::regs::{self, Regs};
use crate::{Delay, Error};

/// Polls `done` every `interval_us` for at most `timeout_us`.
fn poll(
    d: &mut dyn Delay,
    interval_us: u32,
    timeout_us: u32,
    what: &'static str,
    mut done: impl FnMut() -> bool,
) -> Result<(), Error> {
    let mut waited = 0;
    loop {
        if done() {
            return Ok(());
        }
        if waited >= timeout_us {
            return Err(Error::Timeout(what));
        }
        d.delay_us(interval_us);
        waited += interval_us;
    }
}

/// Masks every event of an interrupt block and drops what is latched (03 §3.8.3, before a
/// handler is installed).
pub fn mask_block(r: &dyn Regs, block: usize) {
    r.write32(block + regs::INT_MASK, 0);
    r.write32(block + regs::INT_CLEAR, u32::MAX);
}

/// Unmasks `events` of a block, dropping anything latched while it was masked (03 §3.8.3).
pub fn unmask_block(r: &dyn Regs, block: usize, events: u32) {
    r.write32(block + regs::INT_CLEAR, events);
    r.write32(block + regs::INT_MASK, events);
}

/// Soft reset (03 §3.3.1): clears a stale completion, resets, and waits up to 100 ms for
/// `RESET_COMPLETED` in the raw status. The interrupt masks of every block are rewritten to 0
/// afterwards: whether a reset clears them is not established (03 §3.3.1, open question).
pub fn soft_reset(r: &dyn Regs, d: &mut dyn Delay) -> Result<(), Error> {
    r.write32(regs::GPU_INT + regs::INT_CLEAR, regs::INT_RESET_COMPLETED);
    r.write32(regs::GPU_CMD, regs::CMD_SOFT_RESET);
    poll(d, 100, 100_000, "soft reset", || {
        r.read32(regs::GPU_INT + regs::INT_RAWSTAT) & regs::INT_RESET_COMPLETED != 0
    })?;
    r.write32(regs::GPU_INT + regs::INT_CLEAR, regs::INT_RESET_COMPLETED);
    for block in [regs::GPU_INT, regs::JOB_INT, regs::MMU_INT] {
        mask_block(r, block);
    }
    Ok(())
}

/// Powers the first L2 slice (03 §3.4.1): the coherency protocol first (it must precede the
/// power-up), then `L2_PWRON` once no transition is under way, then waits for `L2_READY` bit 0
/// (the specification's reading of the intended condition, not the subset test it warns of).
pub fn l2_power_up(r: &dyn Regs, d: &mut dyn Delay, coherency: u32) -> Result<(), Error> {
    r.write32(regs::COHERENCY_PROTOCOL, coherency);
    poll(d, 100, 20_000, "L2 transition before power-up", || {
        r.read64(regs::L2_PWRTRANS) & 1 == 0
    })?;
    r.write64(regs::L2_PWRON, 1);
    poll(d, 100, 20_000, "L2 ready", || {
        r.read64(regs::L2_READY) & 1 == 1
    })
}

/// Powers every L2 slice down (03 §3.4.2). The MCU must be halted or disabled first.
pub fn l2_power_down(r: &dyn Regs, d: &mut dyn Delay) -> Result<(), Error> {
    let mask = r.read64(regs::L2_PRESENT);
    poll(d, 100, 20_000, "L2 transition before power-down", || {
        r.read64(regs::L2_PWRTRANS) & mask == 0
    })?;
    r.write64(regs::L2_PWROFF, mask);
    poll(d, 100, 20_000, "L2 power-down", || {
        r.read64(regs::L2_PWRTRANS) & mask == 0
    })
}

/// A cache flush's payload (02 §2.3.3): per cache, bit 0 cleans, bit 1 invalidates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Flush {
    pub l2: u8,
    pub lsc: u8,
    pub other: u8,
}

impl Flush {
    /// Clean and invalidate L2 and the load/store cache, invalidate the rest: after page-table
    /// changes (03 §3.6.2).
    pub const PAGE_TABLES: Self = Self {
        l2: 3,
        lsc: 3,
        other: 2,
    };
    /// Clean L2 and the load/store cache: after groups are suspended (03 §3.6.2).
    pub const SUSPEND: Self = Self {
        l2: 1,
        lsc: 1,
        other: 0,
    };

    pub fn command(self) -> u32 {
        let payload =
            u32::from(self.l2 & 3) | u32::from(self.lsc & 3) << 4 | u32::from(self.other & 3) << 8;
        regs::CMD_FLUSH_CACHES | payload << 8
    }
}

/// Flushes caches and waits up to 100 ms for `CLEAN_CACHES_COMPLETED` (03 §3.6.1). A completion
/// already latched means an earlier flush was never acknowledged: that is refused, as the
/// specification has a flush still pending fail at once.
pub fn flush(r: &dyn Regs, d: &mut dyn Delay, f: Flush) -> Result<(), Error> {
    if r.read32(regs::GPU_INT + regs::INT_RAWSTAT) & regs::INT_CLEAN_CACHES_COMPLETED != 0 {
        return Err(Error::FlushPending);
    }
    r.write32(regs::GPU_CMD, f.command());
    poll(d, 10, 100_000, "cache flush", || {
        r.read32(regs::GPU_INT + regs::INT_RAWSTAT) & regs::INT_CLEAN_CACHES_COMPLETED != 0
    })?;
    r.write32(
        regs::GPU_INT + regs::INT_CLEAR,
        regs::INT_CLEAN_CACHES_COMPLETED,
    );
    Ok(())
}
