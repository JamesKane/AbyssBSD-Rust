// SPDX-License-Identifier: BSD-2-Clause
//! Arm PL011 and SBSA generic UART: output polled, input polled or by
//! interrupt.
//!
//! Only the data, flag, and interrupt mask and clear registers are used,
//! which the SBSA generic UART guarantees (32-bit accesses only). Line
//! settings stay as firmware programmed them.
//!
//! Reference: Arm PrimeCell UART (PL011) TRM, Arm SBSA Appendix B.

#![no_std]
#![forbid(unsafe_code)]

use regs::Mmio;

const DR: usize = 0x00;
const FR: usize = 0x18;
const CR: usize = 0x30;
/// `UARTCR.LBE`: loop back.
const CR_LBE: u32 = 1 << 7;
const IMSC: usize = 0x38;
const ICR: usize = 0x44;
/// Receive and receive-timeout interrupts.
const RX_INTERRUPTS: u32 = (1 << 4) | (1 << 6);
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

    /// Loops what is sent back to the receiver instead of the line (for
    /// tests of the receive path; a PL011 has it, the SBSA subset not).
    pub fn set_loopback(&self, on: bool) {
        let cr = self.regs.read32(CR) & !CR_LBE;
        self.regs.write32(CR, if on { cr | CR_LBE } else { cr });
    }

    /// Interrupts when data arrives (or the FIFO times out with some).
    pub fn enable_rx_interrupt(&self) {
        self.regs.write32(IMSC, RX_INTERRUPTS);
    }

    /// Masks the receive interrupts (`false`) while what was read waits
    /// to be taken, leaving new bytes in the FIFO; unmasked, bytes
    /// waiting there raise them again.
    pub fn set_rx_interrupt(&self, on: bool) {
        self.regs.write32(IMSC, if on { RX_INTERRUPTS } else { 0 });
    }

    /// Clears the receive interrupts. Done before reading the FIFO empty,
    /// so a byte that arrives meanwhile raises them again.
    pub fn ack_rx(&self) {
        self.regs.write32(ICR, RX_INTERRUPTS);
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
