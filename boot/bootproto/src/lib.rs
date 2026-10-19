// SPDX-License-Identifier: BSD-2-Clause
//! The loader → kernel handoff contract, version 1.
//!
//! The loader builds a [`BootInfo`] in memory it allocates as
//! [`MemoryKind::BootData`], builds the initial page tables described in
//! [`layout`], exits boot services, and calls the kernel's entry point at its
//! **physical** address with the firmware's identity mapping still live:
//!
//! ```text
//! entry(boot_info: PhysAddr, kernel_root: PhysAddr, identity_root: PhysAddr) -> !
//! ```
//!
//! using the System V calling convention on x86_64 (`rdi`, `rsi`, `rdx`) and
//! AAPCS64 on aarch64 (`x0`, `x1`, `x2`). `kernel_root` maps the upper half
//! (DMAP, device window, kernel image); `identity_root` maps every RAM range
//! 1:1 in the lower half so the entry code can switch tables while executing
//! from physical addresses. On x86_64 both are the same PML4. On aarch64 they
//! go into TTBR1_EL1 and TTBR0_EL1, and the entry code also leaves EL2 if
//! firmware ran there. Leaf entries use the memory-attribute encodings in
//! `pt::x86_64::PAT` and `pt::aarch64::MAIR`, which the entry code programs
//! before switching tables.
//!
//! The kernel validates [`Header`] before reading anything else, and rejects
//! a handoff whose version or size it does not know. Every field is plain
//! data (`#[repr(C)]`, enums with explicit `repr`), and every pointer is a
//! physical address the kernel reaches through the DMAP.

#![no_std]
#![forbid(unsafe_code)]

pub mod layout;

pub use addr::{PhysAddr, PhysRange, VirtAddr};

/// `"ABYSSBI\0"`, little-endian.
pub const MAGIC: u64 = u64::from_le_bytes(*b"ABYSSBI\0");
/// The handoff version this crate describes.
pub const VERSION: u32 = 2;

/// A C-layout optional value.
///
/// `Option<T>` has no defined layout for most `T`, so the handoff uses this
/// instead and converts at the edges.
#[repr(C, u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Opt<T> {
    /// No value.
    None,
    /// A value.
    Some(T),
}

impl<T: Copy> Opt<T> {
    /// Converts to an [`Option`].
    pub const fn get(self) -> Option<T> {
        match self {
            Self::None => None,
            Self::Some(v) => Some(v),
        }
    }
}

impl<T> From<Option<T>> for Opt<T> {
    fn from(o: Option<T>) -> Self {
        match o {
            None => Self::None,
            Some(v) => Self::Some(v),
        }
    }
}

/// A physically contiguous array of `T` in boot data.
#[repr(C)]
#[derive(Debug)]
pub struct Table<T> {
    /// Physical address of the first element.
    pub base: PhysAddr,
    /// Number of elements.
    pub len: u64,
    _elem: core::marker::PhantomData<T>,
}

impl<T> Table<T> {
    /// Describes `len` elements at `base`.
    pub const fn new(base: PhysAddr, len: u64) -> Self {
        Self {
            base,
            len,
            _elem: core::marker::PhantomData,
        }
    }

    /// The empty table.
    pub const EMPTY: Self = Self::new(PhysAddr::ZERO, 0);
}

impl<T> Clone for Table<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Table<T> {}

/// Identifies a handoff structure. Always first.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    /// [`MAGIC`].
    pub magic: u64,
    /// [`VERSION`].
    pub version: u32,
    /// `size_of::<BootInfo>()` as built by the loader.
    pub size: u32,
}

impl Header {
    /// The header this crate's loader writes.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "size checked by the assertion below"
    )]
    pub const CURRENT: Self = Self {
        magic: MAGIC,
        version: VERSION,
        size: size_of::<BootInfo>() as u32,
    };

    /// Checks that a handoff with this header can be read as [`BootInfo`].
    pub fn validate(&self) -> Result<(), HandoffError> {
        if self.magic != MAGIC {
            return Err(HandoffError::BadMagic(self.magic));
        }
        if self.version != VERSION {
            return Err(HandoffError::UnsupportedVersion(self.version));
        }
        if self.size != Self::CURRENT.size {
            return Err(HandoffError::SizeMismatch {
                expected: Self::CURRENT.size,
                found: self.size,
            });
        }
        Ok(())
    }
}

const _: () = assert!(size_of::<BootInfo>() < 1 << 16);

/// Why a handoff was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandoffError {
    /// Not a handoff at all.
    BadMagic(u64),
    /// A version this kernel does not understand.
    UnsupportedVersion(u32),
    /// Same version, different layout: loader and kernel are out of step.
    SizeMismatch {
        /// What this kernel expects.
        expected: u32,
        /// What the loader wrote.
        found: u32,
    },
}

/// Everything the loader hands the kernel.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BootInfo {
    /// Must be [`Header::CURRENT`].
    pub header: Header,
    /// Where the kernel image was placed.
    pub kernel: KernelImage,
    /// The physical memory map after `ExitBootServices`, sorted by address,
    /// with no overlaps and adjacent regions of the same kind merged.
    pub memory_map: Table<MemoryRegion>,
    /// The early serial console, if the loader found one.
    pub serial: Opt<Serial>,
    /// The GOP framebuffer, if there is one.
    pub framebuffer: Opt<Framebuffer>,
    /// Firmware tables and services.
    pub firmware: Firmware,
    /// The kernel command line, UTF-8, not NUL-terminated.
    pub cmdline: Table<u8>,
    /// The kernel's ELF symbol table, for symbolized backtraces.
    pub symbols: Opt<Symbols>,
    /// Random bytes from the firmware RNG, or zero if it had none.
    pub entropy: Entropy,
    /// The persistent log ring, formatted and empty.
    pub log: Opt<LogRing>,
    /// The memory disk (`EFI/ABYSS/MD` on the ESP, a ustar archive), in
    /// [`MemoryKind::BootData`], if there was one.
    pub md: Opt<PhysRange>,
}

/// The persistent log ring (format: `lib/logring`), in memory of kind
/// [`MemoryKind::LogRing`].
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LogRing {
    /// Where it is.
    pub region: PhysRange,
    /// Bytes of the previous boot's log the loader saved to the ESP
    /// (`EFI/ABYSS/PREVLOG.TXT`); zero if there was none.
    pub recovered: u64,
    /// Boots since the ring was first created.
    pub boots: u64,
}

/// Placement of the kernel image.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KernelImage {
    /// Physical address of the first byte of the image (2 MiB aligned).
    pub phys: PhysAddr,
    /// Virtual address the first byte is mapped at: the link address plus
    /// `slide`.
    pub virt: VirtAddr,
    /// Size of the mapped image in bytes (a multiple of the page size).
    pub size: u64,
    /// The KASLR slide applied to the link address.
    pub slide: u64,
}

/// One physical memory map entry. Lengths are in base pages.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryRegion {
    /// First byte.
    pub start: PhysAddr,
    /// Length in [`addr::PAGE_SIZE`] pages.
    pub pages: u64,
    /// What the region holds.
    pub kind: MemoryKind,
    /// Must be zero.
    pub reserved: u32,
    /// The firmware's attribute bits for the region (UEFI `EFI_MEMORY_*`).
    pub firmware_attributes: u64,
}

impl MemoryRegion {
    /// The region as a physical range.
    pub const fn range(&self) -> PhysRange {
        PhysRange {
            start: self.start,
            end: PhysAddr::new(self.start.get() + self.pages * addr::PAGE_SIZE),
        }
    }
}

/// What a memory region holds.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryKind {
    /// Usable RAM, including everything firmware boot services used.
    Free = 1,
    /// The kernel image.
    Kernel = 2,
    /// Loader data the kernel reads at boot: this structure, the memory
    /// map, the initial page tables, symbols. Reclaimable once consumed.
    BootData = 3,
    /// The persistent log ring, which survives a warm reset.
    LogRing = 4,
    /// ACPI tables; reclaimable once parsed.
    AcpiReclaimable = 5,
    /// ACPI non-volatile storage; never touch.
    AcpiNvs = 6,
    /// UEFI runtime services code.
    RuntimeCode = 7,
    /// UEFI runtime services data.
    RuntimeData = 8,
    /// Memory-mapped I/O the firmware described.
    Mmio = 9,
    /// Reserved by firmware or a device.
    Reserved = 10,
    /// RAM with errors.
    Unusable = 11,
    /// Byte-addressable persistent memory.
    Persistent = 12,
}

impl MemoryKind {
    /// Whether the region is RAM (and so belongs in the DMAP).
    pub const fn is_ram(self) -> bool {
        matches!(
            self,
            Self::Free
                | Self::Kernel
                | Self::BootData
                | Self::LogRing
                | Self::AcpiReclaimable
                | Self::AcpiNvs
                | Self::RuntimeCode
                | Self::RuntimeData
        )
    }
}

/// The early serial console. The loader leaves the UART configured as
/// firmware set it up (baud rate, framing); the kernel only reads and writes
/// data.
#[repr(C, u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Serial {
    /// A 16550-compatible UART in x86 I/O port space.
    Ns16550Port {
        /// Base I/O port.
        port: u16,
    },
    /// A 16550-compatible UART in MMIO space.
    Ns16550Mmio(Mmio),
    /// An Arm PL011 or SBSA generic UART.
    Pl011(Mmio),
}

/// A device register block the loader mapped into the device window.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mmio {
    /// Physical base.
    pub phys: PhysAddr,
    /// Where the loader mapped it (Device memory, non-executable).
    pub virt: VirtAddr,
    /// Bytes mapped.
    pub size: u64,
    /// Register stride as a shift: register `n` is at `n << reg_shift`.
    pub reg_shift: u8,
    /// Access width in bytes (1, 2, or 4).
    pub access_width: u8,
}

/// A linear framebuffer, mapped write-combining in the device window.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Framebuffer {
    /// Physical base.
    pub phys: PhysAddr,
    /// Where the loader mapped it.
    pub virt: VirtAddr,
    /// Bytes mapped.
    pub size: u64,
    /// Visible width in pixels.
    pub width: u32,
    /// Visible height in pixels.
    pub height: u32,
    /// Pixels per scan line (≥ `width`).
    pub stride: u32,
    /// How a 32-bit pixel is laid out.
    pub format: PixelFormat,
}

/// Layout of a 32-bit framebuffer pixel.
#[repr(C, u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    /// Byte 0 red, 1 green, 2 blue, 3 reserved.
    Rgbx,
    /// Byte 0 blue, 1 green, 2 red, 3 reserved.
    Bgrx,
    /// Arbitrary channel masks.
    Masks {
        /// Red channel bits.
        red: u32,
        /// Green channel bits.
        green: u32,
        /// Blue channel bits.
        blue: u32,
    },
}

/// Firmware tables and services, all physical addresses.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Firmware {
    /// ACPI 2.0+ RSDP.
    pub rsdp: Opt<PhysAddr>,
    /// SMBIOS 3.0 entry point.
    pub smbios3: Opt<PhysAddr>,
    /// The UEFI system table.
    pub uefi_system_table: PhysAddr,
    /// The UEFI runtime services table. Callable through a 1:1 mapping of
    /// the runtime regions: the loader does not call `SetVirtualAddressMap`.
    pub uefi_runtime_services: PhysAddr,
    /// UEFI revision (`major << 16 | minor`).
    pub uefi_revision: u32,
    /// Must be zero.
    pub reserved: u32,
}

/// Where the kernel's ELF symbols are.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Symbols {
    /// `.symtab` contents (an array of `Elf64_Sym`).
    pub symtab: PhysRange,
    /// `.strtab` contents.
    pub strtab: PhysRange,
}

/// Seed material for the kernel's RNG.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entropy {
    /// Random bytes.
    pub bytes: [u8; 32],
    /// Where they came from.
    pub source: EntropySource,
}

/// Where [`Entropy`] came from.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntropySource {
    /// No source; the bytes are zero.
    None = 0,
    /// `EFI_RNG_PROTOCOL`.
    UefiRng = 1,
    /// A CPU instruction (`RDRAND`, `RNDR`).
    Cpu = 2,
    /// Timer jitter only. Not cryptographically useful.
    Timer = 3,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_validation() {
        assert_eq!(Header::CURRENT.validate(), Ok(()));
        let mut h = Header::CURRENT;
        h.version = VERSION + 1;
        assert_eq!(
            h.validate(),
            Err(HandoffError::UnsupportedVersion(VERSION + 1))
        );
        h = Header::CURRENT;
        h.size += 8;
        assert!(matches!(
            h.validate(),
            Err(HandoffError::SizeMismatch { .. })
        ));
        h.magic = 0;
        assert_eq!(h.validate(), Err(HandoffError::BadMagic(0)));
    }

    #[test]
    fn opt_round_trips() {
        let o: Opt<PhysAddr> = Some(PhysAddr::new(0x1000)).into();
        assert_eq!(o.get(), Some(PhysAddr::new(0x1000)));
        assert_eq!(Opt::<u64>::None.get(), None);
    }

    #[test]
    fn region_range() {
        let r = MemoryRegion {
            start: PhysAddr::new(0x10_0000),
            pages: 16,
            kind: MemoryKind::Free,
            reserved: 0,
            firmware_attributes: 0,
        };
        assert_eq!(r.range().end, PhysAddr::new(0x11_0000));
        assert!(MemoryKind::AcpiNvs.is_ram());
        assert!(!MemoryKind::Mmio.is_ram());
    }
}
