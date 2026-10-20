// SPDX-License-Identifier: BSD-2-Clause
//! In-firmware tests: run under QEMU by `cargo xtask test --tier boot`
//! before the loader boots the (test) kernel.

use ktest::ktest;

ktest! {
    fn crc32_matches_host() {
        assert_eq!(crc::crc32(b"123456789"), 0xCBF4_3926);
    }
}

ktest! {
    fn firmware_publishes_acpi() {
        let st = super::SYSTEM_TABLE.load(core::sync::atomic::Ordering::Acquire);
        // SAFETY: `efi_main` stored the live system table before running tests.
        let system = unsafe { efi::SystemTable::from_raw(st) }.expect("system table");
        assert!(
            system.configuration_table(efi::tables::ACPI_20).is_some(),
            "no ACPI 2.0+ RSDP: AbyssBSD requires ACPI"
        );
    }
}

ktest! {
    fn heap_allocates() {
        let v: alloc::vec::Vec<u64> = (0..1000).collect();
        assert_eq!(v.iter().sum::<u64>(), 499_500);
        let b = alloc::boxed::Box::new([0u8; 9000]);
        assert_eq!(b.len(), 9000);
    }
}

ktest! {
    fn fstab_names_the_root() {
        let fstab = "# Device\tMountpoint\tFStype\tOptions\tDump\tPass#\n\
                     /dev/gpt/efiboot0\t/boot/efi\tmsdosfs\trw\t2\t2\n\
                     /dev/gpt/rootfs0\t/\tufs\trw\t1\t1 # the root\n\
                     /dev/nda0p3\t/\tufs\trw\t1\t1\n";
        assert_eq!(
            crate::ufs::fstab_root(fstab).as_deref(),
            Some("ufs:/dev/gpt/rootfs0")
        );
        assert_eq!(crate::ufs::fstab_root("#/dev/x / ufs rw 1 1\n\n"), None);
    }
}
