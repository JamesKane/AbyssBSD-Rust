// SPDX-License-Identifier: BSD-2-Clause
//! Reading the Video BIOS (specification 01, section 6): the image chain in
//! the register aperture's read-only memory window, the table of
//! information (BIT) in its first image, and the lookup table that leads to
//! the firmware security microcode's descriptor.
//!
//! Every offset and length comes from the ROM and is checked. The window is
//! read through [`WordSource`], so one parser serves a byte slice (a saved
//! ROM, a test) and the aperture itself.

use alloc::vec::Vec;

/// The ROM window's size: 1 MiB at BAR0 `0x0030_0000`.
pub const WINDOW: usize = 0x10_0000;
/// Where the window is in BAR0.
pub const WINDOW_BASE: usize = 0x0030_0000;

/// Images start on 512-byte boundaries and are measured in 512-byte units.
const UNIT: usize = 512;
/// How much of an image's start is read to learn its length.
const HEADER_READ: usize = 0x400;
/// "NVGI", the Init-From-ROM header's signature.
const IFR_SIGNATURE: u32 = 0x4947_564e;
/// "RFRD", the version 3 directory's signature.
const IFR_DIRECTORY: u32 = 0x4452_4652;

/// Where ROM words come from: 32-bit aligned little-endian reads.
pub trait WordSource {
    /// The word at byte `offset` (a multiple of four) in the window, or
    /// `None` if it lies outside.
    fn read32(&self, offset: usize) -> Option<u32>;
}

impl WordSource for [u8] {
    fn read32(&self, offset: usize) -> Option<u32> {
        let w = self.get(offset..offset.checked_add(4)?)?;
        Some(u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
    }
}

/// Why the ROM could not be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VbiosError {
    /// A read fell outside the window.
    OutOfWindow,
    /// The Init-From-ROM header is of a version, or has a directory, this
    /// code does not know.
    BadIfr,
    /// An image does not start with a ROM signature.
    BadRomSignature {
        /// Where it was looked for.
        offset: usize,
    },
    /// An image's PCI data structure is missing or malformed.
    BadPciData {
        /// The image's offset.
        offset: usize,
    },
    /// An image claims a length of zero.
    EmptyImage {
        /// The image's offset.
        offset: usize,
    },
    /// An image extends past the window.
    ImageTooLong {
        /// The image's offset.
        offset: usize,
    },
    /// There is no image with code type 0 (the one that holds the table of
    /// information).
    NoPcAtImage,
    /// There is no firmware security image (code type `0xe0`).
    NoFwsecImage,
    /// The table of information, or its Falcon data token, is missing or
    /// malformed.
    BadBit,
    /// The lookup table has no production firmware security entry, or it
    /// is malformed.
    BadLookupTable,
    /// The microcode descriptor is of an unknown version or runs past its
    /// buffer.
    BadDescriptor,
}

/// A ROM image's place in the chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Image {
    /// Its offset in the window, from the start of the ROM chain.
    pub offset: usize,
    /// Its length in bytes.
    pub size: usize,
    /// Its code type: `0x00` PC-AT, `0x03` EFI, `0x70` NBSI, `0xe0` firmware
    /// security.
    pub code_type: u8,
}

fn read_bytes<S: WordSource + ?Sized>(
    src: &S,
    offset: usize,
    len: usize,
) -> Result<Vec<u8>, VbiosError> {
    let end = offset.checked_add(len).ok_or(VbiosError::OutOfWindow)?;
    if end > WINDOW {
        return Err(VbiosError::OutOfWindow);
    }
    let first = offset & !3;
    let mut out = Vec::with_capacity(len);
    let mut at = first;
    while at < end {
        let w = src.read32(at).ok_or(VbiosError::OutOfWindow)?.to_le_bytes();
        for (i, b) in w.into_iter().enumerate() {
            let pos = at + i;
            if pos >= offset && pos < end {
                out.push(b);
            }
        }
        at += 4;
    }
    Ok(out)
}

/// Where the PCI expansion ROM chain begins (`IFR` handling).
fn rom_offset<S: WordSource + ?Sized>(src: &S) -> Result<usize, VbiosError> {
    let word = |o: usize| src.read32(o).ok_or(VbiosError::OutOfWindow);
    if word(0)? != IFR_SIGNATURE {
        return Ok(0);
    }
    let fixed1 = word(4)?;
    let version = fixed1 >> 8 & 0xff;
    let fixed_data_size = usize::try_from(fixed1 >> 16 & 0x7fff).map_err(|_| VbiosError::BadIfr)?;
    let to_usize = |v: u32| usize::try_from(v).map_err(|_| VbiosError::BadIfr);
    let off = match version {
        1 | 2 => {
            let at = fixed_data_size.checked_add(4).ok_or(VbiosError::BadIfr)?;
            to_usize(word(at & !3)?)?
        }
        3 => {
            let total = to_usize(word(8)? & 0xf_ffff)?;
            let flash = to_usize(word(total & !3)?)?;
            let dir = flash.checked_add(0x1000).ok_or(VbiosError::BadIfr)?;
            if word(dir & !3)? != IFR_DIRECTORY {
                return Err(VbiosError::BadIfr);
            }
            to_usize(word(dir.checked_add(8).ok_or(VbiosError::BadIfr)? & !3)?)?
        }
        _ => return Err(VbiosError::BadIfr),
    };
    if off >= WINDOW {
        return Err(VbiosError::BadIfr);
    }
    Ok(off)
}

fn le16(b: &[u8], at: usize) -> Option<u16> {
    let w = b.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([w[0], w[1]]))
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    let w = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
}

/// Reads the image at `offset` far enough to know its place and length.
/// Returns the image and whether it is the last.
fn image_at<S: WordSource + ?Sized>(src: &S, offset: usize) -> Result<(Image, bool), VbiosError> {
    // A short read at the end of the window is enough if it holds the
    // structures.
    let want = HEADER_READ.min(WINDOW - offset);
    let head = read_bytes(src, offset, want)?;
    match le16(&head, 0) {
        Some(0xaa55 | 0x4e56) => {}
        _ => return Err(VbiosError::BadRomSignature { offset }),
    }
    let bad = VbiosError::BadPciData { offset };
    let pdo = usize::from(le16(&head, 0x18).ok_or(bad)?);
    let pcir = head
        .get(pdo..pdo.checked_add(0x18).ok_or(bad)?)
        .ok_or(bad)?;
    if &pcir[..4] != b"PCIR" && &pcir[..4] != b"NPDS" {
        return Err(bad);
    }
    let pci_len = usize::from(le16(pcir, 0x0a).ok_or(bad)?);
    let image_len = usize::from(le16(pcir, 0x10).ok_or(bad)?);
    let code_type = pcir[0x14];
    let mut last = pcir[0x15] & 0x80 != 0;
    let mut units = image_len;
    if code_type == 0x70 {
        last = true;
    } else {
        // The extension, on the next 16-byte boundary after the structure.
        let npde = pdo
            .checked_add(pci_len)
            .and_then(|e| e.checked_add(0x0f))
            .map(|e| e & !0x0f)
            .ok_or(bad)?;
        if let Some(n) = head.get(npde..npde.saturating_add(11))
            && &n[..4] == b"NPDE"
        {
            let sub = usize::from(le16(n, 8).ok_or(bad)?);
            if sub > 0 {
                units = sub;
                last = n[10] & 0x80 != 0;
            }
        }
    }
    if units == 0 {
        return Err(VbiosError::EmptyImage { offset });
    }
    let size = units * UNIT;
    if offset.checked_add(size).is_none_or(|e| e > WINDOW) {
        return Err(VbiosError::ImageTooLong { offset });
    }
    Ok((
        Image {
            offset,
            size,
            code_type,
        },
        last,
    ))
}

/// The ROM images in order, from the window's start (through the
/// Init-From-ROM header if there is one). The chain ends at the last image,
/// or at the end of the window.
///
/// # Errors
///
/// [`VbiosError`] for a malformed header or image.
pub fn images<S: WordSource + ?Sized>(src: &S) -> Result<Vec<Image>, VbiosError> {
    let mut out = Vec::new();
    let mut at = rom_offset(src)?;
    while at < WINDOW {
        let (image, last) = image_at(src, at)?;
        out.push(image);
        if last {
            break;
        }
        // The next image starts on the next 512-byte boundary.
        at = (image.offset + image.size).next_multiple_of(UNIT);
    }
    Ok(out)
}

/// The BIT token that leads to the Falcon data.
const TOKEN_FALCON_DATA: u8 = 0x70;
/// The lookup-table application ID of the production firmware security
/// microcode.
const APP_FWSEC_PRODUCTION: u8 = 0x85;
/// Bytes of one RSA-3K signature a version 3 descriptor carries.
const SIGNATURE_BYTES: usize = 384;

/// A version 2 microcode descriptor (60 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DescriptorV2 {
    /// Stored size.
    pub stored_size: u32,
    /// Uncompressed size.
    pub uncompressed_size: u32,
    /// Virtual entry point.
    pub virtual_entry: u32,
    /// Data memory offset of the application interface table.
    pub interface_offset: u32,
    /// Instruction memory physical base.
    pub imem_phys_base: u32,
    /// Instruction memory load size.
    pub imem_load_size: u32,
    /// Instruction memory virtual base.
    pub imem_virt_base: u32,
    /// Secure instruction memory base.
    pub imem_sec_base: u32,
    /// Secure instruction memory size.
    pub imem_sec_size: u32,
    /// Data memory offset.
    pub dmem_offset: u32,
    /// Data memory physical base.
    pub dmem_phys_base: u32,
    /// Data memory load size.
    pub dmem_load_size: u32,
    /// Alternate instruction memory load size.
    pub alt_imem_load_size: u32,
    /// Alternate data memory load size.
    pub alt_dmem_load_size: u32,
}

/// A version 3 microcode descriptor (44 bytes, then signatures).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DescriptorV3 {
    /// Stored size.
    pub stored_size: u32,
    /// Data memory offset where the signature is patched.
    pub pkc_data_offset: u32,
    /// Data memory offset of the application interface table.
    pub interface_offset: u32,
    /// Instruction memory physical base.
    pub imem_phys_base: u32,
    /// Instruction memory load size.
    pub imem_load_size: u32,
    /// Instruction memory virtual base.
    pub imem_virt_base: u32,
    /// Data memory physical base.
    pub dmem_phys_base: u32,
    /// Data memory load size.
    pub dmem_load_size: u32,
    /// The engines it may run on.
    pub engine_id_mask: u16,
    /// Its microcode ID.
    pub ucode_id: u8,
    /// How many signatures follow the descriptor.
    pub signature_count: u8,
    /// Which fuse versions the signatures are for.
    pub signature_versions: u16,
}

/// A microcode descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Descriptor {
    /// Version 2.
    V2(DescriptorV2),
    /// Version 3.
    V3(DescriptorV3),
}

/// What reading the ROM found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Vbios {
    /// The images in the chain.
    pub images: Vec<Image>,
    /// The first PC-AT image's bytes (it holds the table of information).
    pub pc_at: Vec<u8>,
    /// The first firmware security image's bytes and those of every image
    /// after it, joined.
    pub fwsec: Vec<u8>,
    /// The microcode descriptor's offset in [`Self::fwsec`].
    pub descriptor_offset: usize,
    /// The descriptor.
    pub descriptor: Descriptor,
    /// The microcode (signatures excluded for version 3) starts here in
    /// [`Self::fwsec`].
    pub payload_offset: usize,
}

fn find_bit(pc_at: &[u8]) -> Result<u32, VbiosError> {
    let bad = VbiosError::BadBit;
    const SIGNATURE: [u8; 6] = [0xff, 0xb8, 0x42, 0x49, 0x54, 0x00];
    let bit = pc_at
        .windows(SIGNATURE.len())
        .position(|w| w == SIGNATURE)
        .ok_or(bad)?;
    let header_size = usize::from(*pc_at.get(bit + 8).ok_or(bad)?);
    let token_size = usize::from(*pc_at.get(bit + 9).ok_or(bad)?);
    let entries = usize::from(*pc_at.get(bit + 10).ok_or(bad)?);
    if header_size < 12 || token_size < 6 {
        return Err(bad);
    }
    for i in 0..entries {
        let at = i
            .checked_mul(token_size)
            .and_then(|o| o.checked_add(bit + header_size))
            .ok_or(bad)?;
        let token = pc_at.get(at..at.checked_add(6).ok_or(bad)?).ok_or(bad)?;
        if token[0] == TOKEN_FALCON_DATA {
            let data_offset = usize::from(le16(token, 4).ok_or(bad)?);
            return le32(pc_at, data_offset).ok_or(bad);
        }
    }
    Err(bad)
}

fn descriptor(fwsec: &[u8], at: usize) -> Result<(Descriptor, usize), VbiosError> {
    let bad = VbiosError::BadDescriptor;
    let hdr = le32(fwsec, at).ok_or(bad)?;
    let version = hdr >> 8 & 0xff;
    let total = usize::try_from(hdr >> 16).map_err(|_| bad)?;
    let f = |o: usize| le32(fwsec, at.checked_add(o)?);
    let payload = at.checked_add(total).ok_or(bad)?;
    match version {
        2 => {
            if total < 60 || payload > fwsec.len() {
                return Err(bad);
            }
            let w = |o| f(o).ok_or(bad);
            Ok((
                Descriptor::V2(DescriptorV2 {
                    stored_size: w(4)?,
                    uncompressed_size: w(8)?,
                    virtual_entry: w(0x0c)?,
                    interface_offset: w(0x10)?,
                    imem_phys_base: w(0x14)?,
                    imem_load_size: w(0x18)?,
                    imem_virt_base: w(0x1c)?,
                    imem_sec_base: w(0x20)?,
                    imem_sec_size: w(0x24)?,
                    dmem_offset: w(0x28)?,
                    dmem_phys_base: w(0x2c)?,
                    dmem_load_size: w(0x30)?,
                    alt_imem_load_size: w(0x34)?,
                    alt_dmem_load_size: w(0x38)?,
                }),
                payload,
            ))
        }
        3 => {
            let w = |o| f(o).ok_or(bad);
            let engine_and_ids = w(0x24)?;
            let versions = w(0x28)?;
            let d = DescriptorV3 {
                stored_size: w(4)?,
                pkc_data_offset: w(8)?,
                interface_offset: w(0x0c)?,
                imem_phys_base: w(0x10)?,
                imem_load_size: w(0x14)?,
                imem_virt_base: w(0x18)?,
                dmem_phys_base: w(0x1c)?,
                dmem_load_size: w(0x20)?,
                engine_id_mask: u16::try_from(engine_and_ids & 0xffff).map_err(|_| bad)?,
                ucode_id: u8::try_from(engine_and_ids >> 16 & 0xff).map_err(|_| bad)?,
                signature_count: u8::try_from(engine_and_ids >> 24).map_err(|_| bad)?,
                signature_versions: u16::try_from(versions & 0xffff).map_err(|_| bad)?,
            };
            // The header size must cover the fixed part and every
            // signature, and the microcode must fit after it.
            let signatures = usize::from(d.signature_count)
                .checked_mul(SIGNATURE_BYTES)
                .and_then(|s| s.checked_add(44))
                .ok_or(bad)?;
            let code = usize::try_from(d.imem_load_size)
                .ok()
                .zip(usize::try_from(d.dmem_load_size).ok())
                .and_then(|(i, d)| i.checked_add(d))
                .ok_or(bad)?;
            if total < signatures || payload.checked_add(code).is_none_or(|e| e > fwsec.len()) {
                return Err(bad);
            }
            Ok((Descriptor::V3(d), payload))
        }
        _ => Err(bad),
    }
}

/// Reads the ROM: the image chain, the table of information, the lookup
/// table, and the production firmware security microcode's descriptor.
///
/// # Errors
///
/// [`VbiosError`] naming the first thing wrong.
pub fn read<S: WordSource + ?Sized>(src: &S) -> Result<Vbios, VbiosError> {
    let images = images(src)?;
    let pc_at_image = images
        .iter()
        .find(|i| i.code_type == 0x00)
        .copied()
        .ok_or(VbiosError::NoPcAtImage)?;
    let pc_at = read_bytes(src, pc_at_image.offset, pc_at_image.size)?;
    let first_fwsec = images
        .iter()
        .position(|i| i.code_type == 0xe0)
        .ok_or(VbiosError::NoFwsecImage)?;
    let mut fwsec = Vec::new();
    for i in &images[first_fwsec..] {
        fwsec.extend(read_bytes(src, i.offset, i.size)?);
    }
    // Offsets in the tables assume the PC-AT and firmware security images
    // are adjacent: any image between them (an EFI one) is not counted.
    let adjust = |logical: u32| {
        usize::try_from(logical)
            .ok()
            .and_then(|l| l.checked_sub(pc_at_image.size))
    };
    let table = find_bit(&pc_at)?;
    let table = adjust(table).ok_or(VbiosError::BadLookupTable)?;
    let bad = VbiosError::BadLookupTable;
    let header_len = usize::from(*fwsec.get(table + 1).ok_or(bad)?);
    let entry_len = usize::from(*fwsec.get(table + 2).ok_or(bad)?);
    let entry_count = usize::from(*fwsec.get(table + 3).ok_or(bad)?);
    if entry_len < 6 {
        return Err(bad);
    }
    let mut at = None;
    for i in 0..entry_count {
        let e = i
            .checked_mul(entry_len)
            .and_then(|o| o.checked_add(table))
            .and_then(|o| o.checked_add(header_len))
            .ok_or(bad)?;
        let entry = fwsec.get(e..e.checked_add(6).ok_or(bad)?).ok_or(bad)?;
        if entry[0] == APP_FWSEC_PRODUCTION {
            at = adjust(le32(entry, 2).ok_or(bad)?);
            break;
        }
    }
    let descriptor_offset = at.ok_or(bad)?;
    let (descriptor, payload_offset) = descriptor(&fwsec, descriptor_offset)?;
    Ok(Vbios {
        images,
        pc_at,
        fwsec,
        descriptor_offset,
        descriptor,
        payload_offset,
    })
}

#[cfg(test)]
#[path = "vbios_tests.rs"]
mod tests;
