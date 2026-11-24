// SPDX-License-Identifier: BSD-2-Clause
//! Preparing the firmware security microcode (`FWSEC`) from the Video BIOS
//! to run on the GPU system processor (specification 02, sections 5.5.2 and
//! 5.5.3): patching the command into its data memory and choosing the
//! signature the chip's fuses accept.
//!
//! Ada (`GA102` on) only: the descriptor is version 3.

use alloc::vec::Vec;

use crate::falcon::{Rom, Segment, pad_to_block};
use crate::vbios::{Descriptor, DescriptorV3, Vbios};

/// What to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Carve out the protected region 2's firmware runtime security part:
    /// `size` bytes at `addr` in video memory.
    Frts {
        /// Start in video memory.
        addr: u64,
        /// Size in bytes.
        size: u64,
    },
    /// Reset the GPU system processor's security state (at unload).
    Sb,
}

/// Why the microcode could not be prepared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FwsecError {
    /// Not a version 3 descriptor.
    NotV3,
    /// The ROM's descriptor or payload is inconsistent.
    BadImage(&'static str),
    /// No application interface table entry for the command mapper.
    NoMapper,
    /// The command's region does not fit the fields.
    BadRegion,
    /// The ROM holds no signature for the chip's fuse version.
    NoSignature,
}

/// The microcode ready to load.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fwsec {
    /// The instruction and data images, one after another, zero-padded to
    /// 256 bytes: the DMA buffer's contents.
    pub image: Vec<u8>,
    /// The secure instruction memory segment.
    pub imem: Segment,
    /// The data memory segment.
    pub dmem: Segment,
    /// What the boot ROM verifies it against.
    pub rom: Rom,
}

const SIGNATURE_BYTES: usize = 384;
const CMD_FRTS: u32 = 0x15;
const CMD_SB: u32 = 0x19;
const APPIF_DMEMMAPPER: u32 = 4;

fn le32(b: &[u8], at: usize) -> Option<u32> {
    let w = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
}

fn put32(b: &mut [u8], at: usize, v: u32) -> Option<()> {
    b.get_mut(at..at.checked_add(4)?)?
        .copy_from_slice(&v.to_le_bytes());
    Some(())
}

fn usize_of(v: u32) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

/// Prepares the microcode for `cmd`. `reg_fuse_version` is the position of
/// the highest set bit of the chip's fuse register for this microcode
/// ([`crate::fuse::version`]).
///
/// # Errors
///
/// [`FwsecError`] if the ROM's data does not hold what the sections say, or
/// holds no signature for the chip.
pub fn prepare(v: &Vbios, cmd: Command, reg_fuse_version: u32) -> Result<Fwsec, FwsecError> {
    let Descriptor::V3(d) = v.descriptor else {
        return Err(FwsecError::NotV3);
    };
    let bad = FwsecError::BadImage;
    let imem = usize_of(d.imem_load_size);
    let dmem = usize_of(d.dmem_load_size);
    let total = imem.checked_add(dmem).ok_or(bad("sizes"))?;
    let end = v.payload_offset.checked_add(total).ok_or(bad("payload"))?;
    let mut image = v
        .fwsec
        .get(v.payload_offset..end)
        .ok_or(bad("payload"))?
        .to_vec();

    patch_command(&mut image, imem, &d, cmd)?;
    patch_signature(&mut image, imem, v, &d, reg_fuse_version)?;

    Ok(Fwsec {
        image: pad_to_block(image),
        imem: Segment {
            src: 0,
            dst: usize_of(d.imem_phys_base),
            len: imem,
        },
        dmem: Segment {
            src: imem,
            dst: usize_of(d.dmem_phys_base),
            len: dmem,
        },
        rom: Rom {
            pkc_data_offset: d.pkc_data_offset,
            engine_id_mask: d.engine_id_mask,
            ucode_id: d.ucode_id,
        },
    })
}

/// Patches the command mapper's `init_cmd` and the command input buffer
/// (5.5.2).
fn patch_command(
    image: &mut [u8],
    imem: usize,
    d: &DescriptorV3,
    cmd: Command,
) -> Result<(), FwsecError> {
    let bad = FwsecError::BadImage;
    // The application interface table, in data memory.
    let hdr = imem
        .checked_add(usize_of(d.interface_offset))
        .ok_or(bad("interface"))?;
    let h = image
        .get(hdr..hdr.checked_add(4).ok_or(bad("interface"))?)
        .ok_or(bad("interface"))?;
    let (version, header_size, entry_size, count) = (
        h[0],
        usize::from(h[1]),
        usize::from(h[2]),
        usize::from(h[3]),
    );
    if version != 1 || entry_size < 8 {
        return Err(bad("interface table"));
    }
    let mut dmem_base = None;
    for i in 0..count {
        let at = i
            .checked_mul(entry_size)
            .and_then(|o| o.checked_add(header_size))
            .and_then(|o| o.checked_add(hdr))
            .ok_or(bad("interface entry"))?;
        if le32(image, at).ok_or(bad("interface entry"))? == APPIF_DMEMMAPPER {
            dmem_base = le32(image, at + 4);
            break;
        }
    }
    let mapper = imem
        .checked_add(usize_of(dmem_base.ok_or(FwsecError::NoMapper)?))
        .ok_or(bad("mapper"))?;
    // "DMAP", version 3, 64 bytes.
    if le32(image, mapper) != Some(0x5041_4d44) {
        return Err(bad("mapper signature"));
    }
    let cmd_in = imem
        .checked_add(usize_of(le32(image, mapper + 8).ok_or(bad("mapper"))?))
        .ok_or(bad("command buffer"))?;
    let in_size = usize_of(le32(image, mapper + 0x0c).ok_or(bad("mapper"))?);
    if in_size < 0x2c {
        return Err(bad("command buffer size"));
    }
    let p = |image: &mut [u8], at: usize, v: u32| {
        put32(image, at, v).ok_or(bad("command buffer range"))
    };
    p(
        image,
        mapper + 0x2c,
        match cmd {
            Command::Frts { .. } => CMD_FRTS,
            Command::Sb => CMD_SB,
        },
    )?;
    // `ReadVbios`: version 1, size 24, read it from the ROM (address 0,
    // size 0), flags 2.
    p(image, cmd_in, 1)?;
    p(image, cmd_in + 4, 24)?;
    p(image, cmd_in + 8, 0)?;
    p(image, cmd_in + 12, 0)?;
    p(image, cmd_in + 0x10, 0)?;
    p(image, cmd_in + 0x14, 2)?;
    if let Command::Frts { addr, size } = cmd {
        let pages = |v: u64| u32::try_from(v >> 12).map_err(|_| FwsecError::BadRegion);
        if addr & 0xfff != 0 || size & 0xfff != 0 || size == 0 {
            return Err(FwsecError::BadRegion);
        }
        // `FrtsRegion`: version 1, size 20, start page, page count, type 2
        // (video memory).
        p(image, cmd_in + 0x18, 1)?;
        p(image, cmd_in + 0x1c, 20)?;
        p(image, cmd_in + 0x20, pages(addr)?)?;
        p(image, cmd_in + 0x24, pages(size)?)?;
        p(image, cmd_in + 0x28, 2)?;
    }
    Ok(())
}

/// Chooses the signature for the chip's fuse version and patches it into
/// data memory (5.5.3).
fn patch_signature(
    image: &mut [u8],
    imem: usize,
    v: &Vbios,
    d: &DescriptorV3,
    reg_fuse_version: u32,
) -> Result<(), FwsecError> {
    if d.signature_count == 0 {
        return Ok(());
    }
    if reg_fuse_version >= 16 {
        return Err(FwsecError::NoSignature);
    }
    let bit = 1u32 << reg_fuse_version;
    let versions = u32::from(d.signature_versions);
    if versions & bit == 0 {
        return Err(FwsecError::NoSignature);
    }
    // The index is how many versions below it have a signature.
    let index = usize::try_from((versions & (bit - 1)).count_ones()).unwrap_or(usize::MAX);
    if index >= usize::from(d.signature_count) {
        return Err(FwsecError::NoSignature);
    }
    let first = v
        .descriptor_offset
        .checked_add(44)
        .ok_or(FwsecError::BadImage("signatures"))?;
    let from = index
        .checked_mul(SIGNATURE_BYTES)
        .and_then(|o| o.checked_add(first))
        .ok_or(FwsecError::BadImage("signatures"))?;
    let sig = v
        .fwsec
        .get(from..from + SIGNATURE_BYTES)
        .ok_or(FwsecError::BadImage("signature range"))?;
    let at = imem
        .checked_add(usize_of(d.pkc_data_offset))
        .ok_or(FwsecError::BadImage("signature position"))?;
    let slot = image
        .get_mut(
            at..at
                .checked_add(SIGNATURE_BYTES)
                .ok_or(FwsecError::BadImage("signature position"))?,
        )
        .ok_or(FwsecError::BadImage("signature position"))?;
    slot.copy_from_slice(sig);
    Ok(())
}

#[cfg(test)]
#[path = "fwsec_tests.rs"]
mod tests;
