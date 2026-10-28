// SPDX-License-Identifier: BSD-2-Clause
//! Serial ports other than the console's, as terminals (`uart_tty.c`):
//! `ttyu<N>` and `cuau<N>`, with their init and lock state nodes, on a
//! port the `uart` driver drives by interrupt. Line settings reach the
//! port (`uart_tty_param`), the modem lines are the port's, the last close
//! drops DTR and RTS, and losing the carrier without `CLOCAL` hangs the
//! terminal up.
//!
//! Behavior reference: sys/dev/uart/uart_tty.c.

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::errno::Errno;
use abi::ioctl::{TIOCCBRK, TIOCSBRK};
use abi::termios::{CNO_RTSDTR, CS5, CS6, CS7, CSIZE, CSTOPB, PARENB, PARODD, Termios};
use sched::Sleepable;
use sync::SpinLock;

use super::{Driver, Make, Output, SER_DTR, SER_RTS, Tty};

/// The terminal layer, as the `uart` driver reaches it.
pub(crate) struct Host;

/// The one [`Host`].
pub(crate) static HOST: Host = Host;

/// The ports that are terminals, by `uart` unit.
static PORTS: SpinLock<Vec<(u32, Arc<dyn uart::Port>)>> = SpinLock::new(Vec::new());

/// The port of `uart<unit>`, if it is a terminal.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))]
pub(crate) fn port(unit: u32) -> Option<Arc<dyn uart::Port>> {
    PORTS
        .lock()
        .iter()
        .find(|(u, _)| *u == unit)
        .map(|(_, p)| Arc::clone(p))
}

impl uart::Terminals for Host {
    /// `uart_tty_attach`.
    fn attach(&self, unit: u32, port: Arc<dyn uart::Port>) -> Arc<dyn uart::Line> {
        PORTS.lock().push((unit, Arc::clone(&port)));
        let tty = super::create(
            &alloc::format!("ttyu{unit}"),
            Arc::new(Port(port)),
            Make {
                initlock: true,
                callout: true,
                user: None,
            },
        );
        Arc::new(Line(tty))
    }
}

/// A port as its terminal's device (`uart_tty_class`).
struct Port(Arc<dyn uart::Port>);

impl Driver for Port {
    fn outwakeup(&self, out: &mut Output<'_>) {
        self.0.transmit(&mut |buf| out.getc(buf));
    }

    /// `uart_tty_param`: the speed and framing go to the port; DTR and
    /// RTS come up unless `CNO_RTSDTR`, and a speed of 0 hangs up.
    fn param(&self, t: &mut Termios) -> Result<(), Errno> {
        if t.c_ispeed != t.c_ospeed && t.c_ospeed != 0 {
            return Err(Errno::EINVAL);
        }
        if t.c_ospeed == 0 {
            self.0.modem(0, SER_DTR | SER_RTS);
            return Ok(());
        }
        let bits = match t.c_cflag & CSIZE {
            CS5 => 5,
            CS6 => 6,
            CS7 => 7,
            _ => 8,
        };
        let parity = (t.c_cflag & PARENB != 0).then_some(t.c_cflag & PARODD == 0);
        if !self
            .0
            .param(t.c_ospeed, bits, t.c_cflag & CSTOPB != 0, parity)
        {
            return Err(Errno::EINVAL);
        }
        if t.c_cflag & CNO_RTSDTR == 0 {
            self.0.modem(SER_DTR | SER_RTS, 0);
        }
        Ok(())
    }

    fn modem(&self, on: i32, off: i32) -> i32 {
        self.0.modem(on, off)
    }

    /// `uart_tty_close`: DTR and RTS drop.
    fn close(&self) {
        self.0.modem(0, SER_DTR | SER_RTS);
    }

    fn ioctl(&self, _s: &Sleepable, req: u32, _arg: &mut [u8]) -> Result<(), Errno> {
        match req {
            TIOCSBRK | TIOCCBRK => {
                self.0.set_break(req == TIOCSBRK);
                Ok(())
            }
            _ => Err(Errno::ENOTTY),
        }
    }
}

/// The terminal, as its port's interrupt thread tells it.
struct Line(Arc<Tty>);

impl uart::Line for Line {
    fn input(&self, s: &Sleepable, bytes: &[u8]) -> usize {
        self.0.input(s, bytes)
    }

    fn transmit(&self, s: &Sleepable) {
        self.0.transmit(s);
    }

    fn carrier(&self, s: &Sleepable, on: bool) {
        self.0.carrier(s, on);
    }

    /// `uart_tty_detach`: the terminal is gone, and freed with its nodes
    /// once nothing holds it.
    fn gone(&self, s: &Sleepable) {
        self.0.rel_gone(s);
    }
}
