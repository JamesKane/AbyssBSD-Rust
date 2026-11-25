// SPDX-License-Identifier: BSD-2-Clause
//! The graphics, compute and DMA engines' firmware (specification 02, 03
//! section 7): the files' containers read, and the list of images the
//! security processor is handed, in the order it is handed them.
//!
//! The files are the GPU vendor's and are not in this repository: the driver
//! reads them from `/lib/firmware/amdgpu/`. Every offset and size in them is
//! untrusted input and is checked in 64 bits.

use alloc::vec::Vec;

/// The security processor's firmware type codes (02 section 9).
pub mod kind {
    /// CP ME.
    pub const ME: u32 = 1;
    /// CP PFP.
    pub const PFP: u32 = 2;
    /// CP CE.
    pub const CE: u32 = 3;
    /// CP MEC (the code image of both MEC1 and MEC2).
    pub const MEC: u32 = 4;
    /// RLC_G.
    pub const RLC_G: u32 = 8;
    /// SDMA0.
    pub const SDMA0: u32 = 9;
    /// RLC restore list GPM.
    pub const RLC_RESTORE_LIST_GPM: u32 = 20;
    /// RLC restore list SRM.
    pub const RLC_RESTORE_LIST_SRM: u32 = 21;
    /// RLC restore list CNTL.
    pub const RLC_RESTORE_LIST_CNTL: u32 = 22;
    /// RLC IRAM.
    pub const RLC_IRAM: u32 = 26;
    /// Global tap delays.
    pub const GLOBAL_TAP_DELAYS: u32 = 27;
    /// SE0 tap delays.
    pub const SE0_TAP_DELAYS: u32 = 28;
    /// SE1 tap delays.
    pub const SE1_TAP_DELAYS: u32 = 29;
    /// RLC DRAM boot.
    pub const RLC_DRAM_BOOT: u32 = 48;
}

/// Why a file was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Shorter than its header.
    Short,
    /// The size field is not the file's length (RLC files excepted).
    Size,
    /// The header size, payload offset or payload size do not fit.
    Layout,
    /// A header version this does not read.
    Version,
    /// An offset and size pair outside the file.
    Range,
}

/// One image to hand the security processor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Image<'a> {
    /// What it is, for the log.
    pub name: &'static str,
    /// The firmware type code (see [`kind`]).
    pub kind: u32,
    /// The bytes.
    pub data: &'a [u8],
}

/// The files the graphics engines' load needs.
#[derive(Clone, Copy, Debug)]
pub struct Files<'a> {
    pub sdma: &'a [u8],
    pub ce: &'a [u8],
    pub pfp: &'a [u8],
    pub me: &'a [u8],
    pub mec: &'a [u8],
    pub mec2: &'a [u8],
    pub rlc: &'a [u8],
}

fn le32(b: &[u8], at: usize) -> Option<u32> {
    let w = b.get(at..at.checked_add(4)?)?;
    Some(u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
}

fn le16(b: &[u8], at: usize) -> Option<u16> {
    let w = b.get(at..at.checked_add(2)?)?;
    Some(u16::from_le_bytes([w[0], w[1]]))
}

/// The common header of a file (02 section 2): its major and minor version
/// and the main image.
struct Header<'a> {
    major: u16,
    minor: u16,
    payload: &'a [u8],
}

fn header(file: &[u8], size_must_match: bool) -> Result<Header<'_>, Error> {
    if file.len() < 0x20 {
        return Err(Error::Short);
    }
    let size = le32(file, 0).ok_or(Error::Short)?;
    let header_size = u64::from(le32(file, 4).ok_or(Error::Short)?);
    let payload_size = u64::from(le32(file, 0x14).ok_or(Error::Short)?);
    let payload_at = u64::from(le32(file, 0x18).ok_or(Error::Short)?);
    if size_must_match && u64::from(size) != file.len() as u64 {
        return Err(Error::Size);
    }
    if header_size > payload_at || header_size < 0x20 {
        return Err(Error::Layout);
    }
    let end = payload_at.checked_add(payload_size).ok_or(Error::Layout)?;
    if end > file.len() as u64 {
        return Err(Error::Layout);
    }
    Ok(Header {
        major: le16(file, 8).ok_or(Error::Short)?,
        minor: le16(file, 0xa).ok_or(Error::Short)?,
        payload: &file[usize::try_from(payload_at).map_err(|_| Error::Layout)?
            ..usize::try_from(end).map_err(|_| Error::Layout)?],
    })
}

/// A file of the CP or SDMA container (major 1): the whole payload, or for
/// the MEC the code image without the jump table that ends it.
fn simple(file: &[u8], jump_table_dwords_at: Option<usize>) -> Result<&[u8], Error> {
    let h = header(file, true)?;
    if h.major != 1 {
        return Err(Error::Version);
    }
    let Some(at) = jump_table_dwords_at else {
        return Ok(h.payload);
    };
    // The MEC: the jump table is the last part of the payload.
    let dwords = le32(file, at).ok_or(Error::Short)? as usize;
    let code = h
        .payload
        .len()
        .checked_sub(dwords.checked_mul(4).ok_or(Error::Range)?)
        .ok_or(Error::Range)?;
    Ok(&h.payload[..code])
}

/// A (size, file offset) pair of the RLC header, as a slice of the file; an
/// image with size 0 is absent.
fn rlc_image(file: &[u8], size_at: usize, offset_at: usize) -> Result<&[u8], Error> {
    let size = u64::from(le32(file, size_at).ok_or(Error::Short)?);
    if size == 0 {
        return Ok(&[]);
    }
    let offset = u64::from(le32(file, offset_at).ok_or(Error::Short)?);
    let end = offset.checked_add(size).ok_or(Error::Range)?;
    if end > file.len() as u64 {
        return Err(Error::Range);
    }
    Ok(&file[usize::try_from(offset).map_err(|_| Error::Range)?
        ..usize::try_from(end).map_err(|_| Error::Range)?])
}

/// The images to hand the security processor, in its order (03 section 7):
/// SDMA0, CE, PFP, ME, the MEC code (twice, MEC1 and MEC2), the tap delays
/// if the RLC file has them, the RLC restore lists CNTL, GPM, SRM, then the
/// RLC IRAM and DRAM, then RLC_G. Absent (size 0) images are left out.
/// The caller then starts the RLC autoload.
///
/// # Errors
///
/// [`Error`] for a file that does not read as its container.
pub fn load_list<'a>(f: &Files<'a>) -> Result<Vec<Image<'a>>, Error> {
    let mut out = Vec::new();
    let mut push = |name: &'static str, kind: u32, data: &'a [u8]| {
        if !data.is_empty() {
            out.push(Image { name, kind, data });
        }
    };
    push("SDMA0", kind::SDMA0, simple(f.sdma, None)?);
    push("CE", kind::CE, simple(f.ce, None)?);
    push("PFP", kind::PFP, simple(f.pfp, None)?);
    push("ME", kind::ME, simple(f.me, None)?);
    // The CP container's jump-table size is at 0x28 (dwords).
    push("MEC1", kind::MEC, simple(f.mec, Some(0x28))?);
    push("MEC2", kind::MEC, simple(f.mec2, Some(0x28))?);
    // The RLC (container major 2; the size field is not checked, 02 section 2).
    let rlc = header(f.rlc, false)?;
    if rlc.major != 2 {
        return Err(Error::Version);
    }
    if rlc.minor >= 4 {
        push(
            "tap delays (global)",
            kind::GLOBAL_TAP_DELAYS,
            rlc_image(f.rlc, 0xcc, 0xd0)?,
        );
        push(
            "tap delays (SE0)",
            kind::SE0_TAP_DELAYS,
            rlc_image(f.rlc, 0xd4, 0xd8)?,
        );
        push(
            "tap delays (SE1)",
            kind::SE1_TAP_DELAYS,
            rlc_image(f.rlc, 0xdc, 0xe0)?,
        );
    }
    if rlc.minor >= 1 {
        push(
            "RLC restore list CNTL",
            kind::RLC_RESTORE_LIST_CNTL,
            rlc_image(f.rlc, 0x74, 0x78)?,
        );
        push(
            "RLC restore list GPM",
            kind::RLC_RESTORE_LIST_GPM,
            rlc_image(f.rlc, 0x84, 0x88)?,
        );
        push(
            "RLC restore list SRM",
            kind::RLC_RESTORE_LIST_SRM,
            rlc_image(f.rlc, 0x94, 0x98)?,
        );
    }
    if rlc.minor >= 2 {
        push("RLC IRAM", kind::RLC_IRAM, rlc_image(f.rlc, 0x9c, 0xa0)?);
        push(
            "RLC DRAM",
            kind::RLC_DRAM_BOOT,
            rlc_image(f.rlc, 0xa4, 0xa8)?,
        );
    }
    push("RLC_G", kind::RLC_G, rlc.payload);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::format;
    use std::vec;

    fn put32(b: &mut [u8], at: usize, v: u32) {
        b[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }

    /// A container: the common header, `payload` at 0x100.
    fn file(major: u16, minor: u16, header_size: u32, payload: &[u8]) -> Vec<u8> {
        let mut f = vec![0u8; 0x100 + payload.len()];
        let len = u32::try_from(f.len()).unwrap();
        put32(&mut f, 0, len);
        put32(&mut f, 4, header_size);
        f[8..10].copy_from_slice(&major.to_le_bytes());
        f[10..12].copy_from_slice(&minor.to_le_bytes());
        put32(&mut f, 0x14, u32::try_from(payload.len()).unwrap());
        put32(&mut f, 0x18, 0x100);
        f[0x100..].copy_from_slice(payload);
        f
    }

    /// A MEC file: code then a jump table of `jt` dwords at the end.
    fn mec(code: usize, jt: u32) -> Vec<u8> {
        let mut payload = vec![0xaau8; code];
        payload.extend(vec![0xbbu8; jt as usize * 4]);
        let mut f = file(1, 0, 44, &payload);
        put32(&mut f, 0x28, jt);
        f
    }

    /// An RLC v2.`minor` file with the main image and the listed images
    /// appended after the payload: (header size field, offset field, bytes).
    fn rlc(minor: u16, extra: &[(usize, usize, Vec<u8>)]) -> Vec<u8> {
        let mut f = file(2, minor, 0xf4, &[0x11; 64]);
        for (size_at, offset_at, data) in extra {
            let at = f.len();
            f.extend(data);
            put32(&mut f, *size_at, u32::try_from(data.len()).unwrap());
            put32(&mut f, *offset_at, u32::try_from(at).unwrap());
        }
        f
    }

    fn files<'a>(sdma: &'a [u8], cp: &'a [u8], mec: &'a [u8], rlc: &'a [u8]) -> Files<'a> {
        Files {
            sdma,
            ce: cp,
            pfp: cp,
            me: cp,
            mec,
            mec2: mec,
            rlc,
        }
    }

    #[test]
    fn the_list_is_in_the_security_processors_order() {
        let cp = file(1, 0, 44, &[1; 512]);
        let sdma = file(1, 0, 48, &[2; 256]);
        let m = mec(400, 8);
        let r = rlc(
            2,
            &[
                (0x74, 0x78, vec![3; 16]),
                (0x84, 0x88, vec![4; 16]),
                (0x94, 0x98, vec![5; 16]),
                (0x9c, 0xa0, vec![6; 32]),
                (0xa4, 0xa8, vec![7; 32]),
            ],
        );
        let list = load_list(&files(&sdma, &cp, &m, &r)).unwrap();
        let order: Vec<(&str, u32, usize)> = list
            .iter()
            .map(|i| (i.name, i.kind, i.data.len()))
            .collect();
        assert_eq!(
            order,
            [
                ("SDMA0", 9, 256),
                ("CE", 3, 512),
                ("PFP", 2, 512),
                ("ME", 1, 512),
                ("MEC1", 4, 400),
                ("MEC2", 4, 400),
                ("RLC restore list CNTL", 22, 16),
                ("RLC restore list GPM", 20, 16),
                ("RLC restore list SRM", 21, 16),
                ("RLC IRAM", 26, 32),
                ("RLC DRAM", 48, 32),
                ("RLC_G", 8, 64),
            ]
        );
        // The MEC image is the code only, not the jump table.
        assert!(list[4].data.iter().all(|b| *b == 0xaa));
    }

    #[test]
    fn absent_images_and_tap_delays_follow_the_rlc_version() {
        let cp = file(1, 0, 44, &[1; 64]);
        let sdma = file(1, 0, 48, &[2; 64]);
        let m = mec(32, 4);
        // v2.4: tap delays come before the restore lists; sizes of 0 leave
        // an image out (SE1 here).
        let r = rlc(
            4,
            &[
                (0xcc, 0xd0, vec![8; 12]),
                (0xd4, 0xd8, vec![9; 12]),
                (0x74, 0x78, vec![3; 16]),
            ],
        );
        let list = load_list(&files(&sdma, &cp, &m, &r)).unwrap();
        let kinds: Vec<u32> = list.iter().map(|i| i.kind).collect();
        assert_eq!(kinds, [9, 3, 2, 1, 4, 4, 27, 28, 22, 8]);
        // v2.0 has no restore lists or IRAM at all, even with bytes there.
        let r = rlc(0, &[(0x74, 0x78, vec![3; 16]), (0x9c, 0xa0, vec![6; 16])]);
        let kinds: Vec<u32> = load_list(&files(&sdma, &cp, &m, &r))
            .unwrap()
            .iter()
            .map(|i| i.kind)
            .collect();
        assert_eq!(kinds, [9, 3, 2, 1, 4, 4, 8]);
    }

    #[test]
    fn a_bad_file_is_refused_not_trusted() {
        let cp = file(1, 0, 44, &[1; 64]);
        let sdma = file(1, 0, 48, &[2; 64]);
        let m = mec(32, 4);
        let r = rlc(2, &[]);
        let ok = files(&sdma, &cp, &m, &r);
        assert!(load_list(&ok).is_ok());
        // Too short, a wrong size field, a payload past the end, a wrong
        // major version.
        assert_eq!(
            load_list(&Files { ce: &cp[..8], ..ok }).unwrap_err(),
            Error::Short
        );
        let mut bad = cp.clone();
        put32(&mut bad, 0, 12345);
        assert_eq!(
            load_list(&Files { pfp: &bad, ..ok }).unwrap_err(),
            Error::Size
        );
        let mut bad = cp.clone();
        put32(&mut bad, 0x14, 0xffff_0000);
        assert_eq!(
            load_list(&Files { me: &bad, ..ok }).unwrap_err(),
            Error::Layout
        );
        let bad = file(3, 0, 44, &[1; 64]);
        assert_eq!(
            load_list(&Files { ce: &bad, ..ok }).unwrap_err(),
            Error::Version
        );
        // A jump table bigger than the payload.
        let mut bad = m.clone();
        put32(&mut bad, 0x28, 0x1000);
        assert_eq!(
            load_list(&Files { mec: &bad, ..ok }).unwrap_err(),
            Error::Range
        );
        // An RLC image pointing outside the file.
        let mut bad = r.clone();
        put32(&mut bad, 0x9c, 64);
        put32(&mut bad, 0xa0, 0xffff_fff0);
        assert_eq!(
            load_list(&Files { rlc: &bad, ..ok }).unwrap_err(),
            Error::Range
        );
        // The RLC file's size field is not checked.
        let mut odd = r.clone();
        put32(&mut odd, 0, 1);
        assert!(load_list(&Files { rlc: &odd, ..ok }).is_ok());
    }

    /// With the vendor's files at `$AMDGPU_FIRMWARE_DIR`, the specification's
    /// own test vectors (02 section 11): skipped without them.
    #[test]
    fn the_real_raphael_files_give_the_specifications_images() {
        let Ok(dir) = std::env::var("AMDGPU_FIRMWARE_DIR") else {
            return;
        };
        let read = |n: &str| std::fs::read(format!("{dir}/{n}")).unwrap();
        let (sdma, ce, pfp, me, mec, mec2, rlc) = (
            read("sdma_5_2_6.bin"),
            read("gc_10_3_6_ce.bin"),
            read("gc_10_3_6_pfp.bin"),
            read("gc_10_3_6_me.bin"),
            read("gc_10_3_6_mec.bin"),
            read("gc_10_3_6_mec2.bin"),
            read("gc_10_3_6_rlc.bin"),
        );
        let list = load_list(&Files {
            sdma: &sdma,
            ce: &ce,
            pfp: &pfp,
            me: &me,
            mec: &mec,
            mec2: &mec2,
            rlc: &rlc,
        })
        .unwrap();
        let got: Vec<(u32, usize)> = list.iter().map(|i| (i.kind, i.data.len())).collect();
        assert_eq!(
            got,
            [
                (9, 33_792),
                (3, 263_040),
                (2, 263_168),
                (1, 263_168),
                (4, 267_440),
                (4, 267_440),
                (22, 0x250),
                (20, 0x600),
                (21, 0x4480),
                (26, 0x10200),
                (48, 0x10200),
                (8, 25_088),
            ]
        );
    }
}
