// SPDX-License-Identifier: BSD-2-Clause
//! The NVMe control devices (`dev/nvme/nvme.h`): the requests
//! `nvmecontrol(8)` and smartmontools send to `/dev/nvmeN` and
//! `/dev/nvmeNnsM`, and the command and completion they pass.
//!
//! Pointers are user addresses (`u64`); C's padding is spelled out.
//!
//! Behavior reference: sys/dev/nvme/nvme.h.

use crate::ioctl::{IOC_IN, IOC_OUT, IOC_VOID, ioc, ioc_of};
use crate::layout::{Arches, ConstValue};

/// `NVME_CONTROLLER_DATA` and namespace data: an identify page.
pub const IDENTIFY_SIZE: usize = 4096;

/// `struct nvme_command`: a submission queue entry.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NvmeCommand {
    pub opc: u8,
    pub fuse: u8,
    pub cid: u16,
    pub nsid: u32,
    pub rsvd2: u32,
    pub rsvd3: u32,
    pub mptr: u64,
    pub prp1: u64,
    pub prp2: u64,
    pub cdw10: u32,
    pub cdw11: u32,
    pub cdw12: u32,
    pub cdw13: u32,
    pub cdw14: u32,
    pub cdw15: u32,
}

/// `struct nvme_completion`: a completion queue entry.
#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NvmeCompletion {
    pub cdw0: u32,
    pub rsvd1: u32,
    pub sqhd: u16,
    pub sqid: u16,
    pub cid: u16,
    pub status: u16,
}

/// `struct nvme_pt_command` (`NVME_PASSTHROUGH_CMD`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NvmePtCommand {
    pub cmd: NvmeCommand,
    pub cpl: NvmeCompletion,
    pub buf: u64,
    pub len: u32,
    pub is_read: u32,
    pub driver_lock: u64,
}

/// `SPECNAMELEN + 1`.
pub const CDEV_NAME_LEN: usize = 256;

/// `struct nvme_get_nsid` (`NVME_GET_NSID`).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NvmeGetNsid {
    pub cdev: [u8; CDEV_NAME_LEN],
    pub nsid: u32,
}

impl Default for NvmeGetNsid {
    fn default() -> Self {
        Self {
            cdev: [0; CDEV_NAME_LEN],
            nsid: 0,
        }
    }
}

/// `struct nvme_io_test` (`NVME_IO_TEST`, `NVME_BIO_TEST`: `nvmecontrol
/// perftest`).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NvmeIoTest {
    pub opc: u32,
    pub size: u32,
    pub time: u32,
    pub num_threads: u32,
    pub flags: u32,
    pub _pad0: [u8; 4],
    pub io_completed: [u64; 128],
}

pub const NVME_PASSTHROUGH_CMD: u32 = ioc_of(IOC_IN | IOC_OUT, b'n', 0, size_of::<NvmePtCommand>());
pub const NVME_RESET_CONTROLLER: u32 = ioc(IOC_VOID, b'n', 1, 0);
pub const NVME_GET_NSID: u32 = ioc_of(IOC_OUT, b'n', 2, size_of::<NvmeGetNsid>());
pub const NVME_GET_MAX_XFER_SIZE: u32 = ioc(IOC_OUT, b'n', 3, 8);
pub const NVME_GET_CONTROLLER_DATA: u32 = ioc_of(IOC_OUT, b'n', 4, IDENTIFY_SIZE);
pub const NVME_IO_TEST: u32 = ioc_of(IOC_IN | IOC_OUT, b'n', 100, size_of::<NvmeIoTest>());
pub const NVME_BIO_TEST: u32 = ioc_of(IOC_IN | IOC_OUT, b'n', 101, size_of::<NvmeIoTest>());

/// The completion status's phase bit (`NVME_STATUS_P`), which a
/// passthrough command's status leaves out.
pub const STATUS_P: u16 = 1;

const _: () = {
    assert!(size_of::<NvmeCommand>() == 64);
    assert!(size_of::<NvmeCompletion>() == 16);
    assert!(size_of::<NvmePtCommand>() == 104);
    assert!(size_of::<NvmeGetNsid>() == 260);
    assert!(size_of::<NvmeIoTest>() == 1048);
    assert!(NVME_PASSTHROUGH_CMD == 0xc068_6e00);
    assert!(NVME_RESET_CONTROLLER == 0x2000_6e01);
    assert!(NVME_GET_NSID == 0x4104_6e02);
    assert!(NVME_GET_MAX_XFER_SIZE == 0x4008_6e03);
    assert!(NVME_GET_CONTROLLER_DATA == 0x5000_6e04);
    assert!(NVME_IO_TEST == 0xc418_6e64);
};

const fn c(c_name: &'static str, value: u32) -> ConstValue {
    ConstValue {
        c_name,
        header: "dev/nvme/nvme.h",
        value: value as i64,
        arches: Arches::All,
    }
}

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("NVME_PASSTHROUGH_CMD", NVME_PASSTHROUGH_CMD),
    c("NVME_RESET_CONTROLLER", NVME_RESET_CONTROLLER),
    c("NVME_GET_NSID", NVME_GET_NSID),
    c("NVME_GET_MAX_XFER_SIZE", NVME_GET_MAX_XFER_SIZE),
    c("NVME_GET_CONTROLLER_DATA", NVME_GET_CONTROLLER_DATA),
    c("NVME_IO_TEST", NVME_IO_TEST),
    c("NVME_BIO_TEST", NVME_BIO_TEST),
];
