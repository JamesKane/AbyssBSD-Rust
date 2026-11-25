// SPDX-License-Identifier: BSD-2-Clause
//! The graphics ring (stages R4.2 to R4.4 of docs/GRAPHICS.md §16): the
//! golden settings, the shader memory configuration, the clear-state buffer,
//! graphics ring 0 programmed directly in video memory, the processors
//! released, the clear state submitted, and the ring test.

use alloc::sync::Arc;

use amdgpu::gc::{self, Ring};
use amdgpu::pm4::Packets;
use amdgpu::psp::Memory;
use amdgpu::regs::Window;
use vgapci::Aperture;

use crate::amd::Bar5;

/// Where the ring's memory is (a BAR0 offset, which is also the video memory
/// offset): the ring, 64 KiB; then the read-pointer report, the write-pointer
/// poll, a fence and a scratch word; then the clear-state buffer.
const RING_AT: u64 = 170 << 20;
const RING_BYTES: u32 = 64 * 1024;
const REPORT_AT: u64 = RING_AT + 0x1_0000;
const WPTR_POLL_AT: u64 = REPORT_AT + 0x100;
const FENCE_AT: u64 = REPORT_AT + 0x200;
const SCRATCH_AT: u64 = REPORT_AT + 0x300;
const CSB_AT: u64 = RING_AT + 0x1_1000;
/// `RLC_RLCS_BOOTLOAD_STATUS` bit 31.
const BOOT_COMPLETE: u32 = 1 << 31;

/// The started ring.
#[derive(Debug)]
pub(crate) struct Gfx {
    /// The GPU address of the first byte of video memory.
    base: u64,
    #[allow(
        dead_code,
        reason = "kept for the compute queues and the rings after it"
    )]
    ring: Ring,
    /// The write pointer, in dwords.
    wptr: u32,
}

/// Why it stopped.
#[derive(Debug)]
#[allow(dead_code, reason = "the data is shown through Debug in the log")]
pub(crate) enum Stopped {
    /// The firmware autoload has not finished (the status register).
    NotLoaded(u32),
    /// The processor did not read what was written (the read pointer and
    /// where the write pointer was).
    Stalled(u32, u32),
}

/// Writes `data` at video memory offset `at`.
fn put(gpu: &Arc<Aperture>, at: u64, data: &[u32]) {
    for (i, d) in data.iter().enumerate() {
        gpu.vram_write32(usize::try_from(at).unwrap_or(0) + 4 * i, *d);
    }
}

impl Gfx {
    /// Appends `packets` to the ring and moves the write pointer.
    fn submit(&mut self, gpu: &Arc<Aperture>, mem: &dyn Memory, packets: &[u32]) {
        let dwords = RING_BYTES / 4;
        for p in packets {
            let at = RING_AT + 4 * u64::from(self.wptr);
            gpu.vram_write32(usize::try_from(at).unwrap_or(0), *p);
            self.wptr = (self.wptr + 1) % dwords;
        }
        mem.flush();
        let w = Bar5(gpu);
        w.write32(gc::CP_RB0_WPTR, self.wptr);
        w.write32(gc::CP_RB0_WPTR_HI, 0);
    }

    /// Waits until the processor has read everything written (its read
    /// pointer register equals the write pointer), `ms` at most.
    fn drain(&self, gpu: &Arc<Aperture>, ms: u32) -> Result<(), Stopped> {
        let w = Bar5(gpu);
        let mut rptr = 0;
        for _ in 0..ms {
            rptr = w.read32(gc::CP_RB0_RPTR);
            if rptr == self.wptr {
                return Ok(());
            }
            bus::manager().delay_us(1000);
        }
        Err(Stopped::Stalled(rptr, self.wptr))
    }
}

/// Starts ring 0: the golden settings and shader memory, the clear-state
/// buffer in video memory and its address given to the RLC, the ring
/// programmed with the processors halted, then released, the clear state and
/// `SET_BASE` submitted.
///
/// # Errors
///
/// [`Stopped`] says where it stopped.
pub(crate) fn start(
    gpu: &Arc<Aperture>,
    log: &dyn Fn(core::fmt::Arguments<'_>),
) -> Result<Gfx, Stopped> {
    let w = Bar5(gpu);
    let boot = w.read32(crate::gcload::BOOTLOAD_STATUS);
    if boot & BOOT_COMPLETE == 0 {
        return Err(Stopped::NotLoaded(boot));
    }
    let base = u64::from(w.read32(amdgpu::flip::FB_LOCATION_BASE) & 0xffff) << 24;
    let mem = crate::psp::vram_memory(gpu, base);
    let pause = |us: u32| bus::manager().delay_us(u64::from(us));
    // The memory: the ring and the report words zero, the clear-state buffer
    // written, all flushed before the processor looks.
    for at in (0..0x1_1000u64).step_by(4) {
        gpu.vram_write32(usize::try_from(RING_AT + at).unwrap_or(0), 0);
    }
    let csb = gc::clear_state_buffer();
    put(gpu, CSB_AT, &csb);
    let _ = gpu.vram_read32(0);
    amdgpu::flip::hdp_flush(&w);
    log(format_args!(
        "GC: ring at {:#x}, clear-state buffer {} dwords at {:#x}",
        base + RING_AT,
        csb.len(),
        base + CSB_AT
    ));
    gc::apply_golden(&w);
    gc::shader_memory(&w);
    // The clear-state buffer, for the RLC to replay.
    let csb_gpu = base + CSB_AT;
    w.write32(gc::RLC_CSIB_ADDR_LO, (csb_gpu & 0xffff_fffc) as u32);
    w.write32(gc::RLC_CSIB_ADDR_HI, (csb_gpu >> 32) as u32);
    w.write32(gc::RLC_CSIB_LENGTH, u32::try_from(csb.len()).unwrap_or(0));
    let ring = Ring {
        base: base + RING_AT,
        bytes: RING_BYTES,
        rptr_report: base + REPORT_AT,
        wptr_poll: base + WPTR_POLL_AT,
    };
    // Halted while the ring is programmed (so this can be run again), then
    // released. A released processor is busy running its firmware: CP_STAT
    // reads 0 only after a halt, so it is logged, not waited for.
    w.write32(gc::CP_ME_CNTL, w.read32(gc::CP_ME_CNTL) | gc::ME_HALT_ALL);
    gc::program_ring(&w, &ring, &pause);
    log(format_args!(
        "GC: ring 0 programmed; releasing the processors"
    ));
    let idle = gc::unhalt(&w, 3, &pause, 1000);
    log(format_args!(
        "GC: processors released: CP_ME_CNTL {:#010x}, CP_STAT {:#x} ({})",
        w.read32(gc::CP_ME_CNTL),
        w.read32(gc::CP_STAT),
        if idle {
            "idle"
        } else {
            "busy, as a running processor is"
        }
    ));
    let mut gfx = Gfx {
        base,
        ring,
        wptr: 0,
    };
    let mut p = Packets(csb);
    p.0.extend(gc::set_base_packets());
    gfx.submit(gpu, &mem, &p.0);
    gfx.drain(gpu, 200)?;
    log(format_args!(
        "GC: clear state taken: read pointer {} of {}",
        w.read32(gc::CP_RB0_RPTR),
        gfx.wptr
    ));
    Ok(gfx)
}

/// The ring test (06 section 11.6): a register written by the ring, then a
/// word of memory and a fence. Logs and returns whether both were seen.
pub(crate) fn test(
    gpu: &Arc<Aperture>,
    gfx: &mut Gfx,
    log: &dyn Fn(core::fmt::Arguments<'_>),
) -> bool {
    let w = Bar5(gpu);
    let mem = crate::psp::vram_memory(gpu, gfx.base);
    // 1. A scratch register: the CPU puts one value, the ring another.
    w.write32(gc::SCRATCH_REG0, 0xcafe_dead);
    let mut p = Packets::new();
    p.set_uconfig(gc::SCRATCH_REG0_DWORD, 0xdead_beef);
    gfx.submit(gpu, &mem, &p.0);
    let mut seen = w.read32(gc::SCRATCH_REG0);
    for _ in 0..200 {
        if seen == 0xdead_beef {
            break;
        }
        bus::manager().delay_us(1000);
        seen = w.read32(gc::SCRATCH_REG0);
    }
    let reg_ok = seen == 0xdead_beef;
    log(format_args!(
        "GC: ring test, register: SCRATCH_REG0 {seen:#010x} ({})",
        if reg_ok {
            "the ring's value"
        } else {
            "NOT the ring's value"
        }
    ));
    // 2. Memory: a write-data and a fence.
    put(gpu, SCRATCH_AT, &[0, 0]);
    put(gpu, FENCE_AT, &[0, 0]);
    amdgpu::flip::hdp_flush(&w);
    let mut p = Packets::new();
    p.write_data_mem(gfx.base + SCRATCH_AT, &[0x1234_abcd]);
    p.fence(gfx.base + FENCE_AT, 0xfeed_f00d, false, false);
    gfx.submit(gpu, &mem, &p.0);
    let (mut data, mut fence) = (0, 0);
    for _ in 0..200 {
        mem.invalidate();
        data = mem.read32(gfx.base + SCRATCH_AT);
        fence = mem.read32(gfx.base + FENCE_AT);
        if data == 0x1234_abcd && fence == 0xfeed_f00d {
            break;
        }
        bus::manager().delay_us(1000);
    }
    let mem_ok = data == 0x1234_abcd && fence == 0xfeed_f00d;
    log(format_args!(
        "GC: ring test, memory: data {data:#010x}, fence {fence:#010x} ({})",
        if mem_ok { "both seen" } else { "NOT both seen" }
    ));
    reg_ok && mem_ok
}
