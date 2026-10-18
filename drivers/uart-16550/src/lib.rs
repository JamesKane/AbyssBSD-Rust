// SPDX-License-Identifier: BSD-2-Clause
//! NS16550-compatible UART, polled.
//!
//! The early console keeps the line settings firmware programmed (from
//! SPCR); [`Uart::init`] only masks interrupts and enables the FIFOs.
//!
//! Behavior reference: sys/dev/uart/uart_dev_ns8250.c.

#![no_std]
#![forbid(unsafe_code)]

use regs::RegisterIo;

const THR: usize = 0;
const RBR: usize = 0;
const IER: usize = 1;
const FCR: usize = 2;
const LSR: usize = 5;

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
