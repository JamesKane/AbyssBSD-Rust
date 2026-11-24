// SPDX-License-Identifier: BSD-2-Clause
//! `nova`: the native driver for NVIDIA GPUs, as `drmn0` under `vgapci0`.
//!
//! At attach it reads the identification registers and the Video BIOS and
//! names the chip; it writes nothing, turns on no bus mastering, and does not
//! touch the display, which firmware set up and `gop-kms` keeps showing. The
//! stages that write to the GPU run only when asked, through sysctls
//! ([`control`]). A chip it does not know, or a device that does not
//! answer, is declined (the node stays in the tree without a driver).
//!
//! Layouts: the clean-room specification `nvidia-nova` (section 01).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod control;

use alloc::string::String;
use core::any::Any;

use alloc::sync::Arc;

use bus::{AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, StartCx, StopCx, StopReason};
use control::{Control, Prom};
use nvgpu::chip::{self, ChipError, Identity};
use nvgpu::vbios::{self, Descriptor};
use nvgpu::{fuse, mem};
use vgapci::{Aperture, Registers};

/// Says what the GPU's boot left in the registers the driver will need:
/// reads only.
fn report_state(aperture: &Aperture, log: &dyn Fn(core::fmt::Arguments<'_>)) {
    let r = |o: usize| aperture.read32(o).unwrap_or(u32::MAX);
    let mask = r(mem::NV_PGC6_AON_SECURE_SCRATCH_GROUP_05_PRIV_LEVEL_MASK);
    let boot =
        match mem::gfw_boot_done(mask, r(mem::NV_PGC6_AON_SECURE_SCRATCH_GROUP_05_0_GFW_BOOT)) {
            Some(true) => "done",
            Some(false) => "not done",
            None => "progress not readable",
        };
    let fb = mem::usable_fb_bytes(r(mem::NV_USABLE_FB_SIZE_IN_MB));
    let display_disabled = r(mem::NV_FUSE_STATUS_OPT_DISPLAY) & 1 != 0;
    let vga_register = r(mem::NV_PDISP_VGA_WORKSPACE_BASE);
    let vga = mem::vga_workspace_base(fb, display_disabled, vga_register);
    let wpr2 = mem::wpr2_bounds(
        r(mem::NV_PFB_PRI_MMU_WPR2_ADDR_LO),
        r(mem::NV_PFB_PRI_MMU_WPR2_ADDR_HI),
    );
    log(format_args!(
        "drmn0: boot firmware {boot}; {} MiB of video memory; display {}; VGA workspace at \
         {vga:#x} (register {vga_register:#010x}); protected region 2 {}",
        fb >> 20,
        if display_disabled {
            "fused off"
        } else {
            "present"
        },
        match wpr2 {
            Some((lo, hi)) => alloc::format!("{lo:#x}..{hi:#x}"),
            None => alloc::string::String::from("not set up"),
        },
    ));
    if let Ok(reg) = fuse::register(0x0400, 9) {
        let v = r(reg);
        log(format_args!(
            "drmn0: fuse version of GPU system processor microcode 9: register {reg:#x} = \
             {v:#010x}, version {}",
            fuse::version(v),
        ));
    }
}

/// Says what the Video BIOS holds, or why it could not be read.
fn report_vbios(aperture: &Aperture, log: &dyn Fn(core::fmt::Arguments<'_>)) {
    match vbios::read(&Prom(aperture)) {
        Ok(v) => {
            for i in &v.images {
                log(format_args!(
                    "drmn0: video BIOS image at {:#x}, {:#x} bytes, code type {:#04x}",
                    i.offset, i.size, i.code_type
                ));
            }
            match v.descriptor {
                Descriptor::V3(d) => log(format_args!(
                    "drmn0: firmware security microcode, descriptor version 3 at {:#x}: \
                     {:#x} bytes of instructions, {:#x} of data, engines {:#06x}, microcode {}, \
                     {} signature(s), signature patch at data offset {:#x}",
                    v.descriptor_offset,
                    d.imem_load_size,
                    d.dmem_load_size,
                    d.engine_id_mask,
                    d.ucode_id,
                    d.signature_count,
                    d.pkc_data_offset,
                )),
                Descriptor::V2(d) => log(format_args!(
                    "drmn0: firmware security microcode, descriptor version 2 at {:#x}: \
                     {:#x} bytes of instructions, {:#x} of data",
                    v.descriptor_offset, d.imem_load_size, d.dmem_load_size,
                )),
            }
        }
        Err(e) => log(format_args!("drmn0: video BIOS not read: {e:?}")),
    }
}

/// `drmnN`.
#[derive(Debug)]
pub struct Nova {
    /// Which chip this is.
    identity: Identity,
    control: Arc<Control>,
}

impl Driver for Nova {
    const NAME: &'static str = "drmn";
    const IDS: Ids = Ids::Nexus(&["drmn"]);

    fn describe(_: &Ident) -> String {
        "NVIDIA GPU".into()
    }

    fn describe_with(_: &Ident, _: Option<&(dyn Any + Send + Sync)>) -> String {
        "NVIDIA GPU".into()
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let registers = cx.bus_data_arc::<Registers>().ok_or(AttachError::Decline)?;
        let aperture = registers.0.upgrade().ok_or(AttachError::Decline)?;
        let boot_0 = aperture
            .read32(chip::NV_PMC_BOOT_0)
            .ok_or(AttachError::Decline)?;
        let identity = chip::identify(boot_0, || {
            aperture.read32(chip::NV_PMC_BOOT_42).unwrap_or(u32::MAX)
        });
        let log = |args: core::fmt::Arguments<'_>| bus::manager().log(args);
        match identity {
            Ok(identity) => {
                log(format_args!(
                    "drmn0: NVIDIA {} (architecture {:#x}, revision {:x}.{:x}), identified; no \
                     driver beyond this yet",
                    identity.chipset.name(),
                    identity.chipset.architecture().code(),
                    identity.major,
                    identity.minor,
                ));
                report_state(&aperture, &log);
                report_vbios(&aperture, &log);
                Ok(Self {
                    identity,
                    control: Arc::new(Control::new(registers.0.clone(), boot_0)),
                })
            }
            Err(ChipError::Unsupported { id }) => {
                log(format_args!(
                    "drmn0: NVIDIA chipset {id:#x} is not supported"
                ));
                Err(AttachError::Decline)
            }
            Err(e) => {
                log(format_args!(
                    "drmn0: not an NVIDIA GPU this code knows ({e:?})"
                ));
                Err(AttachError::Decline)
            }
        }
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let c = Arc::clone(&self.control);
        let unit = cx.unit();
        bus::manager().sysctl(cx.sleepable(), &mut |t| {
            if let Some(node) = bus::device_node(t, Self::NAME, unit) {
                control::publish(t, node, &c);
            }
        });
        Ok(())
    }

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {
        let _ = self.identity;
        // The flush registers must not point at memory that is freed.
        self.control.release();
    }
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<Nova>();
