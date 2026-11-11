// SPDX-License-Identifier: BSD-2-Clause
//! The pass-through requests a SIM runs: a SCSI CDB, an ATA command, or
//! an NVMe command, with its data in a kernel buffer (copied in before,
//! and out after, by the pass device).

use alloc::vec::Vec;

use abi::cam::{AtaCmd, AtaRes, flags};
use abi::nvme::{NvmeCommand, NvmeCompletion};

/// Which way a request's data moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    /// No data (`CAM_DIR_NONE`).
    None,
    /// From the device (`CAM_DIR_IN`).
    In,
    /// To the device (`CAM_DIR_OUT`).
    Out,
    /// Both (`CAM_DIR_BOTH`).
    Both,
}

impl Dir {
    /// From a CCB's flags.
    #[must_use]
    pub fn of(f: u32) -> Self {
        match f & flags::DIR_MASK {
            flags::DIR_NONE => Self::None,
            flags::DIR_IN => Self::In,
            flags::DIR_OUT => Self::Out,
            _ => Self::Both,
        }
    }

    /// Whether the data is copied in before the request runs.
    #[must_use]
    pub fn copies_in(self) -> bool {
        matches!(self, Self::Out | Self::Both)
    }

    /// Whether the data is copied out after it.
    #[must_use]
    pub fn copies_out(self) -> bool {
        matches!(self, Self::In | Self::Both)
    }
}

/// `XPT_SCSI_IO`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScsiIo {
    /// The CDB.
    pub cdb: Vec<u8>,
    /// Its data's direction.
    pub dir: Dir,
    /// The data: what is sent, or room for what comes back.
    pub data: Vec<u8>,
    /// The sense data wanted, at most (`sense_len`).
    pub sense_len: u8,
    /// The timeout, in milliseconds (0 for the SIM's default).
    pub timeout_ms: u32,
    /// The SCSI status the device returned.
    pub scsi_status: u8,
    /// Sense data, if the SIM fetched it (`CAM_AUTOSNS_VALID`).
    pub sense: Vec<u8>,
    /// Bytes not moved.
    pub resid: u32,
}

impl ScsiIo {
    /// A request with nothing back yet.
    #[must_use]
    pub fn new(cdb: Vec<u8>, dir: Dir, data: Vec<u8>, sense_len: u8) -> Self {
        Self {
            cdb,
            dir,
            data,
            sense_len,
            timeout_ms: 0,
            scsi_status: 0,
            sense: Vec::new(),
            resid: 0,
        }
    }
}

/// `XPT_ATA_IO`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AtaIo {
    /// The command's registers.
    pub cmd: AtaCmd,
    /// Its data's direction.
    pub dir: Dir,
    /// The data.
    pub data: Vec<u8>,
    /// The timeout, in milliseconds (0 for the SIM's default).
    pub timeout_ms: u32,
    /// The registers it ended with (filled on an error, or when the
    /// command asks for them: `CAM_ATAIO_NEEDRESULT`).
    pub res: AtaRes,
    /// Bytes not moved.
    pub resid: u32,
}

/// `XPT_NVME_IO` and `XPT_NVME_ADMIN`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NvmeIo {
    /// The command (its namespace already the path's, for I/O).
    pub cmd: NvmeCommand,
    /// Its data's direction.
    pub dir: Dir,
    /// The data.
    pub data: Vec<u8>,
    /// The completion.
    pub cpl: NvmeCompletion,
}
