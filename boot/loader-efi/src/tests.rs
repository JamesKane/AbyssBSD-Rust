// SPDX-License-Identifier: BSD-2-Clause
//! In-firmware tests: run under QEMU by `cargo xtask test --tier boot`.

use ktest::ktest;

ktest! {
    fn harness_runs() {
        assert!(ktest::tests().count() >= 3);
    }
}

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
        let acpi = system
            .configuration_tables()
            .iter()
            .any(|t| t.vendor_guid.get() == efi::tables::ACPI_20);
        assert!(acpi, "no ACPI 2.0+ RSDP: AbyssBSD requires ACPI");
    }
}
