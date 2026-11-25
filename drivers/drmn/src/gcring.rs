// SPDX-License-Identifier: BSD-2-Clause
//! The graphics ring through the kernel interface queue (stages R4.2 to R4.4
//! of docs/GRAPHICS.md §16): the golden settings, the shader memory
//! configuration and the clear-state buffer; the KIQ programmed register by
//! register and tested; graphics queue 0 mapped through it with an MQD;
//! the graphics processors released, the clear state submitted, and the ring
//! test, every ring written through its doorbell.

use alloc::sync::Arc;

use amdgpu::gc;
use amdgpu::gmc::{Hub, Reg};
use amdgpu::kiq;
use amdgpu::mqd::{self, Queue};
use amdgpu::pm4::Packets;
use amdgpu::psp::Memory;
use amdgpu::regs::Window;
use vgapci::Aperture;

use crate::amd::Bar5;

// Video memory the graphics block uses (BAR0 offsets, also video memory
// offsets): the graphics ring, its report words, the clear-state buffer and
// its queue descriptor; then the KIQ's ring, end-of-pipe buffer, queue
// descriptor and report words.
const GFX_AT: u64 = 170 << 20;
const GFX_RING_BYTES: u32 = 64 * 1024;
const GFX_REPORT_AT: u64 = GFX_AT + 0x1_0000;
const GFX_WPTR_POLL_AT: u64 = GFX_REPORT_AT + 0x100;
const FENCE_AT: u64 = GFX_REPORT_AT + 0x200;
const SCRATCH_AT: u64 = GFX_REPORT_AT + 0x300;
const CSB_AT: u64 = GFX_AT + 0x1_1000;
const GFX_MQD_AT: u64 = GFX_AT + 0x1_2000;
const KIQ_AT: u64 = 171 << 20;
const KIQ_RING_BYTES: u32 = 4096;
const KIQ_EOP_AT: u64 = KIQ_AT + 0x1000;
const KIQ_MQD_AT: u64 = KIQ_AT + 0x2000;
const KIQ_REPORT_AT: u64 = KIQ_AT + 0x3000;
const KIQ_WPTR_POLL_AT: u64 = KIQ_AT + 0x3100;
const KIQ_TEST_AT: u64 = KIQ_AT + 0x3200;
/// The graphics ring 0's doorbell (a dword index in BAR2; 05 section 10.2).
const GFX_DOORBELL_DWORD: u32 = 0x116;
/// `CP_RB_DOORBELL_RANGE_LOWER` and `_UPPER` (bits 11:2).
const CP_RB_DOORBELL_RANGE_LOWER: usize = 0xc168;
const CP_RB_DOORBELL_RANGE_UPPER: usize = 0xc16c;
/// `CP_GFX_MQD_BASE_ADDR` and its high half; `CP_GFX_HQD_ACTIVE`;
/// `CP_GFX_HQD_BASE` and its high half (read only, to see the mapping).
const CP_GFX_MQD_BASE_ADDR: usize = 0xc378;
const CP_GFX_MQD_BASE_ADDR_HI: usize = 0xc37c;
const CP_GFX_HQD_ACTIVE: usize = 0xc380;
const CP_GFX_HQD_BASE: usize = 0xc398;
const CP_GFX_HQD_BASE_HI: usize = 0xc39c;
/// `RLC_RLCS_BOOTLOAD_STATUS` bit 31.
const BOOT_COMPLETE: u32 = 1 << 31;

/// A ring written through a doorbell.
#[derive(Debug)]
struct Ring {
    /// Where its memory is (a video memory offset).
    at: u64,
    bytes: u32,
    /// The doorbell's byte offset in BAR2.
    doorbell: usize,
    /// The write pointer, in dwords.
    wptr: u32,
    /// Whether the packets are compute packets.
    compute: bool,
    /// Where it reports its read pointer.
    report: u64,
}

/// The started graphics block.
#[derive(Debug)]
pub(crate) struct Gfx {
    /// The GPU address of the first byte of video memory.
    base: u64,
    #[allow(dead_code, reason = "the interface queue maps the queues after it")]
    kiq: Ring,
    gfx: Ring,
}

/// Why it stopped.
#[derive(Debug)]
#[allow(dead_code, reason = "the data is shown through Debug in the log")]
pub(crate) enum Stopped {
    /// The firmware autoload has not finished (the status register).
    NotLoaded(u32),
    /// The KIQ did not read back as active.
    KiqInactive,
    /// A ring did not read what was written (which, the read pointer and the
    /// write pointer).
    Stalled(&'static str, u32, u32),
}

fn put(gpu: &Arc<Aperture>, at: u64, data: &[u32]) {
    for (i, d) in data.iter().enumerate() {
        gpu.vram_write32(usize::try_from(at).unwrap_or(0) + 4 * i, *d);
    }
}

impl Ring {
    /// Appends `packets` to the ring and rings the doorbell with the new
    /// write pointer.
    fn submit(&mut self, gpu: &Arc<Aperture>, mem: &dyn Memory, packets: &[u32]) {
        let dwords = self.bytes / 4;
        let mut p = Packets(packets.to_vec());
        if self.compute {
            p.mark_compute();
        }
        for d in &p.0 {
            let at = self.at + 4 * u64::from(self.wptr);
            gpu.vram_write32(usize::try_from(at).unwrap_or(0), *d);
            self.wptr = (self.wptr + 1) % dwords;
        }
        mem.flush();
        gpu.doorbell_write64(self.doorbell, u64::from(self.wptr));
    }

    /// Waits until the ring has reported reading everything written.
    fn drain(
        &self,
        name: &'static str,
        base: u64,
        mem: &dyn Memory,
        ms: u32,
    ) -> Result<(), Stopped> {
        let mut rptr = 0;
        for _ in 0..ms {
            mem.invalidate();
            rptr = mem.read32(base + self.report);
            if rptr == self.wptr {
                return Ok(());
            }
            bus::manager().delay_us(1000);
        }
        Err(Stopped::Stalled(name, rptr, self.wptr))
    }
}

/// Logs the translation hub's fault status if there is one.
fn fault(gpu: &Arc<Aperture>, log: &dyn Fn(core::fmt::Arguments<'_>)) {
    let st = Bar5(gpu).read32(Hub::Gfx.reg(Reg::FaultStatus));
    if st != 0 {
        log(format_args!(
            "GC: GFXHUB FAULT status {st:#010x}: client {}, vmid {}, write {}",
            (st >> 9) & 0x1ff,
            (st >> 20) & 0xf,
            (st >> 18) & 1
        ));
    }
}

/// Starts the graphics block: the settings, the KIQ and its test, graphics
/// queue 0 mapped, the processors released, the clear state submitted.
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
    // The memory: all of it zero, the clear-state buffer written.
    for at in (0..0x1_3000u64).step_by(4) {
        gpu.vram_write32(usize::try_from(GFX_AT + at).unwrap_or(0), 0);
    }
    for at in (0..0x4000u64).step_by(4) {
        gpu.vram_write32(usize::try_from(KIQ_AT + at).unwrap_or(0), 0);
    }
    let csb = gc::clear_state_buffer();
    put(gpu, CSB_AT, &csb);
    let _ = gpu.vram_read32(0);
    amdgpu::flip::hdp_flush(&w);
    gc::apply_golden(&w);
    gc::shader_memory(&w);
    let csb_gpu = base + CSB_AT;
    w.write32(gc::RLC_CSIB_ADDR_LO, (csb_gpu & 0xffff_fffc) as u32);
    w.write32(gc::RLC_CSIB_ADDR_HI, (csb_gpu >> 32) as u32);
    w.write32(gc::RLC_CSIB_LENGTH, u32::try_from(csb.len()).unwrap_or(0));
    log(format_args!(
        "GC: golden settings, shader memory and the {}-dword clear-state buffer at {csb_gpu:#x} set",
        csb.len()
    ));
    // The doorbells: the aperture, and the graphics ring's range.
    amdgpu::doorbell::enable_aperture(&w);
    w.write32(CP_RB_DOORBELL_RANGE_LOWER, GFX_DOORBELL_DWORD << 2);
    w.write32(CP_RB_DOORBELL_RANGE_UPPER, 0xffc);

    // The KIQ: its descriptor, programmed, tested.
    let kiq_q = Queue {
        mqd: base + KIQ_MQD_AT,
        ring: base + KIQ_AT,
        ring_bytes: KIQ_RING_BYTES,
        rptr_report: base + KIQ_REPORT_AT,
        wptr_poll: base + KIQ_WPTR_POLL_AT,
        doorbell_dword: kiq::KIQ_DOORBELL_DWORD,
        eop: base + KIQ_EOP_AT,
    };
    let kiq_mqd = mqd::compute_mqd(&kiq_q);
    put(gpu, KIQ_MQD_AT, &kiq_mqd);
    let _ = gpu.vram_read32(0);
    amdgpu::flip::hdp_flush(&w);
    let active = kiq::start(&w, &kiq_mqd, &pause);
    log(format_args!(
        "GC: KIQ programmed (MEC2 pipe 1): active {active}, CP_MEC_CNTL {:#x}",
        w.read32(kiq::CP_MEC_CNTL)
    ));
    if !active {
        return Err(Stopped::KiqInactive);
    }
    let mut kiq_ring = Ring {
        at: KIQ_AT,
        bytes: KIQ_RING_BYTES,
        doorbell: 4 * kiq::KIQ_DOORBELL_DWORD as usize,
        wptr: 0,
        compute: true,
        report: KIQ_REPORT_AT,
    };
    // The KIQ test: a write of a word to memory through it.
    let mut p = Packets::new();
    p.write_data_mem(base + KIQ_TEST_AT, &[0x4b49_5121]);
    kiq_ring.submit(gpu, &mem, &p.0);
    let mut seen = 0;
    for _ in 0..200 {
        mem.invalidate();
        seen = mem.read32(base + KIQ_TEST_AT);
        if seen == 0x4b49_5121 {
            break;
        }
        bus::manager().delay_us(1000);
    }
    log(format_args!(
        "GC: KIQ test: the word written through it reads {seen:#010x} ({}), its read pointer {} of {}",
        if seen == 0x4b49_5121 {
            "seen"
        } else {
            "NOT seen"
        },
        mem.read32(base + KIQ_REPORT_AT),
        kiq_ring.wptr
    ));
    fault(gpu, log);
    if seen != 0x4b49_5121 {
        return Err(Stopped::Stalled(
            "KIQ",
            mem.read32(base + KIQ_REPORT_AT),
            kiq_ring.wptr,
        ));
    }

    // Graphics queue 0: its descriptor, mapped through the KIQ.
    let gfx_q = Queue {
        mqd: base + GFX_MQD_AT,
        ring: base + GFX_AT,
        ring_bytes: GFX_RING_BYTES,
        rptr_report: base + GFX_REPORT_AT,
        wptr_poll: base + GFX_WPTR_POLL_AT,
        doorbell_dword: GFX_DOORBELL_DWORD,
        eop: 0,
    };
    put(gpu, GFX_MQD_AT, &mqd::gfx_mqd(&gfx_q));
    let _ = gpu.vram_read32(0);
    amdgpu::flip::hdp_flush(&w);
    let mut map = mqd::set_resources(0);
    map.0
        .extend(mqd::map_gfx_queue(0, GFX_DOORBELL_DWORD, gfx_q.mqd, gfx_q.wptr_poll).0);
    kiq_ring.submit(gpu, &mem, &map.0);
    let mapped = kiq_ring.drain("KIQ map", base, &mem, 200);
    gc::select(&w, 0, 0, 0, 0);
    log(format_args!(
        "GC: graphics queue mapping ({mapped:?}): CP_GFX_MQD_BASE_ADDR {:#x}:{:#010x}, CP_GFX_HQD_ACTIVE {:#x}, CP_GFX_HQD_BASE {:#x}:{:#010x}",
        w.read32(CP_GFX_MQD_BASE_ADDR_HI),
        w.read32(CP_GFX_MQD_BASE_ADDR),
        w.read32(CP_GFX_HQD_ACTIVE),
        w.read32(CP_GFX_HQD_BASE_HI),
        w.read32(CP_GFX_HQD_BASE),
    ));
    fault(gpu, log);
    mapped?;

    // The graphics start (06 section 7.3): release the processors, submit
    // the clear state and `SET_BASE` on the ring.
    let idle = gc::unhalt(&w, 3, &pause, 1000);
    log(format_args!(
        "GC: graphics processors released: CP_ME_CNTL {:#010x}, CP_STAT {:#x} ({})",
        w.read32(gc::CP_ME_CNTL),
        w.read32(gc::CP_STAT),
        if idle { "idle" } else { "busy" }
    ));
    let mut gfx = Ring {
        at: GFX_AT,
        bytes: GFX_RING_BYTES,
        doorbell: 4 * GFX_DOORBELL_DWORD as usize,
        wptr: 0,
        compute: false,
        report: GFX_REPORT_AT,
    };
    let mut p = csb;
    p.extend(gc::set_base_packets());
    gfx.submit(gpu, &mem, &p);
    let r = gfx.drain("graphics", base, &mem, 200);
    log(format_args!(
        "GC: clear state submitted: read pointer {} of {}",
        mem.read32(base + GFX_REPORT_AT),
        gfx.wptr
    ));
    fault(gpu, log);
    r?;
    Ok(Gfx {
        base,
        kiq: kiq_ring,
        gfx,
    })
}

/// The ring test (06 section 11.6): a register written by the ring, then a
/// word of memory and a fence. Logs and returns whether both were seen.
pub(crate) fn test(
    gpu: &Arc<Aperture>,
    g: &mut Gfx,
    log: &dyn Fn(core::fmt::Arguments<'_>),
) -> bool {
    let w = Bar5(gpu);
    let mem = crate::psp::vram_memory(gpu, g.base);
    // 1. A scratch register: the CPU puts one value, the ring another.
    w.write32(gc::SCRATCH_REG0, 0xcafe_dead);
    let mut p = Packets::new();
    p.set_uconfig(gc::SCRATCH_REG0_DWORD, 0xdead_beef);
    g.gfx.submit(gpu, &mem, &p.0);
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
    p.write_data_mem(g.base + SCRATCH_AT, &[0x1234_abcd]);
    p.fence(g.base + FENCE_AT, 0xfeed_f00d, false, false);
    g.gfx.submit(gpu, &mem, &p.0);
    let (mut data, mut fence) = (0, 0);
    for _ in 0..200 {
        mem.invalidate();
        data = mem.read32(g.base + SCRATCH_AT);
        fence = mem.read32(g.base + FENCE_AT);
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
    fault(gpu, log);
    reg_ok && mem_ok
}
