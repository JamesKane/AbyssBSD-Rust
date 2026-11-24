// SPDX-License-Identifier: BSD-2-Clause
//! Tests for preparing the firmware security microcode, on a microcode
//! image built here from the layouts of specification 02 section 5.5.

use alloc::vec;
use alloc::vec::Vec;

use super::*;
use crate::vbios::Image;

const IMEM: usize = 0x200;
const DMEM: usize = 0x400;
const DESC: usize = 0x100;
const SIGS: usize = 2;
/// Offsets inside the data memory.
const INTERFACE: usize = 0x20;
const MAPPER: usize = 0x80;
const CMD_IN: usize = 0x100;

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// A ROM's firmware security buffer: the descriptor, two signatures, then
/// the instruction and data images.
fn vbios(signature_versions: u16) -> Vbios {
    let payload = DESC + 44 + SIGS * 384;
    let mut f = vec![0u8; payload + IMEM + DMEM];
    // Signatures of recognizable bytes.
    f[DESC + 44..DESC + 44 + 384].fill(0x11);
    f[DESC + 44 + 384..DESC + 44 + 768].fill(0x22);
    let d = payload + IMEM;
    // The interface table: version 1, header 4, entry 8, one entry.
    f[d + INTERFACE..d + INTERFACE + 4].copy_from_slice(&[1, 4, 8, 1]);
    put32(&mut f, d + INTERFACE + 4, 4);
    put32(&mut f, d + INTERFACE + 8, u32::try_from(MAPPER).unwrap());
    // The mapper.
    put32(&mut f, d + MAPPER, 0x5041_4d44);
    put32(&mut f, d + MAPPER + 8, u32::try_from(CMD_IN).unwrap());
    put32(&mut f, d + MAPPER + 0x0c, 0x2c);
    // Marker bytes the patches must overwrite.
    f[d + CMD_IN..d + CMD_IN + 0x2c].fill(0xee);
    Vbios {
        images: vec![Image {
            offset: 0,
            size: f.len(),
            code_type: 0xe0,
        }],
        pc_at: Vec::new(),
        fwsec: f,
        descriptor_offset: DESC,
        descriptor: Descriptor::V3(DescriptorV3 {
            stored_size: 0,
            pkc_data_offset: 0x200,
            interface_offset: u32::try_from(INTERFACE).unwrap(),
            imem_phys_base: 0,
            imem_load_size: u32::try_from(IMEM).unwrap(),
            imem_virt_base: 0,
            dmem_phys_base: 0,
            dmem_load_size: u32::try_from(DMEM).unwrap(),
            engine_id_mask: 0x400,
            ucode_id: 9,
            signature_count: u8::try_from(SIGS).unwrap(),
            signature_versions,
        }),
        payload_offset: payload,
    }
}

fn word(f: &Fwsec, dmem_at: usize) -> u32 {
    let at = IMEM + dmem_at;
    u32::from_le_bytes(f.image[at..at + 4].try_into().unwrap())
}

#[test]
fn the_frts_command_is_patched_in() {
    let v = vbios(0b0110);
    let f = prepare(
        &v,
        Command::Frts {
            addr: 0x2_ff80_0000,
            size: 1 << 20,
        },
        1,
    )
    .unwrap();
    assert_eq!(word(&f, MAPPER + 0x2c), 0x15);
    // ReadVbios: version 1, size 24, address 0, size 0, flags 2.
    let cmd = |o| word(&f, CMD_IN + o);
    assert_eq!(
        (cmd(0), cmd(4), cmd(8), cmd(12), cmd(0x10), cmd(0x14)),
        (1, 24, 0, 0, 0, 2)
    );
    // FrtsRegion: version 1, size 20, pages, type 2.
    assert_eq!((cmd(0x18), cmd(0x1c)), (1, 20));
    assert_eq!(cmd(0x20), u32::try_from(0x2_ff80_0000u64 >> 12).unwrap());
    assert_eq!((cmd(0x24), cmd(0x28)), (0x100, 2));
    // The segments: secure instruction memory first, data after it.
    assert_eq!((f.imem.src, f.imem.len), (0, IMEM));
    assert_eq!((f.dmem.src, f.dmem.len), (IMEM, DMEM));
    assert_eq!(f.image.len() % 256, 0);
    assert_eq!(f.rom.engine_id_mask, 0x400);
    assert_eq!((f.rom.ucode_id, f.rom.pkc_data_offset), (9, 0x200));
}

#[test]
fn the_sb_command_leaves_the_region_alone() {
    let v = vbios(0b0110);
    let f = prepare(&v, Command::Sb, 1).unwrap();
    assert_eq!(word(&f, MAPPER + 0x2c), 0x19);
    assert_eq!(word(&f, CMD_IN + 0x14), 2);
    // The region fields keep what the ROM had.
    assert_eq!(word(&f, CMD_IN + 0x18), 0xeeee_eeee);
}

#[test]
fn the_signature_is_chosen_by_the_fuse_version_bits() {
    // Versions 1 and 2 have signatures (in that order).
    let v = vbios(0b0110);
    let sig = |fuse| {
        let f = prepare(&v, Command::Sb, fuse).unwrap();
        let at = IMEM + 0x200;
        f.image[at..at + 384].to_vec()
    };
    assert_eq!(sig(1), vec![0x11; 384]);
    assert_eq!(sig(2), vec![0x22; 384]);
    // A fuse version the ROM has no signature for.
    for fuse in [0, 3, 15, 16, 31, u32::MAX] {
        assert_eq!(
            prepare(&v, Command::Sb, fuse).unwrap_err(),
            FwsecError::NoSignature,
            "{fuse}"
        );
    }
}

#[test]
fn more_versions_than_signatures_is_refused() {
    // Three version bits, two signatures: version 3 would be the third.
    let v = vbios(0b1110);
    assert_eq!(
        prepare(&v, Command::Sb, 3).unwrap_err(),
        FwsecError::NoSignature
    );
}

#[test]
fn bad_regions_are_refused() {
    let v = vbios(0b0110);
    for (addr, size) in [
        (0x1001, 0x1000),
        (0x1000, 0x1001),
        (0x1000, 0),
        (1 << 44, 0x1000),
        (0x1000, 1 << 44),
    ] {
        assert_eq!(
            prepare(&v, Command::Frts { addr, size }, 1).unwrap_err(),
            FwsecError::BadRegion,
            "{addr:#x} {size:#x}"
        );
    }
}

#[test]
fn hostile_images_are_errors() {
    let bad = |fix: &dyn Fn(&mut Vbios)| {
        let mut v = vbios(0b0110);
        fix(&mut v);
        prepare(&v, Command::Sb, 1).unwrap_err()
    };
    let d = DESC + 44 + SIGS * 384 + IMEM;
    // A version 2 descriptor.
    assert_eq!(
        bad(&|v| {
            v.descriptor = Descriptor::V2(crate::vbios::DescriptorV2 {
                stored_size: 0,
                uncompressed_size: 0,
                virtual_entry: 0,
                interface_offset: 0,
                imem_phys_base: 0,
                imem_load_size: 0,
                imem_virt_base: 0,
                imem_sec_base: 0,
                imem_sec_size: 0,
                dmem_offset: 0,
                dmem_phys_base: 0,
                dmem_load_size: 0,
                alt_imem_load_size: 0,
                alt_dmem_load_size: 0,
            });
        }),
        FwsecError::NotV3
    );
    // A payload past the buffer.
    assert!(matches!(
        bad(&|v| v.payload_offset = usize::MAX - 4),
        FwsecError::BadImage(_)
    ));
    assert!(matches!(
        bad(&|v| v.fwsec.truncate(DESC + 50)),
        FwsecError::BadImage(_)
    ));
    // The interface table: a wrong version, no mapper entry, a bad
    // signature, an interface offset outside the image.
    assert!(matches!(
        bad(&|v| v.fwsec[d + INTERFACE] = 2),
        FwsecError::BadImage(_)
    ));
    assert_eq!(
        bad(&|v| put32(&mut v.fwsec, d + INTERFACE + 4, 5)),
        FwsecError::NoMapper
    );
    assert!(matches!(
        bad(&|v| put32(&mut v.fwsec, d + MAPPER, 0)),
        FwsecError::BadImage(_)
    ));
    assert!(matches!(
        bad(&|v| {
            if let Descriptor::V3(x) = &mut v.descriptor {
                x.interface_offset = u32::MAX;
            }
        }),
        FwsecError::BadImage(_)
    ));
    // A mapper that points the command buffer past the image, or one too
    // small for the command.
    assert!(matches!(
        bad(&|v| put32(&mut v.fwsec, d + MAPPER + 8, u32::MAX)),
        FwsecError::BadImage(_)
    ));
    assert!(matches!(
        bad(&|v| put32(&mut v.fwsec, d + MAPPER + 0x0c, 8)),
        FwsecError::BadImage(_)
    ));
    assert!(matches!(
        bad(&|v| put32(&mut v.fwsec, d + MAPPER + 8, 0x3f0)),
        FwsecError::BadImage(_)
    ));
    // A signature slot past the data image.
    assert!(matches!(
        bad(&|v| {
            if let Descriptor::V3(x) = &mut v.descriptor {
                x.pkc_data_offset = u32::try_from(DMEM - 100).unwrap();
            }
        }),
        FwsecError::BadImage(_)
    ));
}
