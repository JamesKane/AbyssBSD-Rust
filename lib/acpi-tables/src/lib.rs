// SPDX-License-Identifier: BSD-2-Clause
//! ACPI static tables (ACPI 6.5 §5.2): finding tables from the RSDP, and
//! parsing the ones early boot needs.
//!
//! Firmware tables are untrusted input: every length is checked against the
//! bytes actually readable, every checksum is verified, and malformed tables
//! produce an [`Error`]. Table memory is reached through [`PhysMem`], so the
//! same code runs in the loader (identity mapped), the kernel (DMAP), and
//! host tests (a byte vector).
//!
//! Each table module has a `parse(&Sdt) -> Result<_, Error>` that checks the
//! signature; the parsed views borrow the table bytes. Structure lists are
//! iterators whose items are `Result`s: a malformed entry yields an error
//! rather than a panic or an endless loop.
//!
//! Behavior reference: `sys/dev/acpica/acpi.c` (table discovery),
//! `sys/dev/uart/uart_cpu_acpi.c` (SPCR/DBG2 console selection),
//! `sys/x86/acpica/madt.c` (MADT), `sys/dev/acpica/acpi_pxm.c` (SRAT/SLIT),
//! `sys/dev/acpica/acpi_hpet.c` (HPET), `sys/arm/arm/generic_timer.c` (GTDT),
//! `sys/dev/pci/pci_host_generic_acpi.c` (MCFG).

#![no_std]
#![forbid(unsafe_code)]

pub mod dbg2;
pub mod fadt;
pub mod gas;
pub mod gtdt;
pub mod hpet;
pub mod madt;
pub mod mcfg;
pub mod pptt;
mod raw;
pub mod slit;
pub mod spcr;
pub mod srat;

pub use gas::{AddressSpace, Gas};

/// Read access to physical memory holding ACPI tables.
pub trait PhysMem<'a> {
    /// The `len` bytes at physical address `pa`, or `None` if they cannot be
    /// reached.
    fn bytes(&self, pa: u64, len: usize) -> Option<&'a [u8]>;
}

/// Why a table was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The memory could not be read.
    Unreachable(u64),
    /// Wrong signature.
    BadSignature,
    /// The checksum does not sum to zero.
    BadChecksum([u8; 4]),
    /// A length field is too small or inconsistent.
    BadLength([u8; 4]),
    /// The RSDP is ACPI 1.0 (no XSDT), which is not supported.
    Acpi1,
    /// A field holds a value the parser cannot use.
    Malformed(&'static str),
}

pub(crate) fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}
pub(crate) fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}
pub(crate) fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

fn checksum_ok(b: &[u8]) -> bool {
    b.iter().fold(0u8, |a, &x| a.wrapping_add(x)) == 0
}

const RSDP_V1_LEN: usize = 20;
const RSDP_V2_LEN: usize = 36;
/// Size of the common table header.
pub const HEADER_LEN: usize = 36;

/// A located and checksummed system description table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sdt<'a> {
    /// Physical address of the table.
    pub phys: u64,
    /// The whole table, header included.
    pub bytes: &'a [u8],
}

impl<'a> Sdt<'a> {
    /// Reads and validates the table at `pa`.
    ///
    /// # Errors
    ///
    /// [`Error::Unreachable`] if the memory cannot be read,
    /// [`Error::BadLength`] or [`Error::BadChecksum`] for a corrupt table.
    pub fn read(mem: &impl PhysMem<'a>, pa: u64) -> Result<Self, Error> {
        let header = mem.bytes(pa, HEADER_LEN).ok_or(Error::Unreachable(pa))?;
        let mut sig = [0; 4];
        sig.copy_from_slice(&header[..4]);
        let len = u32_at(header, 4).ok_or(Error::BadLength(sig))?;
        let len = usize::try_from(len).map_err(|_| Error::BadLength(sig))?;
        if len < HEADER_LEN {
            return Err(Error::BadLength(sig));
        }
        let bytes = mem.bytes(pa, len).ok_or(Error::Unreachable(pa))?;
        if !checksum_ok(bytes) {
            return Err(Error::BadChecksum(sig));
        }
        Ok(Self { phys: pa, bytes })
    }

    /// The four-byte signature, e.g. `*b"APIC"`.
    #[must_use]
    pub fn signature(&self) -> [u8; 4] {
        let mut s = [0; 4];
        s.copy_from_slice(&self.bytes[..4]);
        s
    }

    /// The table revision.
    #[must_use]
    pub fn revision(&self) -> u8 {
        self.bytes[8]
    }

    /// The OEM ID.
    #[must_use]
    pub fn oem_id(&self) -> &'a [u8] {
        &self.bytes[10..16]
    }

    /// The bytes after the header.
    #[must_use]
    pub fn body(&self) -> &'a [u8] {
        &self.bytes[HEADER_LEN..]
    }
}

/// The root of the table tree: the XSDT found through the RSDP.
#[derive(Clone, Copy, Debug)]
pub struct Tables<'a, M> {
    mem: M,
    xsdt: Sdt<'a>,
    /// The RSDP's OEM ID.
    pub oem_id: [u8; 6],
}

impl<'a, M: PhysMem<'a>> Tables<'a, M> {
    /// Validates the RSDP at `rsdp` and the XSDT it points to.
    ///
    /// # Errors
    ///
    /// Any [`Error`] describing why the RSDP or XSDT was rejected.
    pub fn new(mem: M, rsdp: u64) -> Result<Self, Error> {
        let v1 = mem
            .bytes(rsdp, RSDP_V1_LEN)
            .ok_or(Error::Unreachable(rsdp))?;
        if &v1[..8] != b"RSD PTR " {
            return Err(Error::BadSignature);
        }
        if !checksum_ok(v1) {
            return Err(Error::BadChecksum(*b"RSD "));
        }
        if v1[15] < 2 {
            return Err(Error::Acpi1);
        }
        let v2 = mem
            .bytes(rsdp, RSDP_V2_LEN)
            .ok_or(Error::Unreachable(rsdp))?;
        let len = u32_at(v2, 20).ok_or(Error::BadLength(*b"RSD "))?;
        if usize::try_from(len).ok() != Some(RSDP_V2_LEN) {
            return Err(Error::BadLength(*b"RSD "));
        }
        if !checksum_ok(v2) {
            return Err(Error::BadChecksum(*b"RSD "));
        }
        let xsdt_pa = u64_at(v2, 24).ok_or(Error::BadLength(*b"RSD "))?;
        let xsdt = Sdt::read(&mem, xsdt_pa)?;
        if xsdt.signature() != *b"XSDT" {
            return Err(Error::BadSignature);
        }
        if xsdt.body().len() % 8 != 0 {
            return Err(Error::BadLength(*b"XSDT"));
        }
        let mut oem_id = [0; 6];
        oem_id.copy_from_slice(&v1[9..15]);
        Ok(Self { mem, xsdt, oem_id })
    }

    /// The physical addresses listed in the XSDT.
    pub fn addresses(&self) -> impl Iterator<Item = u64> + 'a {
        self.xsdt
            .body()
            .as_chunks::<8>()
            .0
            .iter()
            .filter_map(|c| u64_at(c, 0))
    }

    /// Every table, validated. Bad entries yield errors rather than stopping
    /// the walk.
    pub fn iter(&self) -> impl Iterator<Item = Result<Sdt<'a>, Error>> + '_ {
        self.addresses().map(|pa| Sdt::read(&self.mem, pa))
    }

    /// The first valid table with signature `sig`.
    pub fn find(&self, sig: [u8; 4]) -> Option<Sdt<'a>> {
        self.iter()
            .filter_map(Result::ok)
            .find(|t| t.signature() == sig)
    }

    /// The memory accessor.
    pub fn mem(&self) -> &M {
        &self.mem
    }
}

/// A 16550 or PL011-family serial port described by SPCR or DBG2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SerialPort {
    /// The UART programming model.
    pub kind: UartKind,
    /// Where its registers are.
    pub base: Gas,
}

/// UART programming models, from the DBG2 serial port subtypes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UartKind {
    /// A 16550-compatible UART (subtypes 0x0000, 0x0001, 0x0005, 0x0012).
    Ns16550,
    /// An Arm PL011 (0x0003).
    Pl011,
    /// An Arm SBSA generic UART (0x000D, 0x000E): the PL011 data and flag
    /// registers, 32-bit accesses only.
    SbsaGeneric,
    /// Anything else.
    Other(u16),
}

impl UartKind {
    /// Decodes a DBG2 serial subtype (SPCR uses the same numbers).
    #[must_use]
    pub const fn from_subtype(v: u16) -> Self {
        match v {
            0x0000 | 0x0001 | 0x0005 | 0x0012 => Self::Ns16550,
            0x0003 => Self::Pl011,
            0x000d | 0x000e => Self::SbsaGeneric,
            v => Self::Other(v),
        }
    }
}

/// Picks the console UART: SPCR first, then the first serial port in DBG2.
pub fn console_uart<'a, M: PhysMem<'a>>(tables: &Tables<'a, M>) -> Option<SerialPort> {
    if let Some(port) = tables.find(*b"SPCR").and_then(|t| spcr::parse(&t).ok()) {
        return Some(port.port);
    }
    let dbg2 = tables.find(*b"DBG2")?;
    dbg2::serial_ports(&dbg2).find_map(Result::ok)
}

#[cfg(test)]
pub(crate) mod testutil;

#[cfg(test)]
mod tests;
