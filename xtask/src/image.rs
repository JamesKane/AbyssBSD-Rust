// SPDX-License-Identifier: BSD-2-Clause
//! Disk images: a GPT disk with a FAT32 EFI System Partition holding the
//! loader at the removable-media boot path.
//!
//! Images are reproducible: GUIDs and the volume serial derive from stable
//! names, and FAT timestamps are fixed.

use std::fs;
use std::path::PathBuf;

use crate::arch::Arch;
use crate::build;
use crate::cli::Args;
use crate::util::{self, Result};

const MIB: usize = 1 << 20;
const DISK_BYTES: usize = 128 * MIB;
/// The ESP starts 1 MiB in, the usual alignment.
const ESP_FIRST_LBA: u64 = 2048;
const ESP_BYTES: usize = 64 * MIB;

pub(crate) fn cmd(mut args: Args) -> Result<()> {
    let arch = Arch::one_from_args(&mut args)?;
    let opts = build::Options::from_args(&mut args);
    args.finish()?;
    let path = build(arch, opts)?;
    println!("{}", path.display());
    Ok(())
}

/// Builds the loader and the disk image, returning the image's path.
pub(crate) fn build(arch: Arch, opts: build::Options) -> Result<PathBuf> {
    let loader = build::loader(arch, opts)?;
    let efi = fs::read(&loader).map_err(|e| format!("{}: {e}", loader.display()))?;
    let disk = make_disk(arch, &efi)?;
    let dir = util::root().join("build").join(arch.to_string());
    fs::create_dir_all(&dir)?;
    let name = if opts.ktest {
        "disk-ktest.img"
    } else {
        "disk.img"
    };
    let path = dir.join(name);
    fs::write(&path, &disk)?;
    Ok(path)
}

/// A GUID derived from a stable name, so rebuilt images are identical.
fn stable_guid(name: &str) -> guid::Guid {
    let mut bytes = [0u8; 16];
    for (salt, chunk) in (0u8..).zip(bytes.as_chunks_mut::<4>().0) {
        let mut c = crc::Crc32::new();
        c.update(name.as_bytes());
        c.update(&[salt]);
        *chunk = c.finish().to_le_bytes();
    }
    guid::Guid::v8_from_bytes(bytes)
}

fn make_disk(arch: Arch, efi: &[u8]) -> Result<Vec<u8>> {
    let mut disk = vec![0u8; DISK_BYTES];
    let esp_sectors = (ESP_BYTES / gpt::SECTOR) as u64;
    let esp = gpt::PartitionSpec {
        type_guid: gpt::types::EFI_SYSTEM,
        unique_guid: stable_guid(&format!("abyss-{arch}-esp")),
        first_lba: ESP_FIRST_LBA,
        last_lba: ESP_FIRST_LBA + esp_sectors - 1,
        attributes: 0,
        name: "efiboot0",
    };
    gpt::write(
        &mut disk,
        stable_guid(&format!("abyss-{arch}-disk")),
        &[esp],
    )
    .map_err(|e| format!("GPT: {e:?}"))?;

    let mut root = fat::Dir::new();
    root.insert_file(arch.efi_boot_file(), efi)
        .map_err(|e| format!("ESP tree: {e:?}"))?;
    let opts = fat::FormatOptions {
        volume_id: crc::crc32(format!("abyss-{arch}-esp").as_bytes()),
        label: fat::VolumeLabel::new("ABYSS_ESP").map_err(|e| format!("{e:?}"))?,
        hidden_sectors: u32::try_from(ESP_FIRST_LBA).unwrap_or(0),
        timestamp: fat::Timestamp::EPOCH,
    };
    let start = usize::try_from(ESP_FIRST_LBA).unwrap_or(0) * gpt::SECTOR;
    let volume = &mut disk[start..start + ESP_BYTES];
    fat::format(volume, &opts, &root).map_err(|e| format!("FAT32: {e:?}"))?;

    // Read everything back through the parsers before handing it out.
    let table = gpt::Table::read(&disk).map_err(|e| format!("GPT read-back: {e:?}"))?;
    if table.partitions().count() != 1 {
        return Err("GPT read-back: wrong partition count".into());
    }
    let v = fat::Volume::open(&disk[start..start + ESP_BYTES])
        .map_err(|e| format!("FAT read-back: {e:?}"))?;
    if v.read_file(arch.efi_boot_file())
        .map_err(|e| format!("FAT read-back: {e:?}"))?
        != efi
    {
        return Err("FAT read-back: loader contents differ".into());
    }
    Ok(disk)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_are_reproducible() {
        let a = make_disk(Arch::X86_64, b"MZ fake loader").unwrap();
        let b = make_disk(Arch::X86_64, b"MZ fake loader").unwrap();
        assert!(a == b);
        let c = make_disk(Arch::Aarch64, b"MZ fake loader").unwrap();
        assert!(a != c);
    }
}
