// SPDX-License-Identifier: BSD-2-Clause
//! NS16550-compatible UART: output polled, input polled or by interrupt.
//!
//! The early console keeps the line settings firmware programmed (from
//! SPCR); [`Uart::init`] only masks interrupts and enables the FIFOs.
//! [`Uart::enable_rx_interrupt`] interrupts on received data, which
//! [`Uart::get`] then reads until the receiver is empty.
//!
//! A port that is a terminal of its own is driven fully: its speed and
//! framing ([`Uart::set_line`]), its modem lines ([`Uart::set_modem`],
//! [`Uart::modem_status`]), breaks, and interrupts for received data,
//! room to transmit, and modem changes ([`Uart::set_interrupts`]).
//!
//! Behavior reference: sys/dev/uart/uart_dev_ns8250.c.

#![no_std]
#![forbid(unsafe_code)]

use regs::RegisterIo;

const THR: usize = 0;
const RBR: usize = 0;
const DLL: usize = 0;
const IER: usize = 1;
const DLM: usize = 1;
const IIR: usize = 2;
const FCR: usize = 2;
const LCR: usize = 3;
const MCR: usize = 4;
const LSR: usize = 5;
const MSR: usize = 6;

const IER_RXRDY: u8 = 1 << 0;
const IER_TXRDY: u8 = 1 << 1;
const IER_MSI: u8 = 1 << 3;
const IIR_NOPEND: u8 = 1 << 0;
const LCR_STOPB: u8 = 1 << 2;
const LCR_PENAB: u8 = 1 << 3;
const LCR_PEVEN: u8 = 1 << 4;
const LCR_SBREAK: u8 = 1 << 6;
const LCR_DLAB: u8 = 1 << 7;
const MCR_DTR: u8 = 1 << 0;
const MCR_RTS: u8 = 1 << 1;
const MCR_LOOP: u8 = 1 << 4;
const FCR_RCV_RST: u8 = 1 << 1;
const FCR_XMT_RST: u8 = 1 << 2;

/// The modem status bits (`MSR`): clear to send, data set ready, ring,
/// carrier, and the change bits below them.
pub const MSR_CTS: u8 = 1 << 4;
pub const MSR_DSR: u8 = 1 << 5;
pub const MSR_RI: u8 = 1 << 6;
pub const MSR_DCD: u8 = 1 << 7;
pub const MSR_DELTA: u8 = 0x0f;

/// The bytes the transmit FIFO takes once empty.
pub const TX_FIFO: usize = 16;
/// A PC COM port's clock (1.8432 MHz), sixteen per bit.
pub const RCLK: u32 = 1_843_200;
/// On a PC, OUT2 gates the UART's interrupt to the interrupt controller.
const MCR_OUT2: u8 = 1 << 3;

const FCR_ENABLE: u8 = 1 << 0;
const LSR_DATA_READY: u8 = 1 << 0;
const LSR_THR_EMPTY: u8 = 1 << 5;

/// Polls before giving up on a stuck transmitter, so a missing UART cannot
/// hang the console.
const SPIN_LIMIT: u32 = 1_000_000;

/// A 16550 on some register bus.
#[derive(Debug)]
pub struct Uart<R> {
    regs: R,
}

impl<R: RegisterIo> Uart<R> {
    /// Takes over a UART firmware already configured.
    pub fn init(regs: R) -> Self {
        regs.write(IER, 0);
        regs.write(FCR, FCR_ENABLE);
        Self { regs }
    }

    /// Sends one byte, waiting for room.
    pub fn put(&self, b: u8) {
        for _ in 0..SPIN_LIMIT {
            if self.regs.read(LSR) & LSR_THR_EMPTY != 0 {
                break;
            }
            core::hint::spin_loop();
        }
        self.regs.write(THR, b);
    }

    /// A received byte, if one is waiting.
    pub fn get(&self) -> Option<u8> {
        (self.regs.read(LSR) & LSR_DATA_READY != 0).then(|| self.regs.read(RBR))
    }

    /// Loops what is sent back to the receiver instead of the line (for
    /// tests of the receive path).
    pub fn set_loopback(&self, on: bool) {
        let mcr = self.regs.read(MCR) & !MCR_LOOP;
        self.regs.write(MCR, if on { mcr | MCR_LOOP } else { mcr });
    }

    /// Interrupts when data arrives (or the FIFO times out with some).
    /// Reading everything with [`Uart::get`] clears the interrupt.
    pub fn enable_rx_interrupt(&self) {
        let mcr = self.regs.read(MCR);
        self.regs.write(MCR, mcr | MCR_OUT2);
        self.regs.write(IER, IER_RXRDY);
    }

    /// Masks the receive interrupt (`false`) while what was read waits to
    /// be taken, leaving new bytes in the FIFO; unmasked, bytes waiting
    /// there raise it again.
    pub fn set_rx_interrupt(&self, on: bool) {
        self.regs.write(IER, if on { IER_RXRDY } else { 0 });
    }

    /// Interrupts for received data, room to transmit, and modem changes,
    /// with the PC's interrupt gate open.
    pub fn set_interrupts(&self, rx: bool, tx: bool, modem: bool) {
        let mcr = self.regs.read(MCR);
        self.regs.write(MCR, mcr | MCR_OUT2);
        let mut ier = 0;
        if rx {
            ier |= IER_RXRDY;
        }
        if tx {
            ier |= IER_TXRDY;
        }
        if modem {
            ier |= IER_MSI;
        }
        self.regs.write(IER, ier);
    }

    /// Whether an interrupt is pending (`IIR`); reading it acknowledges a
    /// transmitter-empty one.
    pub fn pending(&self) -> bool {
        self.regs.read(IIR) & IIR_NOPEND == 0
    }

    /// Whether the transmitter is empty: [`TX_FIFO`] bytes fit.
    pub fn tx_empty(&self) -> bool {
        self.regs.read(LSR) & LSR_THR_EMPTY != 0
    }

    /// Sends a byte without waiting (after [`Uart::tx_empty`]).
    pub fn put_now(&self, b: u8) {
        self.regs.write(THR, b);
    }

    /// Empties both FIFOs.
    pub fn reset_fifos(&self) {
        self.regs.write(FCR, FCR_ENABLE | FCR_RCV_RST | FCR_XMT_RST);
    }

    /// Sets the speed (from the port's clock `rclk`), the data bits (5 to
    /// 8), two stop bits, and parity (`Some(even)`) (`ns8250_param`).
    /// `false` for a speed the divisor cannot reach.
    pub fn set_line(
        &self,
        rclk: u32,
        speed: u32,
        bits: u8,
        stop2: bool,
        parity: Option<bool>,
    ) -> bool {
        if speed == 0 {
            return false;
        }
        // Rounded to the nearest divisor, as `ns8250_divisor` does, and
        // within 3% of the speed.
        let divisor = (rclk / 8 / speed).div_ceil(2);
        if divisor == 0 || divisor > 0xffff {
            return false;
        }
        let actual = i64::from(rclk / (divisor * 16));
        let error = (actual - i64::from(speed)) * 1000 / i64::from(speed);
        if !(-30..=30).contains(&error) {
            return false;
        }
        let mut lcr = bits.clamp(5, 8) - 5;
        if stop2 {
            lcr |= LCR_STOPB;
        }
        if let Some(even) = parity {
            lcr |= LCR_PENAB;
            if even {
                lcr |= LCR_PEVEN;
            }
        }
        let [lo, hi, ..] = divisor.to_le_bytes();
        self.regs.write(LCR, lcr | LCR_DLAB);
        self.regs.write(DLL, lo);
        self.regs.write(DLM, hi);
        self.regs.write(LCR, lcr);
        true
    }

    /// Raises or drops DTR and RTS.
    pub fn set_modem(&self, dtr: bool, rts: bool) {
        let mut mcr = self.regs.read(MCR) & !(MCR_DTR | MCR_RTS);
        if dtr {
            mcr |= MCR_DTR;
        }
        if rts {
            mcr |= MCR_RTS;
        }
        self.regs.write(MCR, mcr);
    }

    /// DTR and RTS as set.
    pub fn modem_out(&self) -> (bool, bool) {
        let mcr = self.regs.read(MCR);
        (mcr & MCR_DTR != 0, mcr & MCR_RTS != 0)
    }

    /// The modem status (`MSR`: the `MSR_*` bits); reading clears the
    /// change bits and their interrupt.
    pub fn modem_status(&self) -> u8 {
        self.regs.read(MSR)
    }

    /// Sends a break while `on`.
    pub fn set_break(&self, on: bool) {
        let lcr = self.regs.read(LCR) & !LCR_SBREAK;
        self.regs
            .write(LCR, if on { lcr | LCR_SBREAK } else { lcr });
    }
}

impl<R: RegisterIo> core::fmt::Write for Uart<R> {
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

#[cfg(test)]
mod tests {
    extern crate std;
    use core::cell::RefCell;
    use core::fmt::Write;
    use std::vec::Vec;

    use super::*;

    #[derive(Default)]
    struct Fake {
        writes: RefCell<Vec<(usize, u8)>>,
    }

    impl RegisterIo for &Fake {
        fn read(&self, n: usize) -> u8 {
            match n {
                LSR => LSR_THR_EMPTY,
                _ => 0,
            }
        }
        fn write(&self, n: usize, v: u8) {
            self.writes.borrow_mut().push((n, v));
        }
    }

    #[test]
    fn translates_newlines() {
        let fake = Fake::default();
        let mut u = Uart::init(&fake);
        writeln!(u, "a").unwrap();
        let sent: Vec<u8> = fake
            .writes
            .borrow()
            .iter()
            .filter(|(r, _)| *r == THR)
            .map(|(_, v)| *v)
            .collect();
        assert_eq!(sent, b"a\r\n");
        assert_eq!(fake.writes.borrow()[..2], [(IER, 0), (FCR, FCR_ENABLE)]);
        assert_eq!(u.get(), None);
    }
}
