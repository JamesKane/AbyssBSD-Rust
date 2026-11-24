// SPDX-License-Identifier: BSD-2-Clause
//! Tests for the stock firmware readers: files built here in the layout of
//! specification 02 section 5.7, hostile variants of them, and (when the
//! distribution's firmware package is installed) the real files against the
//! specification's test vectors (5.7.10). No firmware bytes are in the
//! repository.

use alloc::vec;
use alloc::vec::Vec;
use std::path::Path;
use std::process::Command;

use super::*;

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

/// An HS file in the real files' layout: signature block at 0x3c, the
/// referenced words from 0x33c, the load descriptor at 0x354, the payload
/// at 0x378.
fn hs_file() -> Vec<u8> {
    let payload = 0x2000usize;
    let mut f = vec![0u8; 0x378 + payload];
    put32(&mut f, 0, 0x10de);
    put32(&mut f, 4, 1);
    put32(&mut f, 8, 0x2400);
    put32(&mut f, 12, 0x18);
    put32(&mut f, 16, 0x378);
    put32(&mut f, 20, u32::try_from(payload).unwrap());
    // The descriptor.
    put32(&mut f, 0x18, 0x3c); // signature block
    put32(&mut f, 0x1c, 0x300);
    put32(&mut f, 0x20, 0x33c); // patch location, by reference
    put32(&mut f, 0x24, 0x340); // skip, by reference
    put32(&mut f, 0x28, 0x344); // metadata
    put32(&mut f, 0x2c, 12);
    put32(&mut f, 0x30, 0x350); // count, by reference
    put32(&mut f, 0x34, 0x354); // load descriptor
    put32(&mut f, 0x38, 0x24);
    put32(&mut f, 0x33c, 0x1010); // patch location
    put32(&mut f, 0x340, 0); // skip
    put32(&mut f, 0x344, 1); // fuse version
    put32(&mut f, 0x348, 1); // engine mask
    put32(&mut f, 0x34c, 3); // microcode ID
    put32(&mut f, 0x350, 2); // signature count
    // The load descriptor.
    put32(&mut f, 0x354, 0); // code offset
    put32(&mut f, 0x358, 0x100);
    put32(&mut f, 0x35c, 0x1000); // data offset
    put32(&mut f, 0x360, 0x800);
    put32(&mut f, 0x364, 1); // one application
    put32(&mut f, 0x368, 0x100); // its code
    put32(&mut f, 0x36c, 0xe00);
    // Signatures with recognizable bytes.
    f[0x3c..0x3c + 384].fill(0xa1);
    f[0x3c + 384..0x3c + 768].fill(0xb2);
    f
}

fn bootloader_file() -> Vec<u8> {
    let mut f = vec![0u8; 0x6c + 0x9000];
    put32(&mut f, 0, 0x10de);
    put32(&mut f, 4, 1);
    put32(&mut f, 12, 0x18);
    put32(&mut f, 16, 0x6c);
    put32(&mut f, 20, 0x9000);
    let words = [
        5, 0x8000, 0x880, 0x8880, 0x10, 0, 0, 7, 0, 0x800, 0x800, 0x4000, 0x4800, 0x2900, 1, 0, 0,
        0, 0, 0x9000, 0,
    ];
    for (i, w) in words.into_iter().enumerate() {
        put32(&mut f, 0x18 + 4 * i, w);
    }
    f
}

#[test]
fn a_well_formed_hs_file() {
    let f = hs_file();
    let h = hs_firmware(&f).unwrap();
    assert_eq!((h.signature_count, h.signature_size), (2, 384));
    assert_eq!((h.patch_loc, h.fuse_version), (0x1010, 1));
    assert_eq!((h.engine_id_mask, h.ucode_id), (1, 3));
    assert_eq!(h.pkc_data_offset(), Some(0x10));
    assert_eq!(h.signature(0).unwrap(), &[0xa1; 384][..]);
    assert_eq!(h.signature(1).unwrap(), &[0xb2; 384][..]);
    assert_eq!(h.signature(2), None);
    assert_eq!(h.payload.len(), 0x2000);
    assert_eq!((h.app0_code_offset, h.app0_code_size), (0x100, 0xe00));
}

#[test]
fn hostile_hs_files() {
    let bad = |fix: &dyn Fn(&mut Vec<u8>)| {
        let mut f = hs_file();
        fix(&mut f);
        hs_firmware(&f).unwrap_err()
    };
    use StockError::*;
    // The header.
    assert_eq!(bad(&|f| put32(f, 0, 0x10df)), BadHeader);
    assert_eq!(hs_firmware(&[0x10, 0xde]).unwrap_err(), BadHeader);
    assert_eq!(hs_firmware(&[]).unwrap_err(), BadHeader);
    assert!(matches!(bad(&|f| put32(f, 20, u32::MAX)), OutOfRange(_)));
    assert!(matches!(bad(&|f| put32(f, 16, u32::MAX)), OutOfRange(_)));
    // The descriptor off the end of the file.
    assert!(matches!(
        bad(&|f| put32(f, 12, u32::MAX - 8)),
        OutOfRange(_)
    ));
    // Referenced words off the end.
    assert!(matches!(bad(&|f| put32(f, 0x20, u32::MAX)), OutOfRange(_)));
    assert!(matches!(
        bad(&|f| put32(f, 0x30, 0xffff_fffd)),
        OutOfRange(_)
    ));
    // Metadata too small or out of range.
    assert_eq!(bad(&|f| put32(f, 0x2c, 8)), BadField("metadata size"));
    assert!(matches!(
        bad(&|f| put32(f, 0x28, u32::MAX - 2)),
        OutOfRange(_)
    ));
    // Engine mask and microcode ID.
    assert_eq!(bad(&|f| put32(f, 0x348, 0x1_0000)), BadField("engine mask"));
    assert_eq!(bad(&|f| put32(f, 0x34c, 0)), BadField("microcode ID"));
    assert_eq!(bad(&|f| put32(f, 0x34c, 17)), BadField("microcode ID"));
    assert_eq!(bad(&|f| put32(f, 0x34c, 0x103)), BadField("microcode ID"));
    // The load descriptor: out of range, no applications, too many.
    assert!(matches!(bad(&|f| put32(f, 0x34, u32::MAX)), OutOfRange(_)));
    assert_eq!(bad(&|f| put32(f, 0x364, 0)), BadField("application count"));
    assert_eq!(bad(&|f| put32(f, 0x364, 2)), BadField("application count"));
    assert_eq!(
        bad(&|f| put32(f, 0x364, u32::MAX)),
        BadField("application count")
    );
    // Signatures: none, uneven, out of range, a skip that runs off.
    assert_eq!(bad(&|f| put32(f, 0x350, 0)), BadField("signature count"));
    assert_eq!(bad(&|f| put32(f, 0x350, 7)), BadField("signature count"));
    assert!(matches!(
        bad(&|f| put32(f, 0x1c, 0xffff_ff00)),
        OutOfRange(_)
    ));
    assert!(matches!(bad(&|f| put32(f, 0x340, u32::MAX)), OutOfRange(_)));
    // Payload-relative ranges.
    assert!(matches!(bad(&|f| put32(f, 0x358, u32::MAX)), OutOfRange(_)));
    assert!(matches!(bad(&|f| put32(f, 0x35c, 0x1_0000)), OutOfRange(_)));
    assert!(matches!(bad(&|f| put32(f, 0x36c, 0x2000)), OutOfRange(_)));
    // A patch outside the data image, below or past it.
    assert!(matches!(bad(&|f| put32(f, 0x33c, 0xf00)), OutOfRange(_)));
    assert!(matches!(
        bad(&|f| put32(f, 0x33c, 0x1800 - 100)),
        OutOfRange(_)
    ));
}

#[test]
fn the_signature_lies_where_the_file_says() {
    // The block is elsewhere in the file, and a skip moves the first
    // signature within it.
    let mut f = hs_file();
    put32(&mut f, 0x18, 0x1000);
    put32(&mut f, 0x340, 0x10);
    f[0x1010..0x1010 + 384].fill(0xc3);
    f[0x1010 + 384..0x1010 + 768].fill(0xd4);
    let h = hs_firmware(&f).unwrap();
    assert_eq!(h.signature(0).unwrap(), &[0xc3; 384][..]);
    assert_eq!(h.signature(1).unwrap(), &[0xd4; 384][..]);
}

#[test]
fn a_well_formed_bootloader() {
    let f = bootloader_file();
    let b = bootloader(&f).unwrap();
    assert_eq!(b.app_version, 7);
    assert_eq!(b.manifest, (0, 0x800));
    assert_eq!(b.monitor_data, (0x800, 0x4000));
    assert_eq!(b.monitor_code, (0x4800, 0x2900));
    assert_eq!(b.payload.len(), 0x9000);
}

#[test]
fn hostile_bootloaders() {
    let bad = |fix: &dyn Fn(&mut Vec<u8>)| {
        let mut f = bootloader_file();
        fix(&mut f);
        bootloader(&f).unwrap_err()
    };
    use StockError::*;
    assert_eq!(bad(&|f| put32(f, 0x18, 6)), BadVersion);
    assert_eq!(bad(&|f| put32(f, 0, 1)), BadHeader);
    // The descriptor is 84 bytes: a file that ends sooner is refused.
    let f = bootloader_file();
    assert!(matches!(
        bootloader(&f[..0x18 + 80]).unwrap_err(),
        OutOfRange(_)
    ));
    for word in [8, 10, 12, 1, 3] {
        // Each range's offset (descriptor word `word`) past the payload.
        assert!(
            matches!(bad(&|f| put32(f, 0x18 + 4 * word, 0x9001)), OutOfRange(_)),
            "word {word}"
        );
    }
    // A size that overflows.
    assert!(matches!(
        bad(&|f| put32(f, 0x18 + 4 * 13, u32::MAX)),
        OutOfRange(_)
    ));
}

// --- The real files, when the distribution's package is installed. ---

const DIR: &str = "/usr/lib/firmware/nvidia/ad102/gsp";

/// A stock file, decompressed, or `None` if the package is not installed.
fn stock(name: &str) -> Option<Vec<u8>> {
    let path = std::format!("{DIR}/{name}.xz");
    if !Path::new(&path).exists() {
        std::eprintln!("skipping: {path} is not installed");
        return None;
    }
    let out = Command::new("xz").args(["-dc", &path]).output().ok()?;
    out.status.success().then_some(out.stdout)
}

#[test]
fn the_real_booters_match_the_specifications_test_vectors() {
    // (file, payload size, PLOC, DAOF, DASZ, A0CO, A0CS, UCID, CDSZ)
    let vectors = [
        (
            "booter_load-570.144.bin",
            0xde00,
            0x7e10,
            0x7e00,
            0x6000,
            0x100,
            0x7d00,
            3,
        ),
        (
            "booter_unload-570.144.bin",
            0x9f00,
            0x5010,
            0x5000,
            0x4e00,
            0x100,
            0x4f00,
            3,
        ),
        (
            "scrubber-570.144.bin",
            0x1d00,
            0x1a00,
            0x1a00,
            0x300,
            0x100,
            0x1900,
            13,
        ),
    ];
    for (name, payload, ploc, daof, dasz, a0co, a0cs, ucid) in vectors {
        let Some(f) = stock(name) else { return };
        let h = hs_firmware(&f).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!(h.payload.len(), payload, "{name}");
        assert_eq!((h.signature_count, h.signature_size), (2, 384), "{name}");
        assert_eq!(
            (h.patch_loc, h.os_data_offset, h.os_data_size),
            (ploc, daof, dasz),
            "{name}"
        );
        assert_eq!(
            (h.app0_code_offset, h.app0_code_size),
            (a0co, a0cs),
            "{name}"
        );
        assert_eq!(
            (h.fuse_version, h.engine_id_mask, h.ucode_id),
            (1, 1, ucid),
            "{name}"
        );
        assert_eq!((h.os_code_offset, h.os_code_size), (0, 0x100), "{name}");
        // The signature's position in data memory.
        assert_eq!(h.pkc_data_offset(), Some(ploc - daof), "{name}");
        assert!(h.signature(0).is_some() && h.signature(1).is_some());
        // The layout checks of 5.7.10: the signature block ends at 0x33c.
        assert_eq!(
            f.len(),
            0x378 + payload,
            "{name}: the payload ends the file"
        );
    }
}

#[test]
fn the_real_bootloader_matches_the_specifications_test_vector() {
    let Some(f) = stock("bootloader-570.144.bin") else {
        return;
    };
    let b = bootloader(&f).unwrap();
    assert_eq!(b.payload.len(), 0x9000);
    assert_eq!(b.app_version, 0);
    assert_eq!(b.manifest, (0, 0x800));
    assert_eq!(b.monitor_data, (0x800, 0x4000));
    assert_eq!(b.monitor_code, (0x4800, 0x2900));
    assert_eq!(b.bootloader, (0x8000, 0x880));
    assert_eq!(b.parameters, (0x8880, 0x10));
}

#[test]
fn the_real_gsp_firmware_matches_the_specifications_test_vector() {
    let Some(f) = stock("gsp-570.144.bin") else {
        return;
    };
    assert_eq!(f.len(), 63_571_696);
    let g = gsp_image(&f).unwrap();
    assert_eq!(g.version, "570.144");
    assert_eq!(g.image.len(), 0x3c9_9000);
    assert_eq!(g.signature_ada.len(), 0x1000);
    // The last non-zero byte of the signature section is at 0x884.
    assert_eq!(g.signature_ada.iter().rposition(|&b| b != 0), Some(0x884));
}

#[test]
fn not_an_elf_is_not_a_gsp_image() {
    assert!(matches!(
        gsp_image(b"not an elf"),
        Err(StockError::BadElf(_))
    ));
    assert!(matches!(gsp_image(&[]), Err(StockError::BadElf(_))));
}
