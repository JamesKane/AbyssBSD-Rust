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
fn the_head_of_a_file_parses_for_loading() {
    // Only the start of the file: the program headers, not the sections.
    let b = image(62, 8);
    let head = &b[..0x200];
    assert!(Elf::parse(head).is_err(), "the sections lie past the head");
    let elf = Elf::parse_head(head).unwrap();
    assert_eq!(elf.entry, BASE + 0x100);
    assert_eq!(elf.segments().count(), 2);
    assert_eq!(elf.sections().count(), 0);
    // The program headers themselves must be there.
    assert!(Elf::parse_head(&b[..80]).is_err());
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

/// A FreeBSD-branded executable header with one `PT_NOTE` holding the ABI
/// tag (name "FreeBSD", type 1, the 32-bit `__FreeBSD_version`) and a
/// second note after it.
#[test]
fn branding_and_notes() {
    let mut b = vec![0u8; 0x200];
    put(&mut b, 0, b"\x7fELF\x02\x01\x01\x09");
    put(&mut b, 16, &2u16.to_le_bytes()); // ET_EXEC
    put(&mut b, 18, &62u16.to_le_bytes());
    put(&mut b, 20, &1u32.to_le_bytes());
    put(&mut b, 32, &64u64.to_le_bytes());
    put(&mut b, 48, &0x1234u32.to_le_bytes()); // e_flags
    put(&mut b, 54, &56u16.to_le_bytes());
    put(&mut b, 56, &1u16.to_le_bytes());
    // PT_NOTE at 0x100.
    put(&mut b, 64, &4u32.to_le_bytes());
    put(&mut b, 64 + 8, &0x100u64.to_le_bytes());
    let mut notes = Vec::new();
    for (name, kind, desc) in [
        (&b"FreeBSD\0"[..], 1u32, &1_501_000u32.to_le_bytes()[..]),
        (b"X\0", 7, b"abcde"),
    ] {
        notes.extend_from_slice(&u32::try_from(name.len()).unwrap().to_le_bytes());
        notes.extend_from_slice(&u32::try_from(desc.len()).unwrap().to_le_bytes());
        notes.extend_from_slice(&kind.to_le_bytes());
        notes.extend_from_slice(name);
        notes.resize(notes.len().next_multiple_of(4), 0);
        notes.extend_from_slice(desc);
        notes.resize(notes.len().next_multiple_of(4), 0);
    }
    put(&mut b, 64 + 32, &(notes.len() as u64).to_le_bytes());
    put(&mut b, 64 + 40, &(notes.len() as u64).to_le_bytes());
    put(&mut b, 0x100, &notes);
    let elf = Elf::parse(&b).unwrap();
    assert_eq!(
        (elf.os_abi, elf.flags, elf.file_type),
        (9, 0x1234, FileType::Executable)
    );
    assert_eq!(elf.program_headers(), (64, 1));
    let seg = elf.segments().next().unwrap().unwrap();
    assert_eq!(seg.kind, SegmentKind::Note);
    let found: Vec<_> = elf.notes(&seg).unwrap().collect();
    assert_eq!(found.len(), 2);
    assert_eq!((found[0].name, found[0].kind), (&b"FreeBSD"[..], 1));
    assert_eq!(found[0].desc, 1_501_000u32.to_le_bytes());
    assert_eq!(
        (found[1].name, found[1].kind, found[1].desc),
        (&b"X"[..], 7, &b"abcde"[..])
    );
    // A truncated note ends the list instead of failing.
    let short = Notes {
        data: &notes[..notes.len() - 6],
    };
    assert_eq!(short.count(), 1);
}

/// A vdso shaped like amd64's: its trampoline's 22 bytes and its CFI.
fn vdso_image() -> (Vec<u8>, vdso::Written) {
    let mut cie = vdso::Cfa::new(-8);
    cie.def_cfa(7, 8).offset(16, -8);
    let mut fde = vdso::Cfa::new(-8);
    fde.def_cfa(7, 0);
    for (reg, off) in [(5, 40), (4, 48), (1, 56), (16, 192), (7, 216)] {
        fde.offset(reg, off);
    }
    fde.advance(10).def_cfa(7, 8);
    let code = [0x90u8; 22];
    let v = vdso::Vdso {
        machine: Machine::X86_64,
        soname: b"elf-vdso.so.1",
        version: b"FBSD_1.7",
        symbol: b"__vdso_sigcode",
        code: &code,
        return_column: 16,
        cie: &cie,
        fde: &fde,
    };
    let mut b = vec![0xffu8; 2048];
    let w = v.write(&mut b).unwrap();
    b.truncate(w.len);
    if let Some(path) = std::env::var_os("VDSO_OUT") {
        std::fs::write(path, &b).unwrap();
    }
    (b, w)
}

#[test]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "offsets in a 2 KiB image"
)]
fn vdso_is_one_loadable_segment_with_dynamic_and_frames_inside() {
    let (b, w) = vdso_image();
    assert!(b.len() <= 2048, "FreeBSD's limit for the shared page");
    let e = Elf::parse(&b).unwrap();
    assert_eq!(e.file_type, FileType::Shared);
    assert_eq!(e.os_abi, 9);
    let segs: Vec<_> = e.segments().map(Result::unwrap).collect();
    assert_eq!(segs.len(), 3);
    let load = segs[0];
    assert_eq!(load.kind, SegmentKind::Load);
    assert_eq!((load.offset, load.vaddr), (0, 0));
    assert_eq!(load.file_size, (w.code + 22) as u64);
    for s in &segs {
        assert!(s.perms.read && s.perms.exec && !s.perms.write);
        assert!(s.vaddr + s.mem_size <= load.mem_size);
        assert_eq!(s.offset, s.vaddr);
    }
    assert_eq!(w.code % 16, 0);
    // No relocations: rtld does not relocate a preloaded object.
    assert!(e.relocations().unwrap().is_empty());
}

#[test]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "offsets in a 2 KiB image"
)]
fn vdso_sections_name_the_symbol_and_its_version() {
    let (b, w) = vdso_image();
    let e = Elf::parse(&b).unwrap();
    let names: Vec<_> = e
        .sections()
        .map(|s| e.section_name(&s.unwrap()).unwrap().to_vec())
        .collect();
    let want: [&[u8]; 11] = [
        b"",
        b".hash",
        b".dynsym",
        b".dynstr",
        b".gnu.version",
        b".gnu.version_d",
        b".eh_frame_hdr",
        b".eh_frame",
        b".dynamic",
        b".text",
        b".shstrtab",
    ];
    assert_eq!(names, want);
    let dynsym = e.section(2).unwrap();
    let dynstr = e.section(3).unwrap();
    let t = SymbolTable::new(
        e.bytes(dynsym.offset, dynsym.size).unwrap(),
        e.bytes(dynstr.offset, dynstr.size).unwrap(),
    )
    .unwrap();
    let syms: Vec<_> = t.symbols().collect();
    assert_eq!(syms.len(), 1);
    assert_eq!(syms[0].name, b"__vdso_sigcode");
    assert_eq!((syms[0].value, syms[0].size), (w.code as u64, 22));
    assert!(syms[0].is_function);
    // Its version index is 2, FBSD_1.7's definition.
    let versym = e.section(4).unwrap();
    assert_eq!(&b[versym.offset as usize..][..4], &[0, 0, 2, 0]);
    let vd = e.section(5).unwrap().offset as usize;
    let at = |o: usize| u32::from_le_bytes(b[o..o + 4].try_into().unwrap());
    let half = |o: usize| u16::from_le_bytes(b[o..o + 2].try_into().unwrap());
    assert_eq!((half(vd + 2), half(vd + 4)), (1, 1)); // VER_FLG_BASE
    assert_eq!(at(vd + 8), vdso::elf_hash(b"elf-vdso.so.1"));
    let second = vd + at(vd + 16) as usize;
    assert_eq!((half(second + 2), half(second + 4)), (0, 2));
    assert_eq!(at(second + 8), vdso::elf_hash(b"FBSD_1.7"));
    let name = at(second + at(second + 12) as usize) as usize;
    assert_eq!(&b[dynstr.offset as usize + name..][..9], b"FBSD_1.7\0");
}

#[test]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "offsets in a 2 KiB image"
)]
fn vdso_frame_header_finds_the_fde_for_the_code() {
    let (b, w) = vdso_image();
    let e = Elf::parse(&b).unwrap();
    let hdr = e.section(6).unwrap().offset as usize;
    let eh = e.section(7).unwrap().offset as usize;
    let i32_at = |o: usize| i32::from_le_bytes(b[o..o + 4].try_into().unwrap()) as isize;
    assert_eq!(&b[hdr..hdr + 4], &[1, 0x1b, 0x03, 0x3b]);
    assert_eq!((hdr as isize + 4 + i32_at(hdr + 4)) as usize, eh);
    assert_eq!(i32_at(hdr + 8), 1);
    assert_eq!((hdr as isize + i32_at(hdr + 12)) as usize, w.code);
    let fde = (hdr as isize + i32_at(hdr + 16)) as usize;
    // The CIE is a signal frame's, and the FDE points back at it and
    // covers the code.
    assert_eq!(&b[eh + 9..eh + 13], b"zRS\0");
    assert_eq!(fde + 4 - i32_at(fde + 4) as usize, eh);
    assert_eq!((fde as isize + 8 + i32_at(fde + 8)) as usize, w.code);
    assert_eq!(i32_at(fde + 12), 22);
    for at in [eh, fde] {
        assert_eq!((4 + i32_at(at)) % 8, 0);
    }
    // A zero-length terminator ends .eh_frame.
    let end = fde + 4 + i32_at(fde) as usize;
    assert_eq!(end + 4, eh + e.section(7).unwrap().size as usize);
    assert_eq!(i32_at(end), 0);
}

#[test]
fn vdso_rejects_a_short_buffer_and_unencodable_frames() {
    let mut cie = vdso::Cfa::new(-8);
    cie.offset(16, -4); // not a multiple of the alignment
    let fde = vdso::Cfa::new(-8);
    let v = vdso::Vdso {
        machine: Machine::X86_64,
        soname: b"a",
        version: b"b",
        symbol: b"c",
        code: &[0],
        return_column: 16,
        cie: &cie,
        fde: &fde,
    };
    assert!(matches!(v.write(&mut [0; 2048]), Err(Error::Malformed(_))));
    let ok = vdso::Cfa::new(-8);
    let v = vdso::Vdso { cie: &ok, ..v };
    assert_eq!(v.write(&mut [0; 100]), Err(Error::Truncated));
    assert!(v.write(&mut [0; 2048]).is_ok());
}

#[test]
fn elf_hash_matches_the_gabi_examples() {
    assert_eq!(vdso::elf_hash(b""), 0);
    assert_eq!(vdso::elf_hash(b"printf"), 0x077905a6);
    assert_eq!(vdso::elf_hash(b"exit"), 0x0006cf04);
}
