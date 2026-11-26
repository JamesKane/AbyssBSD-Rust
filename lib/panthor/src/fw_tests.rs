// SPDX-License-Identifier: BSD-2-Clause
//! The image parser against images built from specification 05 §5.2, and, when
//! `PANTHOR_FIRMWARE` names a real `mali_csffw.bin` (never committed: the image is Arm's), against
//! that.

#![allow(clippy::cast_possible_truncation)] // Test images are small.

use std::vec::Vec;

use crate::fw::{
    self, CacheMode, Error, FLAG_EX, FLAG_PROT, FLAG_RD, FLAG_SHARED, FLAG_WR, FLAG_ZERO,
};

/// An image builder: the header, then entries, then data.
struct Builder {
    entries: Vec<u8>,
    data: Vec<u8>,
}

impl Builder {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            data: Vec::new(),
        }
    }
    fn entry(&mut self, ty: u8, optional: bool, payload: &[u32]) -> &mut Self {
        let len = 4 + 4 * payload.len() as u32;
        let hdr = u32::from(ty) | len << 8 | u32::from(optional) << 31;
        self.entries.extend_from_slice(&hdr.to_le_bytes());
        for w in payload {
            self.entries.extend_from_slice(&w.to_le_bytes());
        }
        self
    }
    /// A section whose `data` the builder places after the entries; `None` data = empty.
    fn section(&mut self, flags: u32, va: (u32, u32), data: &[u8]) -> &mut Self {
        let marker = self.data.len() as u32;
        self.data.extend_from_slice(data);
        // Data offsets are fixed up in `build`: here they are relative to the data area, tagged.
        self.entry(
            0,
            false,
            &[
                flags,
                va.0,
                va.1,
                marker | 1 << 31,
                (marker + data.len() as u32) | 1 << 31,
            ],
        )
    }
    fn build(&self) -> Vec<u8> {
        let base = 0x14 + self.entries.len() as u32;
        let mut entries = self.entries.clone();
        // Fix up tagged data offsets in section entries.
        let mut at = 0;
        while at < entries.len() {
            let hdr = u32::from_le_bytes(entries[at..at + 4].try_into().unwrap());
            let len = (hdr >> 8 & 0xFF) as usize;
            if hdr & 0xFF == 0 && len >= 24 {
                for k in [at + 16, at + 20] {
                    let v = u32::from_le_bytes(entries[k..k + 4].try_into().unwrap());
                    if v & 1 << 31 != 0 {
                        entries[k..k + 4].copy_from_slice(&((v & !(1 << 31)) + base).to_le_bytes());
                    }
                }
            }
            at += len;
        }
        let mut f = Vec::new();
        f.extend_from_slice(&0xC3F1_3A6Eu32.to_le_bytes());
        f.extend_from_slice(&[0, 0, 0, 0]);
        f.extend_from_slice(&0x1234_5678u32.to_le_bytes());
        f.extend_from_slice(&[0; 4]);
        f.extend_from_slice(&base.to_le_bytes());
        f.extend_from_slice(&entries);
        f.extend_from_slice(&self.data);
        f
    }
}

fn minimal() -> Builder {
    let mut b = Builder::new();
    b.section(FLAG_RD | FLAG_EX | 1 << 3, (0x0, 0x2000), &[0xAA; 16])
        .section(
            FLAG_RD | FLAG_WR | FLAG_SHARED | FLAG_ZERO,
            (fw::SHARED_VA, fw::SHARED_VA + 0x1000),
            &[],
        );
    b
}

#[test]
fn the_path_follows_the_architecture() {
    let mut out = [0u8; 64];
    let n = fw::path(12, 8, &mut out);
    assert_eq!(&out[..n], b"arm/mali/arch12.8/mali_csffw.bin");
}

#[test]
fn a_well_formed_image_gives_its_sections() {
    let f = minimal().build();
    let img = fw::parse::<8>(&f).unwrap();
    assert_eq!(img.version_hash, 0x1234_5678);
    assert_eq!(img.count, 2);
    let code = img.sections[0].unwrap();
    assert_eq!((code.va_start, code.size()), (0, 0x2000));
    assert!(code.executable() && !code.writable() && code.cacheable());
    assert_eq!(code.cache(), CacheMode::Cached);
    assert_eq!(
        &f[code.data_start as usize..code.data_end as usize],
        &[0xAA; 16]
    );
    let shared = img.sections[1].unwrap();
    assert!(shared.shared() && shared.zero() && shared.writable());
}

#[test]
fn bad_headers_are_refused() {
    assert_eq!(fw::parse::<4>(&[0; 8]), Err(Error::Short));
    let mut f = minimal().build();
    f[0] ^= 1;
    assert_eq!(fw::parse::<4>(&f), Err(Error::Magic));
    let mut f = minimal().build();
    f[5] = 1;
    assert_eq!(fw::parse::<4>(&f), Err(Error::HeaderVersion(1)));
    let mut f = minimal().build();
    let big = (f.len() as u32 + 4).to_le_bytes();
    f[0x10..0x14].copy_from_slice(&big);
    assert_eq!(fw::parse::<4>(&f), Err(Error::Size));
}

#[test]
fn sections_are_checked_as_the_specification_orders() {
    // Misaligned addresses.
    let mut b = Builder::new();
    b.section(FLAG_RD, (0x10, 0x1000), &[]);
    assert!(matches!(
        fw::parse::<4>(&b.build()),
        Err(Error::Entry(_, "addresses not page aligned"))
    ));
    // Unknown flags.
    let mut b = Builder::new();
    b.section(FLAG_RD | 1 << 10, (0, 0x1000), &[]);
    assert!(matches!(
        fw::parse::<4>(&b.build()),
        Err(Error::Entry(_, "unknown section flags"))
    ));
    // The interface section must be shared.
    let mut b = Builder::new();
    b.section(
        FLAG_RD | FLAG_WR,
        (fw::SHARED_VA, fw::SHARED_VA + 0x1000),
        &[],
    );
    assert!(matches!(
        fw::parse::<4>(&b.build()),
        Err(Error::Entry(_, "the interface section is not shared"))
    ));
    // More data than room.
    let mut b = Builder::new();
    b.section(FLAG_RD, (0, 0x1000), &[0; 0x1004]);
    assert!(matches!(
        fw::parse::<4>(&b.build()),
        Err(Error::Entry(_, "more data than the section holds"))
    ));
    // No interface section at all.
    let mut b = Builder::new();
    b.section(FLAG_RD, (0, 0x1000), &[]);
    assert_eq!(fw::parse::<4>(&b.build()), Err(Error::NoSharedSection));
}

#[test]
fn protected_and_empty_sections_are_skipped() {
    let mut b = minimal();
    b.section(FLAG_RD | FLAG_PROT, (0x10_0000, 0x10_1000), &[])
        .section(FLAG_RD, (0x20_0000, 0x20_0000), &[]);
    let img = fw::parse::<8>(&b.build()).unwrap();
    assert_eq!((img.count, img.skipped_protected), (2, 1));
}

#[test]
fn entry_types_are_skipped_or_refused() {
    let mut b = minimal();
    b.entry(3, false, &[0, 0]).entry(9, true, &[0]);
    assert!(fw::parse::<8>(&b.build()).is_ok());
    let mut b = minimal();
    b.entry(9, false, &[0]);
    assert_eq!(fw::parse::<8>(&b.build()), Err(Error::UnknownEntry(9)));
    // A size that is not a multiple of four.
    let mut f = minimal().build();
    f[0x15] = 0x19;
    assert!(matches!(
        fw::parse::<8>(&f),
        Err(Error::Entry(0x14, "entry size"))
    ));
}

#[test]
fn the_build_info_names_the_revision() {
    let mut b = minimal();
    let f0 = b.build();
    let blob_at = f0.len() as u32 + 12;
    b.entry(6, false, &[blob_at, 14]);
    let mut f = b.build();
    f.extend_from_slice(b"git_sha: abcd\0");
    let img = fw::parse::<8>(&f).unwrap();
    let (s, e) = img.git_sha.unwrap();
    assert_eq!(&f[s..e], b"abcd");
}

/// The real image, when one is at hand: it parses, and has the interface section.
#[test]
fn the_real_image_parses() {
    let Ok(path) = std::env::var("PANTHOR_FIRMWARE") else {
        return;
    };
    let f = std::fs::read(path).unwrap();
    let img = fw::parse::<32>(&f).unwrap();
    std::println!(
        "hash {:#x}, {} sections, {} protected skipped",
        img.version_hash,
        img.count,
        img.skipped_protected
    );
    for s in img.sections.iter().flatten() {
        std::println!(
            "  {:#010x}..{:#010x} flags {:#010x} data {} bytes cache {:?}",
            s.va_start,
            s.va_end,
            s.flags,
            s.data_end - s.data_start,
            s.cache()
        );
    }
    if let Some((a, b)) = img.git_sha {
        std::println!("  git_sha {}", std::str::from_utf8(&f[a..b]).unwrap_or("?"));
    }
}
