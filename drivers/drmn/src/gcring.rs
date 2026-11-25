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
    Ok(Gfx {
        base,
        kiq: kiq_ring,
        gfx,
    })
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
        let mem = crate::psp::vram_memory(gpu, self.base);
        let size = self.gfx.bytes / 4;
        let need = u32::try_from(packets.len()).map_err(|_| abi::errno::Errno::E2BIG)?;
        if need + 1 >= size {
            return Err(abi::errno::Errno::E2BIG);
        }
        for _ in 0..=ms {
            mem.invalidate();
            let rptr = mem.read32(self.base + self.gfx.report) % size;
            let at = u32::try_from(self.gfx.wptr % u64::from(size)).unwrap_or(0);
            let used = (at + size - rptr) % size;
            if size - used > need + 1 {
                self.gfx.submit(gpu, &mem, packets);
                return Ok(());
            }
            bus::manager().delay_us(1000);
        }
        Err(abi::errno::Errno::EAGAIN)
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
