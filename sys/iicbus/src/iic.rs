// SPDX-License-Identifier: BSD-2-Clause
//! `iic`'s requests on an open of `/dev/iicN`, over the bus's controller
//! and the caller's memory.
//!
//! The controllers here (`cdnc_i2c`, later DesignWare) do whole transfers
//! only, as FreeBSD's `cdnc_i2c` does: they have no `iicbus_start`, so
//! `I2CSTART`, and `read(2)` or `write(2)` once an address is set, fail
//! with `ENODEV` as FreeBSD's `iicbus_nosupport` makes them, and a
//! sequence is never started: `I2CWRITE`, `I2CREAD`, and `I2CRPTSTART`
//! are `EINVAL`, `I2CSTOP` does nothing. `i2c(8)` falls back to
//! `I2CRDWR` (its default "transfer" mode, and its scan's second method).
//!
//! `I2CRDWR` takes at most `IIC_RDRW_MAX_MSGS` messages of at most 64 KiB
//! each (their `uint16_t` length), as FreeBSD does. A message without
//! `IIC_M_NOSTOP` ends with a STOP, the next starts afresh; one with it is
//! followed by a repeated START, or, when the next has `IIC_M_NOSTART`, by
//! that message's bytes as more of the same message (`i2c -m tr` writes a
//! register offset that way). The bus is held for the whole request.
//!
//! Errors are FreeBSD's `iic`'s: a transfer's `IIC_E*` status is returned
//! as it is, so a target that does not answer is error 2 (`IIC_ENOACK`,
//! which `errno` names `ENOENT`), as on FreeBSD.
//!
//! Behavior reference: sys/dev/iicbus/iic.c, sys/dev/iicbus/iiconf.c,
//! sys/dev/iicbus/controller/cadence/cdnc_i2c.c (`cdnc_i2c_transfer`).

use alloc::vec::Vec;

use abi::errno::Errno;
use abi::iic::{
    I2CRDWR, I2CREAD, I2CRPTSTART, I2CRSTCARD, I2CSADDR, I2CSTART, I2CSTOP, I2CWRITE,
    IIC_M_NOSTART, IIC_M_NOSTOP, IIC_M_RD, IIC_MSG_LEN, IIC_RDRW_MAX_MSGS, IicMsg, IicRdwrData,
};
use bus::i2c::{I2cBus, I2cSource, Msg, Op};
use bus::provider::ProviderError;
use sched::Sleepable;

/// The caller's memory, as the kernel's `copyin` and `copyout` reach it.
pub trait UserMemory: Sync {
    /// Copies `dst.len()` bytes from user address `src`.
    fn copy_in(&self, dst: &mut [u8], src: u64) -> Result<(), Errno>;
    /// Copies `src` to user address `dst`.
    fn copy_out(&self, dst: u64, src: &[u8]) -> Result<(), Errno>;
}

/// FreeBSD's `IIC_E*` statuses (`iiconf.h`), which `iic` returns as
/// error numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum IicStatus {
    /// `IIC_EBUSERR`.
    BusErr = 1,
    /// `IIC_ENOACK`.
    NoAck = 2,
    /// `IIC_ETIMEOUT`.
    Timeout = 3,
    /// `IIC_EBUSBSY`.
    BusBusy = 4,
    /// `IIC_ESTATUS`.
    Status = 5,
    /// `IIC_EUNDERFLOW`.
    Underflow = 6,
    /// `IIC_EOVERFLOW`.
    Overflow = 7,
    /// `IIC_ENOTSUPP`.
    NotSupported = 8,
    /// `IIC_ENOADDR`.
    NoAddr = 9,
    /// `IIC_ERESOURCE`.
    Resource = 10,
}

impl IicStatus {
    /// The error number a caller of `iic` sees: the status's value.
    #[must_use]
    pub fn errno(self) -> Errno {
        match self {
            Self::BusErr => Errno::EPERM,
            Self::NoAck => Errno::ENOENT,
            Self::Timeout => Errno::ESRCH,
            Self::BusBusy => Errno::EINTR,
            Self::Status => Errno::EIO,
            Self::Underflow => Errno::ENXIO,
            Self::Overflow => Errno::E2BIG,
            Self::NotSupported => Errno::ENOEXEC,
            Self::NoAddr => Errno::EBADF,
            Self::Resource => Errno::ECHILD,
        }
    }
}

/// A provider's error as `iic` returns it: the status FreeBSD's
/// controller would give, or `ENXIO` once the controller is gone.
#[must_use]
pub fn errno_of(e: ProviderError) -> Errno {
    let status = match e {
        ProviderError::NoAck => IicStatus::NoAck,
        ProviderError::ArbitrationLost | ProviderError::BusError => IicStatus::BusErr,
        ProviderError::BusBusy => IicStatus::BusBusy,
        ProviderError::Timeout => IicStatus::Timeout,
        ProviderError::Unsupported | ProviderError::OutOfRange => IicStatus::NotSupported,
        ProviderError::Revoked => return Errno::ENXIO,
        _ => return Errno::EIO,
    };
    status.errno()
}

/// What an open of `/dev/iicN` remembers between requests
/// (`struct iic_cdevpriv`): the address `read(2)` and `write(2)` use.
#[derive(Debug, Default)]
pub struct OpenState {
    /// Set by `I2CSADDR` (8-bit form); 0 until then.
    pub addr: u8,
}

/// Takes the bus, waiting for it unless `nonblock` (`O_NONBLOCK`:
/// `IIC_DONTWAIT`).
fn request_bus<'a>(src: &'a I2cSource, s: &Sleepable, nonblock: bool) -> Result<I2cBus<'a>, Errno> {
    let r = if nonblock {
        src.try_acquire()
    } else {
        src.acquire(s)
    };
    r.map_err(errno_of)
}

/// Runs request `req` with argument `arg` (copied in already) for an open
/// whose state is `st`, as FreeBSD's `iicioctl`.
pub fn ioctl(
    st: &mut OpenState,
    src: &I2cSource,
    s: &Sleepable,
    mem: &dyn UserMemory,
    req: u32,
    arg: &[u8],
    nonblock: bool,
) -> Result<(), Errno> {
    match req {
        I2CSTART => {
            // The bus is taken, the controller cannot START, and the bus
            // goes back.
            let _bus = request_bus(src, s, nonblock)?;
            Err(Errno::ENODEV)
        }
        // Nothing was started.
        I2CSTOP => Ok(()),
        I2CRSTCARD => {
            let bus = request_bus(src, s, nonblock)?;
            match bus.reset(s) {
                Ok(()) => Ok(()),
                Err(e) => Err(errno_of(e)),
            }
        }
        I2CWRITE | I2CREAD | I2CRPTSTART => Err(Errno::EINVAL),
        I2CRDWR => {
            let d = IicRdwrData::from_bytes(arg).ok_or(Errno::EINVAL)?;
            rdwr(src, s, mem, d, nonblock)
        }
        I2CSADDR => {
            st.addr = *arg.first().ok_or(Errno::EINVAL)?;
            Ok(())
        }
        _ => Err(Errno::ENOTTY),
    }
}

/// `read(2)` or `write(2)` on an open (`iicuio`): `ENXIO` before
/// `I2CSADDR`, else the START the controller cannot send.
pub fn uio(st: &OpenState, src: &I2cSource, s: &Sleepable, nonblock: bool) -> Result<(), Errno> {
    if st.addr == 0 {
        return Err(Errno::ENXIO);
    }
    let _bus = request_bus(src, s, nonblock)?;
    Err(Errno::ENODEV)
}

/// A zeroed buffer of `n` bytes, or `ENOMEM`.
fn buffer(n: usize) -> Result<Vec<u8>, Errno> {
    let mut v = Vec::new();
    v.try_reserve_exact(n).map_err(|_| Errno::ENOMEM)?;
    v.resize(n, 0);
    Ok(v)
}

/// One message on the wire: an address, a direction, and the request's
/// messages whose bytes it carries (more than one where `IIC_M_NOSTART`
/// continued it).
#[derive(Debug, PartialEq, Eq)]
pub struct Wire {
    /// The 7-bit address.
    pub address: u16,
    /// A read.
    pub read: bool,
    /// The request's messages it carries, in order.
    pub parts: Vec<usize>,
}

/// How a request's messages go on the wire: transfers (each ending in a
/// STOP) of messages (joined by repeated STARTs). A message with
/// `IIC_M_NOSTART` must follow one with `IIC_M_NOSTOP` in the same
/// direction; anything else is `IIC_ENOTSUPP`, as `cdnc_i2c` answers a
/// NOSTART it cannot honor. A last message's `IIC_M_NOSTOP` is ignored:
/// the bus is not kept from one request to the next.
pub fn plan(msgs: &[IicMsg]) -> Result<Vec<Vec<Wire>>, IicStatus> {
    let mut transfers: Vec<Vec<Wire>> = Vec::new();
    let mut held = false;
    for (i, m) in msgs.iter().enumerate() {
        let read = m.flags & IIC_M_RD != 0;
        if m.flags & IIC_M_NOSTART != 0 {
            let last = transfers
                .last_mut()
                .and_then(|t| t.last_mut())
                .filter(|w| held && w.read == read)
                .ok_or(IicStatus::NotSupported)?;
            last.parts.push(i);
        } else {
            let wire = Wire {
                address: m.slave >> 1,
                read,
                parts: alloc::vec![i],
            };
            match transfers.last_mut() {
                Some(t) if held => t.push(wire),
                _ => transfers.push(alloc::vec![wire]),
            }
        }
        held = m.flags & IIC_M_NOSTOP != 0;
    }
    Ok(transfers)
}

/// `I2CRDWR` (`iicrdwr`): the messages and their written bytes copied in,
/// the transfers run with the bus held, and the read bytes copied out.
pub fn rdwr(
    src: &I2cSource,
    s: &Sleepable,
    mem: &dyn UserMemory,
    d: IicRdwrData,
    nonblock: bool,
) -> Result<(), Errno> {
    if d.nmsgs > IIC_RDRW_MAX_MSGS {
        return Err(Errno::EINVAL);
    }
    // At most 42 messages: the sizes are small.
    let n = usize::try_from(d.nmsgs).map_err(|_| Errno::EINVAL)?;
    let mut raw = buffer(n * IIC_MSG_LEN)?;
    mem.copy_in(&mut raw, d.msgs)?;
    let msgs: Vec<IicMsg> = raw
        .as_chunks::<IIC_MSG_LEN>()
        .0
        .iter()
        .filter_map(|c| IicMsg::from_bytes(c))
        .collect();
    // Each message's bytes; a write's copied in.
    let mut data: Vec<Vec<u8>> = Vec::new();
    data.try_reserve_exact(n).map_err(|_| Errno::ENOMEM)?;
    for m in &msgs {
        let mut b = buffer(usize::from(m.len))?;
        if m.flags & IIC_M_RD == 0 {
            mem.copy_in(&mut b, m.buf)?;
        }
        data.push(b);
    }
    let transfers = plan(&msgs).map_err(IicStatus::errno)?;
    {
        let bus = request_bus(src, s, nonblock)?;
        for t in &transfers {
            run(&bus, s, t, &mut data)?;
        }
    }
    for (m, b) in msgs.iter().zip(&data) {
        if m.flags & IIC_M_RD != 0 {
            mem.copy_out(m.buf, b)?;
        }
    }
    Ok(())
}

/// Runs one transfer: its wire messages, each over the bytes of the
/// request's messages it carries.
fn run(bus: &I2cBus<'_>, s: &Sleepable, t: &[Wire], data: &mut [Vec<u8>]) -> Result<(), Errno> {
    // A wire message carrying several request messages runs over one
    // buffer: the written bytes joined, or the read ones split after.
    let mut joined: Vec<Vec<u8>> = Vec::new();
    for w in t {
        let total: usize = w.parts.iter().map(|&i| data[i].len()).sum();
        let mut b = buffer(total)?;
        if !w.read {
            let mut at = 0;
            for &i in &w.parts {
                let len = data[i].len();
                b[at..at + len].copy_from_slice(&data[i]);
                at += len;
            }
        }
        joined.push(b);
    }
    let mut msgs: Vec<Msg<'_>> = t
        .iter()
        .zip(joined.iter_mut())
        .map(|(w, b)| Msg {
            address: w.address,
            ten_bit: false,
            op: if w.read {
                Op::Read(b.as_mut_slice())
            } else {
                Op::Write(b.as_slice())
            },
        })
        .collect();
    bus.transfer(s, &mut msgs).map_err(errno_of)?;
    drop(msgs);
    for (w, b) in t.iter().zip(&joined) {
        if w.read {
            let mut at = 0;
            for &i in &w.parts {
                let len = data[i].len();
                data[i].copy_from_slice(&b[at..at + len]);
                at += len;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
