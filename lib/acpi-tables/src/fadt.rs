// SPDX-License-Identifier: BSD-2-Clause
//! Fixed ACPI Description Table (ACPI 6.5 §5.2.9): where the fixed hardware
//! registers, the FACS, and the DSDT are.
//!
//! Older revisions are shorter; every accessor returns `None` for a field the
//! table is too short to hold. Where the table has both a 64-bit `X_` field
//! and its 32-bit predecessor, a nonzero `X_` field wins.
//!
//! Behavior reference: `sys/dev/acpica/acpi.c`, `sys/dev/acpica/acpi_timer.c`.

use crate::{AddressSpace, Error, Gas, Sdt, u16_at, u32_at, u64_at};

const FIRMWARE_CTRL: usize = 36;
const DSDT: usize = 40;
const PREFERRED_PM_PROFILE: usize = 45;
const SCI_INT: usize = 46;
const SMI_CMD: usize = 48;
const ACPI_ENABLE: usize = 52;
const ACPI_DISABLE: usize = 53;
const PM1A_EVT_BLK: usize = 56;
const PM1B_EVT_BLK: usize = 60;
const PM1A_CNT_BLK: usize = 64;
const PM1B_CNT_BLK: usize = 68;
const PM2_CNT_BLK: usize = 72;
const PM_TMR_BLK: usize = 76;
const GPE0_BLK: usize = 80;
const GPE1_BLK: usize = 84;
const PM1_EVT_LEN: usize = 88;
const PM1_CNT_LEN: usize = 89;
const PM2_CNT_LEN: usize = 90;
const PM_TMR_LEN: usize = 91;
const GPE0_BLK_LEN: usize = 92;
const GPE1_BLK_LEN: usize = 93;
const GPE1_BASE: usize = 94;
const CENTURY: usize = 108;
const IAPC_BOOT_ARCH: usize = 109;
const FLAGS: usize = 112;
const RESET_REG: usize = 116;
const RESET_VALUE: usize = 128;
const ARM_BOOT_ARCH: usize = 129;
const MINOR_VERSION: usize = 131;
const X_FIRMWARE_CTRL: usize = 132;
const X_DSDT: usize = 140;
const X_PM1A_EVT_BLK: usize = 148;
const X_PM1B_EVT_BLK: usize = 160;
const X_PM1A_CNT_BLK: usize = 172;
const X_PM1B_CNT_BLK: usize = 184;
const X_PM2_CNT_BLK: usize = 196;
const X_PM_TMR_BLK: usize = 208;
const X_GPE0_BLK: usize = 220;
const X_GPE1_BLK: usize = 232;
const SLEEP_CONTROL_REG: usize = 244;
const SLEEP_STATUS_REG: usize = 256;
const HYPERVISOR_VENDOR: usize = 268;

/// A parsed FADT.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fadt<'a> {
    bytes: &'a [u8],
}

/// Parses a FADT (signature `FACP`). Any length is accepted; fields past the
/// end read as `None`.
///
/// # Errors
///
/// [`Error::BadSignature`] for another table.
pub fn parse<'a>(t: &Sdt<'a>) -> Result<Fadt<'a>, Error> {
    if t.signature() != *b"FACP" {
        return Err(Error::BadSignature);
    }
    Ok(Fadt { bytes: t.bytes })
}

/// The ACPI PM timer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PmTimer {
    /// The counter register.
    pub block: Gas,
    /// Counter width: 24 or 32 bits (`TMR_VAL_EXT`).
    pub bits: u8,
}

/// The reset register and the value that resets the system when written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResetRegister {
    /// The register.
    pub register: Gas,
    /// The value to write.
    pub value: u8,
}

impl Fadt<'_> {
    fn u8(&self, off: usize) -> Option<u8> {
        self.bytes.get(off).copied()
    }

    fn u16(&self, off: usize) -> Option<u16> {
        u16_at(self.bytes, off)
    }

    fn u32(&self, off: usize) -> Option<u32> {
        u32_at(self.bytes, off)
    }

    fn gas(&self, off: usize) -> Option<Gas> {
        Gas::parse(self.bytes, off).ok().filter(|g| g.address != 0)
    }

    /// An address preferring the 64-bit field, ignoring zeros.
    fn address(&self, x: usize, legacy: usize) -> Option<u64> {
        u64_at(self.bytes, x)
            .filter(|&a| a != 0)
            .or_else(|| self.u32(legacy).filter(|&a| a != 0).map(u64::from))
    }

    /// A register block preferring the `X_` GAS, else the 32-bit I/O port
    /// with its length byte (in bytes).
    fn block(&self, x: usize, legacy: usize, len: usize) -> Option<Gas> {
        self.gas(x).or_else(|| {
            let port = self.u32(legacy).filter(|&a| a != 0)?;
            let bytes = self.u8(len)?;
            Some(Gas {
                space: AddressSpace::Io,
                bit_width: bytes.saturating_mul(8),
                bit_offset: 0,
                access_size: 0,
                address: u64::from(port),
            })
        })
    }

    /// The table revision (the FADT major version).
    #[must_use]
    pub fn major_version(&self) -> u8 {
        self.bytes.get(8).copied().unwrap_or(0)
    }

    /// The FADT minor version (ACPI 5.1).
    #[must_use]
    pub fn minor_version(&self) -> Option<u8> {
        self.u8(MINOR_VERSION).map(|v| v & 0xf)
    }

    /// Physical address of the FACS.
    #[must_use]
    pub fn facs(&self) -> Option<u64> {
        self.address(X_FIRMWARE_CTRL, FIRMWARE_CTRL)
    }

    /// Physical address of the DSDT.
    #[must_use]
    pub fn dsdt(&self) -> Option<u64> {
        self.address(X_DSDT, DSDT)
    }

    /// Preferred power management profile (0 unspecified, 1 desktop,
    /// 2 mobile, 3 workstation, 4 enterprise server, …).
    #[must_use]
    pub fn preferred_pm_profile(&self) -> Option<u8> {
        self.u8(PREFERRED_PM_PROFILE)
    }

    /// The SCI interrupt: an 8259 IRQ on PC-AT systems, else a GSI.
    #[must_use]
    pub fn sci_interrupt(&self) -> Option<u16> {
        self.u16(SCI_INT)
    }

    /// The SMI command port, or `None` if zero (no SMM, or hardware-reduced).
    #[must_use]
    pub fn smi_command_port(&self) -> Option<u32> {
        self.u32(SMI_CMD).filter(|&p| p != 0)
    }

    /// The values to write to the SMI command port to enable and disable
    /// ACPI mode.
    #[must_use]
    pub fn acpi_enable_disable(&self) -> Option<(u8, u8)> {
        Some((self.u8(ACPI_ENABLE)?, self.u8(ACPI_DISABLE)?))
    }

    /// `PM1a` event register block.
    #[must_use]
    pub fn pm1a_event_block(&self) -> Option<Gas> {
        self.block(X_PM1A_EVT_BLK, PM1A_EVT_BLK, PM1_EVT_LEN)
    }

    /// `PM1b` event register block.
    #[must_use]
    pub fn pm1b_event_block(&self) -> Option<Gas> {
        self.block(X_PM1B_EVT_BLK, PM1B_EVT_BLK, PM1_EVT_LEN)
    }

    /// `PM1a` control register block.
    #[must_use]
    pub fn pm1a_control_block(&self) -> Option<Gas> {
        self.block(X_PM1A_CNT_BLK, PM1A_CNT_BLK, PM1_CNT_LEN)
    }

    /// `PM1b` control register block.
    #[must_use]
    pub fn pm1b_control_block(&self) -> Option<Gas> {
        self.block(X_PM1B_CNT_BLK, PM1B_CNT_BLK, PM1_CNT_LEN)
    }

    /// PM2 control register block.
    #[must_use]
    pub fn pm2_control_block(&self) -> Option<Gas> {
        self.block(X_PM2_CNT_BLK, PM2_CNT_BLK, PM2_CNT_LEN)
    }

    /// The PM timer, with its width from `TMR_VAL_EXT`.
    #[must_use]
    pub fn pm_timer(&self) -> Option<PmTimer> {
        let block = self.block(X_PM_TMR_BLK, PM_TMR_BLK, PM_TMR_LEN)?;
        let ext = self.flags().is_some_and(FadtFlags::tmr_val_ext);
        Some(PmTimer {
            block,
            bits: if ext { 32 } else { 24 },
        })
    }

    /// General-purpose event 0 register block.
    #[must_use]
    pub fn gpe0_block(&self) -> Option<Gas> {
        self.block(X_GPE0_BLK, GPE0_BLK, GPE0_BLK_LEN)
    }

    /// General-purpose event 1 register block.
    #[must_use]
    pub fn gpe1_block(&self) -> Option<Gas> {
        self.block(X_GPE1_BLK, GPE1_BLK, GPE1_BLK_LEN)
    }

    /// PM1 event block length in bytes (status plus enable halves).
    #[must_use]
    pub fn pm1_event_len(&self) -> Option<u8> {
        self.u8(PM1_EVT_LEN)
    }

    /// PM1 control block length in bytes.
    #[must_use]
    pub fn pm1_control_len(&self) -> Option<u8> {
        self.u8(PM1_CNT_LEN)
    }

    /// GPE0 block length in bytes (status plus enable halves). Lengths
    /// above 31 bytes cannot be expressed in a GAS bit width, so callers
    /// sizing GPE blocks should use this.
    #[must_use]
    pub fn gpe0_len(&self) -> Option<u8> {
        self.u8(GPE0_BLK_LEN)
    }

    /// GPE1 block length in bytes.
    #[must_use]
    pub fn gpe1_len(&self) -> Option<u8> {
        self.u8(GPE1_BLK_LEN)
    }

    /// The GPE number of the first GPE1 event.
    #[must_use]
    pub fn gpe1_base(&self) -> Option<u8> {
        self.u8(GPE1_BASE)
    }

    /// The RTC CMOS index of the century byte, or `None` if zero.
    #[must_use]
    pub fn century(&self) -> Option<u8> {
        self.u8(CENTURY).filter(|&c| c != 0)
    }

    /// Fixed feature flags.
    #[must_use]
    pub fn flags(&self) -> Option<FadtFlags> {
        self.u32(FLAGS).map(FadtFlags)
    }

    /// IA-PC boot architecture flags (ACPI 2.0).
    #[must_use]
    pub fn iapc_boot_arch(&self) -> Option<IapcBootArch> {
        self.u16(IAPC_BOOT_ARCH).map(IapcBootArch)
    }

    /// Arm boot architecture flags (ACPI 5.1).
    #[must_use]
    pub fn arm_boot_arch(&self) -> Option<ArmBootArch> {
        self.u16(ARM_BOOT_ARCH).map(ArmBootArch)
    }

    /// The reset register, if `RESET_REG_SUP` says it is supported.
    #[must_use]
    pub fn reset_register(&self) -> Option<ResetRegister> {
        if !self.flags()?.reset_reg_sup() {
            return None;
        }
        Some(ResetRegister {
            register: self.gas(RESET_REG)?,
            value: self.u8(RESET_VALUE)?,
        })
    }

    /// The sleep control register (hardware-reduced systems, ACPI 5.0).
    #[must_use]
    pub fn sleep_control_register(&self) -> Option<Gas> {
        self.gas(SLEEP_CONTROL_REG)
    }

    /// The sleep status register (hardware-reduced systems, ACPI 5.0).
    #[must_use]
    pub fn sleep_status_register(&self) -> Option<Gas> {
        self.gas(SLEEP_STATUS_REG)
    }

    /// The hypervisor vendor identity (ACPI 6.0), or `None` if zero.
    #[must_use]
    pub fn hypervisor_vendor(&self) -> Option<u64> {
        u64_at(self.bytes, HYPERVISOR_VENDOR).filter(|&v| v != 0)
    }
}

/// FADT fixed feature flags (ACPI 6.5 Table 5.10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FadtFlags(pub u32);

impl FadtFlags {
    const fn bit(self, n: u32) -> bool {
        self.0 & (1 << n) != 0
    }

    /// WBINVD works correctly.
    #[must_use]
    pub const fn wbinvd(self) -> bool {
        self.bit(0)
    }

    /// WBINVD flushes but does not invalidate.
    #[must_use]
    pub const fn wbinvd_flush(self) -> bool {
        self.bit(1)
    }

    /// C1 is supported on all processors.
    #[must_use]
    pub const fn proc_c1(self) -> bool {
        self.bit(2)
    }

    /// C2 works on multiprocessor systems.
    #[must_use]
    pub const fn p_lvl2_up(self) -> bool {
        self.bit(3)
    }

    /// The power button is a control method device, not fixed hardware.
    #[must_use]
    pub const fn pwr_button(self) -> bool {
        self.bit(4)
    }

    /// The sleep button is a control method device, or absent.
    #[must_use]
    pub const fn slp_button(self) -> bool {
        self.bit(5)
    }

    /// RTC wake status is not in the fixed register space.
    #[must_use]
    pub const fn fix_rtc(self) -> bool {
        self.bit(6)
    }

    /// The RTC alarm can wake from S4.
    #[must_use]
    pub const fn rtc_s4(self) -> bool {
        self.bit(7)
    }

    /// The PM timer is 32 bits wide (else 24).
    #[must_use]
    pub const fn tmr_val_ext(self) -> bool {
        self.bit(8)
    }

    /// The system supports docking.
    #[must_use]
    pub const fn dck_cap(self) -> bool {
        self.bit(9)
    }

    /// The reset register is supported.
    #[must_use]
    pub const fn reset_reg_sup(self) -> bool {
        self.bit(10)
    }

    /// No internal expansion capabilities; the case is sealed.
    #[must_use]
    pub const fn sealed_case(self) -> bool {
        self.bit(11)
    }

    /// No local input or output devices.
    #[must_use]
    pub const fn headless(self) -> bool {
        self.bit(12)
    }

    /// A processor native instruction must follow writing `SLP_TYP`.
    #[must_use]
    pub const fn cpu_sw_slp(self) -> bool {
        self.bit(13)
    }

    /// `PCIEXP_WAKE_STS` and `PCIEXP_WAKE_DIS` are supported.
    #[must_use]
    pub const fn pci_exp_wak(self) -> bool {
        self.bit(14)
    }

    /// Use the platform clock, not the processor's TSC, for timekeeping.
    #[must_use]
    pub const fn use_platform_clock(self) -> bool {
        self.bit(15)
    }

    /// `RTC_STS` is valid after waking from S4.
    #[must_use]
    pub const fn s4_rtc_sts_valid(self) -> bool {
        self.bit(16)
    }

    /// Remote power-on is supported.
    #[must_use]
    pub const fn remote_power_on_capable(self) -> bool {
        self.bit(17)
    }

    /// Local APICs must use cluster destination mode.
    #[must_use]
    pub const fn force_apic_cluster_model(self) -> bool {
        self.bit(18)
    }

    /// Local APICs must use physical destination mode.
    #[must_use]
    pub const fn force_apic_physical_destination_mode(self) -> bool {
        self.bit(19)
    }

    /// The hardware-reduced ACPI model: no fixed hardware, no legacy
    /// register blocks.
    #[must_use]
    pub const fn hw_reduced_acpi(self) -> bool {
        self.bit(20)
    }

    /// Low-power S0 idle is at least as good as S3.
    #[must_use]
    pub const fn low_power_s0_idle_capable(self) -> bool {
        self.bit(21)
    }

    /// Persistent CPU caches support, bits 23:22 (ACPI 6.5): 0 not
    /// reported, 1 not persistent, 2 persistent.
    #[must_use]
    pub const fn persistent_cpu_caches(self) -> u8 {
        match (self.0 >> 22) & 3 {
            0 => 0,
            1 => 1,
            2 => 2,
            _ => 3,
        }
    }
}

/// IA-PC boot architecture flags (ACPI 6.5 Table 5.11).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IapcBootArch(pub u16);

impl IapcBootArch {
    /// `LEGACY_DEVICES`: there are user-visible ISA/LPC devices.
    #[must_use]
    pub const fn legacy_devices(self) -> bool {
        self.0 & 1 != 0
    }

    /// 8042: there is a PS/2 keyboard controller.
    #[must_use]
    pub const fn has_8042(self) -> bool {
        self.0 & 2 != 0
    }

    /// VGA Not Present: do not probe VGA hardware.
    #[must_use]
    pub const fn vga_not_present(self) -> bool {
        self.0 & 4 != 0
    }

    /// MSI Not Supported: do not enable message signaled interrupts.
    #[must_use]
    pub const fn msi_not_supported(self) -> bool {
        self.0 & 8 != 0
    }

    /// `PCIe` ASPM Controls: the OS must not enable ASPM.
    #[must_use]
    pub const fn pcie_aspm_controls(self) -> bool {
        self.0 & 0x10 != 0
    }

    /// CMOS RTC Not Present.
    #[must_use]
    pub const fn cmos_rtc_not_present(self) -> bool {
        self.0 & 0x20 != 0
    }
}

/// Arm boot architecture flags (ACPI 6.5 Table 5.12).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArmBootArch(pub u16);

impl ArmBootArch {
    /// `PSCI_COMPLIANT`: PSCI is implemented.
    #[must_use]
    pub const fn psci_compliant(self) -> bool {
        self.0 & 1 != 0
    }

    /// `PSCI_USE_HVC`: call PSCI with HVC instead of SMC.
    #[must_use]
    pub const fn psci_use_hvc(self) -> bool {
        self.0 & 2 != 0
    }
}
