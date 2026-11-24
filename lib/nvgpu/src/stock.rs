// SPDX-License-Identifier: BSD-2-Clause
//! The stock firmware files distributions ship (`gsp-570.144.bin`,
//! `bootloader-570.144.bin`, `booter_load-570.144.bin`,
//! `booter_unload-570.144.bin`, `scrubber-570.144.bin`), read directly:
//! specification 02, section 5.7. The `NVFW` containers of section 5.2 are
//! not needed; this reads the same values from where the stock files keep
//! them.
//!
//! The files are untrusted input. Every range is checked in 64-bit
//! arithmetic before it is used (section 5.7.8).

use alloc::string::String;

/// Why a stock file was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StockError {
    /// Shorter than its header, or `magic` is not `0x10de`.
    BadHeader,
    /// A range lies outside the file or the payload.
    OutOfRange(&'static str),
    /// A descriptor of a version this code does not know.
    BadVersion,
    /// A field has a value the format does not allow.
    BadField(&'static str),
    /// The ELF file is malformed, or lacks a section it must have.
    BadElf(&'static str),
}

fn u32_at(b: &[u8], at: u64) -> Option<u32> {
    let at = usize::try_from(at).ok()?;
    let w = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
}

/// `[start, start + len)` of `b`, or an error naming `what`.
fn range<'a>(
    b: &'a [u8],
    start: u64,
    len: u64,
    what: &'static str,
) -> Result<&'a [u8], StockError> {
    let end = start.checked_add(len).ok_or(StockError::OutOfRange(what))?;
    let (s, e) = (
        usize::try_from(start).map_err(|_| StockError::OutOfRange(what))?,
        usize::try_from(end).map_err(|_| StockError::OutOfRange(what))?,
    );
    b.get(s..e).ok_or(StockError::OutOfRange(what))
}

/// The 24-byte outer header (5.7.1) and the payload it describes.
#[derive(Clone, Copy, Debug)]
struct Outer<'a> {
    file: &'a [u8],
    descriptor_offset: u64,
    payload: &'a [u8],
}

fn outer(file: &[u8]) -> Result<Outer<'_>, StockError> {
    let w = |o| u32_at(file, o).ok_or(StockError::BadHeader);
    if w(0)? != 0x10de {
        return Err(StockError::BadHeader);
    }
    // `header_version` (word 1) is 1 in every file; no reader checked it,
    // and `total_size` (word 2) is not the file length.
    let descriptor_offset = u64::from(w(12)?);
    let payload_offset = u64::from(w(16)?);
    let payload_size = u64::from(w(20)?);
    let payload = range(file, payload_offset, payload_size, "payload")?;
    Ok(Outer {
        file,
        descriptor_offset,
        payload,
    })
}

/// A signed microcode that runs in heavy-secure mode: `booter_load`,
/// `booter_unload`, and `scrubber` (5.7.2 to 5.7.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HsFirmware<'a> {
    /// The payload (the image that gets loaded), unpatched.
    pub payload: &'a [u8],
    /// All the signatures, one after another.
    pub signatures: &'a [u8],
    /// How many signatures.
    pub signature_count: u32,
    /// Each signature's size.
    pub signature_size: u32,
    /// Where the chosen signature is written, payload-relative.
    pub patch_loc: u32,
    /// The microcode's fuse version.
    pub fuse_version: u32,
    /// The engines it may run on.
    pub engine_id_mask: u16,
    /// Its microcode ID, `1..=16`.
    pub ucode_id: u8,
    /// Operating-system code offset (payload-relative).
    pub os_code_offset: u32,
    /// Operating-system code size.
    pub os_code_size: u32,
    /// Operating-system data offset, the data memory image's source.
    pub os_data_offset: u32,
    /// Operating-system data size.
    pub os_data_size: u32,
    /// Application 0's code offset: the secure instruction memory source.
    pub app0_code_offset: u32,
    /// Application 0's code size.
    pub app0_code_size: u32,
}

impl HsFirmware<'_> {
    /// Where, in the data memory image, the signature is patched
    /// (`pkc_data_offset`): `patch_loc - os_data_offset`.
    #[must_use]
    pub fn pkc_data_offset(&self) -> Option<u32> {
        self.patch_loc.checked_sub(self.os_data_offset)
    }

    /// Signature `index`.
    #[must_use]
    pub fn signature(&self, index: u32) -> Option<&[u8]> {
        if index >= self.signature_count {
            return None;
        }
        let size = usize::try_from(self.signature_size).ok()?;
        let at = usize::try_from(index).ok()?.checked_mul(size)?;
        self.signatures.get(at..at.checked_add(size)?)
    }
}

/// Reads an HS firmware file.
///
/// # Errors
///
/// [`StockError`] for anything section 5.7.8 says to refuse.
pub fn hs_firmware(file: &[u8]) -> Result<HsFirmware<'_>, StockError> {
    let o = outer(file)?;
    let d = o.descriptor_offset;
    // The descriptor: nine words.
    range(file, d, 36, "HS descriptor")?;
    let w = |off: u64| u32_at(file, d + off).ok_or(StockError::OutOfRange("HS descriptor"));
    let sig_block_offset = u64::from(w(0)?);
    let sig_block_size = u64::from(w(4)?);
    let deref = |off: u64, what: &'static str| -> Result<u32, StockError> {
        u32_at(file, u64::from(w(off)?)).ok_or(StockError::OutOfRange(what))
    };
    let patch_loc = deref(8, "patch location")?;
    let sig_skip = deref(0x0c, "signature skip")?;
    let meta_offset = u64::from(w(0x10)?);
    let meta_size = u64::from(w(0x14)?);
    let count = deref(0x18, "signature count")?;
    let load_offset = u64::from(w(0x1c)?);
    let load_size = u64::from(w(0x20)?);

    // The metadata.
    if meta_size < 12 {
        return Err(StockError::BadField("metadata size"));
    }
    range(file, meta_offset, meta_size, "metadata")?;
    let m = |off: u64| u32_at(file, meta_offset + off).ok_or(StockError::OutOfRange("metadata"));
    let (fuse_version, engine, ucode) = (m(0)?, m(4)?, m(8)?);
    let engine_id_mask = u16::try_from(engine).map_err(|_| StockError::BadField("engine mask"))?;
    let ucode_id = u8::try_from(ucode)
        .ok()
        .filter(|i| (1..=16).contains(i))
        .ok_or(StockError::BadField("microcode ID"))?;

    // The load descriptor.
    range(file, load_offset, load_size, "load descriptor")?;
    let l =
        |off: u64| u32_at(file, load_offset + off).ok_or(StockError::OutOfRange("load descriptor"));
    let app_count = u64::from(l(0x10)?);
    if app_count < 1
        || app_count
            .checked_mul(16)
            .and_then(|a| a.checked_add(20))
            .is_none_or(|n| n > load_size)
    {
        return Err(StockError::BadField("application count"));
    }
    let (os_code_offset, os_code_size) = (l(0)?, l(4)?);
    let (os_data_offset, os_data_size) = (l(8)?, l(0x0c)?);
    let (app0_code_offset, app0_code_size) = (l(0x14)?, l(0x18)?);

    // The signatures.
    if count == 0 || !sig_block_size.is_multiple_of(u64::from(count)) {
        return Err(StockError::BadField("signature count"));
    }
    let first = sig_block_offset
        .checked_add(u64::from(sig_skip))
        .ok_or(StockError::OutOfRange("signatures"))?;
    let signatures = range(file, first, sig_block_size, "signatures")?;
    let signature_size = u32::try_from(sig_block_size / u64::from(count))
        .map_err(|_| StockError::BadField("signature size"))?;

    // Everything payload-relative is inside the payload, and the patch is
    // inside the data memory image.
    let p = o.payload;
    let inside = |start: u32, len: u32, what| range(p, u64::from(start), u64::from(len), what);
    inside(os_code_offset, os_code_size, "operating-system code")?;
    inside(os_data_offset, os_data_size, "operating-system data")?;
    inside(app0_code_offset, app0_code_size, "application code")?;
    inside(patch_loc, signature_size, "signature patch")?;
    let patch_end = u64::from(patch_loc) + u64::from(signature_size);
    if patch_loc < os_data_offset || patch_end > u64::from(os_data_offset) + u64::from(os_data_size)
    {
        return Err(StockError::OutOfRange("signature patch in data memory"));
    }
    Ok(HsFirmware {
        payload: p,
        signatures,
        signature_count: count,
        signature_size,
        patch_loc,
        fuse_version,
        engine_id_mask,
        ucode_id,
        os_code_offset,
        os_code_size,
        os_data_offset,
        os_data_size,
        app0_code_offset,
        app0_code_size,
    })
}

/// The RISC-V bootloader (`bootloader-570.144.bin`, 5.7.5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bootloader<'a> {
    /// The payload.
    pub payload: &'a [u8],
    /// The application version, written to the GPU system processor's OS
    /// register after boot (`APPV`).
    pub app_version: u32,
    /// The manifest's offset (`MFOF`, payload-relative) and size.
    pub manifest: (u32, u32),
    /// The monitor's data (`DAOF`) and its size.
    pub monitor_data: (u32, u32),
    /// The monitor's code (`CDOF`) and its size.
    pub monitor_code: (u32, u32),
    /// The bootloader's own code and its size.
    pub bootloader: (u32, u32),
    /// The bootloader's parameters and their size.
    pub parameters: (u32, u32),
}

/// Reads the bootloader file.
///
/// # Errors
///
/// [`StockError`] for anything section 5.7.8 says to refuse.
pub fn bootloader(file: &[u8]) -> Result<Bootloader<'_>, StockError> {
    let o = outer(file)?;
    let d = o.descriptor_offset;
    range(o.file, d, 84, "bootloader descriptor")?;
    let w = |n: u64| u32_at(file, d + 4 * n).ok_or(StockError::OutOfRange("bootloader descriptor"));
    if w(0)? != 5 {
        return Err(StockError::BadVersion);
    }
    let b = Bootloader {
        payload: o.payload,
        app_version: w(7)?,
        manifest: (w(8)?, w(9)?),
        monitor_data: (w(10)?, w(11)?),
        monitor_code: (w(12)?, w(13)?),
        bootloader: (w(1)?, w(2)?),
        parameters: (w(3)?, w(4)?),
    };
    for (r, what) in [
        (b.manifest, "manifest"),
        (b.monitor_data, "monitor data"),
        (b.monitor_code, "monitor code"),
        (b.bootloader, "bootloader code"),
        (b.parameters, "bootloader parameters"),
    ] {
        range(o.payload, u64::from(r.0), u64::from(r.1), what)?;
    }
    Ok(b)
}

/// The GPU system processor's resource-manager firmware, from the ELF
/// (`gsp-570.144.bin`, 5.7.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GspImage<'a> {
    /// The release string (`.fwversion` without its terminator).
    pub version: String,
    /// The raw firmware image (`.fwimage`).
    pub image: &'a [u8],
    /// The Ada signature, the whole `.fwsignature_ad10x` section.
    pub signature_ada: &'a [u8],
}

/// Reads the ELF.
///
/// # Errors
///
/// [`StockError::BadElf`] if it does not parse or a section is missing or
/// outside the file.
pub fn gsp_image(file: &[u8]) -> Result<GspImage<'_>, StockError> {
    let bad = StockError::BadElf("not an ELF64 file");
    let elf = elf::Elf::parse(file).map_err(|_| bad)?;
    let section = |name: &[u8], what: &'static str| -> Result<&[u8], StockError> {
        let s = elf
            .section_by_name(name)
            .map_err(|_| StockError::BadElf(what))?
            .ok_or(StockError::BadElf(what))?;
        elf.bytes(s.offset, s.size)
            .map_err(|_| StockError::BadElf(what))
    };
    let version = section(b".fwversion", "no .fwversion")?;
    let version = version.strip_suffix(&[0]).unwrap_or(version);
    if !version.iter().all(|c| (0x20..=0x7e).contains(c)) {
        return Err(StockError::BadElf("version is not printable"));
    }
    Ok(GspImage {
        version: String::from_utf8_lossy(version).into_owned(),
        image: section(b".fwimage", "no .fwimage")?,
        signature_ada: section(b".fwsignature_ad10x", "no .fwsignature_ad10x")?,
    })
}

#[cfg(test)]
#[path = "stock_tests.rs"]
mod tests;
