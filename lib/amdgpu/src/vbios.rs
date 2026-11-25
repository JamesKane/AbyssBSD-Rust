// SPDX-License-Identifier: MIT
// Copyright (c) 2026 The AbyssBSD Project
//! The video BIOS (specification 01 sections 5.2 to 5.4): the image the
//! firmware's `VFCT` table holds for the device, its ATOM header and the
//! lists of command and data tables.

use alloc::vec::Vec;

/// Why an image was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The table is too short for its header or a block.
    Short,
    /// No block is for this device.
    NotFound,
    /// The image lacks the ROM signature or the ATOM header.
    BadImage,
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*b.get(at)?, *b.get(at + 1)?]))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes([
        *b.get(at)?,
        *b.get(at + 1)?,
        *b.get(at + 2)?,
        *b.get(at + 3)?,
    ]))
}

/// The image for the device `vendor:device` at PCI device number `dev` and
/// function `func` in the `VFCT` table `vfct` (the whole table with its ACPI
/// header). A bus mismatch is tolerated (buses are renumbered); a block with
/// no image is skipped (01 section 5.2).
///
/// # Errors
///
/// [`Error::Short`] for a block that runs past the table, [`Error::NotFound`]
/// if none matches.
pub fn vfct_image(
    vfct: &[u8],
    vendor: u16,
    device: u16,
    dev: u32,
    func: u32,
) -> Result<Vec<u8>, Error> {
    let length = usize::try_from(u32_at(vfct, 4).ok_or(Error::Short)?).map_err(|_| Error::Short)?;
    let table = vfct.get(..length).ok_or(Error::Short)?;
    let mut at =
        usize::try_from(u32_at(table, 0x34).ok_or(Error::Short)?).map_err(|_| Error::Short)?;
    while at + 28 <= table.len() {
        let image_len = usize::try_from(u32_at(table, at + 0x18).ok_or(Error::Short)?)
            .map_err(|_| Error::Short)?;
        let end = at + 28 + image_len;
        if end > table.len() {
            return Err(Error::Short);
        }
        let matches = u16_at(table, at + 0x0c) == Some(vendor)
            && u16_at(table, at + 0x0e) == Some(device)
            && u32_at(table, at + 4) == Some(dev)
            && u32_at(table, at + 8) == Some(func);
        if image_len != 0 && matches {
            return Ok(table[at + 28..end].to_vec());
        }
        at = end;
    }
    Err(Error::NotFound)
}

/// The tables of an image: the ATOM header's lists of command and data
/// tables (01 sections 5.3 and 5.4, 12 section 1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    pub bytes: Vec<u8>,
    /// The offsets of the two table lists.
    command_list: usize,
    data_list: usize,
}

impl Image {
    /// Checks the image (the ROM signature, the ATOM header) and finds the
    /// table lists.
    ///
    /// # Errors
    ///
    /// [`Error::BadImage`] if it is not an AtomBIOS image.
    pub fn parse(bytes: Vec<u8>) -> Result<Self, Error> {
        if bytes.len() < 0x4a || bytes[0] != 0x55 || bytes[1] != 0xaa {
            return Err(Error::BadImage);
        }
        let header = usize::from(u16_at(&bytes, 0x48).ok_or(Error::BadImage)?);
        let atom = bytes.get(header + 4..header + 8).ok_or(Error::BadImage)?;
        if atom != b"ATOM" && atom != b"MOTA" {
            return Err(Error::BadImage);
        }
        let command_list = usize::from(u16_at(&bytes, header + 0x1e).ok_or(Error::BadImage)?);
        let data_list = usize::from(u16_at(&bytes, header + 0x20).ok_or(Error::BadImage)?);
        if command_list + 4 > bytes.len() || data_list + 4 > bytes.len() {
            return Err(Error::BadImage);
        }
        Ok(Self {
            bytes,
            command_list,
            data_list,
        })
    }

    fn entry(&self, list: usize, index: usize) -> Option<usize> {
        let size = usize::from(u16_at(&self.bytes, list)?);
        let count = size.checked_sub(4)? / 2;
        if index >= count {
            return None;
        }
        let at = usize::from(u16_at(&self.bytes, list + 4 + 2 * index)?);
        (at != 0 && at + 4 <= self.bytes.len()).then_some(at)
    }

    /// The image offset of command table `index`, if present.
    #[must_use]
    pub fn command_table(&self, index: usize) -> Option<usize> {
        self.entry(self.command_list, index)
    }

    /// The image offset of data table `index`, if present.
    #[must_use]
    pub fn data_table(&self, index: usize) -> Option<usize> {
        self.entry(self.data_list, index)
    }

    /// The boot engine and memory clocks (10 kHz units) of the firmware-info
    /// table (data table 4, offsets `0x08` and `0x0c`; 01 section 5.5).
    #[must_use]
    pub fn boot_clocks(&self) -> Option<(u32, u32)> {
        let t = self.data_table(4)?;
        Some((
            u32_at(&self.bytes, t + 0x08)?,
            u32_at(&self.bytes, t + 0x0c)?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A block of a VFCT: (bus, dev, func, vendor, device, image).
    type Block<'a> = (u32, u32, u32, u16, u16, &'a [u8]);

    /// A VFCT with the given blocks.
    fn vfct(blocks: &[Block<'_>]) -> Vec<u8> {
        let mut t = alloc::vec![0u8; 0x40];
        t[..4].copy_from_slice(b"VFCT");
        t[0x34..0x38].copy_from_slice(&0x40u32.to_le_bytes());
        for (bus, dev, func, vendor, device, image) in blocks {
            let mut h = alloc::vec![0u8; 28];
            h[0..4].copy_from_slice(&bus.to_le_bytes());
            h[4..8].copy_from_slice(&dev.to_le_bytes());
            h[8..12].copy_from_slice(&func.to_le_bytes());
            h[0x0c..0x0e].copy_from_slice(&vendor.to_le_bytes());
            h[0x0e..0x10].copy_from_slice(&device.to_le_bytes());
            h[0x18..0x1c].copy_from_slice(&u32::try_from(image.len()).unwrap().to_le_bytes());
            t.extend_from_slice(&h);
            t.extend_from_slice(image);
        }
        let len = u32::try_from(t.len()).unwrap();
        t[4..8].copy_from_slice(&len.to_le_bytes());
        t
    }

    /// A small AtomBIOS image: the ATOM header at 0x60, the command list at
    /// 0x100 (index 0 at 0x120, index 3 absent) and the data list at 0x140.
    fn image() -> Vec<u8> {
        let mut b = alloc::vec![0u8; 0x200];
        b[0] = 0x55;
        b[1] = 0xaa;
        b[0x48..0x4a].copy_from_slice(&0x60u16.to_le_bytes());
        b[0x64..0x68].copy_from_slice(b"ATOM");
        b[0x60 + 0x1e..0x60 + 0x20].copy_from_slice(&0x100u16.to_le_bytes());
        b[0x60 + 0x20..0x60 + 0x22].copy_from_slice(&0x140u16.to_le_bytes());
        // Command list: size 8 (two entries), entry 0 = 0x120, entry 1 = 0.
        b[0x100..0x102].copy_from_slice(&8u16.to_le_bytes());
        b[0x104..0x106].copy_from_slice(&0x120u16.to_le_bytes());
        // Data list: size 14 (five entries); entry 4 = 0x180.
        b[0x140..0x142].copy_from_slice(&14u16.to_le_bytes());
        b[0x144 + 8..0x144 + 10].copy_from_slice(&0x180u16.to_le_bytes());
        // Firmware info: boot clocks at +8 and +12.
        b[0x188..0x18c].copy_from_slice(&40_000u32.to_le_bytes());
        b[0x18c..0x190].copy_from_slice(&120_000u32.to_le_bytes());
        b
    }

    #[test]
    fn the_image_for_a_device_is_found_by_its_ids_and_slot() {
        let a = alloc::vec![1u8; 8];
        let b = alloc::vec![2u8; 16];
        let t = vfct(&[
            (3, 1, 0, 0x1002, 0x73df, &a),
            // A bus other than the table's: tolerated.
            (9, 0, 0, 0x1002, 0x164e, &b),
            // No image: skipped.
            (0, 0, 0, 0x1002, 0x164e, &[]),
        ]);
        assert_eq!(vfct_image(&t, 0x1002, 0x164e, 0, 0), Ok(b));
        assert_eq!(vfct_image(&t, 0x1002, 0x73df, 1, 0), Ok(a));
        // The wrong function or device does not match.
        assert_eq!(vfct_image(&t, 0x1002, 0x164e, 0, 1), Err(Error::NotFound));
        assert_eq!(vfct_image(&t, 0x1002, 0x9999, 0, 0), Err(Error::NotFound));
    }

    #[test]
    fn a_block_past_the_end_is_refused() {
        let mut t = vfct(&[(0, 0, 0, 0x1002, 0x164e, &[7; 8])]);
        // Say the image is longer than the table.
        t[0x40 + 0x18..0x40 + 0x1c].copy_from_slice(&100u32.to_le_bytes());
        assert_eq!(vfct_image(&t, 0x1002, 0x164e, 0, 0), Err(Error::Short));
        assert_eq!(vfct_image(&[0; 4], 0x1002, 0x164e, 0, 0), Err(Error::Short));
    }

    #[test]
    fn the_tables_are_found_through_the_lists() {
        let i = Image::parse(image()).unwrap();
        assert_eq!(i.command_table(0), Some(0x120));
        // Absent and out of the list.
        assert_eq!(i.command_table(1), None);
        assert_eq!(i.command_table(7), None);
        assert_eq!(i.data_table(4), Some(0x180));
        assert_eq!(i.boot_clocks(), Some((40_000, 120_000)));
    }

    #[test]
    fn an_image_without_the_signatures_is_refused() {
        let mut b = image();
        b[0] = 0;
        assert_eq!(Image::parse(b), Err(Error::BadImage));
        let mut b = image();
        b[0x64] = b'X';
        assert_eq!(Image::parse(b), Err(Error::BadImage));
        assert_eq!(Image::parse(alloc::vec![0x55, 0xaa]), Err(Error::BadImage));
    }
}
