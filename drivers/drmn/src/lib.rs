// SPDX-License-Identifier: BSD-2-Clause
//! `drmn`: the native driver for GPUs, as `drmn0` under `vgapci0`.
//!
//! `drmn` is the name FreeBSD's `drm-kmod` gives a graphics device's node
//! (`drmn0`): the device a native GPU driver attaches as, under the
//! display function's `vgapci` node. "DRM" is the Direct Rendering
//! Manager, the kernel's graphics interface.
//!
//! One driver in the tree, two vendors behind it, told apart by the PCI
//! vendor `vgapci` passes down:
//!
//! - NVIDIA ([`control`]): reads the identification registers and the Video
//!   BIOS at attach; the stages that write to the GPU run only on request,
//!   through sysctls.
//! - AMD ([`amd`]): reads two fixed registers at attach; the IP discovery
//!   table and the display state the firmware left are read on request.
//!
//! At attach nothing is written to the GPU, no bus mastering is turned on,
//! and the display (which firmware set up and `gop-kms` keeps showing) is not
//! touched. A chip it does not know, or a device that does not answer, is
//! declined (the node stays in the tree without a driver).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

mod amd;
mod control;
mod kms;

use alloc::string::String;
use alloc::sync::Arc;
use core::any::Any;

use bus::{AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, StartCx, StopCx, StopReason};
use control::{Control, Prom};
use nvgpu::chip::{self, ChipError, Identity};
use nvgpu::vbios::{self, Descriptor};
use nvgpu::{fuse, mem};
use vgapci::{Aperture, Registers};

const VENDOR_NVIDIA: u16 = 0x10de;
const VENDOR_AMD: u16 = 0x1002;

/// Says what the NVIDIA GPU's boot left in the registers the driver will
/// need: reads only.
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
        "boot firmware {boot}; {} MiB of video memory; display {}; VGA workspace at \
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
            "fuse version of GPU system processor microcode 9: register {reg:#x} = \
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
                    "video BIOS image at {:#x}, {:#x} bytes, code type {:#04x}",
                    i.offset, i.size, i.code_type
                ));
            }
            match v.descriptor {
                Descriptor::V3(d) => log(format_args!(
                    "firmware security microcode, descriptor version 3 at {:#x}: \
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
                    "firmware security microcode, descriptor version 2 at {:#x}: \
                     {:#x} bytes of instructions, {:#x} of data",
                    v.descriptor_offset, d.imem_load_size, d.dmem_load_size,
                )),
            }
        }
        Err(e) => log(format_args!("video BIOS not read: {e:?}")),
    }
}

/// Whether a Radeon's display becomes a graphics card (`hw.drmn.kms`).
static KMS: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Takes the boot-time tunables (`TUNABLE_INT_FETCH`): `get` gives a
/// name's integer value, if set. The kernel calls it before drivers
/// attach.
pub fn set_tunables(get: &dyn Fn(&str) -> Option<i32>) {
    if let Some(v) = get("hw.drmn.kms") {
        // Relaxed: a tunable, read at start.
        KMS.store(v != 0, core::sync::atomic::Ordering::Relaxed);
    }
}

/// What is behind a `drmn` node.
#[derive(Debug)]
enum Backend {
    Nvidia {
        identity: Identity,
        control: Arc<Control>,
    },
    Amd(Arc<amd::Amd>),
}

/// `drmnN`.
#[derive(Debug)]
pub struct Drmn {
    backend: Backend,
}

impl Driver for Drmn {
    const NAME: &'static str = "drmn";
    const IDS: Ids = Ids::Nexus(&["drmn"]);

    fn describe(_: &Ident) -> String {
        "GPU".into()
    }

    fn describe_with(_: &Ident, bus_data: Option<&(dyn Any + Send + Sync)>) -> String {
        match bus_data.and_then(|d| d.downcast_ref::<Registers>()) {
            Some(Registers(_, VENDOR_NVIDIA)) => "NVIDIA GPU".into(),
            Some(Registers(_, VENDOR_AMD)) => "AMD GPU".into(),
            _ => "GPU".into(),
        }
    }

    fn attach(cx: &mut AttachCx<'_>) -> Result<Self, AttachError> {
        let registers = cx.bus_data_arc::<Registers>().ok_or(AttachError::Decline)?;
        let aperture = registers.0.upgrade().ok_or(AttachError::Decline)?;
        let name = alloc::format!("{}{}", Self::NAME, cx.unit());
        let log = |args: core::fmt::Arguments<'_>| {
            bus::manager().log(format_args!("{name}: {args}"));
        };
        match registers.1 {
            VENDOR_NVIDIA => {
                let boot_0 = aperture
                    .read32(chip::NV_PMC_BOOT_0)
                    .ok_or(AttachError::Decline)?;
                match chip::identify(boot_0, || {
                    aperture.read32(chip::NV_PMC_BOOT_42).unwrap_or(u32::MAX)
                }) {
                    Ok(identity) => {
                        log(format_args!(
                            "NVIDIA {} (architecture {:#x}, revision {:x}.{:x}), identified; no \
                             driver beyond this yet",
                            identity.chipset.name(),
                            identity.chipset.architecture().code(),
                            identity.major,
                            identity.minor,
                        ));
                        report_state(&aperture, &log);
                        report_vbios(&aperture, &log);
                        Ok(Self {
                            backend: Backend::Nvidia {
                                identity,
                                control: Arc::new(Control::new(
                                    name.clone(),
                                    registers.0.clone(),
                                    boot_0,
                                )),
                            },
                        })
                    }
                    Err(ChipError::Unsupported { id }) => {
                        log(format_args!("NVIDIA chipset {id:#x} is not supported"));
                        Err(AttachError::Decline)
                    }
                    Err(e) => {
                        log(format_args!("not an NVIDIA GPU this code knows ({e:?})"));
                        Err(AttachError::Decline)
                    }
                }
            }
            VENDOR_AMD => match amd::identify(&aperture) {
                Some(id) => {
                    log(format_args!(
                        "AMD GPU 1002:{:04x} (revision {:02x}), {} MiB of video memory, \
                         platform security processor {}; no driver beyond this yet",
                        id.device,
                        id.revision,
                        id.vram_mib,
                        if id.psp_ready { "ready" } else { "not ready" },
                    ));
                    Ok(Self {
                        backend: Backend::Amd(Arc::new(amd::Amd::new(name, registers.0.clone()))),
                    })
                }
                None => {
                    log(format_args!("AMD GPU does not answer"));
                    Err(AttachError::Decline)
                }
            },
            _ => Err(AttachError::Decline),
        }
    }

    fn start(&self, cx: &mut StartCx<'_, Self>) -> Result<(), AttachError> {
        let unit = cx.unit();
        if let Backend::Amd(a) = &self.backend
            && KMS.load(core::sync::atomic::Ordering::Relaxed)
        {
            a.register_kms(cx.sleepable());
        }
        bus::manager().sysctl(cx.sleepable(), &mut |t| {
            let Some(node) = bus::device_node(t, Self::NAME, unit) else {
                return;
            };
            match &self.backend {
                Backend::Nvidia { control, .. } => control::publish(t, node, control),
                Backend::Amd(a) => amd::publish(t, node, a),
            }
        });
        Ok(())
    }

    fn stop(&self, cx: &mut StopCx<'_>, why: StopReason) {
        if let Backend::Amd(a) = &self.backend {
            a.unregister_kms(
                cx.sleepable(),
                !matches!(why, StopReason::Shutdown | StopReason::Reset),
            );
        }
        if let Backend::Nvidia { identity, control } = &self.backend {
            let _ = identity;
            // The flush registers must not point at memory that is freed.
            control.release();
        }
    }
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<Drmn>();
