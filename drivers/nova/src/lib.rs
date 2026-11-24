// SPDX-License-Identifier: BSD-2-Clause
//! `nova`: the native driver for NVIDIA GPUs, as `drmn0` under `vgapci0`.
//!
//! This is the first step: it reads the two identification registers and
//! names the chip. It writes nothing to the GPU, turns on no bus mastering,
//! and does not touch the display, which firmware set up and `gop-kms`
//! keeps showing. A chip it does not know, or a device that does not
//! answer, is declined (the node stays in the tree without a driver).
//!
//! Layouts: the clean-room specification `nvidia-nova` (section 01).

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use core::any::Any;

use bus::{AttachCx, AttachError, Driver, DriverDecl, Ident, Ids, StopCx, StopReason};
use nvgpu::chip::{self, ChipError, Identity};
use nvgpu::vbios::{self, Descriptor, WordSource};
use vgapci::{Aperture, Registers};

/// The Video BIOS window of the register aperture, read as the parser
/// wants it.
struct Prom<'a>(&'a Aperture);

impl WordSource for Prom<'_> {
    fn read32(&self, offset: usize) -> Option<u32> {
        if offset >= vbios::WINDOW {
            return None;
        }
        self.0.read32(vbios::WINDOW_BASE.checked_add(offset)?)
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
                report_vbios(&aperture, &log);
                Ok(Self { identity })
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

    fn stop(&self, _: &mut StopCx<'_>, _: StopReason) {
        let _ = self.identity;
    }
}

/// The declaration for the build's driver table.
pub const DRIVER: DriverDecl = DriverDecl::of::<Nova>();
