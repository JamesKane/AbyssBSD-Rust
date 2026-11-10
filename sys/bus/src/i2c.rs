// SPDX-License-Identifier: BSD-2-Clause
//! I2C providers (DRIVER-MODEL.md §9.3): FreeBSD's `iicbus_transfer`.
//!
//! An I2C controller's driver implements [`I2c`] and publishes it with
//! `StartCx::provide_i2c`; the framework wraps it in an [`I2cSource`],
//! which runs one transfer at a time (FreeBSD's `iicbus_request_bus`
//! around `iicbus_transfer`). A device on the bus asks for its connection
//! (ACPI `I2cSerialBusV2`) with `AttachCx::i2c` and gets an [`I2cDevice`]
//! bound to its address.
//!
//! A transfer is FreeBSD's array of `struct iic_msg`: each [`Msg`] is one
//! direction to one address, joined to the next by a repeated START, and
//! the last ends with a STOP.
//!
//! A controller's driver also adds the `iicbus` child FreeBSD's
//! controllers add (`StartCx::add_iicbus`), with [`IicbusOf`] as its bus
//! data; the `iicbus` driver takes the controller's [`I2cSource`] with
//! `AttachCx::parent_i2c`. A caller holding the bus across several
//! transfers (`iic`'s `I2CRDWR`, whose messages may each end in a STOP)
//! takes it with [`I2cSource::acquire`], FreeBSD's `iicbus_request_bus`.

use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use sched::Sleepable;
use sched::lock::{LockClass, Mutex, MutexGuard};

use crate::provider::ProviderError;

static BUS: LockClass = LockClass::new("i2c bus", 261);

/// A device's connection to its bus, as its firmware describes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct I2cTarget {
    /// The device's address (7 or 10 bits, not shifted).
    pub address: u16,
    /// 10-bit addressing.
    pub ten_bit: bool,
    /// The fastest the device runs, in Hz.
    pub speed: u32,
}

/// One message's direction and data.
#[derive(Debug)]
pub enum Op<'a> {
    /// Read into the buffer (`IIC_M_RD`).
    Read(&'a mut [u8]),
    /// Write the bytes.
    Write(&'a [u8]),
}

impl Op<'_> {
    /// The message's length.
    #[must_use]
    pub fn len(&self) -> usize {
        match self {
            Self::Read(b) => b.len(),
            Self::Write(b) => b.len(),
        }
    }

    /// Whether it carries no data (an SMBus quick command).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// One message of a transfer.
#[derive(Debug)]
pub struct Msg<'a> {
    /// The target's address (not shifted).
    pub address: u16,
    /// 10-bit addressing.
    pub ten_bit: bool,
    /// What it does.
    pub op: Op<'a>,
}

/// An I2C controller in host mode.
pub trait I2c: Send + Sync {
    /// Runs `msgs` as one transfer: a START, each message, a repeated
    /// START between messages, and one STOP. Errors: [`ProviderError::NoAck`]
    /// (nothing answered the address, or a byte was refused),
    /// [`ProviderError::ArbitrationLost`], [`ProviderError::BusBusy`],
    /// [`ProviderError::BusError`], [`ProviderError::Timeout`], and
    /// [`ProviderError::Unsupported`] for a message the controller cannot
    /// do (10-bit addressing, a zero-length read).
    fn transfer(&self, s: &Sleepable, msgs: &mut [Msg<'_>]) -> Result<(), ProviderError>;
    /// The bus clock in Hz.
    fn speed(&self) -> u32;
    /// Puts the controller back in its idle state (FreeBSD's
    /// `iicbus_reset`, `I2CRSTCARD`). By default nothing: a controller
    /// that resets itself after every transfer has nothing to do.
    fn reset(&self, s: &Sleepable) -> Result<(), ProviderError> {
        let _ = s;
        Ok(())
    }
}

/// The bus data a controller gives the `iicbus` child it adds: the
/// controller's ACPI path, when firmware describes it (FreeBSD's
/// `acpi_iicbus` attaches instead of the plain `iicbus` then, and finds
/// the devices whose `I2cSerialBusV2` names that path).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IicbusOf {
    /// The controller's namespace path (`\_SB_.I2C3`).
    pub acpi: Option<String>,
    /// The controller's unit, which its bus takes: FreeBSD attaches the
    /// controllers one after another, so `iicbusN` is `cdnc_i2cN`'s;
    /// here they attach at once, in no set order.
    pub unit: u32,
}

/// An I2C controller, run one transfer at a time.
pub struct I2cSource {
    imp: Arc<dyn I2c>,
    bus: Mutex<()>,
    revoked: AtomicBool,
}

impl core::fmt::Debug for I2cSource {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("I2cSource")
            .field("speed", &self.imp.speed())
            .finish_non_exhaustive()
    }
}

impl I2cSource {
    /// Wraps a provider.
    #[must_use]
    pub fn new(imp: Arc<dyn I2c>) -> Arc<Self> {
        Arc::new(Self {
            imp,
            bus: Mutex::new(&BUS, ()),
            revoked: AtomicBool::new(false),
        })
    }

    /// The bus clock in Hz.
    #[must_use]
    pub fn speed(&self) -> u32 {
        self.imp.speed()
    }

    /// A device at `target` on the bus.
    #[must_use]
    pub fn device(self: &Arc<Self>, target: I2cTarget) -> I2cDevice {
        I2cDevice {
            source: Arc::clone(self),
            target,
        }
    }

    /// Runs `msgs` as one transfer, holding the bus for it. Messages are
    /// checked first: a 7-bit address above `0x7f` or a 10-bit one above
    /// `0x3ff` is [`ProviderError::OutOfRange`].
    pub fn transfer(&self, s: &Sleepable, msgs: &mut [Msg<'_>]) -> Result<(), ProviderError> {
        if msgs.is_empty() {
            return Ok(());
        }
        check(msgs)?;
        self.acquire(s)?.run(s, msgs)
    }

    /// Takes the bus until the guard drops, waiting for a transfer under
    /// way (FreeBSD's `iicbus_request_bus` with `IIC_WAIT`).
    pub fn acquire(&self, s: &Sleepable) -> Result<I2cBus<'_>, ProviderError> {
        let guard = self.bus.lock(s);
        self.live()?;
        Ok(I2cBus {
            src: self,
            _guard: guard,
        })
    }

    /// Takes the bus if nobody holds it ([`ProviderError::BusBusy`]
    /// otherwise: `iicbus_request_bus` with `IIC_DONTWAIT`).
    pub fn try_acquire(&self) -> Result<I2cBus<'_>, ProviderError> {
        let guard = self.bus.try_lock().ok_or(ProviderError::BusBusy)?;
        self.live()?;
        Ok(I2cBus {
            src: self,
            _guard: guard,
        })
    }

    /// Stops every transfer: the driver is detaching. A transfer under
    /// way finishes first.
    pub fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
    }

    /// Whether [`Self::revoke`] has run.
    pub fn is_revoked(&self) -> bool {
        self.revoked.load(Ordering::Acquire)
    }

    fn live(&self) -> Result<(), ProviderError> {
        if self.is_revoked() {
            Err(ProviderError::Revoked)
        } else {
            Ok(())
        }
    }
}

/// Refuses an address too wide for its mode.
fn check(msgs: &[Msg<'_>]) -> Result<(), ProviderError> {
    for m in msgs {
        let max = if m.ten_bit { 0x3ff } else { 0x7f };
        if m.address > max {
            return Err(ProviderError::OutOfRange);
        }
    }
    Ok(())
}

/// The bus, held: transfers run back to back with nobody else's between
/// them.
pub struct I2cBus<'a> {
    src: &'a I2cSource,
    _guard: MutexGuard<'a, ()>,
}

impl core::fmt::Debug for I2cBus<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("I2cBus").finish_non_exhaustive()
    }
}

impl I2cBus<'_> {
    /// Runs `msgs` as one transfer (checked as [`I2cSource::transfer`]
    /// checks them).
    pub fn transfer(&self, s: &Sleepable, msgs: &mut [Msg<'_>]) -> Result<(), ProviderError> {
        if msgs.is_empty() {
            return Ok(());
        }
        check(msgs)?;
        self.run(s, msgs)
    }

    fn run(&self, s: &Sleepable, msgs: &mut [Msg<'_>]) -> Result<(), ProviderError> {
        self.src.live()?;
        self.src.imp.transfer(s, msgs)
    }

    /// Resets the controller ([`I2c::reset`]).
    pub fn reset(&self, s: &Sleepable) -> Result<(), ProviderError> {
        self.src.live()?;
        self.src.imp.reset(s)
    }
}

/// A device on an I2C bus, at its address.
#[derive(Debug, Clone)]
pub struct I2cDevice {
    source: Arc<I2cSource>,
    target: I2cTarget,
}

impl I2cDevice {
    /// Where the device is.
    #[must_use]
    pub fn target(&self) -> I2cTarget {
        self.target
    }

    /// The bus clock in Hz.
    #[must_use]
    pub fn bus_speed(&self) -> u32 {
        self.source.speed()
    }

    /// Runs `ops` to this device as one transfer.
    pub fn transfer(&self, s: &Sleepable, ops: &mut [Op<'_>]) -> Result<(), ProviderError> {
        let mut msgs: alloc::vec::Vec<Msg<'_>> = ops
            .iter_mut()
            .map(|op| Msg {
                address: self.target.address,
                ten_bit: self.target.ten_bit,
                op: match op {
                    Op::Read(b) => Op::Read(b),
                    Op::Write(b) => Op::Write(b),
                },
            })
            .collect();
        self.source.transfer(s, &mut msgs)
    }

    /// Writes `data`.
    pub fn write(&self, s: &Sleepable, data: &[u8]) -> Result<(), ProviderError> {
        self.transfer(s, &mut [Op::Write(data)])
    }

    /// Reads into `buf`.
    pub fn read(&self, s: &Sleepable, buf: &mut [u8]) -> Result<(), ProviderError> {
        self.transfer(s, &mut [Op::Read(buf)])
    }

    /// Writes `cmd` (a register address), then reads into `buf` after a
    /// repeated START: FreeBSD's `iicdev_readfrom`.
    pub fn write_read(
        &self,
        s: &Sleepable,
        cmd: &[u8],
        buf: &mut [u8],
    ) -> Result<(), ProviderError> {
        self.transfer(s, &mut [Op::Write(cmd), Op::Read(buf)])
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::vec;
    use alloc::vec::Vec;
    use std::sync::Mutex as StdMutex;

    use super::*;

    /// A bus with one register file at 0x32: a write sets the register
    /// pointer, then writes registers; a read reads from the pointer.
    #[derive(Default)]
    struct Fake {
        regs: StdMutex<([u8; 16], usize)>,
        log: StdMutex<Vec<(u16, bool, usize)>>,
    }

    impl I2c for Fake {
        fn transfer(&self, _: &Sleepable, msgs: &mut [Msg<'_>]) -> Result<(), ProviderError> {
            let mut r = self.regs.lock().unwrap();
            for m in msgs.iter_mut() {
                self.log
                    .lock()
                    .unwrap()
                    .push((m.address, matches!(m.op, Op::Read(_)), m.op.len()));
                if m.address != 0x32 {
                    return Err(ProviderError::NoAck);
                }
                match &mut m.op {
                    Op::Write(b) => {
                        if let Some((&p, rest)) = b.split_first() {
                            r.1 = usize::from(p);
                            for &v in rest {
                                let i = r.1 % 16;
                                r.0[i] = v;
                                r.1 += 1;
                            }
                        }
                    }
                    Op::Read(b) => {
                        for v in b.iter_mut() {
                            *v = r.0[r.1 % 16];
                            r.1 += 1;
                        }
                    }
                }
            }
            Ok(())
        }
        fn speed(&self) -> u32 {
            100_000
        }
    }

    fn s() -> Sleepable {
        // SAFETY: host tests run on threads that may block.
        unsafe { Sleepable::assume() }
    }

    #[test]
    fn a_device_reads_its_registers() {
        let s = s();
        let fake = Arc::new(Fake::default());
        let src = I2cSource::new(fake.clone());
        let rtc = src.device(I2cTarget {
            address: 0x32,
            ten_bit: false,
            speed: 400_000,
        });
        rtc.write(&s, &[0x00, 0x59, 0x30, 0x12]).unwrap();
        let mut t = [0u8; 3];
        rtc.write_read(&s, &[0x00], &mut t).unwrap();
        assert_eq!(t, [0x59, 0x30, 0x12]);
        assert_eq!(rtc.bus_speed(), 100_000);
        let absent = src.device(I2cTarget {
            address: 0x33,
            ten_bit: false,
            speed: 100_000,
        });
        assert_eq!(absent.read(&s, &mut t), Err(ProviderError::NoAck));
        let bad = src.device(I2cTarget {
            address: 0x80,
            ten_bit: false,
            speed: 100_000,
        });
        assert_eq!(bad.read(&s, &mut t), Err(ProviderError::OutOfRange));
        assert_eq!(
            *fake.log.lock().unwrap(),
            vec![
                (0x32, false, 4),
                (0x32, false, 1),
                (0x32, true, 3),
                (0x33, true, 3)
            ]
        );
        src.revoke();
        assert_eq!(rtc.read(&s, &mut t), Err(ProviderError::Revoked));
    }
}
