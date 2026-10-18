// SPDX-License-Identifier: BSD-2-Clause
//! Reading the kernel ELF from the ESP, placing it, and applying its KASLR
//! slide.

use addr::{PhysAddr, PhysRange, VirtAddr};
use bootproto::layout::{KASLR_ALIGN, KERNEL_LINK_BASE, KERNEL_MAX_SIZE};
use efi::{Allocate, File, Mode, SystemTable};
use elf::{Elf, FileType, Relocation, SegmentKind};

use crate::{Error, memtype};

/// A file read into boot-data pages, which the kernel can still reach.
#[derive(Debug)]
pub(crate) struct Loaded {
    pub(crate) phys: PhysAddr,
    pub(crate) len: usize,
}

impl Loaded {
    pub(crate) fn bytes(&self) -> &'static [u8] {
        // SAFETY: the pages were allocated for this file and are never
        // freed; the loader runs identity mapped.
        unsafe { core::slice::from_raw_parts(self.phys.get() as *const u8, self.len) }
    }
}

/// Reads `path` into pages of `kind`.
pub(crate) fn read_file(st: &SystemTable, root: &File<'_>, path: &str) -> Result<Loaded, Error> {
    let file = root
        .open(path, Mode::Read)
        .map_err(|s| Error::Efi("open", s))?;
    let len = file.size().map_err(|s| Error::Efi("file size", s))?;
    let len = usize::try_from(len).map_err(|_| Error::Kernel("file too large"))?;
    let pages = len.div_ceil(4096).max(1);
    let phys = st
        .allocate_pages(Allocate::Anywhere, memtype::BOOT_DATA, pages)
        .map_err(|s| Error::Efi("allocate file buffer", s))?;
    // SAFETY: freshly allocated pages, identity mapped, `pages * 4096` ≥ len.
    let buf = unsafe { core::slice::from_raw_parts_mut(phys as *mut u8, len) };
    file.read_exact(buf).map_err(|s| Error::Efi("read", s))?;
    Ok(Loaded {
        phys: PhysAddr::new(phys),
        len,
    })
}

/// A kernel image placed in memory.
#[derive(Debug)]
pub(crate) struct Kernel {
    pub(crate) image: bootproto::KernelImage,
    /// Physical address of the entry point.
    pub(crate) entry: PhysAddr,
    /// Loadable segments: link address, size, permissions.
    pub(crate) segments: alloc::vec::Vec<Segment>,
    pub(crate) symbols: Option<bootproto::Symbols>,
}

/// One loadable segment, page-rounded.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Segment {
    /// Slid virtual address.
    pub(crate) virt: VirtAddr,
    pub(crate) phys: PhysAddr,
    pub(crate) len: u64,
    pub(crate) write: bool,
    pub(crate) exec: bool,
}

/// Places `file` in memory at `slide`, zeroes BSS, and applies relocations.
pub(crate) fn load(st: &SystemTable, file: &Loaded, slide: u64) -> Result<Kernel, Error> {
    let elf = Elf::parse(file.bytes()).map_err(Error::Elf)?;
    let machine = if cfg!(target_arch = "x86_64") {
        elf::Machine::X86_64
    } else {
        elf::Machine::Aarch64
    };
    if elf.machine != machine || elf.file_type != FileType::Shared {
        return Err(Error::Kernel(
            "not a position-independent kernel for this CPU",
        ));
    }

    // Extent of the image, from the link base.
    let mut end = 0u64;
    for s in elf.segments() {
        let s = s.map_err(Error::Elf)?;
        if s.kind != SegmentKind::Load {
            continue;
        }
        if s.vaddr < KERNEL_LINK_BASE || s.vaddr % 4096 != 0 {
            return Err(Error::Kernel("segment below link base or unaligned"));
        }
        end = end.max(s.vaddr - KERNEL_LINK_BASE + s.mem_size);
    }
    let size = addr::align_up(end, 4096).ok_or(Error::Kernel("image size"))?;
    if size == 0 || size > KERNEL_MAX_SIZE {
        return Err(Error::Kernel("image size out of range"));
    }

    let phys = allocate_aligned(st, size)?;
    // SAFETY: freshly allocated, identity mapped, `size` bytes.
    let image = unsafe { core::slice::from_raw_parts_mut(phys.get() as *mut u8, to_usize(size)?) };
    image.fill(0);

    let mut segments = alloc::vec::Vec::new();
    for s in elf.segments() {
        let s = s.map_err(Error::Elf)?;
        if s.kind != SegmentKind::Load {
            continue;
        }
        let off = to_usize(s.vaddr - KERNEL_LINK_BASE)?;
        let data = elf.segment_data(&s).map_err(Error::Elf)?;
        image[off..off + data.len()].copy_from_slice(data);
        segments.push(Segment {
            virt: VirtAddr::new(s.vaddr + slide),
            phys: phys + (s.vaddr - KERNEL_LINK_BASE),
            len: addr::align_up(s.mem_size, 4096).ok_or(Error::Kernel("segment size"))?,
            write: s.perms.write,
            exec: s.perms.exec,
        });
    }

    for r in elf.relocations().map_err(Error::Elf)?.iter() {
        match r {
            Relocation::None => {}
            Relocation::Relative { offset, addend } => {
                let at = offset
                    .checked_sub(KERNEL_LINK_BASE)
                    .filter(|o| o + 8 <= size)
                    .ok_or(Error::Kernel("relocation outside image"))?;
                let at = to_usize(at)?;
                let value = addend.cast_unsigned().wrapping_add(slide);
                image[at..at + 8].copy_from_slice(&value.to_le_bytes());
            }
            Relocation::Unsupported { .. } => {
                return Err(Error::Kernel("unsupported relocation type"));
            }
        }
    }

    #[cfg(target_arch = "aarch64")]
    arch_aarch64::cache::prepare_code(image.as_ptr() as usize, image.len());

    let entry = elf
        .entry
        .checked_sub(KERNEL_LINK_BASE)
        .filter(|o| *o < size)
        .ok_or(Error::Kernel("entry point outside image"))?;

    let symbols = symbols(&elf, file.phys)?;
    Ok(Kernel {
        image: bootproto::KernelImage {
            phys,
            virt: VirtAddr::new(KERNEL_LINK_BASE + slide),
            size,
            slide,
        },
        entry: phys + entry,
        segments,
        symbols,
    })
}

/// Physical ranges of `.symtab` and `.strtab` inside the file buffer.
fn symbols(elf: &Elf<'_>, file: PhysAddr) -> Result<Option<bootproto::Symbols>, Error> {
    for s in elf.sections() {
        let s = s.map_err(Error::Elf)?;
        if s.kind != elf::SectionKind::SymbolTable {
            continue;
        }
        let strtab = elf
            .section(to_usize(u64::from(s.link))?)
            .map_err(Error::Elf)?;
        // Validate both lie in the file.
        elf.bytes(s.offset, s.size).map_err(Error::Elf)?;
        elf.bytes(strtab.offset, strtab.size).map_err(Error::Elf)?;
        let range = |off: u64, len: u64| PhysRange::from_len(file + off, len);
        return Ok(range(s.offset, s.size)
            .zip(range(strtab.offset, strtab.size))
            .map(|(symtab, strtab)| bootproto::Symbols { symtab, strtab }));
    }
    Ok(None)
}

/// Allocates `size` bytes of kernel memory aligned to `KASLR_ALIGN`,
/// returning the slack around the aligned block to firmware.
///
/// The pages are `LoaderCode`: firmware that enforces a no-execute policy
/// (edk2 on aarch64) maps every other type, OS-defined ones included,
/// non-executable, and the kernel's entry code runs under the firmware's
/// mapping. The handoff re-labels the range as kernel memory.
fn allocate_aligned(st: &SystemTable, size: u64) -> Result<PhysAddr, Error> {
    let page = 4096;
    let pages = to_usize((size + KASLR_ALIGN) / page)?;
    let base = st
        .allocate_pages(Allocate::Anywhere, efi::MemoryType::LOADER_CODE, pages)
        .map_err(|s| Error::Efi("allocate kernel memory", s))?;
    let aligned = addr::align_up(base, KASLR_ALIGN).ok_or(Error::Kernel("alignment"))?;
    let head = to_usize((aligned - base) / page)?;
    let used = to_usize(size / page)?;
    let tail = pages - head - used;
    // SAFETY: the head and tail pages are ours and unused. Firmware that
    // refuses partial frees just keeps the slack typed as kernel memory.
    unsafe {
        if head > 0 {
            let _ = st.free_pages(base, head);
        }
        if tail > 0 {
            let _ = st.free_pages(aligned + size, tail);
        }
    }
    Ok(PhysAddr::new(aligned))
}

fn to_usize(v: u64) -> Result<usize, Error> {
    usize::try_from(v).map_err(|_| Error::Kernel("size does not fit"))
}
