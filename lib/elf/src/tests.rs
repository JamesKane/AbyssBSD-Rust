// SPDX-License-Identifier: BSD-2-Clause
//! Tests against a small ELF image assembled here byte by byte.

extern crate std;
use std::vec;
use std::vec::Vec;

use super::*;

const BASE: u64 = 0xffff_ffff_8000_0000;

fn put(b: &mut Vec<u8>, off: usize, v: &[u8]) {
    if b.len() < off + v.len() {
        b.resize(off + v.len(), 0);
    }
    b[off..off + v.len()].copy_from_slice(v);
}

/// A position-independent x86_64 image with:
/// - one `PT_LOAD` at file offset 0 covering everything,
/// - a `PT_DYNAMIC` pointing at DT_RELA/DT_RELASZ/DT_RELAENT,
/// - two relocations (RELATIVE and NONE),
/// - `.symtab`, `.strtab`, `.shstrtab`.
fn image(machine: u16, relative: u32) -> Vec<u8> {
    let mut b = vec![0u8; 0x400];
    // File header.
    put(&mut b, 0, b"\x7fELF\x02\x01\x01");
    put(&mut b, 16, &3u16.to_le_bytes()); // ET_DYN
    put(&mut b, 18, &machine.to_le_bytes());
    put(&mut b, 20, &1u32.to_le_bytes());
    put(&mut b, 24, &(BASE + 0x100).to_le_bytes()); // e_entry
    put(&mut b, 32, &64u64.to_le_bytes()); // e_phoff
    put(&mut b, 40, &0x300u64.to_le_bytes()); // e_shoff
    put(&mut b, 54, &56u16.to_le_bytes());
    put(&mut b, 56, &2u16.to_le_bytes());
    put(&mut b, 58, &64u16.to_le_bytes());
    put(&mut b, 60, &4u16.to_le_bytes());
    put(&mut b, 62, &3u16.to_le_bytes()); // e_shstrndx

    // PT_LOAD, R+X, the whole file.
    let ph = |b: &mut Vec<u8>, at: usize, kind: u32, flags: u32, off: u64, size: u64| {
        put(b, at, &kind.to_le_bytes());
        put(b, at + 4, &flags.to_le_bytes());
        put(b, at + 8, &off.to_le_bytes());
        put(b, at + 16, &(BASE + off).to_le_bytes());
        put(b, at + 24, &(BASE + off).to_le_bytes());
        put(b, at + 32, &size.to_le_bytes());
        put(b, at + 40, &(size + 0x1000).to_le_bytes());
        put(b, at + 48, &0x1000u64.to_le_bytes());
    };
    ph(&mut b, 64, 1, 5, 0, 0x400);
    ph(&mut b, 120, 2, 6, 0x200, 0x40);

    // Dynamic section at 0x200.
    for (i, (tag, val)) in [(7u64, BASE + 0x240), (8, 48), (9, 24), (0, 0)]
        .into_iter()
        .enumerate()
    {
        put(&mut b, 0x200 + i * 16, &tag.to_le_bytes());
        put(&mut b, 0x208 + i * 16, &val.to_le_bytes());
    }
    // Relocations at 0x240.
    put(&mut b, 0x240, &(BASE + 0x180).to_le_bytes());
    put(&mut b, 0x248, &u64::from(relative).to_le_bytes());
    put(&mut b, 0x250, &0x1234i64.to_le_bytes());
    put(&mut b, 0x258, &(BASE + 0x188).to_le_bytes());
    put(&mut b, 0x260, &0u64.to_le_bytes());

    // .strtab at 0x280, .shstrtab at 0x2a0.
    put(&mut b, 0x280, b"\0kmain\0table\0");
    put(&mut b, 0x2a0, b"\0.symtab\0.strtab\0.shstrtab\0");
    // .symtab at 0x2c0: null symbol + kmain (FUNC) + table (OBJECT)... only
    // two fit before the section headers, so: null + kmain.
    let sym = |b: &mut Vec<u8>, at: usize, name: u32, info: u8, value: u64, size: u64| {
        put(b, at, &name.to_le_bytes());
        b[at + 4] = info;
        put(b, at + 6, &1u16.to_le_bytes());
        put(b, at + 8, &value.to_le_bytes());
        put(b, at + 16, &size.to_le_bytes());
    };
    sym(&mut b, 0x2d8, 1, 0x12, BASE + 0x100, 0x40);

    // Section headers at 0x300: null, .symtab, .strtab, .shstrtab.
    let sh = |b: &mut Vec<u8>, i: usize, name: u32, kind: u32, off: u64, size: u64, link: u32| {
        let at = 0x300 + i * 64;
        put(b, at, &name.to_le_bytes());
        put(b, at + 4, &kind.to_le_bytes());
        put(b, at + 24, &off.to_le_bytes());
        put(b, at + 32, &size.to_le_bytes());
        put(b, at + 40, &link.to_le_bytes());
    };
    sh(&mut b, 1, 1, 2, 0x2c0, 48, 2);
    sh(&mut b, 2, 9, 3, 0x280, 13, 0);
    sh(&mut b, 3, 17, 3, 0x2a0, 27, 0);
    b
}

#[test]
fn header_and_segments() {
    let b = image(62, 8);
    let elf = Elf::parse(&b).unwrap();
    assert_eq!(elf.machine, Machine::X86_64);
    assert_eq!(elf.file_type, FileType::Shared);
    assert_eq!(elf.entry, BASE + 0x100);
    let segs: Vec<_> = elf.segments().map(Result::unwrap).collect();
    assert_eq!(segs.len(), 2);
    assert_eq!(segs[0].kind, SegmentKind::Load);
    assert_eq!(
        segs[0].perms,
        Perms {
            read: true,
            write: false,
            exec: true
        }
    );
    assert_eq!(segs[1].kind, SegmentKind::Dynamic);
}

#[test]
fn relocations_x86_64() {
    let b = image(62, 8);
    let relocs = Elf::parse(&b).unwrap().relocations().unwrap();
    assert_eq!(relocs.len(), 2);
    let all: Vec<_> = relocs.iter().collect();
    assert_eq!(
        all,
        [
            Relocation::Relative {
                offset: BASE + 0x180,
                addend: 0x1234
            },
            Relocation::None
        ]
    );
}

#[test]
fn relocations_aarch64() {
    let b = image(183, 1027);
    let elf = Elf::parse(&b).unwrap();
    assert_eq!(elf.machine, Machine::Aarch64);
    assert!(matches!(
        elf.relocations().unwrap().iter().next(),
        Some(Relocation::Relative { .. })
    ));
    // An x86 RELATIVE number means something else on AArch64.
    let b = image(183, 8);
    assert!(matches!(
        Elf::parse(&b).unwrap().relocations().unwrap().iter().next(),
        Some(Relocation::Unsupported { kind: 8, .. })
    ));
}

#[test]
fn sections_and_symbols() {
    let b = image(62, 8);
    let elf = Elf::parse(&b).unwrap();
    assert!(elf.section_by_name(b".strtab").unwrap().is_some());
    assert!(elf.section_by_name(b".text").unwrap().is_none());
    let syms = elf.symbol_table().unwrap().unwrap();
    let (s, off) = syms.lookup(BASE + 0x123).unwrap();
    assert_eq!(s.name, b"kmain");
    assert!(s.is_function);
    assert_eq!(off, 0x23);
    assert!(syms.lookup(BASE + 0x140).is_none());
}

#[test]
fn vaddr_translation() {
    let b = image(62, 8);
    let elf = Elf::parse(&b).unwrap();
    assert_eq!(elf.vaddr_to_offset(BASE + 0x240, 48), Ok(0x240));
    assert!(elf.vaddr_to_offset(BASE + 0x3f0, 0x20).is_err());
    assert!(elf.vaddr_to_offset(0x1000, 1).is_err());
}

#[test]
fn rejects_garbage() {
    assert_eq!(Elf::parse(b"").unwrap_err(), Error::Truncated);
    assert_eq!(Elf::parse(&[0u8; 64]).unwrap_err(), Error::BadMagic);
    let mut b = image(62, 8);
    b[4] = 1; // ELFCLASS32
    assert_eq!(Elf::parse(&b).unwrap_err(), Error::Unsupported);
    let mut b = image(62, 8);
    b[56] = 200; // e_phnum past the end of the file
    assert_eq!(Elf::parse(&b).unwrap_err(), Error::Truncated);
    let mut b = image(62, 8);
    b[64 + 32] = 0xff; // p_filesz > p_memsz
    b[64 + 33] = 0xff;
    assert!(Elf::parse(&b).unwrap().segments().next().unwrap().is_err());
    // Every truncation of a valid file fails cleanly or parses a prefix.
    let good = image(62, 8);
    for n in 0..good.len() {
        if let Ok(elf) = Elf::parse(&good[..n]) {
            let _ = elf.segments().count();
            let _ = elf.relocations().map(|r| r.iter().count());
            let _ = elf.symbol_table().map(|s| s.map(|s| s.lookup(BASE)));
        }
    }
}
