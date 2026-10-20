// SPDX-License-Identifier: BSD-2-Clause
//! Disk images: a GPT disk with a FAT32 EFI System Partition holding the
//! loader at the removable-media boot path, the kernel at
//! `EFI/ABYSS/KERNEL`, the command line at `EFI/ABYSS/CMDLINE`, and the
//! memory disk (FreeBSD's `/rescue` and our test programs as a ustar
//! archive, once `cargo xtask fetch-base` has run) at `EFI/ABYSS/MD`.
//!
//! A UFS2 root variant (on FreeBSD, where `makefs` is) moves the kernel
//! and the memory disk to `/boot/kernel/kernel` and `/boot/abyss/md` on a
//! `freebsd-ufs` partition after the ESP, where the loader looks first.
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
const DISK_BYTES: usize = 192 * MIB;
/// The ESP starts 1 MiB in, the usual alignment.
const ESP_FIRST_LBA: u64 = 2048;
const ESP_BYTES: usize = 128 * MIB;

pub(crate) fn cmd(mut args: Args) -> Result<()> {
    let arch = Arch::one_from_args(&mut args)?;
    let opts = build::Options::from_args(&mut args);
    let cmdline = args.take_value("--cmdline")?.unwrap_or_default();
    args.finish()?;
    let path = build(arch, opts, &cmdline)?;
    println!("{}", path.display());
    Ok(())
}

/// The files to put on the ESP, and a UFS2 image for a root partition.
#[derive(Debug)]
struct Contents<'a> {
    loader: &'a [u8],
    /// On the ESP; `None` when the UFS2 root holds it.
    kernel: Option<&'a [u8]>,
    cmdline: &'a str,
    md: Option<&'a [u8]>,
    ufs: Option<&'a [u8]>,
}

/// Builds the loader, the kernel, and the disk image, returning the image's
/// path.
pub(crate) fn build(arch: Arch, opts: build::Options, cmdline: &str) -> Result<PathBuf> {
    build_with(arch, opts, cmdline, &[])
}

/// As [`build`], with `extra` files (path on the memory disk, file here)
/// added to the memory disk.
pub(crate) fn build_with(
    arch: Arch,
    opts: build::Options,
    cmdline: &str,
    extra: &[(&str, &std::path::Path)],
) -> Result<PathBuf> {
    let images = build::all(arch, opts)?;
    let read = |p: &PathBuf| fs::read(p).map_err(|e| format!("{}: {e}", p.display()));
    let (loader, kernel) = (read(&images.loader)?, read(&images.kernel)?);
    let md = crate::base::md_image(arch, extra)?;
    let disk = make_disk(
        arch,
        &Contents {
            loader: &loader,
            kernel: Some(&kernel),
            cmdline,
            md: md.as_deref(),
            ufs: None,
        },
    )?;
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

/// As [`build`], with the kernel and the memory disk on a UFS2 root
/// partition that `makefs` makes (FreeBSD only), not on the ESP.
pub(crate) fn build_ufs_root(arch: Arch, opts: build::Options, cmdline: &str) -> Result<PathBuf> {
    let images = build::all(arch, opts)?;
    let read = |p: &PathBuf| fs::read(p).map_err(|e| format!("{}: {e}", p.display()));
    let loader = read(&images.loader)?;
    let dir = util::root().join("build").join(arch.to_string());
    let stage = dir.join("ufsroot");
    let _ = fs::remove_dir_all(&stage);
    fs::create_dir_all(stage.join("boot/kernel"))?;
    fs::create_dir_all(stage.join("boot/abyss"))?;
    fs::copy(&images.kernel, stage.join("boot/kernel/kernel"))?;
    if let Some(md) = crate::base::md_image(arch, &[])? {
        fs::write(stage.join("boot/abyss/md"), md)?;
    }
    let img = dir.join("ufsroot.img");
    let _ = fs::remove_file(&img);
    util::run(
        std::process::Command::new("makefs")
            .args(["-t", "ffs", "-o", "version=2"])
            .arg(&img)
            .arg(&stage),
    )?;
    let ufs = read(&img)?;
    let disk = make_disk(
        arch,
        &Contents {
            loader: &loader,
            kernel: None,
            cmdline,
            md: None,
            ufs: Some(&ufs),
        },
    )?;
    let path = dir.join("disk-ufs.img");
    fs::write(&path, &disk)?;
    Ok(path)
}

/// Reads a file from the ESP of a disk image `build` made.
pub(crate) fn read_esp_file(disk: &std::path::Path, path: &str) -> Result<Vec<u8>> {
    let bytes = fs::read(disk).map_err(|e| format!("{}: {e}", disk.display()))?;
    let start = usize::try_from(ESP_FIRST_LBA).unwrap_or(0) * gpt::SECTOR;
    let esp = bytes
        .get(start..start + ESP_BYTES)
        .ok_or("disk image too small")?;
    let v = fat::Volume::open(esp).map_err(|e| format!("ESP: {e:?}"))?;
    v.read_file(path)
        .map_err(|e| format!("ESP {path}: {e:?}").into())
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

fn make_disk(arch: Arch, contents: &Contents<'_>) -> Result<Vec<u8>> {
    let esp_sectors = (ESP_BYTES / gpt::SECTOR) as u64;
    let esp = gpt::PartitionSpec {
        type_guid: gpt::types::EFI_SYSTEM,
        unique_guid: stable_guid(&format!("abyss-{arch}-esp")),
        first_lba: ESP_FIRST_LBA,
        last_lba: ESP_FIRST_LBA + esp_sectors - 1,
        attributes: 0,
        name: "efiboot0",
    };
    // The UFS2 root, 1 MiB-aligned after the ESP; the disk grows to fit
    // it and the backup table.
    let ufs_first = gpt::align_up(ESP_FIRST_LBA + esp_sectors, 2048);
    let ufs_part = contents.ufs.map(|u| gpt::PartitionSpec {
        type_guid: gpt::types::FREEBSD_UFS,
        unique_guid: stable_guid(&format!("abyss-{arch}-rootfs")),
        first_lba: ufs_first,
        last_lba: ufs_first + (u.len() / gpt::SECTOR) as u64 - 1,
        attributes: 0,
        name: "rootfs0",
    });
    let bytes = match contents.ufs {
        Some(u) => (usize::try_from(ufs_first).unwrap_or(0) * gpt::SECTOR + u.len() + MIB)
            .next_multiple_of(MIB)
            .max(DISK_BYTES),
        None => DISK_BYTES,
    };
    let mut disk = vec![0u8; bytes];
    let parts: Vec<gpt::PartitionSpec<'_>> = core::iter::once(esp).chain(ufs_part).collect();
    gpt::write(
        &mut disk,
        stable_guid(&format!("abyss-{arch}-disk")),
        &parts,
    )
    .map_err(|e| format!("GPT: {e:?}"))?;
    if let Some(u) = contents.ufs {
        let at = usize::try_from(ufs_first).unwrap_or(0) * gpt::SECTOR;
        disk[at..at + u.len()].copy_from_slice(u);
    }

    let mut root = fat::Dir::new();
    let md = contents.md.map(|m| ("EFI/ABYSS/MD", m));
    let kernel = contents.kernel.map(|k| ("EFI/ABYSS/KERNEL", k));
    for (path, data) in [
        (arch.efi_boot_file(), contents.loader),
        ("EFI/ABYSS/CMDLINE", contents.cmdline.as_bytes()),
    ]
    .into_iter()
    .chain(kernel)
    .chain(md)
    {
        root.insert_file(path, data)
            .map_err(|e| format!("ESP tree ({path}): {e:?}"))?;
    }
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
    if table.partitions().count() != 1 + usize::from(contents.ufs.is_some()) {
        return Err("GPT read-back: wrong partition count".into());
    }
    let v = fat::Volume::open(&disk[start..start + ESP_BYTES])
        .map_err(|e| format!("FAT read-back: {e:?}"))?;
    for (path, data) in core::iter::once((arch.efi_boot_file(), contents.loader)).chain(kernel) {
        if v.read_file(path)
            .map_err(|e| format!("FAT read-back ({path}): {e:?}"))?
            != data
        {
            return Err(format!("FAT read-back: {path} contents differ").into());
        }
    }
    Ok(disk)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_are_reproducible() {
        let contents = Contents {
            loader: b"MZ fake loader",
            kernel: Some(b"\x7fELF fake kernel"),
            cmdline: "kaslr=0",
            md: Some(b"ustar"),
            ufs: None,
        };
        let a = make_disk(Arch::X86_64, &contents).unwrap();
        let b = make_disk(Arch::X86_64, &contents).unwrap();
        assert!(a == b);
        let c = make_disk(Arch::Aarch64, &contents).unwrap();
        assert!(a != c);
    }
}
