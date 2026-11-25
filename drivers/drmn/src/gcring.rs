// SPDX-License-Identifier: BSD-2-Clause
//! The graphics ring through the kernel interface queue (stages R4.2 to R4.4
//! of docs/GRAPHICS.md §16): the golden settings, the shader memory
//! configuration and the clear-state buffer; the KIQ programmed register by
//! register and tested; graphics queue 0 mapped through it with an MQD;
//! the graphics processors released, the clear state submitted, and the ring
//! test, every ring written through its doorbell.

use alloc::sync::Arc;
use alloc::vec::Vec;

use amdgpu::gc;
use amdgpu::gmc::{Hub, Reg};
use amdgpu::kiq;
use amdgpu::mqd::{self, Queue};
use amdgpu::pm4::Packets;
use amdgpu::psp::Memory;
use amdgpu::regs::Window;
use amdgpu::vm::Tables as _;
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
    /// The write pointer, in dwords: a count that never wraps (the queue's
    /// 64-bit pointers); the ring offset is the count modulo the size.
    wptr: u64,
    /// Whether the packets are compute packets.
    compute: bool,
    /// Where it reports its read pointer.
    report: u64,
    /// Where the queue polls for the write pointer (a video memory offset):
    /// the new value goes there before the doorbell is rung.
    poll: u64,
}

/// The started graphics block.
#[derive(Debug)]
pub(crate) struct Gfx {
    base: u64,
    #[allow(dead_code, reason = "the interface queue maps the queues after it")]
    kiq: Ring,
    gfx: Ring,
    /// The kernel compute queues that answered their test.
    compute: Vec<Ring>,
}

// The eight kernel compute queues' memory (06 section 8.1): 128 KiB each from
// `COMPUTE_AT` (video memory offsets): the ring, the words it reports through
// and the fence word, then the queue descriptor and the end-of-pipe buffer.
const COMPUTE_AT: u64 = 175 << 20;
const COMPUTE_STRIDE: u64 = 0x2_0000;
const COMPUTE_RING_BYTES: u32 = 64 * 1024;
const COMPUTE_REPORT: u64 = 0x1_0000;
const COMPUTE_POLL: u64 = 0x1_0100;
const COMPUTE_SEQ: u64 = 0x1_0200;
const COMPUTE_TEST: u64 = 0x1_0300;
const COMPUTE_PREEMPT: u64 = 0x1_0380;
const COMPUTE_MQD: u64 = 0x1_1000;
const COMPUTE_EOP: u64 = 0x1_2000;
const COMPUTE_QUEUES: u32 = 8;
// Each queue's block holds its descriptor and end-of-pipe buffer, after its
// test word.
const _: () = assert!(COMPUTE_EOP + 2048 <= COMPUTE_STRIDE);
const _: () = assert!(COMPUTE_TEST + 4 <= COMPUTE_MQD);

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

/// Whether a ring of `size` dwords, its write pointer at position `at` and
/// its read pointer at `rptr` (both below `size`), has room for `need` more
/// dwords and `slack` spare ones: the pointers must never meet, or a full
/// ring would look empty.
fn has_room(at: u32, rptr: u32, size: u32, need: u32, slack: u32) -> bool {
    let used = (at + size - rptr) % size;
    size - used > need + slack
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
            let at = self.at + 4 * (self.wptr % u64::from(dwords));
            gpu.vram_write32(usize::try_from(at).unwrap_or(0), *d);
            self.wptr += 1;
        }
        mem.flush();
        // The poll word first: the processor may read the pointer from it as
        // well as from the doorbell.
        let poll = usize::try_from(self.poll).unwrap_or(0);
        gpu.vram_write32(poll, (self.wptr & 0xffff_ffff) as u32);
        gpu.vram_write32(poll + 4, (self.wptr >> 32) as u32);
        mem.flush();
        gpu.doorbell_write64(self.doorbell, self.wptr);
    }

    /// Waits for room for `packets` (at most `ms`), then writes them and
    /// rings the doorbell. `base` is the video memory's GPU address.
    fn run(
        &mut self,
        gpu: &Arc<Aperture>,
        base: u64,
        packets: &[u32],
        ms: u32,
    ) -> Result<(), abi::errno::Errno> {
        let mem = crate::psp::vram_memory(gpu, base);
        let size = self.bytes / 4;
        let need = u32::try_from(packets.len()).map_err(|_| abi::errno::Errno::E2BIG)?;
        if need + 1 >= size {
            return Err(abi::errno::Errno::E2BIG);
        }
        for _ in 0..=ms {
            mem.invalidate();
            let rptr = mem.read32(base + self.report) % size;
            let at = u32::try_from(self.wptr % u64::from(size)).unwrap_or(0);
            if has_room(at, rptr, size, need, 1) {
                self.submit(gpu, &mem, packets);
                return Ok(());
            }
            bus::manager().delay_us(1000);
        }
        Err(abi::errno::Errno::EAGAIN)
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
            if u64::from(rptr) % u64::from(self.bytes / 4) == self.wptr % u64::from(self.bytes / 4)
            {
                return Ok(());
            }
            bus::manager().delay_us(1000);
        }
        Err(Stopped::Stalled(
            name,
            rptr,
            (self.wptr & 0xffff_ffff) as u32,
        ))
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
        poll: KIQ_WPTR_POLL_AT,
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
            (kiq_ring.wptr & 0xffff_ffff) as u32,
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
        poll: GFX_WPTR_POLL_AT,
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
    // Fences interrupt: the end-of-pipe event of ring 0 goes to the
    // interrupt handler, which wakes whoever waits for a job.
    w.write32(
        gc::CP_INT_CNTL_RING0,
        w.read32(gc::CP_INT_CNTL_RING0) | gc::TIME_STAMP_INT_ENABLE,
    );
    // The compute queues, mapped through the KIQ in one submission; a
    // failure leaves the graphics ring as it is.
    let compute = start_compute(gpu, base, &mut kiq_ring, &mem, log);
    Ok(Gfx {
        base,
        kiq: kiq_ring,
        gfx,
        compute,
    })
}

/// Where compute queue `n`'s memory is (a video memory offset) and what the
/// queue descriptor says about it.
fn compute_queue(base: u64, n: u32) -> (Queue, u64) {
    let at = COMPUTE_AT + u64::from(n) * COMPUTE_STRIDE;
    let (_, _, doorbell, _) = mqd::kernel_compute_place(n);
    let q = Queue {
        mqd: base + at + COMPUTE_MQD,
        ring: base + at,
        ring_bytes: COMPUTE_RING_BYTES,
        rptr_report: base + at + COMPUTE_REPORT,
        wptr_poll: base + at + COMPUTE_POLL,
        doorbell_dword: doorbell,
        eop: base + at + COMPUTE_EOP,
    };
    (q, at)
}

/// Maps the eight kernel compute queues (06 section 8.1) and tests each with
/// a word written through it; returns the rings that answered.
fn start_compute(
    gpu: &Arc<Aperture>,
    base: u64,
    kiq: &mut Ring,
    mem: &dyn Memory,
    log: &dyn Fn(core::fmt::Arguments<'_>),
) -> Vec<Ring> {
    let w = Bar5(gpu);
    let mut map = mqd::set_resources(mqd::KERNEL_COMPUTE_MASK);
    let mut rings = Vec::new();
    for n in 0..COMPUTE_QUEUES {
        let (q, at) = compute_queue(base, n);
        let (pipe, queue, doorbell, _) = mqd::kernel_compute_place(n);
        for off in (0..0x1_1000u64).step_by(4) {
            gpu.vram_write32(usize::try_from(at + off).unwrap_or(0), 0);
        }
        put(gpu, at + COMPUTE_MQD, &mqd::kernel_compute_mqd(&q, n));
        map.0
            .extend(mqd::map_compute_queue(pipe, queue, doorbell, q.mqd, q.wptr_poll).0);
        rings.push(Ring {
            at,
            bytes: COMPUTE_RING_BYTES,
            doorbell: 4 * doorbell as usize,
            wptr: 0,
            compute: true,
            report: at + COMPUTE_REPORT,
            poll: at + COMPUTE_POLL,
        });
    }
    let _ = gpu.vram_read32(0);
    amdgpu::flip::hdp_flush(&w);
    kiq.submit(gpu, mem, &map.0);
    let mapped = kiq.drain("KIQ map compute", base, mem, 200);
    log(format_args!(
        "GC: compute queues mapping ({mapped:?}): {COMPUTE_QUEUES} queues"
    ));
    if mapped.is_err() {
        return Vec::new();
    }
    // Each queue writes a word of its own.
    for (n, r) in rings.iter_mut().enumerate() {
        let word = 0xc0de_0000 | u32::try_from(n).unwrap_or(0);
        let mut p = Packets::new();
        p.write_data_mem(base + r.at + COMPUTE_TEST, &[word]);
        r.submit(gpu, mem, &p.0);
    }
    let mut ok = Vec::new();
    for (n, r) in rings.into_iter().enumerate() {
        let want = 0xc0de_0000 | u32::try_from(n).unwrap_or(0);
        let mut seen = 0;
        for _ in 0..200 {
            mem.invalidate();
            seen = mem.read32(base + r.at + COMPUTE_TEST);
            if seen == want {
                break;
            }
            bus::manager().delay_us(1000);
        }
        if seen == want {
            ok.push(r);
        } else {
            log(format_args!(
                "GC: compute queue {n}: word {seen:#010x}, wanted {want:#010x}"
            ));
        }
    }
    log(format_args!(
        "GC: compute queues: {} of {COMPUTE_QUEUES} answered their test",
        ok.len()
    ));
    ok
}

/// The word the kernel's fences are written to (a video memory offset): the
/// sequence number of the last job the ring finished.
const SEQ_AT: u64 = GFX_REPORT_AT + 0x400;

impl Gfx {
    /// Logs where the ring is and the dwords around its read pointer (a
    /// diagnostic for a stuck engine; reads only).
    pub(crate) fn hang_report(&self, gpu: &Arc<Aperture>, log: &dyn Fn(core::fmt::Arguments<'_>)) {
        let mem = crate::psp::vram_memory(gpu, self.base);
        mem.invalidate();
        let size = self.gfx.bytes / 4;
        // The live pointer (the report word is written lazily).
        let rptr = Bar5(gpu).read32(gc::CP_RB0_RPTR) % size;
        log(format_args!(
            "GC: ring: write pointer {}, read pointer {rptr}, last finished job {}",
            self.gfx.wptr % u64::from(size),
            self.finished(gpu)
        ));
        let from = rptr.saturating_sub(24);
        for i in from..(rptr + 12).min(size) {
            log(format_args!(
                "GC:   ring[{i}] = {:#010x}{}",
                mem.read32(self.base + self.gfx.at + 4 * u64::from(i)),
                if i == rptr { "   <- read pointer" } else { "" }
            ));
        }
    }

    /// How many compute rings answered.
    pub(crate) fn compute_rings(&self) -> usize {
        self.compute.len()
    }

    /// The GPU address of compute ring `i`'s fence word.
    pub(crate) fn compute_seq_address(&self, i: usize) -> u64 {
        self.base + self.compute.get(i).map_or(0, |r| r.at) + COMPUTE_SEQ
    }

    /// The number of the last job compute ring `i` finished.
    pub(crate) fn compute_finished(&self, gpu: &Arc<Aperture>, i: usize) -> u32 {
        let mem = crate::psp::vram_memory(gpu, self.base);
        mem.invalidate();
        mem.read32(self.compute_seq_address(i))
    }

    /// Runs `packets` on compute ring `i`.
    pub(crate) fn compute_run(
        &mut self,
        gpu: &Arc<Aperture>,
        i: usize,
        packets: &[u32],
        ms: u32,
    ) -> Result<(), abi::errno::Errno> {
        let base = self.base;
        self.compute
            .get_mut(i)
            .ok_or(abi::errno::Errno::ENODEV)?
            .run(gpu, base, packets, ms)
    }

    /// The GPU address of the fence word.
    pub(crate) fn seq_address(&self) -> u64 {
        self.base + SEQ_AT
    }

    /// The sequence number of the last job the ring has finished.
    pub(crate) fn finished(&self, gpu: &Arc<Aperture>) -> u32 {
        let mem = crate::psp::vram_memory(gpu, self.base);
        mem.invalidate();
        mem.read32(self.base + SEQ_AT)
    }

    /// Waits for room for `dwords` more in the ring, `ms` at most; then
    /// writes `packets` and rings the doorbell.
    pub(crate) fn run(
        &mut self,
        gpu: &Arc<Aperture>,
        packets: &[u32],
        ms: u32,
    ) -> Result<(), abi::errno::Errno> {
        self.gfx.run(gpu, self.base, packets, ms)
    }
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

// ---- a user address space (stage R4.3) ----

/// Where the test's page tables, indirect buffer and data page are (video
/// memory offsets).
const VM_TABLES_AT: u64 = 172 << 20;
const VM_TABLES: usize = 64;
const VM_IB_AT: u64 = 173 << 20;
const VM_DATA_AT: u64 = VM_IB_AT + 0x1000;
/// The test's virtual addresses for them.
const VA_IB: u64 = 0x10_0000;
const VA_DATA: u64 = 0x10_1000;

/// Page tables in video memory: table `n` of a pool is at `at + 4096 n`,
/// written through the aperture.
pub(crate) struct VramTables {
    gpu: Arc<Aperture>,
    /// The video memory's physical offset (the carve-out's address).
    phys: u64,
    /// Where the pool starts (a video memory offset) and how many tables it
    /// holds.
    at: u64,
    count: usize,
    used: usize,
}

impl VramTables {
    /// A pool of `count` tables from video memory offset `at`.
    pub(crate) fn new(gpu: Arc<Aperture>, phys: u64, at: u64, count: usize) -> Self {
        Self {
            gpu,
            phys,
            at,
            count,
            used: 0,
        }
    }

    fn at(&self, handle: usize) -> u64 {
        self.at + 4096 * handle as u64
    }
}

impl amdgpu::vm::Tables for VramTables {
    fn alloc(&mut self) -> Option<usize> {
        if self.used >= self.count {
            return None;
        }
        let h = self.used;
        self.used += 1;
        for i in 0..1024u64 {
            self.gpu
                .vram_write32(usize::try_from(self.at(h) + 4 * i).unwrap_or(0), 0);
        }
        Some(h)
    }

    fn address(&self, handle: usize) -> (u64, bool) {
        (self.phys + self.at(handle), false)
    }

    fn write(&mut self, handle: usize, index: usize, entry: u64) {
        let at = usize::try_from(self.at(handle)).unwrap_or(0) + 8 * index;
        self.gpu.vram_write32(at, (entry & 0xffff_ffff) as u32);
        self.gpu.vram_write32(at + 4, (entry >> 32) as u32);
    }

    fn read(&self, handle: usize, index: usize) -> u64 {
        let at = usize::try_from(self.at(handle)).unwrap_or(0) + 8 * index;
        let lo = self.gpu.vram_read32(at).unwrap_or(0);
        let hi = self.gpu.vram_read32(at + 4).unwrap_or(0);
        u64::from(lo) | (u64::from(hi) << 32)
    }

    fn table_at(&self, entry: u64) -> Option<usize> {
        let a = (entry & 0x0000_ffff_ffff_f000).checked_sub(self.phys + self.at)?;
        let h = usize::try_from(a / 4096).ok()?;
        (h < self.used).then_some(h)
    }
}

/// An indirect buffer at a virtual address, run in VMID 1 (06 section 10.4):
/// the page tables built in video memory, two pages mapped (the buffer and a
/// data page), the page table switched and flushed on the ring, the buffer
/// run. The buffer writes a word to the data page and fences to it, by
/// virtual address; the CPU reads the page back. Logs; returns whether both
/// landed.
pub(crate) fn vm_test(
    gpu: &Arc<Aperture>,
    g: &mut Gfx,
    log: &dyn Fn(core::fmt::Arguments<'_>),
) -> bool {
    use amdgpu::vm::{Flags, Space};
    let w = Bar5(gpu);
    let mem = crate::psp::vram_memory(gpu, g.base);
    let phys = u64::from(w.read32(Hub::Gfx.reg(Reg::FbOffset))) << 24;
    let tables = VramTables::new(Arc::clone(gpu), phys, VM_TABLES_AT, VM_TABLES);
    let Ok(mut space) = Space::new(tables) else {
        log(format_args!("VM: no memory for the root table"));
        return false;
    };
    let ok = space
        .map(
            VA_IB,
            1,
            // The command processor's fetch of an indirect buffer needs the
            // page executable (a permission fault, bit 7, otherwise).
            Flags {
                executable: true,
                ..Flags::VRAM_RW
            },
            |_| phys + VM_IB_AT,
        )
        .and_then(|()| space.map(VA_DATA, 1, Flags::VRAM_RW, |_| phys + VM_DATA_AT));
    if ok.is_err() {
        log(format_args!("VM: mapping failed: {ok:?}"));
        return false;
    }
    let root = space.tables().address(space.root()).0;
    log(format_args!(
        "VM: root table at {root:#x}; {VA_IB:#x} and {VA_DATA:#x} mapped to video memory offsets {VM_IB_AT:#x} and {VM_DATA_AT:#x}"
    ));
    // The indirect buffer: a write and a fence, by virtual address.
    let mut ib = Packets::new();
    ib.write_data_mem(VA_DATA, &[0xcafe_0001]);
    ib.fence(VA_DATA + 8, 0xfeed_0002, false, false);
    ib.pad(8);
    put(gpu, VM_IB_AT, &ib.0);
    put(gpu, VM_DATA_AT, &[0, 0, 0, 0]);
    let _ = gpu.vram_read32(0);
    amdgpu::flip::hdp_flush(&w);
    // The ring: switch and flush VMID 1, run the buffer in it, then a fence of
    // the ring's own to say the buffer is done.
    let mut ring = amdgpu::gmc::ring_vm_flush(1, root, false, 0);
    ring.indirect_buffer(VA_IB, u32::try_from(ib.0.len()).unwrap_or(0), 1, false);
    put(gpu, FENCE_AT, &[0, 0]);
    amdgpu::flip::hdp_flush(&w);
    ring.fence(g.base + FENCE_AT, 0x600d_f00d, false, false);
    g.gfx.submit(gpu, &mem, &ring.0);
    let (mut data, mut fence, mut ring_fence) = (0, 0, 0);
    for _ in 0..300 {
        mem.invalidate();
        data = mem.read32(g.base + VM_DATA_AT);
        fence = mem.read32(g.base + VM_DATA_AT + 8);
        ring_fence = mem.read32(g.base + FENCE_AT);
        if ring_fence == 0x600d_f00d {
            break;
        }
        bus::manager().delay_us(1000);
    }
    let ok = data == 0xcafe_0001 && fence == 0xfeed_0002 && ring_fence == 0x600d_f00d;
    log(format_args!(
        "VM: the buffer ran in VMID 1: data {data:#010x}, its fence {fence:#010x}, the ring's fence {ring_fence:#010x} ({})",
        if ok { "all seen" } else { "NOT all seen" }
    ));
    fault(gpu, log);
    ok
}

// The system DMA engine's memory (video memory offsets): its ring, the words
// it reports through, and the fence word of its jobs.
const SDMA_AT: u64 = 174 << 20;
const SDMA_RING_BYTES: u32 = 64 * 1024;
const SDMA_REPORT_AT: u64 = SDMA_AT + 0x1_0000;
const SDMA_POLL_AT: u64 = SDMA_AT + 0x1_0100;
const SDMA_SEQ_AT: u64 = SDMA_AT + 0x1_0200;
const SDMA_TEST_AT: u64 = SDMA_AT + 0x1_0300;

/// System DMA engine 0's ring: pointers are 64-bit byte offsets.
#[derive(Debug)]
pub(crate) struct Sdma {
    base: u64,
    /// The write pointer in dwords, a count that never wraps.
    wptr: u64,
}

impl Sdma {
    /// The GPU address of the fence word of its jobs.
    pub(crate) fn seq_address(&self) -> u64 {
        self.base + SDMA_SEQ_AT
    }

    /// The sequence number of the last job it finished.
    pub(crate) fn finished(&self, gpu: &Arc<Aperture>) -> u32 {
        let mem = crate::psp::vram_memory(gpu, self.base);
        mem.invalidate();
        mem.read32(self.base + SDMA_SEQ_AT)
    }

    /// The ring position the next packet goes at, in dwords.
    pub(crate) fn position(&self) -> u64 {
        self.wptr % u64::from(SDMA_RING_BYTES / 4)
    }

    /// Writes `packets` and rings the doorbell, waiting up to `ms` for room.
    pub(crate) fn run(
        &mut self,
        gpu: &Arc<Aperture>,
        packets: &[u32],
        ms: u32,
    ) -> Result<(), abi::errno::Errno> {
        let mem = crate::psp::vram_memory(gpu, self.base);
        let size = SDMA_RING_BYTES / 4;
        let need = u32::try_from(packets.len()).map_err(|_| abi::errno::Errno::E2BIG)?;
        if need + 8 >= size {
            return Err(abi::errno::Errno::E2BIG);
        }
        for _ in 0..=ms {
            mem.invalidate();
            // The engine reports its read pointer in bytes.
            let rptr = (mem.read32(self.base + SDMA_REPORT_AT) / 4) % size;
            let at = u32::try_from(self.position()).unwrap_or(0);
            if has_room(at, rptr, size, need, 8) {
                self.submit(gpu, &mem, packets);
                return Ok(());
            }
            bus::manager().delay_us(1000);
        }
        Err(abi::errno::Errno::EAGAIN)
    }

    fn submit(&mut self, gpu: &Arc<Aperture>, mem: &dyn Memory, packets: &[u32]) {
        let size = u64::from(SDMA_RING_BYTES / 4);
        for d in packets {
            let at = SDMA_AT + 4 * (self.wptr % size);
            gpu.vram_write32(usize::try_from(at).unwrap_or(0), *d);
            self.wptr += 1;
        }
        mem.flush();
        // The poll word, then the doorbell, both the pointer in bytes.
        let bytes = self.wptr * 4;
        let poll = usize::try_from(SDMA_POLL_AT).unwrap_or(0);
        gpu.vram_write32(poll, (bytes & 0xffff_ffff) as u32);
        gpu.vram_write32(poll + 4, (bytes >> 32) as u32);
        mem.flush();
        gpu.doorbell_write64(amdgpu::sdma::DOORBELL_BYTE, bytes);
    }

    /// Where the engine is: its report, our pointer and the fence word.
    pub(crate) fn report(&self, gpu: &Arc<Aperture>, log: &dyn Fn(core::fmt::Arguments<'_>)) {
        let mem = crate::psp::vram_memory(gpu, self.base);
        mem.invalidate();
        let w = Bar5(gpu);
        log(format_args!(
            "SDMA: write pointer {} dwords, read pointer {} (reported), {} (register), last finished job {}, STATUS {:#x}",
            self.wptr,
            mem.read32(self.base + SDMA_REPORT_AT) / 4,
            w.read32(amdgpu::sdma::GFX_RB_RPTR) / 4,
            mem.read32(self.base + SDMA_SEQ_AT),
            w.read32(amdgpu::sdma::STATUS_REG)
        ));
    }
}

/// Starts system DMA engine 0 and tests it: a word written, then a fence
/// (07 section 4). Its firmware has been loaded by the security processor.
///
/// # Errors
///
/// [`Stopped::Stalled`] if the test packets are not read.
pub(crate) fn start_sdma(
    gpu: &Arc<Aperture>,
    log: &dyn Fn(core::fmt::Arguments<'_>),
) -> Result<Sdma, Stopped> {
    let w = Bar5(gpu);
    let base = u64::from(w.read32(amdgpu::flip::FB_LOCATION_BASE) & 0xffff) << 24;
    let mem = crate::psp::vram_memory(gpu, base);
    for at in (0..0x1_1000u64).step_by(4) {
        gpu.vram_write32(usize::try_from(SDMA_AT + at).unwrap_or(0), 0);
    }
    let _ = gpu.vram_read32(0);
    amdgpu::flip::hdp_flush(&w);
    // The doorbell range, then the engine.
    amdgpu::doorbell::program_sdma0_range(&w);
    let ring = amdgpu::sdma::Ring {
        base: base + SDMA_AT,
        bytes: SDMA_RING_BYTES,
        rptr_report: base + SDMA_REPORT_AT,
        wptr_poll: base + SDMA_POLL_AT,
    };
    amdgpu::sdma::start(&w, &ring, &|us| bus::manager().delay_us(u64::from(us)));
    let mut s = Sdma { base, wptr: 0 };
    // The test: a word, then a fence.
    let mut p = amdgpu::sdma::Packets::new();
    p.write_linear(base + SDMA_TEST_AT, &[0x5d4a_0001]);
    p.fence(s.seq_address(), 1);
    s.submit(gpu, &mem, &p.0);
    let mut word = 0;
    let mut fence = 0;
    for _ in 0..200 {
        mem.invalidate();
        word = mem.read32(base + SDMA_TEST_AT);
        fence = mem.read32(base + SDMA_SEQ_AT);
        if word == 0x5d4a_0001 && fence == 1 {
            break;
        }
        bus::manager().delay_us(1000);
    }
    log(format_args!(
        "SDMA: ring test: word {word:#010x} ({}), fence {fence} ({}); STATUS {:#x}, F32_CNTL {:#x}",
        if word == 0x5d4a_0001 {
            "seen"
        } else {
            "NOT seen"
        },
        if fence == 1 { "seen" } else { "NOT seen" },
        w.read32(amdgpu::sdma::STATUS_REG),
        w.read32(amdgpu::sdma::F32_CNTL),
    ));
    s.report(gpu, log);
    if word != 0x5d4a_0001 || fence != 1 {
        return Err(Stopped::Stalled(
            "SDMA",
            mem.read32(base + SDMA_REPORT_AT),
            0,
        ));
    }
    // The jobs' numbers start at 1: the test's fence word goes back to 0.
    gpu.vram_write32(usize::try_from(SDMA_SEQ_AT).unwrap_or(0), 0);
    let _ = gpu.vram_read32(0);
    Ok(s)
}

// Recovery (10 sections 4.2 to 4.4): a ring whose job did not finish is reset
// through the interface queue (graphics, compute) or by the engine's own
// soft reset (DMA), mapped again, and the jobs it held are completed by
// writing its fence word.

/// Writes `value` to a fence word (a video memory offset).
fn set_word(gpu: &Arc<Aperture>, at: u64, value: u32) {
    gpu.vram_write32(usize::try_from(at).unwrap_or(0), value);
    let _ = gpu.vram_read32(0);
}

impl Gfx {
    /// Completes every job of the graphics ring: its fence word reads `seq`.
    pub(crate) fn complete_gfx(&self, gpu: &Arc<Aperture>, seq: u32) {
        set_word(gpu, SEQ_AT, seq);
    }

    /// Completes every job of compute queue `n`.
    pub(crate) fn complete_compute(&self, gpu: &Arc<Aperture>, n: usize, seq: u32) {
        if let Some(r) = self.compute.get(n) {
            set_word(gpu, r.at + COMPUTE_SEQ, seq);
        }
    }

    /// Resets compute queue `n` (10 section 4.3) and maps it again: an
    /// unmap with the reset action through the interface queue, the queue
    /// seen inactive, its descriptor and ring made new.
    ///
    /// # Errors
    ///
    /// [`Stopped`] where it stopped.
    pub(crate) fn reset_compute(
        &mut self,
        gpu: &Arc<Aperture>,
        n: usize,
        log: &dyn Fn(core::fmt::Arguments<'_>),
    ) -> Result<(), Stopped> {
        let w = Bar5(gpu);
        let base = self.base;
        let mem = crate::psp::vram_memory(gpu, base);
        let hw = u32::try_from(n).unwrap_or(0);
        let (pipe, queue, doorbell, _) = mqd::kernel_compute_place(hw);
        let seq = self.compute.get(n).map_or(0, |r| r.at + COMPUTE_SEQ);
        let kept = if seq == 0 {
            0
        } else {
            mem.invalidate();
            mem.read32(base + seq)
        };
        // 1. Unmap with the reset action, and see the queue inactive.
        let p = mqd::unmap_queues(mqd::Unmap::Reset, false, doorbell, None);
        self.kiq.submit(gpu, &mem, &p.0);
        self.kiq.drain("KIQ unmap", base, &mem, 200)?;
        gc::select(&w, 1, pipe, queue, 0);
        let mut active = true;
        for _ in 0..200 {
            if w.read32(mqd::hqd_register(mqd::field::ACTIVE)) & 1 == 0 {
                active = false;
                break;
            }
            bus::manager().delay_us(1000);
        }
        gc::select(&w, 0, 0, 0, 0);
        if active {
            return Err(Stopped::Stalled("compute queue reset", 1, hw));
        }
        // 2. A new descriptor and ring, mapped again.
        let (q, at) = compute_queue(base, hw);
        for off in (0..0x1_0400u64).step_by(4) {
            gpu.vram_write32(usize::try_from(at + off).unwrap_or(0), 0);
        }
        put(gpu, at + COMPUTE_MQD, &mqd::kernel_compute_mqd(&q, hw));
        let _ = gpu.vram_read32(0);
        amdgpu::flip::hdp_flush(&w);
        let mut map = mqd::set_resources(mqd::KERNEL_COMPUTE_MASK);
        map.0
            .extend(mqd::map_compute_queue(pipe, queue, doorbell, q.mqd, q.wptr_poll).0);
        self.kiq.submit(gpu, &mem, &map.0);
        self.kiq.drain("KIQ map compute", base, &mem, 200)?;
        if let Some(r) = self.compute.get_mut(n) {
            r.wptr = 0;
        }
        // The fence word keeps its number (the caller completes the jobs).
        set_word(gpu, at + COMPUTE_SEQ, kept);
        // 3. The queue answers a test write.
        let word = 0xc0de_1000 | hw;
        let mut t = Packets::new();
        t.write_data_mem(base + at + COMPUTE_TEST, &[word]);
        if let Some(r) = self.compute.get_mut(n) {
            r.submit(gpu, &mem, &t.0);
        }
        let mut seen = 0;
        for _ in 0..200 {
            mem.invalidate();
            seen = mem.read32(base + at + COMPUTE_TEST);
            if seen == word {
                break;
            }
            bus::manager().delay_us(1000);
        }
        log(format_args!(
            "GC: compute queue {n} reset: test word {seen:#010x} ({})",
            if seen == word { "seen" } else { "NOT seen" }
        ));
        if seen == word {
            Ok(())
        } else {
            Err(Stopped::Stalled("compute queue test", seen, word))
        }
    }

    /// Resets the graphics queue (10 section 4.2) and maps it again: the
    /// reset request for the user address spaces and queue 0, the queue
    /// descriptor seen inactive and the request done, a new descriptor and
    /// ring, the ring's start sequence again.
    ///
    /// # Errors
    ///
    /// [`Stopped`] where it stopped.
    pub(crate) fn reset_gfx(
        &mut self,
        gpu: &Arc<Aperture>,
        log: &dyn Fn(core::fmt::Arguments<'_>),
    ) -> Result<(), Stopped> {
        let w = Bar5(gpu);
        let base = self.base;
        let mem = crate::psp::vram_memory(gpu, base);
        mem.invalidate();
        let kept = mem.read32(base + SEQ_AT);
        // 1. The reset request: the address spaces 1 to 7, queue 0 of pipe 0.
        w.write32(gc::CP_VMID_RESET, gc::vmid_reset_value(0xfffe, 0));
        // 2. The descriptor says inactive; 3. the request reads 0.
        let mut inactive = false;
        for _ in 0..200 {
            mem.invalidate();
            if mem.read32(base + GFX_MQD_AT + 0x208) & 1 == 0 {
                inactive = true;
                break;
            }
            bus::manager().delay_us(1000);
        }
        let mut done = false;
        for _ in 0..200 {
            if w.read32(gc::CP_VMID_RESET) == 0 {
                done = true;
                break;
            }
            bus::manager().delay_us(1000);
        }
        log(format_args!(
            "GC: graphics queue reset: descriptor {}, request {}",
            if inactive { "inactive" } else { "STILL ACTIVE" },
            if done { "done" } else { "NOT done" }
        ));
        if !inactive || !done {
            return Err(Stopped::Stalled("graphics queue reset", 0, 0));
        }
        // 4. A new descriptor and ring, mapped again.
        let q = Queue {
            mqd: base + GFX_MQD_AT,
            ring: base + GFX_AT,
            ring_bytes: GFX_RING_BYTES,
            rptr_report: base + GFX_REPORT_AT,
            wptr_poll: base + GFX_WPTR_POLL_AT,
            doorbell_dword: GFX_DOORBELL_DWORD,
            eop: 0,
        };
        for off in (0..0x1_1000u64).step_by(4) {
            gpu.vram_write32(usize::try_from(GFX_AT + off).unwrap_or(0), 0);
        }
        put(gpu, GFX_MQD_AT, &mqd::gfx_mqd(&q));
        let _ = gpu.vram_read32(0);
        amdgpu::flip::hdp_flush(&w);
        let mut map = mqd::set_resources(mqd::KERNEL_COMPUTE_MASK);
        map.0
            .extend(mqd::map_gfx_queue(0, GFX_DOORBELL_DWORD, q.mqd, q.wptr_poll).0);
        self.kiq.submit(gpu, &mem, &map.0);
        self.kiq.drain("KIQ map", base, &mem, 200)?;
        gc::select(&w, 0, 0, 0, 0);
        // 5. The ring's start sequence: the clear state and `SET_BASE`.
        self.gfx.wptr = 0;
        let mut p = gc::clear_state_buffer();
        p.extend(gc::set_base_packets());
        self.gfx.submit(gpu, &mem, &p);
        self.gfx.drain("graphics", base, &mem, 200)?;
        w.write32(
            gc::CP_INT_CNTL_RING0,
            w.read32(gc::CP_INT_CNTL_RING0) | gc::TIME_STAMP_INT_ENABLE,
        );
        set_word(gpu, SEQ_AT, kept);
        log(format_args!("GC: graphics queue mapped again"));
        Ok(())
    }

    /// Preempts compute queue `n` without unmapping it (10 section 4.3, the
    /// action `3`): the processor writes `value` at a word of the queue's
    /// memory when it is done. Returns whether the word was written.
    pub(crate) fn preempt_compute(
        &mut self,
        gpu: &Arc<Aperture>,
        n: usize,
        value: u32,
    ) -> Result<bool, Stopped> {
        let base = self.base;
        let mem = crate::psp::vram_memory(gpu, base);
        let hw = u32::try_from(n).unwrap_or(0);
        let (_, _, doorbell, _) = mqd::kernel_compute_place(hw);
        let at = self.compute.get(n).map_or(0, |r| r.at) + COMPUTE_PREEMPT;
        set_word(gpu, at, 0);
        let p = mqd::unmap_queues(
            mqd::Unmap::PreemptWithoutUnmapping,
            false,
            doorbell,
            Some((base + at, value)),
        );
        self.kiq.submit(gpu, &mem, &p.0);
        self.kiq.drain("KIQ preempt", base, &mem, 200)?;
        let mut seen = 0;
        for _ in 0..200 {
            mem.invalidate();
            seen = mem.read32(base + at);
            if seen == value {
                break;
            }
            bus::manager().delay_us(1000);
        }
        Ok(seen == value)
    }
}

impl Sdma {
    /// Preempts the queue in the middle of a buffer and lets it run again (07
    /// section 8): a fence packet after what is queued, the preempt register
    /// set, the fence seen, the register cleared. Returns whether the fence
    /// appeared.
    pub(crate) fn preempt(&mut self, gpu: &Arc<Aperture>, value: u32) -> bool {
        let w = Bar5(gpu);
        let base = self.base;
        let mem = crate::psp::vram_memory(gpu, base);
        let slot = SDMA_TEST_AT + 0x40;
        set_word(gpu, slot, 0);
        let mut p = amdgpu::sdma::Packets::new();
        p.fence(base + slot, value);
        self.submit(gpu, &mem, &p.0);
        w.write32(amdgpu::sdma::GFX_PREEMPT, 1);
        let mut seen = 0;
        for _ in 0..200 {
            mem.invalidate();
            seen = mem.read32(base + slot);
            if seen == value {
                break;
            }
            bus::manager().delay_us(1000);
        }
        w.write32(amdgpu::sdma::GFX_PREEMPT, 0);
        seen == value
    }

    /// Completes every job of the DMA engine.
    pub(crate) fn complete(&self, gpu: &Arc<Aperture>, seq: u32) {
        set_word(gpu, SDMA_SEQ_AT, seq);
    }

    /// Resets the engine (10 section 4.4) and starts its ring again, then
    /// tests it with a written word.
    ///
    /// # Errors
    ///
    /// [`Stopped::Stalled`] if the test word is not written.
    pub(crate) fn reset(
        &mut self,
        gpu: &Arc<Aperture>,
        log: &dyn Fn(core::fmt::Arguments<'_>),
    ) -> Result<(), Stopped> {
        let w = Bar5(gpu);
        let base = self.base;
        let mem = crate::psp::vram_memory(gpu, base);
        mem.invalidate();
        let kept = mem.read32(base + SDMA_SEQ_AT);
        amdgpu::sdma::stop(&w, SDMA_RING_BYTES);
        for at in (0..0x1_0200u64).step_by(4) {
            gpu.vram_write32(usize::try_from(SDMA_AT + at).unwrap_or(0), 0);
        }
        let _ = gpu.vram_read32(0);
        amdgpu::flip::hdp_flush(&w);
        let ring = amdgpu::sdma::Ring {
            base: base + SDMA_AT,
            bytes: SDMA_RING_BYTES,
            rptr_report: base + SDMA_REPORT_AT,
            wptr_poll: base + SDMA_POLL_AT,
        };
        amdgpu::sdma::recover(&w, &ring, &|us| bus::manager().delay_us(u64::from(us)));
        self.wptr = 0;
        set_word(gpu, SDMA_SEQ_AT, kept);
        let word = 0x5d4a_0002;
        set_word(gpu, SDMA_TEST_AT, 0);
        let mut p = amdgpu::sdma::Packets::new();
        p.write_linear(base + SDMA_TEST_AT, &[word]);
        self.submit(gpu, &mem, &p.0);
        let mut seen = 0;
        for _ in 0..200 {
            mem.invalidate();
            seen = mem.read32(base + SDMA_TEST_AT);
            if seen == word {
                break;
            }
            bus::manager().delay_us(1000);
        }
        log(format_args!(
            "SDMA: reset: test word {seen:#010x} ({})",
            if seen == word { "seen" } else { "NOT seen" }
        ));
        if seen == word {
            Ok(())
        } else {
            Err(Stopped::Stalled("SDMA reset", seen, word))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ring_has_room_until_the_pointers_would_meet() {
        // Empty: everything but the slack is free.
        assert!(has_room(0, 0, 1024, 1000, 1));
        assert!(!has_room(0, 0, 1024, 1023, 1));
        // The writer ahead of the reader.
        assert!(has_room(100, 50, 1024, 500, 1));
        assert!(!has_room(100, 50, 1024, 973, 1));
        // The writer wrapped behind the reader: the room is the gap.
        assert!(has_room(10, 500, 1024, 400, 1));
        assert!(!has_room(10, 500, 1024, 489, 1));
        // Room never counts what the engine has not read yet.
        assert!(!has_room(499, 500, 1024, 1, 1));
        // The DMA engine's slack of 8.
        assert!(has_room(0, 0, 1024, 1015, 8));
        assert!(!has_room(0, 0, 1024, 1016, 8));
    }

    #[test]
    fn the_cards_video_memory_has_no_overlapping_uses() {
        // Every region the driver puts in video memory, as (name, start,
        // length), must be disjoint from the others and inside the 512 MiB.
        let mib = 1u64 << 20;
        let regions: [(&str, u64, u64); 8] = [
            ("graphics ring block", GFX_AT, 0x1_3000),
            ("kernel interface queue", KIQ_AT, 0x4000),
            (
                "compute queues",
                COMPUTE_AT,
                COMPUTE_STRIDE * u64::from(COMPUTE_QUEUES),
            ),
            ("system DMA engine", SDMA_AT, 0x1_1000),
            (
                "address-space tables",
                crate::render::POOLS_AT,
                7 * crate::render::POOL_BYTES,
            ),
            (
                "fence packet pages",
                crate::render::FENCE_IB_AT,
                7 * crate::render::FENCE_IB_FILE_BYTES,
            ),
            // The address-space test's tables, and its buffer and data page.
            (
                "address-space test tables",
                VM_TABLES_AT,
                VM_TABLES as u64 * 4096,
            ),
            ("address-space test buffers", VM_IB_AT, 0x2000),
        ];
        for (i, a) in regions.iter().enumerate() {
            assert!(a.1 + a.2 <= 512 * mib, "{} leaves video memory", a.0);
            for b in &regions[i + 1..] {
                if a.2 == 0 || b.2 == 0 {
                    continue;
                }
                let apart = a.1 + a.2 <= b.1 || b.1 + b.2 <= a.1;
                assert!(apart, "{} overlaps {}", a.0, b.0);
            }
        }
    }

    #[test]
    fn the_compute_queues_memory_has_the_documented_alignment() {
        // The queue descriptor and the end-of-pipe buffer are 256-byte
        // aligned (06 section 8), and each queue's block holds them.
        assert_eq!(COMPUTE_MQD % 256, 0);
        assert_eq!(COMPUTE_EOP % 256, 0);
        assert_eq!(COMPUTE_AT % 256, 0);
    }
}
