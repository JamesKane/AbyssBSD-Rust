// SPDX-License-Identifier: BSD-2-Clause

extern crate std;
use std::vec::Vec;

use crate::fadt::*;
use crate::testutil::{gas, one, torture};
use crate::{AddressSpace, Error, Gas, Sdt};

/// Writes `b` at absolute table offset `at` of a body.
fn put(body: &mut [u8], at: usize, b: &[u8]) {
    body[at - 36..at - 36 + b.len()].copy_from_slice(b);
}

/// A revision 6.5 FADT with a mix of 32- and 64-bit fields.
fn full() -> Vec<u8> {
    let mut b = std::vec![0u8; 276 - 36];
    put(&mut b, 36, &0x7ffe_0000u32.to_le_bytes()); // FIRMWARE_CTRL
    put(&mut b, 40, &0x7ffd_0000u32.to_le_bytes()); // DSDT (superseded)
    b[45 - 36] = 4; // enterprise server
    put(&mut b, 46, &9u16.to_le_bytes());
    put(&mut b, 48, &0xb2u32.to_le_bytes());
    b[52 - 36] = 0xf1;
    b[53 - 36] = 0xf0;
    put(&mut b, 56, &0x600u32.to_le_bytes()); // PM1a_EVT_BLK
    put(&mut b, 64, &0x604u32.to_le_bytes()); // PM1a_CNT_BLK
    put(&mut b, 76, &0x608u32.to_le_bytes()); // PM_TMR_BLK (superseded)
    put(&mut b, 80, &0x620u32.to_le_bytes()); // GPE0_BLK
    b[88 - 36] = 4;
    b[89 - 36] = 2;
    b[91 - 36] = 4;
    b[92 - 36] = 16;
    b[108 - 36] = 0x32;
    put(&mut b, 109, &0x003eu16.to_le_bytes());
    put(
        &mut b,
        112,
        &((1u32 << 8) | (1 << 10) | (1 << 21)).to_le_bytes(),
    );
    put(&mut b, 116, &gas(1, 8, 1, 0xcf9));
    b[128 - 36] = 6;
    put(&mut b, 129, &3u16.to_le_bytes());
    b[131 - 36] = 0x15;
    put(&mut b, 140, &0x1_0000_0000u64.to_le_bytes()); // X_DSDT
    put(&mut b, 208, &gas(1, 32, 3, 0x708)); // X_PM_TMR_BLK
    put(&mut b, 244, &gas(0, 8, 1, 0x9000_0000));
    put(&mut b, 256, &gas(0, 8, 1, 0x9000_0001));
    put(&mut b, 268, &u64::from_le_bytes(*b"KVMKVMKV").to_le_bytes());
    b
}

fn io(addr: u64, bits: u8) -> Gas {
    Gas {
        space: AddressSpace::Io,
        bit_width: bits,
        bit_offset: 0,
        access_size: 0,
        address: addr,
    }
}

#[test]
fn decodes_a_full_table() {
    let m = one(*b"FACP", 6, &full());
    let f = parse(&Sdt::read(&&m, 0).unwrap()).unwrap();
    assert_eq!(f.major_version(), 6);
    assert_eq!(f.minor_version(), Some(5));
    assert_eq!(f.facs(), Some(0x7ffe_0000));
    assert_eq!(f.dsdt(), Some(0x1_0000_0000));
    assert_eq!(f.preferred_pm_profile(), Some(4));
    assert_eq!(f.sci_interrupt(), Some(9));
    assert_eq!(f.smi_command_port(), Some(0xb2));
    assert_eq!(f.acpi_enable_disable(), Some((0xf1, 0xf0)));
    assert_eq!(f.pm1a_event_block(), Some(io(0x600, 32)));
    assert_eq!(f.pm1b_event_block(), None);
    assert_eq!(f.pm1a_control_block(), Some(io(0x604, 16)));
    assert_eq!(f.pm1b_control_block(), None);
    assert_eq!(f.pm2_control_block(), None);
    let timer = f.pm_timer().unwrap();
    assert_eq!(timer.bits, 32);
    assert_eq!(timer.block.address, 0x708);
    assert_eq!(timer.block.access_bytes(), Some(4));
    // The legacy length is in bytes; the GAS width is in bits.
    assert_eq!(f.gpe0_block(), Some(io(0x620, 128)));
    assert_eq!(f.gpe0_len(), Some(16));
    assert_eq!(f.gpe1_block(), None);
    assert_eq!(f.gpe1_len(), Some(0));
    assert_eq!(f.pm1_event_len(), Some(4));
    assert_eq!(f.pm1_control_len(), Some(2));
    assert_eq!(f.century(), Some(0x32));
    let flags = f.flags().unwrap();
    assert!(flags.tmr_val_ext() && flags.reset_reg_sup() && flags.low_power_s0_idle_capable());
    assert!(!flags.hw_reduced_acpi() && !flags.wbinvd());
    assert_eq!(flags.persistent_cpu_caches(), 0);
    let ia = f.iapc_boot_arch().unwrap();
    assert!(!ia.legacy_devices());
    assert!(ia.has_8042() && ia.vga_not_present() && ia.msi_not_supported());
    assert!(ia.pcie_aspm_controls() && ia.cmos_rtc_not_present());
    let arm = f.arm_boot_arch().unwrap();
    assert!(arm.psci_compliant() && arm.psci_use_hvc());
    let reset = f.reset_register().unwrap();
    assert_eq!(reset.value, 6);
    assert_eq!(reset.register.address, 0xcf9);
    assert_eq!(reset.register.space, AddressSpace::Io);
    assert_eq!(
        f.sleep_control_register().map(|g| g.address),
        Some(0x9000_0000)
    );
    assert_eq!(
        f.sleep_status_register().map(|g| g.address),
        Some(0x9000_0001)
    );
    assert_eq!(
        f.hypervisor_vendor(),
        Some(u64::from_le_bytes(*b"KVMKVMKV"))
    );
}

#[test]
fn acpi_1_table_has_only_legacy_fields() {
    let mut b = full();
    // No TMR_VAL_EXT, so a 24-bit legacy timer.
    put(&mut b, 112, &(1u32 << 10).to_le_bytes());
    b.truncate(116 - 36);
    let m = one(*b"FACP", 1, &b);
    let f = parse(&Sdt::read(&&m, 0).unwrap()).unwrap();
    assert_eq!(f.facs(), Some(0x7ffe_0000));
    assert_eq!(f.dsdt(), Some(0x7ffd_0000));
    assert_eq!(
        f.pm_timer(),
        Some(PmTimer {
            block: io(0x608, 32),
            bits: 24
        })
    );
    assert_eq!(f.reset_register(), None);
    assert_eq!(f.arm_boot_arch(), None);
    assert_eq!(f.minor_version(), None);
    assert_eq!(f.sleep_control_register(), None);
    assert_eq!(f.hypervisor_vendor(), None);
    assert!(f.flags().unwrap().reset_reg_sup());
}

#[test]
fn every_field_is_none_past_the_end() {
    let m = one(*b"FACP", 1, &[]);
    let f = parse(&Sdt::read(&&m, 0).unwrap()).unwrap();
    assert_eq!(f.facs(), None);
    assert_eq!(f.dsdt(), None);
    assert_eq!(f.sci_interrupt(), None);
    assert_eq!(f.pm1a_event_block(), None);
    assert_eq!(f.pm_timer(), None);
    assert_eq!(f.flags(), None);
    assert_eq!(f.iapc_boot_arch(), None);
    // A GAS cut in half is absent, not garbage.
    let mut b = full();
    b.truncate(214 - 36);
    put(&mut b, 76, &0u32.to_le_bytes());
    let m = one(*b"FACP", 3, &b);
    let f = parse(&Sdt::read(&&m, 0).unwrap()).unwrap();
    assert_eq!(f.pm_timer(), None);
    assert_eq!(f.dsdt(), Some(0x1_0000_0000));
}

#[test]
fn hardware_reduced_and_unsupported_reset() {
    let mut b = std::vec![0u8; 276 - 36];
    put(&mut b, 112, &(1u32 << 20).to_le_bytes());
    put(&mut b, 116, &gas(0, 8, 1, 0x1000)); // present but not RESET_REG_SUP
    put(&mut b, 129, &1u16.to_le_bytes());
    let m = one(*b"FACP", 6, &b);
    let f = parse(&Sdt::read(&&m, 0).unwrap()).unwrap();
    assert!(f.flags().unwrap().hw_reduced_acpi());
    assert_eq!(f.reset_register(), None);
    assert_eq!(f.smi_command_port(), None);
    assert_eq!(f.pm1a_event_block(), None);
    assert_eq!(f.pm_timer(), None);
    assert_eq!(f.facs(), None);
    assert_eq!(f.century(), None);
    assert!(f.arm_boot_arch().unwrap().psci_compliant());
    assert!(!f.arm_boot_arch().unwrap().psci_use_hvc());
}

#[test]
fn persistent_cache_field() {
    for (bits, v) in [(0u32, 0u8), (1, 1), (2, 2), (3, 3)] {
        assert_eq!(FadtFlags(bits << 22).persistent_cpu_caches(), v);
    }
}

#[test]
fn rejects_wrong_signature() {
    let m = one(*b"FACS", 6, &full());
    assert_eq!(parse(&Sdt::read(&&m, 0).unwrap()), Err(Error::BadSignature));
}

#[test]
fn survives_corruption() {
    torture(*b"FACP", 6, &full());
}
