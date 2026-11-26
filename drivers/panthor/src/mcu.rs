// SPDX-License-Identifier: BSD-2-Clause
//! Booting the GPU's microcontroller (panthor specification 05 §5.5, §5.6): the L2 powered
//! (03 §3.4.1, with the Sky1's system-cache values first, 11 §11.8), the firmware image loaded
//! into its own address space on slot 0 (05 §5.3, 04), the MCU started, its interface found
//! (05 §5.4), the global configuration given, and a ping answered.
//!
//! It runs on the driver's own thread when `dev.panthor.N.mcu_boot` is set, never at attach, so
//! a boot that stops the machine cannot keep the installed kernel from booting. Each step is
//! logged before it runs.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use panthor::csf::{self, Iface, Shared};
use panthor::fw::{self, Section};
use panthor::mmu::{self, Map, Tables};
use panthor::regs::{self as r, Regs};
use panthor::{control, sky1};
use sched::Sleepable;
use vm::dma::{DmaCoherent, DmaTag};

use crate::{Spin, Window};

/// Where the firmware store keeps the image (FreeBSD's `/boot/firmware`, 05 §5.1's path below).
const STORE: &str = "/boot/firmware/";
/// The most sections an image may have.
const SECTIONS: usize = 16;
/// Doorbell 0: the global interface (02 §2.9).
const DOORBELL_GLOBAL: usize = 0x8_0000;
/// JOB interrupt bit 31: the global interface (02 §2.6), the firmware's boot signal.
const JOB_GLOBAL: u32 = 1 << 31;

/// The page tables, in device memory.
pub(crate) struct TableMem<'a> {
    tag: &'a DmaTag,
    pub(crate) pages: Vec<DmaCoherent<u64>>,
}

impl TableMem<'_> {
    fn page(&self, pa: u64) -> Option<&DmaCoherent<u64>> {
        self.pages.iter().find(|p| p.bus_addr() == pa)
    }
}

impl Tables for TableMem<'_> {
    fn alloc(&mut self) -> Option<u64> {
        let p = self.tag.coherent::<u64>(512).ok()?;
        let pa = p.bus_addr();
        self.pages.push(p);
        Some(pa)
    }
    // Every table address the walk meets is one `alloc` gave: a miss reads as empty.
    fn read(&self, table: u64, index: usize) -> u64 {
        self.page(table).map_or(0, |p| p.read(index))
    }
    fn write(&mut self, table: u64, index: usize, desc: u64) {
        if let Some(p) = self.page(table) {
            p.write(index, desc);
        }
    }
}

/// The shared section.
struct SharedMem<'a>(&'a DmaCoherent<u8>);

impl Shared for SharedMem<'_> {
    fn len(&self) -> usize {
        self.0.len()
    }
    fn read32(&self, off: usize) -> u32 {
        self.0.read_at::<u32>(off)
    }
    fn write32(&self, off: usize, v: u32) {
        self.0.write_at::<u32>(off, v);
    }
}

/// What a boot leaves running: the MCU's memory, which must stay while it runs.
pub(crate) struct Booted {
    pub(crate) iface: Iface,
    _tables: Vec<DmaCoherent<u64>>,
    _sections: Vec<DmaCoherent<u8>>,
    pub(crate) git_sha: String,
}

/// Boots the MCU. `log` writes one line.
pub(crate) fn boot(
    s: &Sleepable,
    gpu: &Window,
    tag: &DmaTag,
    info: &panthor::id::Info,
    core_hz: u64,
    log: &dyn Fn(core::fmt::Arguments<'_>),
) -> Result<Booted, &'static str> {
    // 1. The L2, after the coherency protocol and the system-cache allocation.
    let proto = sky1::coherency_protocol(info);
    log(format_args!(
        "L2: system cache, coherency protocol {proto:#x}, power up"
    ));
    sky1::program_system_cache(gpu);
    control::l2_power_up(gpu, &mut Spin, proto).map_err(|e| {
        log(format_args!("L2 power-up: {e:?}"));
        "L2 power-up failed"
    })?;
    log(format_args!("L2 ready {:#x}", gpu.read64(r::L2_READY)));

    // 2. The image.
    let mut name = [0u8; 64];
    let n = fw::path(
        info.gpu_id.arch_major(),
        info.gpu_id.arch_minor(),
        &mut name,
    );
    let path = format!("{STORE}{}", core::str::from_utf8(&name[..n]).unwrap_or(""));
    let file = crate::firmware()
        .ok_or("no firmware reader")?
        .read(s, &path)
        .map_err(|e| {
            log(format_args!("reading {path}: {e:?}"));
            "firmware image missing"
        })?;
    let img = fw::parse::<SECTIONS>(&file).map_err(|e| {
        log(format_args!("firmware image refused: {e:?}"));
        "firmware image refused"
    })?;
    let git_sha = img
        .git_sha
        .and_then(|(a, b)| core::str::from_utf8(&file[a..b]).ok())
        .unwrap_or("")
        .trim_end()
        .into();
    log(format_args!(
        "loaded firmware image '{path}': {} sections ({} protected skipped), git sha {git_sha}",
        img.count, img.skipped_protected
    ));

    // 3. The MCU's address space: every section at its address.
    let va_bits = info.va_bits();
    let mut t = TableMem {
        tag,
        pages: Vec::new(),
    };
    let root = t.alloc().ok_or("no memory for the page tables")?;
    let mut sections = Vec::new();
    let mut shared_index = None;
    for sec in img.sections.iter().flatten() {
        let mem = load(tag, &file, sec)?;
        let m = Map {
            read_only: !sec.writable(),
            no_exec: !sec.executable(),
            cacheable: sec.cacheable(),
        };
        mmu::map(
            &mut t,
            root,
            va_bits,
            u64::from(sec.va_start),
            mem.bus_addr(),
            u64::from(sec.size()),
            m,
        )
        .map_err(|e| {
            log(format_args!("mapping {:#x}: {e:?}", sec.va_start));
            "MCU mapping failed"
        })?;
        if sec.va_start == fw::SHARED_VA {
            shared_index = Some(sections.len());
        }
        sections.push(mem);
    }
    let shared = &sections[shared_index.ok_or("no interface section")?];
    log(format_args!(
        "MCU address space: {} table pages, root {root:#x}; binding slot 0",
        t.pages.len()
    ));
    mmu::enable(
        gpu,
        &mut Spin,
        0,
        root,
        mmu::memattr(false),
        mmu::transcfg(va_bits, false),
    )
    .map_err(|e| {
        log(format_args!("slot 0: {e:?}"));
        "binding the MCU's address space failed"
    })?;

    // 4. Start it and wait for the global interface's signal (polled: no handler yet).
    gpu.write32(r::JOB_INT + r::INT_CLEAR, u32::MAX);
    log(format_args!("MCU_CONTROL = auto; waiting for the firmware"));
    gpu.write32(r::MCU_CONTROL, 2);
    let mut booted = false;
    for _ in 0..1000 {
        if gpu.read32(r::JOB_INT + r::INT_RAWSTAT) & JOB_GLOBAL != 0 {
            booted = true;
            break;
        }
        bus::manager().sleep_ms(s, 1);
    }
    let status = gpu.read32(r::MCU_STATUS);
    if !booted {
        log(format_args!(
            "the firmware did not signal: MCU_STATUS {status}, slot 0 fault {:#x} at {:#x}, GPU fault {:#x}",
            gpu.read32(mmu::as_reg(0, mmu::AS_FAULTSTATUS)),
            gpu.read64(mmu::as_reg(0, mmu::AS_FAULTADDRESS)),
            gpu.read32(r::GPU_FAULT_STATUS)
        ));
        return Err("the firmware did not boot");
    }
    gpu.write32(r::JOB_INT + r::INT_CLEAR, u32::MAX);

    // 5. The interface, the configuration, a ping.
    let sh = SharedMem(shared);
    let iface = csf::discover(&sh).map_err(|e| {
        log(format_args!("interface: {e:?}"));
        "interface not found"
    })?;
    log(format_args!(
        "CSF FW using interface v{}, Features {:#x} Instrumentation features {:#x}",
        iface.version, iface.features, iface.instr_features
    ));
    log(format_args!(
        "{} command-stream groups of {} streams, {} work registers, MCU_STATUS {status}",
        iface.groups,
        iface.streams,
        iface.work_registers()
    ));
    csf::configure(&sh, &iface, info.shader_present, core_hz);
    ring(gpu);
    let acked = |bits| {
        for _ in 0..100 {
            if csf::acked(&sh, &iface, bits) {
                return true;
            }
            bus::manager().sleep_ms(s, 1);
        }
        csf::acked(&sh, &iface, bits)
    };
    if !acked(csf::CONFIG_BITS) {
        log(format_args!(
            "the global configuration was not acknowledged"
        ));
        return Err("configuration not acknowledged");
    }
    csf::ping(&sh, &iface);
    ring(gpu);
    if !acked(csf::REQ_PING) {
        log(format_args!("the ping was not answered"));
        return Err("ping not answered");
    }
    log(format_args!("firmware configured and answering"));
    Ok(Booted {
        iface,
        _tables: t.pages,
        _sections: sections,
        git_sha,
    })
}

/// Rings doorbell 0, after the shared memory's writes (05 §5.6.3 step 4).
fn ring(gpu: &Window) {
    hal::dma::doorbell_barrier();
    gpu.write32(DOORBELL_GLOBAL, 1);
}

/// One section's memory, with its initial contents (05 §5.3.4); the rest is zero.
fn load(tag: &DmaTag, file: &[u8], sec: &Section) -> Result<DmaCoherent<u8>, &'static str> {
    let mem = tag
        .coherent::<u8>(sec.size() as usize)
        .map_err(|_| "no memory for a firmware section")?;
    let data = file
        .get(sec.data_start as usize..sec.data_end as usize)
        .ok_or("section data outside the file")?;
    mem.copy_from_slice(0, data);
    Ok(mem)
}
