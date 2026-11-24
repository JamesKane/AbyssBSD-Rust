// SPDX-License-Identifier: BSD-2-Clause
//! Tests for the Video BIOS reader, on images built here from the
//! specification's layouts (so they check the code against that reading of
//! the specification, not against a real card's ROM).

use alloc::vec;
use alloc::vec::Vec;

use super::*;

const PC_AT: usize = 1024;
const EFI: usize = 512;
const FWSEC: usize = 1024;
/// Offsets inside the firmware security buffer.
const TABLE: usize = 0x100;
const DESCRIPTOR: usize = 0x180;

fn put16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// One image: ROM header, PCI data structure, and an extension.
fn image(size: usize, code_type: u8, pcir_last: bool, npde_last: bool) -> Vec<u8> {
    let mut b = vec![0u8; size];
    put16(&mut b, 0, 0xaa55);
    put16(&mut b, 0x18, 0x40);
    b[0x40..0x44].copy_from_slice(b"PCIR");
    put16(&mut b, 0x40 + 4, 0x10de);
    put16(&mut b, 0x40 + 0x0a, 0x18);
    put16(&mut b, 0x40 + 0x10, u16::try_from(size / 512).unwrap());
    b[0x40 + 0x14] = code_type;
    b[0x40 + 0x15] = if pcir_last { 0x80 } else { 0 };
    b[0x60..0x64].copy_from_slice(b"NPDE");
    put16(&mut b, 0x60 + 8, u16::try_from(size / 512).unwrap());
    b[0x60 + 10] = if npde_last { 0x80 } else { 0 };
    b
}

/// A version 3 descriptor at `DESCRIPTOR` with one signature.
fn descriptor_v3(f: &mut [u8]) {
    let at = DESCRIPTOR;
    put32(f, at, 3 << 8 | (44 + 384) << 16);
    put32(f, at + 4, 0x500); // stored size
    put32(f, at + 8, 0x40); // pkc data offset
    put32(f, at + 0x14, 0x20); // imem load size
    put32(f, at + 0x20, 0x20); // dmem load size
    put32(f, at + 0x24, 0x0400 | 7 << 16 | 1 << 24); // GSP, ucode 7, 1 signature
    put32(f, at + 0x28, 0x0005);
}

struct Rom {
    pc_at: Vec<u8>,
    efi: Vec<u8>,
    fwsec: Vec<u8>,
}

impl Rom {
    fn new() -> Self {
        let mut pc_at = image(PC_AT, 0x00, true, false);
        // The table of information at 0x100: two tokens.
        let bit = 0x100;
        pc_at[bit..bit + 6].copy_from_slice(&[0xff, 0xb8, 0x42, 0x49, 0x54, 0x00]);
        put16(&mut pc_at, bit + 6, 0x0100);
        pc_at[bit + 8] = 12;
        pc_at[bit + 9] = 6;
        pc_at[bit + 10] = 2;
        pc_at[bit + 12] = 0x01; // some other token
        pc_at[bit + 18] = 0x70; // the Falcon data token
        put16(&mut pc_at, bit + 18 + 4, 0x200);
        // Its data: the lookup table's logical offset (the PC-AT image
        // and the firmware security image taken as adjacent).
        put32(&mut pc_at, 0x200, u32::try_from(PC_AT + TABLE).unwrap());
        let efi = image(EFI, 0x03, false, false);
        let mut fwsec = image(FWSEC, 0xe0, false, true);
        // The lookup table: header 6 bytes, entries 6 bytes, two of them.
        fwsec[TABLE..TABLE + 4].copy_from_slice(&[1, 6, 6, 2]);
        fwsec[TABLE + 6] = 0x01; // the devinit entry
        fwsec[TABLE + 12] = 0x85; // the production entry
        put32(
            &mut fwsec,
            TABLE + 12 + 2,
            u32::try_from(PC_AT + DESCRIPTOR).unwrap(),
        );
        descriptor_v3(&mut fwsec);
        Self { pc_at, efi, fwsec }
    }

    fn bytes(&self) -> Vec<u8> {
        let mut v = self.pc_at.clone();
        v.extend(&self.efi);
        v.extend(&self.fwsec);
        v
    }
}

#[test]
fn reads_the_descriptor_across_the_efi_image() {
    let rom = Rom::new().bytes();
    let v = read(rom.as_slice()).unwrap();
    assert_eq!(v.images.len(), 3);
    assert_eq!(v.images[1].code_type, 0x03);
    assert_eq!(v.pc_at.len(), PC_AT);
    // Only the firmware security image is in the joined buffer.
    assert_eq!(v.fwsec.len(), FWSEC);
    assert_eq!(v.descriptor_offset, DESCRIPTOR);
    assert_eq!(v.payload_offset, DESCRIPTOR + 44 + 384);
    let Descriptor::V3(d) = v.descriptor else {
        panic!("{:?}", v.descriptor)
    };
    assert_eq!(d.pkc_data_offset, 0x40);
    assert_eq!(
        (d.engine_id_mask, d.ucode_id, d.signature_count),
        (0x400, 7, 1)
    );
    assert_eq!(
        (d.imem_load_size, d.dmem_load_size, d.signature_versions),
        (0x20, 0x20, 5)
    );
}

#[test]
fn the_pc_at_image_hides_the_others_but_the_extension_wins() {
    // The PC-AT image says it is last; its extension says it is not.
    let rom = Rom::new().bytes();
    let imgs = images(rom.as_slice()).unwrap();
    assert_eq!(
        imgs.iter()
            .map(|i| (i.offset, i.size, i.code_type))
            .collect::<Vec<_>>(),
        [(0, 1024, 0), (1024, 512, 3), (1536, 1024, 0xe0)]
    );
}

#[test]
fn version_2_descriptors() {
    let mut r = Rom::new();
    let at = DESCRIPTOR;
    r.fwsec[at..at + 0x40].fill(0);
    put32(&mut r.fwsec, at, 2 << 8 | 60 << 16);
    put32(&mut r.fwsec, at + 0x0c, 0x1234);
    put32(&mut r.fwsec, at + 0x38, 9);
    let v = read(r.bytes().as_slice()).unwrap();
    let Descriptor::V2(d) = v.descriptor else {
        panic!("{:?}", v.descriptor)
    };
    assert_eq!((d.virtual_entry, d.alt_dmem_load_size), (0x1234, 9));
    assert_eq!(v.payload_offset, DESCRIPTOR + 60);
}

#[test]
fn an_init_from_rom_header_moves_the_chain() {
    let rom = Rom::new().bytes();
    // Version 1: the offset is at fixed_data_size + 4.
    let mut f = vec![0u8; 0x200];
    put32(&mut f, 0, 0x4947_564e);
    put32(&mut f, 4, 1 << 8 | 0x20 << 16);
    put32(&mut f, 0x24, 0x200);
    f.extend(&rom);
    assert_eq!(read(f.as_slice()).unwrap().images[0].offset, 0x200);
    // Version 3: through the flash status word and the directory.
    let mut f = vec![0u8; 0x2000];
    put32(&mut f, 0, 0x4947_564e);
    put32(&mut f, 4, 3 << 8);
    put32(&mut f, 8, 0x40); // total data size
    put32(&mut f, 0x40, 0x400); // flash status offset
    put32(&mut f, 0x400 + 0x1000, 0x4452_4652);
    put32(&mut f, 0x400 + 0x1000 + 8, 0x1800);
    f.truncate(0x1800);
    f.extend(&rom);
    assert_eq!(read(f.as_slice()).unwrap().images[0].offset, 0x1800);
}

#[test]
fn hostile_init_from_rom_headers() {
    let rom = Rom::new().bytes();
    let with = |fix: &dyn Fn(&mut Vec<u8>)| {
        let mut f = vec![0u8; 0x2000];
        fix(&mut f);
        f.extend(&rom);
        read(f.as_slice()).unwrap_err()
    };
    // An unknown version.
    assert_eq!(
        with(&|f| {
            put32(f, 0, 0x4947_564e);
            put32(f, 4, 9 << 8);
        }),
        VbiosError::BadIfr
    );
    // A version 3 without its directory signature.
    assert_eq!(
        with(&|f| {
            put32(f, 0, 0x4947_564e);
            put32(f, 4, 3 << 8);
            put32(f, 8, 0x40);
            put32(f, 0x40, 0x100);
        }),
        VbiosError::BadIfr
    );
    // Offsets that run off the window or wrap.
    assert!(matches!(
        with(&|f| {
            put32(f, 0, 0x4947_564e);
            put32(f, 4, 3 << 8);
            put32(f, 8, 0xf_ffff);
        }),
        VbiosError::OutOfWindow | VbiosError::BadIfr
    ));
    assert_eq!(
        with(&|f| {
            put32(f, 0, 0x4947_564e);
            put32(f, 4, 1 << 8 | 0x7fff << 16);
        }),
        VbiosError::OutOfWindow
    );
    assert_eq!(
        with(&|f| {
            put32(f, 0, 0x4947_564e);
            put32(f, 4, 3 << 8);
            put32(f, 8, 0x40);
            put32(f, 0x40, u32::MAX - 0x800);
        }),
        VbiosError::OutOfWindow
    );
    // A chain offset at or past the window.
    assert_eq!(
        with(&|f| {
            put32(f, 0, 0x4947_564e);
            put32(f, 4, 1 << 8 | 4 << 16);
            put32(f, 8, 0x10_0000);
        }),
        VbiosError::BadIfr
    );
}

#[test]
fn hostile_images() {
    let ok = Rom::new();
    let run = |fix: &dyn Fn(&mut Rom)| {
        let mut r = Rom::new();
        fix(&mut r);
        read(r.bytes().as_slice()).unwrap_err()
    };
    let _ = ok;
    // Bad ROM signature.
    assert_eq!(
        run(&|r| r.pc_at[0] = 0),
        VbiosError::BadRomSignature { offset: 0 }
    );
    // A PCI data structure that is not there, or that points off the end
    // of what was read.
    assert_eq!(
        run(&|r| r.pc_at[0x40] = b'X'),
        VbiosError::BadPciData { offset: 0 }
    );
    assert_eq!(
        run(&|r| put16(&mut r.pc_at, 0x18, 0xffff)),
        VbiosError::BadPciData { offset: 0 }
    );
    // A length of zero units, in the structure and in the extension.
    assert_eq!(
        run(&|r| {
            put16(&mut r.efi, 0x40 + 0x10, 0);
            put16(&mut r.efi, 0x60 + 8, 0);
        }),
        VbiosError::EmptyImage { offset: 1024 }
    );
    // An image that runs past the window.
    assert_eq!(
        run(&|r| put16(&mut r.efi, 0x60 + 8, 0xffff)),
        VbiosError::ImageTooLong { offset: 1024 }
    );
    // A PCI data structure length that puts the extension past what was
    // read: there is no extension, and the structure's own length stands.
    let mut r = Rom::new();
    put16(&mut r.efi, 0x40 + 0x0a, 0xffff);
    assert!(read(r.bytes().as_slice()).is_ok());
}

#[test]
fn the_extension_overrides_the_structure() {
    // No extension: the structure's length and last-image bit stand.
    let mut r = Rom::new();
    r.pc_at[0x60..0x64].copy_from_slice(b"XXXX");
    let v = read(r.bytes().as_slice());
    // The PC-AT image's own bit says last, so the chain ends there and
    // there is no firmware security image.
    assert_eq!(v.unwrap_err(), VbiosError::NoFwsecImage);
    // A non-boot-sector NBSI image is always last, whatever it says.
    let mut r = Rom::new();
    r.efi = image(EFI, 0x70, false, false);
    assert_eq!(
        read(r.bytes().as_slice()).unwrap_err(),
        VbiosError::NoFwsecImage
    );
}

#[test]
fn a_chain_with_no_last_image_ends_at_the_window() {
    // 2048 images of 512 bytes, none last.
    let mut rom = Vec::new();
    for _ in 0..WINDOW / 512 {
        rom.extend(image(512, 0x03, false, false));
    }
    let imgs = images(rom.as_slice()).unwrap();
    assert_eq!(imgs.len(), WINDOW / 512);
    assert_eq!(read(rom.as_slice()).unwrap_err(), VbiosError::NoPcAtImage);
}

#[test]
fn the_table_of_information() {
    let run = |fix: &dyn Fn(&mut Rom)| {
        let mut r = Rom::new();
        fix(&mut r);
        read(r.bytes().as_slice()).unwrap_err()
    };
    // No signature.
    assert_eq!(run(&|r| r.pc_at[0x100] = 0), VbiosError::BadBit);
    // Header and token sizes come from the data: too small, or so large
    // the tokens are past the image.
    assert_eq!(run(&|r| r.pc_at[0x108] = 4), VbiosError::BadBit);
    assert_eq!(run(&|r| r.pc_at[0x109] = 2), VbiosError::BadBit);
    assert_eq!(run(&|r| r.pc_at[0x109] = 255), VbiosError::BadBit);
    // A count that stops before the Falcon data token, and one that runs
    // the tokens off the end of the image.
    assert_eq!(run(&|r| r.pc_at[0x10a] = 1), VbiosError::BadBit);
    assert_eq!(
        run(&|r| {
            r.pc_at[0x112] = 0x71;
            r.pc_at[0x10a] = 255;
        }),
        VbiosError::BadBit
    );
    // No Falcon data token.
    assert_eq!(run(&|r| r.pc_at[0x112] = 0x71), VbiosError::BadBit);
    // The token's data offset is outside the image.
    assert_eq!(
        run(&|r| put16(&mut r.pc_at, 0x116, 0xfffe)),
        VbiosError::BadBit
    );
}

#[test]
fn the_contiguity_adjustment_cannot_underflow() {
    let run = |fix: &dyn Fn(&mut Rom)| {
        let mut r = Rom::new();
        fix(&mut r);
        read(r.bytes().as_slice()).unwrap_err()
    };
    // The lookup table's logical offset is below the PC-AT image's size.
    assert_eq!(
        run(&|r| put32(&mut r.pc_at, 0x200, 100)),
        VbiosError::BadLookupTable
    );
    // A lookup table off the end of the buffer.
    assert_eq!(
        run(&|r| put32(&mut r.pc_at, 0x200, u32::MAX)),
        VbiosError::BadLookupTable
    );
    // An entry whose data is below the PC-AT size.
    assert_eq!(
        run(&|r| put32(&mut r.fwsec, TABLE + 14, 5)),
        VbiosError::BadLookupTable
    );
}

#[test]
fn the_lookup_table() {
    let run = |fix: &dyn Fn(&mut Rom)| {
        let mut r = Rom::new();
        fix(&mut r);
        read(r.bytes().as_slice()).unwrap_err()
    };
    // No production entry.
    assert_eq!(
        run(&|r| r.fwsec[TABLE + 12] = 0x45),
        VbiosError::BadLookupTable
    );
    // Entries too small, and a count that runs past the buffer.
    assert_eq!(run(&|r| r.fwsec[TABLE + 2] = 2), VbiosError::BadLookupTable);
    assert_eq!(
        run(&|r| {
            r.fwsec[TABLE + 12] = 0;
            r.fwsec[TABLE + 3] = 255;
            r.fwsec[TABLE + 2] = 255;
        }),
        VbiosError::BadLookupTable
    );
    // A header length that moves the entries off the end.
    assert_eq!(
        run(&|r| r.fwsec[TABLE + 1] = 255),
        VbiosError::BadLookupTable
    );
}

#[test]
fn the_descriptor() {
    let run = |fix: &dyn Fn(&mut Rom)| {
        let mut r = Rom::new();
        fix(&mut r);
        read(r.bytes().as_slice()).unwrap_err()
    };
    let at = DESCRIPTOR;
    // An unknown version.
    assert_eq!(
        run(&|r| put32(&mut r.fwsec, at, 4 << 8 | 60 << 16)),
        VbiosError::BadDescriptor
    );
    // A header size that does not cover the fixed part (v2), or the
    // signatures (v3), or that runs past the buffer.
    assert_eq!(
        run(&|r| put32(&mut r.fwsec, at, 2 << 8 | 20 << 16)),
        VbiosError::BadDescriptor
    );
    assert_eq!(
        run(&|r| put32(&mut r.fwsec, at, 3 << 8 | 44 << 16)),
        VbiosError::BadDescriptor
    );
    assert_eq!(
        run(&|r| put32(&mut r.fwsec, at, 3 << 8 | 0xffff << 16)),
        VbiosError::BadDescriptor
    );
    // Many signatures: 255 * 384 does not fit the header size.
    assert_eq!(
        run(&|r| put32(&mut r.fwsec, at + 0x24, 0x0400 | 7 << 16 | 255 << 24)),
        VbiosError::BadDescriptor
    );
    // Microcode sizes that overflow or run past the buffer.
    assert_eq!(
        run(&|r| {
            put32(&mut r.fwsec, at + 0x14, u32::MAX);
            put32(&mut r.fwsec, at + 0x20, u32::MAX);
        }),
        VbiosError::BadDescriptor
    );
    assert_eq!(
        run(&|r| put32(&mut r.fwsec, at + 0x14, 0x1_0000)),
        VbiosError::BadDescriptor
    );
    // A descriptor at the very end of the buffer, so that even its
    // header word is cut off.
    assert_eq!(
        run(&|r| put32(
            &mut r.fwsec,
            TABLE + 14,
            u32::try_from(PC_AT + FWSEC - 2).unwrap()
        )),
        VbiosError::BadDescriptor
    );
}

#[test]
fn reads_need_not_be_aligned_to_the_image() {
    // A chain that starts at a word-aligned but odd 512-multiple works the
    // same through unaligned `read_bytes` use: check the helper directly.
    let src: Vec<u8> = (0..64u8).collect();
    assert_eq!(
        read_bytes(src.as_slice(), 5, 7).unwrap(),
        (5..12).collect::<Vec<u8>>()
    );
    assert_eq!(
        read_bytes(src.as_slice(), 60, 4).unwrap(),
        (60..64).collect::<Vec<u8>>()
    );
    assert_eq!(
        read_bytes(src.as_slice(), 62, 4).unwrap_err(),
        VbiosError::OutOfWindow
    );
    assert_eq!(
        read_bytes(src.as_slice(), usize::MAX, 2).unwrap_err(),
        VbiosError::OutOfWindow
    );
    assert_eq!(
        read_bytes(src.as_slice(), 0, WINDOW + 1).unwrap_err(),
        VbiosError::OutOfWindow
    );
}
