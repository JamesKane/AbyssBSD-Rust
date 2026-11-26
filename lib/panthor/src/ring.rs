// SPDX-License-Identifier: BSD-2-Clause
//! The command-stream instructions the kernel writes into a queue's ring, and the sequence it
//! writes for each job (specification 07 §7.4.4, §7.4.5): flush, call the user's stream, drain
//! its scoreboards, then add one to the queue's sync object.

/// A queue's ring holds these 64-byte lines; every job is a whole number of them.
pub const LINE: usize = 8;

fn op(opcode: u64) -> u64 {
    opcode << 56
}

/// `MOV48 r, imm` (`imm` is 48 bits).
pub fn mov48(r: u8, imm: u64) -> u64 {
    op(0x01) | u64::from(r) << 48 | (imm & 0xFFFF_FFFF_FFFF)
}

/// `MOV32 r, imm`.
pub fn mov32(r: u8, imm: u32) -> u64 {
    op(0x02) | u64::from(r) << 48 | u64::from(imm)
}

/// `WAIT` on the scoreboard slots in `mask`.
pub fn wait(mask: u16) -> u64 {
    op(0x03) | u64::from(mask) << 16
}

/// `CALL` the stream at the address in pair `addr`, of the size in register `size`.
pub fn call(addr: u8, size: u8) -> u64 {
    op(0x20) | u64::from(addr) << 40 | u64::from(size) << 32
}

/// `FLUSH_CACHE2`: L2 and the load/store cache cleaned and invalidated, the others invalidated,
/// signalling scoreboard 0, with the flush-ID token in register `flush_id`.
pub fn flush_cache2(flush_id: u8) -> u64 {
    op(0x24) | u64::from(flush_id) << 40 | 0x233
}

/// `SYNC_ADD64 [addr] += value`, system scope, propagating errors.
pub fn sync_add64(addr: u8, value: u8) -> u64 {
    op(0x33) | u64::from(addr) << 40 | u64::from(value) << 32 | 1
}

/// `ERROR_BARRIER`.
pub fn error_barrier() -> u64 {
    op(0x2F)
}

/// One job's sequence (07 §7.4.4, unprofiled): eleven instructions, padded to two lines.
///
/// `work_regs` is the streams' work-register count (the kernel uses the top four), `scoreboards`
/// the scoreboard slot count, `stream` and `size` the user's stream, `sync` the GPU address of
/// the queue's 64-bit sync object, `flush_id` the token from userspace.
pub fn job(
    work_regs: u32,
    scoreboards: u32,
    stream: u64,
    size: u32,
    sync: u64,
    flush_id: u32,
) -> [u64; 2 * LINE] {
    let ra = (work_regs.saturating_sub(4) & 0xFF) as u8;
    let rv = ra.wrapping_add(2);
    let all = if scoreboards >= 16 {
        0xFFFF
    } else {
        (((1u32 << scoreboards) - 1) & 0xFFFF) as u16
    };
    let mut w = [0u64; 2 * LINE];
    let seq = [
        mov32(rv, flush_id),
        flush_cache2(rv),
        mov48(ra, stream),
        mov32(rv, size),
        wait(1),
        call(ra, rv),
        mov48(ra, sync),
        mov48(rv, 1),
        wait(all),
        sync_add64(ra, rv),
        error_barrier(),
    ];
    w[..seq.len()].copy_from_slice(&seq);
    w
}
