// SPDX-License-Identifier: BSD-2-Clause
//! The PART class's GPT details: a partition type's `gpart` name, its
//! `efimedia` device path, and the disk geometry `g_part` makes up for
//! disks that report none.
//!
//! Behavior reference: sys/geom/part/g_part.c (`g_part_geometry`,
//! `g_part_geometry_heads`, `g_part_alias_list`), g_part_gpt.c
//! (`gpt_uuid_alias_match`, `g_part_gpt_type`, `g_part_gpt_efimedia`),
//! sys/sys/disk/gpt.h.

use alloc::format;
use alloc::string::String;

/// GPT partition types `gpart` names, by GUID (lower case), in
/// `gpt_uuid_alias_match`'s order. The GUIDs and names are FreeBSD's
/// interface (`gpart add -t`, `gpart show`).
const GPT_TYPES: &[(&str, &str)] = &[
    ("7c3457ef-0000-11aa-aa11-00306543ecac", "apple-apfs"),
    ("426f6f74-0000-11aa-aa11-00306543ecac", "apple-boot"),
    ("53746f72-6167-11aa-aa11-00306543ecac", "apple-core-storage"),
    ("48465300-0000-11aa-aa11-00306543ecac", "apple-hfs"),
    ("4c616265-6c00-11aa-aa11-00306543ecac", "apple-label"),
    ("52414944-0000-11aa-aa22-00306543ecac", "apple-raid"),
    ("52414944-5f4f-11aa-aa22-00306543ecac", "apple-raid-offline"),
    ("5265636f-7665-11aa-aa11-00306543ecac", "apple-tv-recovery"),
    ("55465300-0000-11aa-aa11-00306543ecac", "apple-ufs"),
    ("6a898cc3-1dd2-11b2-99a6-080020736631", "apple-zfs"),
    ("21686148-6449-6e6f-744e-656564454649", "bios-boot"),
    ("cab6e88e-abf3-4102-a07a-d4bb9be3c1d3", "chromeos-firmware"),
    ("fe3a2a5d-4f32-41a7-b725-accc3285a309", "chromeos-kernel"),
    ("2e0a753d-9e48-43b0-8337-b15192cb1b5e", "chromeos-reserved"),
    ("3cb8e202-3b7e-47dd-8a3c-7ff2a13cfcec", "chromeos-root"),
    ("dbd5211b-1ca5-11dc-8817-01301bb8a9f5", "dragonfly-ccd"),
    ("61dc63ac-6e38-11dc-8513-01301bb8a9f5", "dragonfly-hammer"),
    ("5cbb9ad1-862d-11dc-a94d-01301bb8a9f5", "dragonfly-hammer2"),
    ("9d087404-1ca5-11dc-8817-01301bb8a9f5", "dragonfly-label32"),
    ("3d48ce54-1d16-11dc-8696-01301bb8a9f5", "dragonfly-label64"),
    ("bd215ab2-1d16-11dc-8696-01301bb8a9f5", "dragonfly-legacy"),
    ("9d58fdbd-1ca5-11dc-8817-01301bb8a9f5", "dragonfly-swap"),
    ("9d94ce7c-1ca5-11dc-8817-01301bb8a9f5", "dragonfly-ufs"),
    ("9dd4478f-1ca5-11dc-8817-01301bb8a9f5", "dragonfly-vinum"),
    ("c12a7328-f81f-11d2-ba4b-00a0c93ec93b", "efi"),
    ("516e7cb4-6ecf-11d6-8ff8-00022d09712b", "freebsd"),
    ("83bd6b9d-7f41-11dc-be0b-001560b84f0f", "freebsd-boot"),
    ("74ba7dd9-a689-11e1-bd04-00e081286acf", "freebsd-nandfs"),
    ("516e7cb5-6ecf-11d6-8ff8-00022d09712b", "freebsd-swap"),
    ("516e7cb6-6ecf-11d6-8ff8-00022d09712b", "freebsd-ufs"),
    ("516e7cb8-6ecf-11d6-8ff8-00022d09712b", "freebsd-vinum"),
    ("516e7cba-6ecf-11d6-8ff8-00022d09712b", "freebsd-zfs"),
    ("5b193300-fc78-40cd-8002-e86c45580b47", "hifive-fsbl"),
    ("2e54b353-1271-4842-806f-e436d6af6985", "hifive-bbl"),
    ("0fc63daf-8483-4772-8e79-3d69d8477de4", "linux-data"),
    ("e6d6d379-f507-44c2-a23c-238f2a3df928", "linux-lvm"),
    ("a19d880f-05fc-4d3b-a006-743f0f84911e", "linux-raid"),
    ("0657fd6d-a4ab-43c4-84e5-0933c84b4f4f", "linux-swap"),
    ("024dee41-33e7-11d3-9d69-0008c781f39f", "mbr"),
    ("ebd0a0a2-b9e5-4433-87c0-68b6b72699c7", "ms-basic-data"),
    ("af9b60a0-1431-4f62-bc68-3311714a69ad", "ms-ldm-data"),
    ("5808c8aa-7e8f-42e0-85d2-e1e90434cfb3", "ms-ldm-metadata"),
    ("de94bba4-06d1-4d40-a16a-bfd50179d6ac", "ms-recovery"),
    ("e3c9e316-0b5c-4db8-817d-f92df00215ae", "ms-reserved"),
    ("e75caf8f-f680-4cee-afa3-b001e56efc2d", "ms-spaces"),
    ("2db519c4-b10f-11dc-b99b-0019d1879648", "netbsd-ccd"),
    ("2db519ec-b10f-11dc-b99b-0019d1879648", "netbsd-cgd"),
    ("49f48d5a-b10e-11dc-b99b-0019d1879648", "netbsd-ffs"),
    ("49f48d82-b10e-11dc-b99b-0019d1879648", "netbsd-lfs"),
    ("49f48daa-b10e-11dc-b99b-0019d1879648", "netbsd-raid"),
    ("49f48d32-b10e-11dc-b99b-0019d1879648", "netbsd-swap"),
    ("824cc7a0-36a8-11e3-890a-952519ad3f61", "openbsd-data"),
    ("9e1a2d38-c612-4316-aa26-8b49521e5a8b", "prep-boot"),
    ("6a82cb45-1dd2-11b2-99a6-080020736631", "solaris-boot"),
    ("6a85cf4d-1dd2-11b2-99a6-080020736631", "solaris-root"),
    ("6a87c46f-1dd2-11b2-99a6-080020736631", "solaris-swap"),
    ("6a8b642b-1dd2-11b2-99a6-080020736631", "solaris-backup"),
    ("6a8ef2e9-1dd2-11b2-99a6-080020736631", "solaris-var"),
    ("6a90ba39-1dd2-11b2-99a6-080020736631", "solaris-home"),
    ("6a9283a5-1dd2-11b2-99a6-080020736631", "solaris-altsec"),
    ("6a945a3b-1dd2-11b2-99a6-080020736631", "solaris-reserved"),
    ("3de21764-95bd-54bd-a5c3-4abe786f38a8", "u-boot-env"),
    ("aa31e02a-400f-11db-9590-000c2911d1b8", "vmware-vmfs"),
    ("9d275380-40ad-11db-bf97-000c2911d1b8", "vmware-vmkdiag"),
    ("9198effc-31c0-11db-8f78-000c2911d1b8", "vmware-reserved"),
    ("381cfccc-7288-11e0-92ee-000c2911d0b2", "vmware-vsanhdr"),
];

/// A GPT type's name as `gpart` shows it (`efi`, `freebsd-ufs`), or `!`
/// and its GUID for a type `gpart` has no name for. `guid` is the GUID's
/// text form, in lower case.
#[must_use]
pub fn gpt_type(guid: &str) -> String {
    GPT_TYPES
        .iter()
        .find(|(g, _)| *g == guid)
        .map_or_else(|| format!("!{guid}"), |(_, name)| String::from(*name))
}

/// `%#jx`: hexadecimal with `0x`, but plain `0` for zero.
fn alt_hex(v: u64) -> String {
    if v == 0 {
        String::from("0")
    } else {
        format!("{v:#x}")
    }
}

/// A GPT partition's UEFI device path (`efimedia`): its index, GUID (in
/// lower case), and first and last sectors.
#[must_use]
pub fn gpt_efimedia(index: u32, guid: &str, start: u64, end: u64) -> String {
    format!(
        "HD({index},GPT,{guid},{},{})",
        alt_hex(start),
        alt_hex(end.wrapping_sub(start).wrapping_add(1))
    )
}

/// The attributes GPT's `dumpconf` shows (`gpart show`'s `[bootme]`), in
/// its order, with their bits (`GPT_ENT_ATTR_*`).
pub const GPT_ATTRIBUTES: &[(u64, &str)] = &[
    (1 << 59, "bootme"),
    (1 << 58, "bootonce"),
    (1 << 57, "bootfailed"),
];

/// `g_part_geometry_heads`: for `blocks` sectors at `sectors` per track,
/// how much the best head count (of 1, 2, 16, 32, 64, 128, 255) whose
/// cylinders (at most 1023) cover covers, and that count (0, 0 if none
/// fits).
fn geometry_heads(blocks: u64, sectors: u64) -> (u64, u64) {
    let (mut best_chs, mut best_heads) = (0, 0);
    for heads in [1u64, 2, 16, 32, 64, 128, 255] {
        let cylinders = blocks / heads / sectors;
        if cylinders < heads || cylinders < sectors {
            break;
        }
        if cylinders > 1023 {
            continue;
        }
        let chs = cylinders * heads * sectors;
        if chs > best_chs || (chs == best_chs && best_heads == 1) {
            best_chs = chs;
            best_heads = heads;
        }
    }
    (best_chs, best_heads)
}

/// The table's geometry (`fwheads`, `fwsectors`) for a disk of `blocks`
/// sectors: the disk's own (`fw`: heads, sectors per track) if it reports
/// both, else the fit `g_part_geometry` picks among 1, 9, 17, 33, and 63
/// sectors per track (more sectors win a tie unless that leaves one
/// head), and 255 and 63 for a disk too big for any.
#[must_use]
pub fn geometry(blocks: u64, fw: Option<(u32, u32)>) -> (u32, u32) {
    if let Some((heads, sectors)) = fw
        && heads != 0
        && sectors != 0
    {
        return (heads, sectors);
    }
    let (mut best_chs, mut heads, mut sectors) = (0u64, 0u64, 0u64);
    for s in [1u64, 9, 17, 33, 63] {
        let (chs, h) = geometry_heads(blocks, s);
        if chs == 0 {
            continue;
        }
        if chs > best_chs || (chs == best_chs && h > 1 && sectors == 1) {
            best_chs = chs;
            heads = h;
            sectors = s;
        }
    }
    if best_chs == 0 {
        return (255, 63);
    }
    // Both come from the candidate lists above.
    (
        u32::try_from(heads).unwrap_or(255),
        u32::try_from(sectors).unwrap_or(63),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn type_names() {
        assert_eq!(gpt_type("c12a7328-f81f-11d2-ba4b-00a0c93ec93b"), "efi");
        assert_eq!(
            gpt_type("516e7cb6-6ecf-11d6-8ff8-00022d09712b"),
            "freebsd-ufs"
        );
        assert_eq!(
            gpt_type("e3c9e316-0b5c-4db8-817d-f92df00215ae"),
            "ms-reserved"
        );
        assert_eq!(
            gpt_type("01234567-89ab-cdef-0123-456789abcdef"),
            "!01234567-89ab-cdef-0123-456789abcdef"
        );
    }

    #[test]
    fn efimedia() {
        // As FreeBSD 15.1 shows nda1p4 on the build server.
        assert_eq!(
            gpt_efimedia(
                4,
                "dfd2ec00-c70f-11f1-8186-d8bbc16c6352",
                4_728_832,
                1_953_523_711
            ),
            "HD(4,GPT,dfd2ec00-c70f-11f1-8186-d8bbc16c6352,0x482800,0x74284000)"
        );
        assert_eq!(gpt_efimedia(1, "x", 0, 0), "HD(1,GPT,x,0,0x1)");
    }

    #[test]
    fn geometries() {
        // A 64 MiB memory disk, as FreeBSD 15.1 shows it.
        assert_eq!(geometry(131_072, None), (255, 1));
        // A terabyte disk: too big for any.
        assert_eq!(geometry(1_953_525_168, None), (255, 63));
        assert_eq!(geometry(1_953_525_168, Some((16, 63))), (16, 63));
        assert_eq!(geometry(131_072, Some((0, 63))), (255, 1));
        // 1.44 MB: 9 sectors and 2 heads tie with 1 sector and 16 heads,
        // and win.
        assert_eq!(geometry(2880, None), (2, 9));
    }
}
