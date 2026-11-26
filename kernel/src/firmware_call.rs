// SPDX-License-Identifier: BSD-2-Clause
//! The firmware calls drivers are granted (DRIVER-MODEL.md §8.7). Drivers cannot issue an SMC or
//! HVC themselves; each grant here makes one call, with the argument shape it takes.
//!
//! - `sky1_pd`: SiP function `0xC2000001`, the Sky1's SCMI power-domain agent in Trusted
//!   Firmware (Sky1 SoC specification 01 §1.3.2), with the shared memory's page number and
//!   offset. The firmware reads and writes only that 128-byte window, which the UEFI memory map
//!   reserves and the bus grants the driver.

/// Gives the drivers their grants, before drivers start.
pub(crate) fn grant() {
    #[cfg(target_arch = "aarch64")]
    sky1_pd::set_conduit(&Sky1Scmi);
}

#[cfg(target_arch = "aarch64")]
struct Sky1Scmi;

#[cfg(target_arch = "aarch64")]
impl sky1_pd::Conduit for Sky1Scmi {
    fn scmi_smc(&self, page: u64, offset: u64) -> i64 {
        const SCMI_SMC: u64 = 0xC200_0001;
        // SAFETY: a fast SiP call to Trusted Firmware through the PSCI conduit. Its only memory
        // effect is on the agent's shared memory (`page` and `offset` name it), which the UEFI
        // map reserves, so no memory the kernel uses is touched; registers are as SMCCC says.
        unsafe {
            crate::arch::smp::psci_call(crate::smp::psci_conduit(), SCMI_SMC, page, offset, 0)
        }
    }
}
