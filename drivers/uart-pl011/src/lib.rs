// SPDX-License-Identifier: BSD-2-Clause
//! Arm PL011 and SBSA generic UART, polled.
//!
//! Only the data and flag registers are used, which is the subset the SBSA
//! generic UART guarantees (32-bit accesses only). Line settings stay as
//! firmware programmed them.
//!
//! Reference: Arm PrimeCell UART (PL011) TRM, Arm SBSA Appendix B.

#![no_std]
#![forbid(unsafe_code)]

use regs::Mmio;

const DR: usize = 0x00;
const FR: usize = 0x18;
const FR_TXFF: u32 = 1 << 5;
const FR_RXFE: u32 = 1 << 4;

/// Polls before giving up on a stuck transmitter.
const SPIN_LIMIT: u32 = 1_000_000;

/// A PL011-compatible UART.
#[derive(Debug)]
pub struct Uart {
    regs: Mmio,
}

impl Uart {
    /// Takes over a UART firmware already configured.
    pub fn init(regs: Mmio) -> Self {
        Self { regs }
    }

    /// Sends one byte, waiting for FIFO room.
    pub fn put(&self, b: u8) {
        for _ in 0..SPIN_LIMIT {
            if self.regs.read32(FR) & FR_TXFF == 0 {
                break;
            }
            core::hint::spin_loop();
        }
        self.regs.write32(DR, u32::from(b));
    }

    /// A received byte, if one is waiting.
    pub fn get(&self) -> Option<u8> {
        (self.regs.read32(FR) & FR_RXFE == 0).then(|| self.regs.read32(DR).to_le_bytes()[0])
    }
}

impl core::fmt::Write for Uart {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for b in s.bytes() {
            if b == b'\n' {
                self.put(b'\r');
            }
            self.put(b);
        }
        Ok(())
    }
}
