// SPDX-License-Identifier: BSD-2-Clause
//! The NVIDIA side of `drmn` (the graphics device node of FreeBSD's `drm-kmod`:
//! see the crate's module comment): the sysctls that run the GPU's write stages, one at a time, on request:
//! nothing here runs at attach, so a stage that hangs the machine can
//! never keep the installed kernel from booting.
//!
//! Each stage logs what it is about to do before it does it, so the log
//! saved to the boot disk's partition names the last step reached.
//!
//! - `dev.drmn.N.engine` (read-only): what the engines and registers show.
//! - `dev.drmn.N.flush_page` (write 1 or 0): register or clear the system
//!   memory flush page (the first thing the GPU needs before an engine
//!   reset).
//! - `dev.drmn.N.gsp_reset` (write 1): reset the GPU system processor.
//! - `dev.drmn.N.frts` (write 1): run the firmware security microcode's
//!   FRTS command from the card's own Video BIOS.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};

use nvgpu::boot::{self, BootError};
use nvgpu::falcon::{self, GSP, Registers, SEC2};
use nvgpu::vbios::{self, Descriptor, WordSource};
use nvgpu::{fuse, fwsec, mem};
use sync::SpinLock;
use vgapci::Aperture;
use vm::dma::DmaCoherent;

/// The GPU's registers for `nvgpu`'s sequences.
struct Hw<'a>(&'a Aperture);

impl Registers for Hw<'_> {
    fn read32(&self, offset: usize) -> u32 {
        self.0.read32(offset).unwrap_or(u32::MAX)
    }
    fn write32(&self, offset: usize, value: u32) {
        self.0.write32(offset, value);
    }
    fn delay_us(&self, us: u32) {
        bus::manager().delay_us(u64::from(us));
    }
}

/// The Video BIOS window, for `nvgpu`'s reader.
pub(crate) struct Prom<'a>(pub(crate) &'a Aperture);

impl WordSource for Prom<'_> {
    fn read32(&self, offset: usize) -> Option<u32> {
        if offset >= vbios::WINDOW {
            return None;
        }
        self.0.read32(vbios::WINDOW_BASE.checked_add(offset)?)
    }
}

/// The 4 KiB page the GPU's memory barriers target, while registered.
struct Flush {
    page: DmaCoherent<u8>,
    address: u64,
}

/// A GPU being driven from its sysctls.
pub(crate) struct Control {
    name: String,
    gpu: Weak<Aperture>,
    boot_0: u32,
    flush: SpinLock<Option<Flush>>,
}

impl core::fmt::Debug for Control {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Control").finish_non_exhaustive()
    }
}

impl Control {
    pub(crate) fn new(name: String, gpu: Weak<Aperture>, boot_0: u32) -> Self {
        Self {
            name,
            gpu,
            boot_0,
            flush: SpinLock::new(None),
        }
    }

    fn log(&self, args: core::fmt::Arguments<'_>) {
        bus::manager().log(format_args!("{}: {args}", self.name));
    }

    fn gpu(&self) -> Result<Arc<Aperture>, abi::errno::Errno> {
        self.gpu.upgrade().ok_or(abi::errno::Errno::ENXIO)
    }

    /// What the engines and registers show.
    pub(crate) fn report(&self) -> String {
        let Ok(gpu) = self.gpu() else {
            return "the GPU is gone".into();
        };
        let hw = Hw(&gpu);
        let line = |name: &str, e| {
            let s = falcon::state(&hw, e);
            format!(
                "{name}: hwcfg1 {:#x} hwcfg2 {:#x} cpuctl {:#x} bcr_ctrl {:#x} mailbox0 {:#x}",
                s.hwcfg1, s.hwcfg2, s.cpuctl, s.bcr_ctrl, s.mailbox0
            )
        };
        let flush = (hw.read32(0x0010_0c40) & 0x00ff_ffff, hw.read32(0x0010_0c10));
        format!(
            "{}; {}; frts error {:#x}; protected region 2 {:?}; flush registers {:#x}:{:#x}; \
             pci command {:#x}",
            line("gsp", GSP),
            line("sec2", SEC2),
            hw.read32(falcon::FRTS_ERR) >> 16,
            boot::wpr2(&hw),
            flush.0,
            flush.1,
            gpu.pci().read16(4),
        )
    }

    /// Registers (or clears) the flush page.
    pub(crate) fn flush_page(&self, on: bool) -> Result<(), abi::errno::Errno> {
        use abi::errno::Errno;
        let gpu = self.gpu()?;
        let mut slot = self.flush.lock();
        if !on {
            self.log(format_args!("clearing the flush page registers"));
            self.clear_flush(&gpu, &mut slot);
            return Ok(());
        }
        if slot.is_some() {
            return Ok(());
        }
        self.log(format_args!("allocating the flush page"));
        let page = gpu
            .dma_tag()
            .ok_or(Errno::ENODEV)?
            .coherent::<u8>(4096)
            .map_err(|_| Errno::ENOMEM)?;
        let address = page.bus_addr();
        let (hi, lo) = mem::flush_registers_ada(address).ok_or(Errno::EINVAL)?;
        self.log(format_args!(
            "registering the flush page at {address:#x} (high {hi:#x}, low {lo:#x})"
        ));
        gpu.write32(0x0010_0c40, hi);
        gpu.write32(0x0010_0c10, lo);
        let back = (
            gpu.read32(0x0010_0c40).unwrap_or(0) & 0x00ff_ffff,
            gpu.read32(0x0010_0c10).unwrap_or(0),
        );
        if back != (hi, lo) {
            self.log(format_args!("the flush registers read back {back:x?}"));
            gpu.write32(0x0010_0c40, 0);
            gpu.write32(0x0010_0c10, 0);
            return Err(Errno::EIO);
        }
        self.log(format_args!("flush page registered"));
        *slot = Some(Flush { page, address });
        Ok(())
    }

    fn clear_flush(&self, gpu: &Aperture, slot: &mut Option<Flush>) {
        if let Some(f) = slot.take() {
            // The registers must not point at memory that is freed.
            gpu.write32(0x0010_0c40, 0);
            gpu.write32(0x0010_0c10, 0);
            self.log(format_args!("flush page at {:#x} released", f.address));
            drop(f.page);
        }
    }

    /// Clears the flush registers before the page goes (the driver's stop).
    pub(crate) fn release(&self) {
        if let Ok(gpu) = self.gpu() {
            let mut slot = self.flush.lock();
            self.clear_flush(&gpu, &mut slot);
        }
    }

    fn need_flush(&self) -> Result<(), abi::errno::Errno> {
        if self.flush.lock().is_none() {
            self.log(format_args!("refused: register the flush page first"));
            return Err(abi::errno::Errno::EINVAL);
        }
        Ok(())
    }

    /// Resets the GPU system processor.
    pub(crate) fn gsp_reset(&self) -> Result<(), abi::errno::Errno> {
        let gpu = self.gpu()?;
        self.need_flush()?;
        self.log(format_args!("resetting the GPU system processor"));
        match falcon::reset(&Hw(&gpu), GSP, self.boot_0) {
            Ok(()) => {
                self.log(format_args!("GPU system processor reset done"));
                Ok(())
            }
            Err(e) => {
                self.log(format_args!("reset failed: {e:?}"));
                Err(abi::errno::Errno::EIO)
            }
        }
    }

    /// Runs the FRTS command.
    pub(crate) fn frts(&self) -> Result<(), abi::errno::Errno> {
        use abi::errno::Errno;
        let gpu = self.gpu()?;
        self.need_flush()?;
        let hw = Hw(&gpu);
        self.log(format_args!("FRTS: reading the Video BIOS"));
        let v = vbios::read(&Prom(&gpu)).map_err(|e| {
            self.log(format_args!("FRTS: Video BIOS: {e:?}"));
            Errno::EIO
        })?;
        let Descriptor::V3(d) = v.descriptor else {
            self.log(format_args!("FRTS: not a version 3 descriptor"));
            return Err(Errno::ENOTSUP);
        };
        let fb = mem::usable_fb_bytes(hw.read32(mem::NV_USABLE_FB_SIZE_IN_MB));
        let vga = mem::vga_workspace_base(
            fb,
            hw.read32(mem::NV_FUSE_STATUS_OPT_DISPLAY) & 1 != 0,
            hw.read32(mem::NV_PDISP_VGA_WORKSPACE_BASE),
        );
        let frts = mem::frts_region(vga).ok_or(Errno::EINVAL)?;
        let fuse_reg = fuse::register(d.engine_id_mask, d.ucode_id).map_err(|_| Errno::EINVAL)?;
        let reg_version = fuse::version(hw.read32(fuse_reg));
        self.log(format_args!(
            "FRTS: region {:#x}..{:#x}, fuse version {reg_version}, signature versions {:#x}",
            frts.start, frts.end, d.signature_versions
        ));
        let prepared = fwsec::prepare(
            &v,
            fwsec::Command::Frts {
                addr: frts.start,
                size: frts.end - frts.start,
            },
            reg_version,
        )
        .map_err(|e| {
            self.log(format_args!("FRTS: preparing the microcode: {e:?}"));
            Errno::EINVAL
        })?;
        self.log(format_args!(
            "FRTS: image {:#x} bytes; instructions {:?}, data {:?}",
            prepared.image.len(),
            prepared.imem,
            prepared.dmem
        ));
        let buf = gpu
            .dma_tag()
            .ok_or(Errno::ENODEV)?
            .coherent::<u8>(prepared.image.len())
            .map_err(|_| Errno::ENOMEM)?;
        for (i, b) in prepared.image.iter().enumerate() {
            buf.write(i, *b);
        }
        let dma = buf.bus_addr();
        self.log(format_args!(
            "FRTS: image in memory at {dma:#x}; turning on bus mastering"
        ));
        gpu.pci().set_bus_master(true);
        let result = boot::run_frts(&hw, self.boot_0, &prepared, dma, frts, &mut |step| {
            self.log(format_args!("FRTS: {step}"));
        });
        match result {
            Ok(()) => {
                self.log(format_args!(
                    "FRTS done: protected region 2 is {:?}",
                    boot::wpr2(&hw)
                ));
                Ok(())
            }
            Err(e) => {
                self.log(format_args!("FRTS failed: {e:?}; {}", self.report()));
                Err(match e {
                    BootError::Wpr2Active => Errno::EBUSY,
                    _ => Errno::EIO,
                })
            }
        }
    }
}

/// A sysctl that runs `action` when 1 is written (and shows 0).
pub(crate) fn trigger(
    t: &mut sysctl::Tree,
    node: sysctl::NodeId,
    name: &str,
    description: &str,
    action: impl Fn() -> Result<(), abi::errno::Errno> + Send + Sync + 'static,
) {
    let _ = t.add_writable(
        node,
        sysctl::Number::Auto,
        name,
        description,
        0,
        sysctl::Value::Int(Box::new(|| 0)),
        sysctl::int_writer(move |v| if v == 1 { action() } else { Ok(()) }),
    );
}

/// The sysctls for `c`, under `node`.
pub(crate) fn publish(t: &mut sysctl::Tree, node: sysctl::NodeId, c: &Arc<Control>) {
    let r = Arc::clone(c);
    let _ = t.add_value(
        node,
        sysctl::Number::Auto,
        "engine",
        "What the engines and registers show",
        sysctl::Value::Str(Box::new(move |out| out.push_str(&r.report()))),
    );
    let f = Arc::clone(c);
    let _ = t.add_writable(
        node,
        sysctl::Number::Auto,
        "flush_page",
        "Register (1) or clear (0) the system memory flush page",
        0,
        sysctl::Value::Int(Box::new(|| 0)),
        sysctl::int_writer(move |v| f.flush_page(v != 0)),
    );
    let r = Arc::clone(c);
    trigger(
        t,
        node,
        "gsp_reset",
        "Write 1 to reset the GPU system processor",
        move || r.gsp_reset(),
    );
    let r = Arc::clone(c);
    trigger(
        t,
        node,
        "frts",
        "Write 1 to run the firmware security microcode's FRTS command",
        move || r.frts(),
    );
}
