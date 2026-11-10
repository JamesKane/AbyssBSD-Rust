// SPDX-License-Identifier: BSD-2-Clause
//! `/dev/iicN` (`dev/iicbus/iic.h`): the requests `i2c(8)` and other I2C
//! tools send to `iic`, and their arguments.
//!
//! Addresses in these structures are FreeBSD's 8-bit form: the 7-bit
//! address shifted left once (`i2c -a 0x50` sends `0xa0`), the low bit
//! the direction where it matters.
//!
//! Behavior reference: sys/dev/iicbus/iic.h, sys/dev/iicbus/iic.c.

use crate::ioctl::{IOC_IN, IOC_VOID};
use crate::layout::{Arches, ConstValue};

/// `struct iic_msg`: one message of an `I2CRDWR` transfer.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IicMsg {
    /// The target's address, shifted left once.
    pub slave: u16,
    /// `IIC_M_*`.
    pub flags: u16,
    /// The buffer's length.
    pub len: u16,
    /// The buffer (a user address).
    pub buf: u64,
}

/// `struct iiccmd`: the argument of `I2CSTART`, `I2CRSTCARD`,
/// `I2CWRITE`, `I2CREAD`, and `I2CRPTSTART`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IicCmd {
    /// The target's address, shifted left once, with the direction bit.
    pub slave: u8,
    /// Bytes to move.
    pub count: i32,
    /// Whether a read is the last (its final byte is not acknowledged).
    pub last: i32,
    /// The data (a user address).
    pub buf: u64,
}

/// `struct iic_rdwr_data`: `I2CRDWR`'s argument.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IicRdwrData {
    /// The messages (a user address of `nmsgs` [`IicMsg`]s).
    pub msgs: u64,
    /// How many.
    pub nmsgs: u32,
}

/// `sizeof(struct iic_msg)`.
pub const IIC_MSG_LEN: usize = 16;
/// `sizeof(struct iiccmd)`.
pub const IICCMD_LEN: usize = 24;
/// `sizeof(struct iic_rdwr_data)`.
pub const IIC_RDWR_DATA_LEN: usize = 16;
const _: () = assert!(size_of::<IicMsg>() == IIC_MSG_LEN);
const _: () = assert!(size_of::<IicCmd>() == IICCMD_LEN);
const _: () = assert!(size_of::<IicRdwrData>() == IIC_RDWR_DATA_LEN);

/// A write (no flag).
pub const IIC_M_WR: u16 = 0;
/// A read.
pub const IIC_M_RD: u16 = 0x0001;
/// No STOP after the message.
pub const IIC_M_NOSTOP: u16 = 0x0002;
/// No START before the message: it continues the one before.
pub const IIC_M_NOSTART: u16 = 0x0004;

/// The most messages one `I2CRDWR` takes.
pub const IIC_RDRW_MAX_MSGS: u32 = 42;

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_ne_bytes([b[at], b[at + 1]])
}

fn i32_at(b: &[u8], at: usize) -> i32 {
    i32::from_ne_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut v = [0; 8];
    v.copy_from_slice(&b[at..at + 8]);
    u64::from_ne_bytes(v)
}

impl IicMsg {
    /// The message in `b` (one element of the array `I2CRDWR` points
    /// to), or `None` if `b` is not one.
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() != IIC_MSG_LEN {
            return None;
        }
        Some(Self {
            slave: u16_at(b, 0),
            flags: u16_at(b, 2),
            len: u16_at(b, 4),
            buf: u64_at(b, 8),
        })
    }

    /// The message's bytes, padding zeroed.
    #[must_use]
    pub fn to_bytes(self) -> [u8; IIC_MSG_LEN] {
        let mut b = [0; IIC_MSG_LEN];
        b[0..2].copy_from_slice(&self.slave.to_ne_bytes());
        b[2..4].copy_from_slice(&self.flags.to_ne_bytes());
        b[4..6].copy_from_slice(&self.len.to_ne_bytes());
        b[8..16].copy_from_slice(&self.buf.to_ne_bytes());
        b
    }
}

impl IicCmd {
    /// The structure in an ioctl argument, or `None` if it is not one.
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() != IICCMD_LEN {
            return None;
        }
        Some(Self {
            slave: b[0],
            count: i32_at(b, 4),
            last: i32_at(b, 8),
            buf: u64_at(b, 16),
        })
    }
}

impl IicRdwrData {
    /// The structure in an ioctl argument, or `None` if it is not one.
    #[must_use]
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        if b.len() != IIC_RDWR_DATA_LEN {
            return None;
        }
        Some(Self {
            msgs: u64_at(b, 0),
            nmsgs: u32::from_ne_bytes([b[8], b[9], b[10], b[11]]),
        })
    }
}

// Every length is a few dozen bytes, well within `IOCPARM_MASK`.
#[allow(clippy::cast_possible_truncation)]
const fn ioc(dir: u32, num: u32, len: usize) -> u32 {
    dir | ((len as u32) << 16) | (b'i' as u32) << 8 | num
}

/// Sends a START to `slave` and takes the bus.
pub const I2CSTART: u32 = ioc(IOC_IN, 1, IICCMD_LEN);
/// Sends a STOP and lets the bus go.
pub const I2CSTOP: u32 = ioc(IOC_VOID, 2, 0);
/// Resets the controller.
pub const I2CRSTCARD: u32 = ioc(IOC_IN, 3, IICCMD_LEN);
/// Writes after `I2CSTART`.
pub const I2CWRITE: u32 = ioc(IOC_IN, 4, IICCMD_LEN);
/// Reads after `I2CSTART`.
pub const I2CREAD: u32 = ioc(IOC_IN, 5, IICCMD_LEN);
/// A whole transfer of messages.
pub const I2CRDWR: u32 = ioc(IOC_IN, 6, IIC_RDWR_DATA_LEN);
/// A repeated START after `I2CSTART`.
pub const I2CRPTSTART: u32 = ioc(IOC_IN, 7, IICCMD_LEN);
/// Sets the address `read(2)` and `write(2)` use.
pub const I2CSADDR: u32 = ioc(IOC_IN, 8, 1);

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("I2CSTART", I2CSTART as i64),
    c("I2CSTOP", I2CSTOP as i64),
    c("I2CRSTCARD", I2CRSTCARD as i64),
    c("I2CWRITE", I2CWRITE as i64),
    c("I2CREAD", I2CREAD as i64),
    c("I2CRDWR", I2CRDWR as i64),
    c("I2CRPTSTART", I2CRPTSTART as i64),
    c("I2CSADDR", I2CSADDR as i64),
    c("IIC_M_WR", IIC_M_WR as i64),
    c("IIC_M_RD", IIC_M_RD as i64),
    c("IIC_M_NOSTOP", IIC_M_NOSTOP as i64),
    c("IIC_M_NOSTART", IIC_M_NOSTART as i64),
    c("IIC_RDRW_MAX_MSGS", IIC_RDRW_MAX_MSGS as i64),
];

const fn c(c_name: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header: "dev/iicbus/iic.h",
        value,
        arches: Arches::All,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_freebsds() {
        // _IOW('i', n, struct iiccmd) on LP64, and the others.
        assert_eq!(I2CSTART, 0x8018_6901);
        assert_eq!(I2CSTOP, 0x2000_6902);
        assert_eq!(I2CRDWR, 0x8010_6906);
        assert_eq!(I2CSADDR, 0x8001_6908);
    }

    #[test]
    fn structures_round_trip() {
        let m = IicMsg {
            slave: 0xa0,
            flags: IIC_M_RD | IIC_M_NOSTOP,
            len: 300,
            buf: 0x1234_5678_9abc,
        };
        assert_eq!(IicMsg::from_bytes(&m.to_bytes()), Some(m));
        assert_eq!(IicMsg::from_bytes(&[0; 15]), None);
        let mut b = [0u8; IICCMD_LEN];
        b[0] = 0x64;
        b[4..8].copy_from_slice(&7i32.to_ne_bytes());
        b[8..12].copy_from_slice(&1i32.to_ne_bytes());
        b[16..24].copy_from_slice(&0x4000u64.to_ne_bytes());
        assert_eq!(
            IicCmd::from_bytes(&b),
            Some(IicCmd {
                slave: 0x64,
                count: 7,
                last: 1,
                buf: 0x4000
            })
        );
        let mut d = [0u8; IIC_RDWR_DATA_LEN];
        d[0..8].copy_from_slice(&0x8000u64.to_ne_bytes());
        d[8..12].copy_from_slice(&2u32.to_ne_bytes());
        assert_eq!(
            IicRdwrData::from_bytes(&d),
            Some(IicRdwrData {
                msgs: 0x8000,
                nmsgs: 2
            })
        );
    }
}
