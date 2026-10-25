// SPDX-License-Identifier: BSD-2-Clause
//! Disk images: a GPT disk with a FAT32 EFI System Partition holding the
//! loader at the removable-media boot path, the kernel at
//! `EFI/ABYSS/KERNEL`, the command line at `EFI/ABYSS/CMDLINE`, and the
//! memory disk (FreeBSD's `/rescue` and our test programs as a ustar
//! archive, once `cargo xtask fetch-base` has run) at `EFI/ABYSS/MD`.
//!
//! A UFS2 root variant (on FreeBSD, where `makefs` is) puts all of
//! `base.txz` and the kernel (`/boot/kernel/kernel`) on a `freebsd-ufs`
//! partition after the ESP, where the loader looks first; the kernel
//! mounts it as root, and there is no memory disk.
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

/// The UFS2 root's `/etc/fstab`: its partitions by GPT label, as
/// FreeBSD's installer writes it.
const FSTAB: &str = "# Device\t\tMountpoint\tFStype\tOptions\tDump\tPass#\n\
                     /dev/gpt/rootfs0\t/\t\tufs\trw\t1\t1\n\
                     /dev/gpt/efiboot0\t/boot/efi\tmsdosfs\trw\t2\t2\n";

/// Where the root image keeps the static tools.
pub(crate) const STATIC_TOOLS: &str = "usr/tests/static";

/// Where the root image keeps pjdfstest.
pub(crate) const PJDFSTEST: &str = "usr/tests/pjdfstest";

/// Stands in for `openssl md5`, which pjdfstest's scripts use to make
/// names, until base's own `openssl` can run (P6).
const OPENSSL_MD5: &str = "#!/bin/sh\n\
                           [ \"$*\" = md5 ] || { echo \"openssl: only md5 here\" >&2; exit 1; }\n\
                           exec md5\n";

/// As [`build`], with the root on a UFS2 partition that `makefs` makes
/// (FreeBSD only), with soft updates: all of `base.txz` (owners and flags
/// from its specification), the kernel at `/boot/kernel/kernel`, an
/// `/etc/fstab`, the memory disk's `/usr/tests`, in [`STATIC_TOOLS`]
/// static `find`, `mtree`, and what pjdfstest's scripts need, with the
/// base's specifications (`base.mtree` for owners, modes, and flags;
/// `base-data.mtree` for sizes and digests), and pjdfstest in
/// [`PJDFSTEST`]. The loader finds the kernel there and
/// hands the kernel the fstab's root; there is no memory disk.
pub(crate) fn build_ufs_root(arch: Arch, opts: build::Options, cmdline: &str) -> Result<PathBuf> {
    ufs_root(arch, opts, cmdline, Root::Tests)
}

/// As [`build_ufs_root`], with what FreeBSD's installer adds to the base
/// ([`crate::multiuser::installed_files`]): the multi-user boot's root
/// (docs/MULTIUSER.md §14).
pub(crate) fn build_installed_root(
    arch: Arch,
    opts: build::Options,
    cmdline: &str,
) -> Result<PathBuf> {
    ufs_root(arch, opts, cmdline, Root::Installed)
}

/// Which UFS2 root to make.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Root {
    /// The base and the test programs.
    Tests,
    /// That, as the installer leaves it.
    Installed,
}

fn ufs_root(arch: Arch, opts: build::Options, cmdline: &str, root: Root) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let images = build::all(arch, opts)?;
    let read = |p: &PathBuf| fs::read(p).map_err(|e| format!("{}: {e}", p.display()));
    let loader = read(&images.loader)?;
    let dir = util::root().join("build").join(arch.to_string());
    let name = match root {
        Root::Tests => "ufsroot",
        Root::Installed => "installedroot",
    };
    // What goes over the base: made afresh each time.
    let stage = dir.join(name);
    let _ = fs::remove_dir_all(&stage);
    fs::create_dir_all(stage.join("boot/kernel"))?;
    fs::create_dir_all(stage.join("etc"))?;
    fs::create_dir_all(stage.join(STATIC_TOOLS))?;
    fs::copy(&images.kernel, stage.join("boot/kernel/kernel"))?;
    fs::write(stage.join("etc/fstab"), FSTAB)?;
    if root == Root::Installed {
        for (path, mode, data) in crate::multiuser::installed_files(&crate::base::full(arch)?)? {
            let dst = stage.join(path);
            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&dst, data)?;
            fs::set_permissions(&dst, fs::Permissions::from_mode(mode))?;
        }
    }
    if let Some(md) = crate::base::md_image(arch, &[])? {
        let tar = dir.join(format!("{name}-md.tar"));
        fs::write(&tar, md)?;
        util::run(
            std::process::Command::new("tar")
                .arg("-xf")
                .arg(&tar)
                .arg("-C")
                .arg(&stage)
                .arg("usr/tests"),
        )?;
        fs::remove_file(&tar)?;
    }
    for (name, bin) in crate::base::static_tools(arch)? {
        fs::copy(&bin, stage.join(STATIC_TOOLS).join(name))?;
    }
    {
        let shim = stage.join(STATIC_TOOLS).join("openssl");
        fs::write(&shim, OPENSSL_MD5)?;
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o555))?;
    }
    util::run(
        std::process::Command::new("cp")
            .arg("-R")
            .arg(crate::base::pjdfstest_suite(arch)?)
            .arg(stage.join(PJDFSTEST)),
    )?;
    // What the root should hold, for its `mtree` to check.
    let tools = stage.join(STATIC_TOOLS);
    fs::copy(crate::base::spec(arch)?, tools.join("base.mtree"))?;
    fs::copy(crate::base::data_spec(arch)?, tools.join("base-data.mtree"))?;
    // Owners and modes for what the base's specification lacks.
    let base_spec = fs::read_to_string(crate::base::spec(arch)?)?;
    let known: std::collections::HashSet<&str> = base_spec
        .lines()
        .filter_map(|l| l.split_ascii_whitespace().next())
        .collect();
    let mut spec = base_spec.clone();
    add_spec(&stage, &stage, &known, &mut spec)?;
    let spec_path = dir.join(format!("{name}.mtree"));
    fs::write(&spec_path, spec)?;

    let img = dir.join(format!("{name}.img"));
    let _ = fs::remove_file(&img);
    util::run(
        std::process::Command::new("makefs")
            .args(["-t", "ffs", "-o", "version=2,softupdates=1"])
            // Room for the tests to write.
            .args(["-b", "25%", "-f", "25%"])
            .arg("-F")
            .arg(&spec_path)
            .arg(&img)
            .arg(crate::base::full(arch)?)
            .arg(&stage),
    )?;
    let ufs = read(&img)?;
    fs::remove_file(&img)?;
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
    let path = dir.join(match root {
        Root::Tests => "disk-ufs.img",
        Root::Installed => "disk-installed.img",
    });
    fs::write(&path, &disk)?;
    Ok(path)
}

/// Appends to `spec` an entry, root's, for everything under `at` (within
/// `stage`) that `known` lacks.
fn add_spec(
    stage: &std::path::Path,
    at: &std::path::Path,
    known: &std::collections::HashSet<&str>,
    spec: &mut String,
) -> Result<()> {
    use std::fmt::Write as _;
    use std::os::unix::fs::PermissionsExt;
    let mut entries: Vec<_> = fs::read_dir(at)?.collect::<std::io::Result<_>>()?;
    entries.sort_by_key(fs::DirEntry::file_name);
    for e in entries {
        let path = e.path();
        let rel = path.strip_prefix(stage).map_err(|e| e.to_string())?;
        let name = format!("./{}", rel.display());
        let meta = e.metadata()?;
        let kind = if meta.is_dir() { "dir" } else { "file" };
        if !known.contains(name.as_str()) {
            let mode = meta.permissions().mode() & 0o7777;
            let _ = writeln!(
                spec,
                "{name} type={kind} mode={mode:o} uid=0 gid=0 flags=none"
            );
        }
        if meta.is_dir() {
            add_spec(stage, &path, known, spec)?;
        }
    }
    Ok(())
}

/// The byte range of the UFS2 partition on a disk image
/// [`build_ufs_root`] made.
pub(crate) fn ufs_partition(disk: &std::path::Path) -> Result<std::ops::Range<u64>> {
    let mut f = fs::File::open(disk)?;
    let mut head = vec![0u8; 34 * gpt::SECTOR];
    std::io::Read::read_exact(&mut f, &mut head)?;
    let h = gpt::PrimaryHeader::read(&head[gpt::SECTOR..2 * gpt::SECTOR])
        .map_err(|e| format!("GPT: {e:?}"))?;
    let at = usize::try_from(h.entries_lba).unwrap_or(0) * gpt::SECTOR;
    let len = h.entries_len;
    let table = gpt::Table::from_parts(&h, head.get(at..at + len).ok_or("GPT entries")?)
        .map_err(|e| format!("GPT: {e:?}"))?;
    let p = table
        .partitions()
        .find(|p| p.type_guid == gpt::types::FREEBSD_UFS)
        .ok_or("no freebsd-ufs partition")?;
    let sector = gpt::SECTOR as u64;
    Ok(p.first_lba * sector..(p.last_lba + 1) * sector)
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
