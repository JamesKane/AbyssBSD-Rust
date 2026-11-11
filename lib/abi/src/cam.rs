// SPDX-License-Identifier: BSD-2-Clause
//! CAM's user interface (`cam/cam.h`, `cam/cam_ccb.h`,
//! `cam/scsi/scsi_pass.h`): the CAM control block (`union ccb`) that
//! `/dev/xpt0` and `/dev/passN` take with `CAMIOCOMMAND` and
//! `CAMGETPASSTHRU`, the members of it the kernel reads and writes, and
//! the patterns and results of an `XPT_DEV_MATCH`.
//!
//! - The padding C leaves between members is spelled out as `_pad`
//!   fields, so every structure here is plain bytes. Pointers are user
//!   addresses (`u64`); unions are byte arrays, their members structures
//!   of their own.
//! - A CCB arrives as [`CCB_SIZE`] bytes; its members are read and
//!   written in place at the offsets these structures give (`offset_of!`),
//!   which `abi-check` holds to FreeBSD's.
//!
//! Behavior reference: sys/cam/cam.h, sys/cam/cam_ccb.h,
//! sys/cam/scsi/scsi_pass.h, sys/cam/ata/ata_all.h.

use crate::ioctl::{IOC_IN, IOC_OUT, ioc_of};
use crate::layout::{Arches, ConstValue};

/// `sizeof(union ccb)`, the same on amd64 and arm64.
pub const CCB_SIZE: usize = 1248;

/// `CAM_VERSION`, the requests' group.
pub const CAM_VERSION: u8 = 0x1a;

/// `CAMIOCOMMAND`: run a CCB.
pub const CAMIOCOMMAND: u32 = ioc_of(IOC_IN | IOC_OUT, CAM_VERSION, 2, CCB_SIZE);
/// `CAMGETPASSTHRU`: the `pass` device of a peripheral.
pub const CAMGETPASSTHRU: u32 = ioc_of(IOC_IN | IOC_OUT, CAM_VERSION, 3, CCB_SIZE);

/// `CAM_XPT_PATH_ID`: the transport layer's own bus.
pub const CAM_XPT_PATH_ID: u32 = u32::MAX;
/// `CAM_TARGET_WILDCARD`.
pub const CAM_TARGET_WILDCARD: u32 = u32::MAX;
/// `CAM_LUN_WILDCARD` (an `u_int` widened to the 64-bit LUN).
pub const CAM_LUN_WILDCARD: u64 = u32::MAX as u64;

/// `xpt_opcode`: what a CCB asks for.
pub mod func {
    /// `XPT_FC_QUEUED`.
    pub const FC_QUEUED: u32 = 0x100;
    /// `XPT_FC_USER_CCB`.
    pub const FC_USER_CCB: u32 = 0x200;
    /// `XPT_FC_XPT_ONLY`: only through `/dev/xpt0`.
    pub const FC_XPT_ONLY: u32 = 0x400;
    /// `XPT_FC_DEV_QUEUED`.
    pub const FC_DEV_QUEUED: u32 = 0x800 | FC_QUEUED;

    pub const NOOP: u32 = 0x00;
    pub const SCSI_IO: u32 = 0x01 | FC_DEV_QUEUED;
    pub const GDEV_TYPE: u32 = 0x02;
    pub const GDEVLIST: u32 = 0x03;
    pub const PATH_INQ: u32 = 0x04;
    pub const REL_SIMQ: u32 = 0x05;
    pub const SASYNC_CB: u32 = 0x06;
    pub const SDEV_TYPE: u32 = 0x07;
    pub const SCAN_BUS: u32 = 0x08 | FC_QUEUED | FC_USER_CCB | FC_XPT_ONLY;
    pub const DEV_MATCH: u32 = 0x09 | FC_XPT_ONLY;
    pub const DEBUG: u32 = 0x0a;
    pub const PATH_STATS: u32 = 0x0b;
    pub const GDEV_STATS: u32 = 0x0c;
    pub const DEV_ADVINFO: u32 = 0x0e;
    pub const ASYNC: u32 = 0x0f | FC_QUEUED | FC_USER_CCB | FC_XPT_ONLY;
    pub const ABORT: u32 = 0x10;
    pub const RESET_BUS: u32 = 0x11 | FC_XPT_ONLY;
    pub const RESET_DEV: u32 = 0x12 | FC_DEV_QUEUED;
    pub const TERM_IO: u32 = 0x13;
    pub const SCAN_LUN: u32 = 0x14 | FC_QUEUED | FC_USER_CCB | FC_XPT_ONLY;
    pub const GET_TRAN_SETTINGS: u32 = 0x15;
    pub const SET_TRAN_SETTINGS: u32 = 0x16;
    pub const CALC_GEOMETRY: u32 = 0x17;
    pub const ATA_IO: u32 = 0x18 | FC_DEV_QUEUED;
    pub const SMP_IO: u32 = 0x1b | FC_DEV_QUEUED;
    pub const NVME_IO: u32 = 0x1c | FC_DEV_QUEUED;
    pub const MMC_IO: u32 = 0x1d | FC_DEV_QUEUED;
    pub const SCAN_TGT: u32 = 0x1e | FC_QUEUED | FC_USER_CCB | FC_XPT_ONLY;
    pub const NVME_ADMIN: u32 = 0x1f | FC_DEV_QUEUED;
    pub const ENG_INQ: u32 = 0x20 | FC_XPT_ONLY;
}

/// `cam_status`: how a CCB ended.
pub mod status {
    pub const REQ_INPROG: u32 = 0x00;
    pub const REQ_CMP: u32 = 0x01;
    pub const REQ_ABORTED: u32 = 0x02;
    pub const REQ_CMP_ERR: u32 = 0x04;
    pub const BUSY: u32 = 0x05;
    pub const REQ_INVALID: u32 = 0x06;
    pub const PATH_INVALID: u32 = 0x07;
    pub const DEV_NOT_THERE: u32 = 0x08;
    pub const SEL_TIMEOUT: u32 = 0x0a;
    pub const CMD_TIMEOUT: u32 = 0x0b;
    pub const SCSI_STATUS_ERROR: u32 = 0x0c;
    pub const AUTOSENSE_FAIL: u32 = 0x10;
    pub const DATA_RUN_ERR: u32 = 0x12;
    pub const REQ_TOO_BIG: u32 = 0x1a;
    pub const ATA_STATUS_ERROR: u32 = 0x1c;
    pub const NVME_STATUS_ERROR: u32 = 0x20;
    pub const RESRC_UNAVAIL: u32 = 0x34;
    pub const FUNC_NOTAVAIL: u32 = 0x3a;
    /// `CAM_DEV_QFRZN`: the device's queue froze on the error.
    pub const DEV_QFRZN: u32 = 0x40;
    /// `CAM_AUTOSNS_VALID`: sense data was fetched.
    pub const AUTOSNS_VALID: u32 = 0x80;
    /// `CAM_QOS_VALID`: a queued request's time in the SIM is recorded.
    pub const QOS_VALID: u32 = 0x400;
    /// `CAM_STATUS_MASK`.
    pub const MASK: u32 = 0x3f;
}

/// `ccb_flags`, the header's `flags`.
pub mod flags {
    pub const CDB_POINTER: u32 = 0x0000_0001;
    pub const DIR_IN: u32 = 0x0000_0040;
    pub const DIR_OUT: u32 = 0x0000_0080;
    pub const DIR_NONE: u32 = 0x0000_00c0;
    pub const DIR_MASK: u32 = 0x0000_00c0;
    /// `CAM_DATA_VADDR`: the data pointer is a (user) virtual address.
    pub const DATA_VADDR: u32 = 0;
    pub const DATA_MASK: u32 = 0x0024_0010;
    pub const DEV_QFRZDIS: u32 = 0x0000_0400;
    pub const PASS_ERR_RECOVER: u32 = 0x0001_0000;
    pub const UNLOCKED: u32 = 0x8000_0000;
}

/// `cam_proto`.
pub mod proto {
    pub const UNKNOWN: u32 = 0;
    pub const UNSPECIFIED: u32 = 1;
    pub const SCSI: u32 = 2;
    pub const ATA: u32 = 3;
    pub const NVME: u32 = 7;
}

/// `cam_xport`.
pub mod xport {
    pub const UNKNOWN: u32 = 0;
    pub const UNSPECIFIED: u32 = 1;
    pub const USB: u32 = 5;
    pub const SATA: u32 = 9;
    pub const NVME: u32 = 12;
}

/// `PROTO_VERSION_UNSPECIFIED` and `XPORT_VERSION_UNSPECIFIED`.
pub const VERSION_UNSPECIFIED: u32 = u32::MAX;

/// `pi_miscflag`, `hba_misc`.
pub mod pim {
    pub const ATA_EXT: u32 = 0x200;
    pub const NO_6_BYTE: u32 = 0x08;
    pub const SEQSCAN: u32 = 0x04;
    pub const UNMAPPED: u32 = 0x02;
    pub const NOSCAN: u32 = 0x01;
}

/// `pi_inqflag`, `hba_inquiry`.
pub mod pi {
    pub const SDTR_ABLE: u8 = 0x10;
    pub const SATAPM: u8 = 0x04;
    pub const TAG_ABLE: u8 = 0x02;
}

/// `dev_match_type`.
pub mod match_type {
    pub const PERIPH: u32 = 0;
    pub const DEVICE: u32 = 1;
    pub const BUS: u32 = 2;
}

/// `periph_pattern_flags`.
pub mod periph_match {
    pub const ANY: u32 = 0x000;
    pub const PATH: u32 = 0x001;
    pub const TARGET: u32 = 0x002;
    pub const LUN: u32 = 0x004;
    pub const NAME: u32 = 0x008;
    pub const UNIT: u32 = 0x010;
}

/// `dev_pattern_flags`.
pub mod dev_match {
    pub const ANY: u32 = 0x000;
    pub const PATH: u32 = 0x001;
    pub const TARGET: u32 = 0x002;
    pub const LUN: u32 = 0x004;
    pub const INQUIRY: u32 = 0x008;
    pub const DEVID: u32 = 0x010;
}

/// `bus_pattern_flags`.
pub mod bus_match {
    pub const ANY: u32 = 0x000;
    pub const PATH: u32 = 0x001;
    pub const NAME: u32 = 0x002;
    pub const UNIT: u32 = 0x004;
    pub const BUS_ID: u32 = 0x008;
}

/// `dev_result_flags`.
pub const DEV_RESULT_UNCONFIGURED: u32 = 0x01;

/// `ccb_dev_match_status`.
pub mod dm_status {
    pub const LAST: u32 = 0;
    pub const MORE: u32 = 1;
    pub const LIST_CHANGED: u32 = 2;
    pub const SIZE_ERROR: u32 = 3;
    pub const ERROR: u32 = 4;
}

/// `dev_pos_type`.
pub mod dev_pos {
    pub const NONE: u32 = 0x000;
    pub const BUS: u32 = 0x001;
    pub const TARGET: u32 = 0x002;
    pub const DEVICE: u32 = 0x004;
    pub const PERIPH: u32 = 0x008;
    pub const PDPTR: u32 = 0x010;
    pub const EDT: u32 = 0x100;
    pub const PDRV: u32 = 0x200;
}

/// `ccb_getdevlist_status_e`.
pub mod gdevlist {
    pub const LAST_DEVICE: u32 = 0;
    pub const LIST_CHANGED: u32 = 1;
    pub const MORE_DEVS: u32 = 2;
    pub const ERROR: u32 = 3;
}

/// `ccb_dev_advinfo`'s flags and buffer types.
pub mod cdai {
    pub const FLAG_STORE: u32 = 0x1;
    pub const TYPE_SCSI_DEVID: u32 = 1;
    pub const TYPE_SERIAL_NUM: u32 = 2;
    pub const TYPE_PHYS_PATH: u32 = 3;
    pub const TYPE_RCAPLONG: u32 = 4;
    pub const TYPE_EXT_INQ: u32 = 5;
    pub const TYPE_NVME_CNTRL: u32 = 6;
    pub const TYPE_NVME_NS: u32 = 7;
    pub const TYPE_MMC_PARAMS: u32 = 8;
}

/// `cts_type`, and the transfer settings' `valid` bits.
pub mod cts {
    pub const TYPE_CURRENT_SETTINGS: u32 = 0;
    pub const TYPE_USER_SETTINGS: u32 = 1;
    pub const SCSI_VALID_TQ: u32 = 0x01;
    pub const ATA_VALID_TQ: u32 = 0x01;
    pub const SATA_VALID_MODE: u32 = 0x01;
    pub const SATA_VALID_BYTECOUNT: u32 = 0x02;
    pub const SATA_VALID_REVISION: u32 = 0x04;
    pub const SATA_VALID_PM: u32 = 0x08;
    pub const SATA_VALID_TAGS: u32 = 0x10;
    pub const SATA_VALID_ATAPI: u32 = 0x20;
    pub const SATA_VALID_CAPS: u32 = 0x40;
    pub const SATA_CAPS_H_DMAAA: u32 = 0x0000_0010;
    pub const SATA_CAPS_H_AN: u32 = 0x0000_0020;
    pub const NVME_VALID_SPEC: u32 = 0x01;
    pub const NVME_VALID_LINK: u32 = 0x04;
}

/// `struct ata_cmd`'s flags.
pub mod ataio {
    pub const BIT48: u8 = 0x01;
    pub const FPDMA: u8 = 0x02;
    pub const CONTROL: u8 = 0x04;
    pub const NEEDRESULT: u8 = 0x08;
    pub const DMA: u8 = 0x10;
}

/// `DEV_IDLEN`, `SIM_IDLEN`, `HBA_IDLEN`.
pub const DEV_IDLEN: usize = 16;

/// `struct ccb_hdr`: what every CCB starts with.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CcbHdr {
    pub pinfo: [u8; 12],
    pub _pad0: [u8; 4],
    pub xpt_links: [u64; 2],
    pub sim_links: [u64; 2],
    pub periph_links: [u64; 2],
    pub retry_count: u16,
    pub alloc_flags: u16,
    pub _pad1: [u8; 4],
    pub cbfcnp: u64,
    pub func_code: u32,
    pub status: u32,
    pub path: u64,
    pub path_id: u32,
    pub target_id: u32,
    pub target_lun: u64,
    pub flags: u32,
    pub xflags: u32,
    pub periph_priv: [u64; 2],
    pub sim_priv: [u64; 2],
    pub qos: [u64; 3],
    pub timeout: u32,
    pub _pad2: [u8; 4],
    pub softtimeout: [u64; 2],
}

/// `struct ccb_getdev` (`XPT_GDEV_TYPE`), the largest member: the size
/// of the whole CCB.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CcbGetdev {
    pub ccb_h: CcbHdr,
    pub protocol: u32,
    pub inq_data: [u8; 256],
    pub ident_data: [u8; 512],
    pub serial_num: [u8; 252],
    pub inq_flags: u8,
    pub serial_num_len: u8,
    pub _pad0: [u8; 6],
    pub padding: [u64; 2],
}

/// `struct ccb_getdevlist` (`XPT_GDEVLIST`, `CAMGETPASSTHRU`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CcbGetdevlist {
    pub ccb_h: CcbHdr,
    pub periph_name: [u8; DEV_IDLEN],
    pub unit_number: u32,
    pub generation: u32,
    pub index: u32,
    pub status: u32,
}

/// `struct ccb_pathinq` (`XPT_PATH_INQ`).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CcbPathinq {
    pub ccb_h: CcbHdr,
    pub version_num: u8,
    pub hba_inquiry: u8,
    pub target_sprt: u16,
    pub hba_misc: u32,
    pub hba_eng_cnt: u16,
    pub vuhba_flags: [u8; 14],
    pub max_target: u32,
    pub max_lun: u32,
    pub async_flags: u32,
    pub hpath_id: u32,
    pub initiator_id: u32,
    pub sim_vid: [u8; 16],
    pub hba_vid: [u8; 16],
    pub dev_name: [u8; DEV_IDLEN],
    pub unit_number: u32,
    pub bus_id: u32,
    pub base_transfer_speed: u32,
    pub protocol: u32,
    pub protocol_version: u32,
    pub transport: u32,
    pub transport_version: u32,
    pub xport_specific: [u8; 128],
    pub maxio: u32,
    pub hba_vendor: u16,
    pub hba_device: u16,
    pub hba_subvendor: u16,
    pub hba_subdevice: u16,
    pub _pad0: [u8; 4],
}

/// `struct ccb_pathinq_settings_nvme`: a path inquiry's NVMe part.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathinqNvme {
    pub nsid: u32,
    pub domain: u32,
    pub bus: u8,
    pub slot: u8,
    pub function: u8,
    pub extra: u8,
    pub dev_name: [u8; 52],
}

/// `struct ccb_dev_position`: where a `XPT_DEV_MATCH` stopped.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CcbDevPosition {
    pub generations: [u32; 4],
    pub position_type: u32,
    pub _pad0: [u8; 4],
    /// `cookie`: bus, target, device, peripheral, and peripheral driver.
    pub cookie: [u64; 5],
}

/// `struct ccb_dev_match` (`XPT_DEV_MATCH`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CcbDevMatch {
    pub ccb_h: CcbHdr,
    pub status: u32,
    pub num_patterns: u32,
    pub pattern_buf_len: u32,
    pub _pad0: [u8; 4],
    pub patterns: u64,
    pub num_matches: u32,
    pub match_buf_len: u32,
    pub matches: u64,
    pub pos: CcbDevPosition,
}

/// `struct ccb_trans_settings` (`XPT_GET_TRAN_SETTINGS`).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CcbTransSettings {
    pub ccb_h: CcbHdr,
    pub type_: u32,
    pub protocol: u32,
    pub protocol_version: u32,
    pub transport: u32,
    pub transport_version: u32,
    pub proto_specific: [u8; 64],
    pub _pad0: [u8; 4],
    pub xport_specific: [u8; 32],
}

/// `struct ccb_trans_settings_sata`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransSettingsSata {
    pub valid: u32,
    pub mode: i32,
    pub bytecount: u32,
    pub revision: i32,
    pub pm_present: u32,
    pub tags: u32,
    pub atapi: u32,
    pub caps: u32,
}

/// `struct ccb_trans_settings_nvme`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransSettingsNvme {
    pub valid: u32,
    pub spec: u32,
    pub max_xfer: u32,
    pub caps: u32,
    pub lanes: u8,
    pub speed: u8,
    pub max_lanes: u8,
    pub max_speed: u8,
}

/// `struct ccb_trans_settings_scsi` (and `_ata`, the same shape).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransSettingsScsi {
    pub valid: u32,
    pub flags: u32,
}

/// `struct ccb_scsiio` (`XPT_SCSI_IO`).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CcbScsiio {
    pub ccb_h: CcbHdr,
    pub next_ccb: u64,
    pub req_map: u64,
    pub data_ptr: u64,
    pub dxfer_len: u32,
    pub sense_data: [u8; 252],
    pub sense_len: u8,
    pub cdb_len: u8,
    pub sglist_cnt: u16,
    pub scsi_status: u8,
    pub sense_resid: u8,
    pub _pad0: [u8; 2],
    pub resid: u32,
    pub _pad1: [u8; 4],
    /// `cdb_io`: the CDB's bytes, or (`CAM_CDB_POINTER`) its address.
    pub cdb_io: [u8; 16],
    pub msg_ptr: u64,
    pub msg_len: u16,
    pub tag_action: u8,
    pub priority: u8,
    pub tag_id: u32,
    pub init_id: u32,
    pub _pad2: [u8; 4],
}

/// `struct ata_cmd`: the registers an ATA command is sent with.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AtaCmd {
    pub flags: u8,
    pub command: u8,
    pub features: u8,
    pub lba_low: u8,
    pub lba_mid: u8,
    pub lba_high: u8,
    pub device: u8,
    pub lba_low_exp: u8,
    pub lba_mid_exp: u8,
    pub lba_high_exp: u8,
    pub features_exp: u8,
    pub sector_count: u8,
    pub sector_count_exp: u8,
    pub control: u8,
}

/// `struct ata_res`: the registers an ATA command ended with.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AtaRes {
    pub flags: u8,
    pub status: u8,
    pub error: u8,
    pub lba_low: u8,
    pub lba_mid: u8,
    pub lba_high: u8,
    pub device: u8,
    pub lba_low_exp: u8,
    pub lba_mid_exp: u8,
    pub lba_high_exp: u8,
    pub sector_count: u8,
    pub sector_count_exp: u8,
}

/// `struct ccb_ataio` (`XPT_ATA_IO`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CcbAtaio {
    pub ccb_h: CcbHdr,
    pub next_ccb: u64,
    pub cmd: AtaCmd,
    pub res: AtaRes,
    pub _pad0: [u8; 6],
    pub data_ptr: u64,
    pub dxfer_len: u32,
    pub resid: u32,
    pub ata_flags: u8,
    pub icc: u8,
    pub _pad1: [u8; 2],
    pub aux: u32,
    pub unused: u32,
    pub _pad2: [u8; 4],
}

/// `struct ccb_nvmeio` (`XPT_NVME_IO`, `XPT_NVME_ADMIN`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CcbNvmeio {
    pub ccb_h: CcbHdr,
    pub next_ccb: u64,
    pub cmd: crate::nvme::NvmeCommand,
    pub cpl: crate::nvme::NvmeCompletion,
    pub data_ptr: u64,
    pub dxfer_len: u32,
    pub sglist_cnt: u16,
    pub unused: u16,
}

/// `struct ccb_dev_advinfo` (`XPT_DEV_ADVINFO`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CcbDevAdvinfo {
    pub ccb_h: CcbHdr,
    pub flags: u32,
    pub buftype: u32,
    pub bufsiz: i64,
    pub provsiz: i64,
    pub buf: u64,
}

/// `struct ccb_getdevstats` (`XPT_GDEV_STATS`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CcbGetdevstats {
    pub ccb_h: CcbHdr,
    pub dev_openings: i32,
    pub dev_active: i32,
    pub allocated: i32,
    pub queued: i32,
    pub held: i32,
    pub maxtags: i32,
    pub mintags: i32,
    pub _pad0: [u8; 4],
    pub last_reset: [u64; 2],
}

/// `struct dev_match_pattern`: a pattern's type, then its member of
/// `union match_pattern` ([`PeriphMatchPattern`], [`DeviceMatchPattern`],
/// [`BusMatchPattern`]).
#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DevMatchPattern {
    pub type_: u32,
    pub _pad0: [u8; 4],
    pub pattern: [u8; 280],
}

/// `struct periph_match_pattern`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PeriphMatchPattern {
    pub periph_name: [u8; DEV_IDLEN],
    pub unit_number: u32,
    pub path_id: u32,
    pub target_id: u32,
    pub _pad0: [u8; 4],
    pub target_lun: u64,
    pub flags: u32,
    pub _pad1: [u8; 4],
}

/// `struct device_match_pattern`; `data` is the union of an inquiry
/// pattern (`struct scsi_static_inquiry_pattern`) and a device ID.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceMatchPattern {
    pub path_id: u32,
    pub target_id: u32,
    pub target_lun: u64,
    pub flags: u32,
    pub data: [u8; 257],
    pub _pad0: [u8; 3],
}

/// `struct scsi_static_inquiry_pattern`: what `DEV_MATCH_INQUIRY`
/// compares, `*` and `?` matching as in a shell.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScsiStaticInquiryPattern {
    pub type_: u8,
    pub media_type: u8,
    pub vendor: [u8; 9],
    pub product: [u8; 17],
    pub revision: [u8; 5],
}

/// `struct bus_match_pattern`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BusMatchPattern {
    pub path_id: u32,
    pub dev_name: [u8; DEV_IDLEN],
    pub unit_number: u32,
    pub bus_id: u32,
    pub flags: u32,
}

/// `struct dev_match_result`: a result's type, then its member of
/// `union match_result`.
#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DevMatchResult {
    pub type_: u32,
    pub _pad0: [u8; 4],
    pub result: [u8; 792],
}

/// `struct periph_match_result`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PeriphMatchResult {
    pub periph_name: [u8; DEV_IDLEN],
    pub unit_number: u32,
    pub path_id: u32,
    pub target_id: u32,
    pub _pad0: [u8; 4],
    pub target_lun: u64,
}

/// `struct device_match_result`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceMatchResult {
    pub path_id: u32,
    pub target_id: u32,
    pub target_lun: u64,
    pub protocol: u32,
    pub inq_data: [u8; 256],
    pub ident_data: [u8; 512],
    pub flags: u32,
}

/// `struct bus_match_result`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BusMatchResult {
    pub path_id: u32,
    pub dev_name: [u8; DEV_IDLEN],
    pub unit_number: u32,
    pub bus_id: u32,
}

/// `union ccb`, as bytes: [`CCB_SIZE`] of them, 8-aligned.
#[repr(C, align(8))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ccb {
    pub bytes: [u8; CCB_SIZE],
}

impl Default for Ccb {
    fn default() -> Self {
        Self {
            bytes: [0; CCB_SIZE],
        }
    }
}

const _: () = {
    assert!(size_of::<CcbHdr>() == 200);
    assert!(size_of::<CcbGetdev>() == CCB_SIZE);
    assert!(size_of::<CcbGetdevlist>() == 232);
    assert!(size_of::<CcbPathinq>() == 464);
    assert!(size_of::<PathinqNvme>() == 64);
    assert!(size_of::<CcbDevMatch>() == 304);
    assert!(size_of::<CcbTransSettings>() == 320);
    assert!(size_of::<CcbScsiio>() == 536);
    assert!(size_of::<CcbAtaio>() == 272);
    assert!(size_of::<CcbNvmeio>() == 304);
    assert!(size_of::<CcbDevAdvinfo>() == 232);
    assert!(size_of::<CcbGetdevstats>() == 248);
    assert!(size_of::<DevMatchPattern>() == 288);
    assert!(size_of::<PeriphMatchPattern>() == 48);
    assert!(size_of::<DeviceMatchPattern>() == 280);
    assert!(size_of::<BusMatchPattern>() == 32);
    assert!(size_of::<DevMatchResult>() == 800);
    assert!(size_of::<PeriphMatchResult>() == 40);
    assert!(size_of::<DeviceMatchResult>() == 792);
    assert!(size_of::<BusMatchResult>() == 28);
    assert!(size_of::<Ccb>() == CCB_SIZE);
    assert!(CAMIOCOMMAND == 0xc4e0_1a02);
    assert!(CAMGETPASSTHRU == 0xc4e0_1a03);
};

const fn h(c_name: &'static str, header: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header,
        value,
        arches: Arches::All,
    }
}

const fn c(c_name: &'static str, value: u32) -> ConstValue {
    h(c_name, "cam/cam_ccb.h", value as i64)
}

const fn s(c_name: &'static str, value: u32) -> ConstValue {
    h(c_name, "cam/cam.h", value as i64)
}

pub(crate) const CONSTANTS: &[ConstValue] = &[
    h("CAMIOCOMMAND", "cam/scsi/scsi_pass.h", CAMIOCOMMAND as i64),
    h(
        "CAMGETPASSTHRU",
        "cam/scsi/scsi_pass.h",
        CAMGETPASSTHRU as i64,
    ),
    c("CAM_VERSION", CAM_VERSION as u32),
    s("CAM_XPT_PATH_ID", CAM_XPT_PATH_ID),
    s("CAM_TARGET_WILDCARD", CAM_TARGET_WILDCARD),
    s("CAM_LUN_WILDCARD", u32::MAX),
    c("XPT_FC_QUEUED", func::FC_QUEUED),
    c("XPT_FC_USER_CCB", func::FC_USER_CCB),
    c("XPT_FC_XPT_ONLY", func::FC_XPT_ONLY),
    c("XPT_FC_DEV_QUEUED", func::FC_DEV_QUEUED),
    c("XPT_NOOP", func::NOOP),
    c("XPT_SCSI_IO", func::SCSI_IO),
    c("XPT_GDEV_TYPE", func::GDEV_TYPE),
    c("XPT_GDEVLIST", func::GDEVLIST),
    c("XPT_PATH_INQ", func::PATH_INQ),
    c("XPT_REL_SIMQ", func::REL_SIMQ),
    c("XPT_SASYNC_CB", func::SASYNC_CB),
    c("XPT_SDEV_TYPE", func::SDEV_TYPE),
    c("XPT_SCAN_BUS", func::SCAN_BUS),
    c("XPT_DEV_MATCH", func::DEV_MATCH),
    c("XPT_DEBUG", func::DEBUG),
    c("XPT_PATH_STATS", func::PATH_STATS),
    c("XPT_GDEV_STATS", func::GDEV_STATS),
    c("XPT_DEV_ADVINFO", func::DEV_ADVINFO),
    c("XPT_ASYNC", func::ASYNC),
    c("XPT_ABORT", func::ABORT),
    c("XPT_RESET_BUS", func::RESET_BUS),
    c("XPT_RESET_DEV", func::RESET_DEV),
    c("XPT_TERM_IO", func::TERM_IO),
    c("XPT_SCAN_LUN", func::SCAN_LUN),
    c("XPT_GET_TRAN_SETTINGS", func::GET_TRAN_SETTINGS),
    c("XPT_SET_TRAN_SETTINGS", func::SET_TRAN_SETTINGS),
    c("XPT_CALC_GEOMETRY", func::CALC_GEOMETRY),
    c("XPT_ATA_IO", func::ATA_IO),
    c("XPT_SMP_IO", func::SMP_IO),
    c("XPT_NVME_IO", func::NVME_IO),
    c("XPT_MMC_IO", func::MMC_IO),
    c("XPT_SCAN_TGT", func::SCAN_TGT),
    c("XPT_NVME_ADMIN", func::NVME_ADMIN),
    c("XPT_ENG_INQ", func::ENG_INQ),
    s("CAM_REQ_INPROG", status::REQ_INPROG),
    s("CAM_REQ_CMP", status::REQ_CMP),
    s("CAM_REQ_ABORTED", status::REQ_ABORTED),
    s("CAM_REQ_CMP_ERR", status::REQ_CMP_ERR),
    s("CAM_BUSY", status::BUSY),
    s("CAM_REQ_INVALID", status::REQ_INVALID),
    s("CAM_PATH_INVALID", status::PATH_INVALID),
    s("CAM_DEV_NOT_THERE", status::DEV_NOT_THERE),
    s("CAM_SEL_TIMEOUT", status::SEL_TIMEOUT),
    s("CAM_CMD_TIMEOUT", status::CMD_TIMEOUT),
    s("CAM_SCSI_STATUS_ERROR", status::SCSI_STATUS_ERROR),
    s("CAM_AUTOSENSE_FAIL", status::AUTOSENSE_FAIL),
    s("CAM_DATA_RUN_ERR", status::DATA_RUN_ERR),
    s("CAM_REQ_TOO_BIG", status::REQ_TOO_BIG),
    s("CAM_ATA_STATUS_ERROR", status::ATA_STATUS_ERROR),
    s("CAM_NVME_STATUS_ERROR", status::NVME_STATUS_ERROR),
    s("CAM_RESRC_UNAVAIL", status::RESRC_UNAVAIL),
    s("CAM_FUNC_NOTAVAIL", status::FUNC_NOTAVAIL),
    s("CAM_DEV_QFRZN", status::DEV_QFRZN),
    s("CAM_AUTOSNS_VALID", status::AUTOSNS_VALID),
    s("CAM_QOS_VALID", status::QOS_VALID),
    s("CAM_STATUS_MASK", status::MASK),
    c("CAM_CDB_POINTER", flags::CDB_POINTER),
    c("CAM_DIR_IN", flags::DIR_IN),
    c("CAM_DIR_OUT", flags::DIR_OUT),
    c("CAM_DIR_NONE", flags::DIR_NONE),
    c("CAM_DIR_MASK", flags::DIR_MASK),
    c("CAM_DATA_VADDR", flags::DATA_VADDR),
    c("CAM_DATA_MASK", flags::DATA_MASK),
    c("CAM_DEV_QFRZDIS", flags::DEV_QFRZDIS),
    c("CAM_PASS_ERR_RECOVER", flags::PASS_ERR_RECOVER),
    c("CAM_UNLOCKED", flags::UNLOCKED),
    c("PROTO_UNKNOWN", proto::UNKNOWN),
    c("PROTO_UNSPECIFIED", proto::UNSPECIFIED),
    c("PROTO_SCSI", proto::SCSI),
    c("PROTO_ATA", proto::ATA),
    c("PROTO_NVME", proto::NVME),
    c("XPORT_UNKNOWN", xport::UNKNOWN),
    c("XPORT_UNSPECIFIED", xport::UNSPECIFIED),
    c("XPORT_USB", xport::USB),
    c("XPORT_SATA", xport::SATA),
    c("XPORT_NVME", xport::NVME),
    c("PROTO_VERSION_UNSPECIFIED", VERSION_UNSPECIFIED),
    c("XPORT_VERSION_UNSPECIFIED", VERSION_UNSPECIFIED),
    c("PIM_ATA_EXT", pim::ATA_EXT),
    c("PIM_NO_6_BYTE", pim::NO_6_BYTE),
    c("PIM_SEQSCAN", pim::SEQSCAN),
    c("PIM_UNMAPPED", pim::UNMAPPED),
    c("PIM_NOSCAN", pim::NOSCAN),
    c("PI_SDTR_ABLE", pi::SDTR_ABLE as u32),
    c("PI_SATAPM", pi::SATAPM as u32),
    c("PI_TAG_ABLE", pi::TAG_ABLE as u32),
    c("DEV_MATCH_PERIPH", match_type::PERIPH),
    c("DEV_MATCH_DEVICE", match_type::DEVICE),
    c("DEV_MATCH_BUS", match_type::BUS),
    c("PERIPH_MATCH_ANY", periph_match::ANY),
    c("PERIPH_MATCH_PATH", periph_match::PATH),
    c("PERIPH_MATCH_TARGET", periph_match::TARGET),
    c("PERIPH_MATCH_LUN", periph_match::LUN),
    c("PERIPH_MATCH_NAME", periph_match::NAME),
    c("PERIPH_MATCH_UNIT", periph_match::UNIT),
    c("DEV_MATCH_ANY", dev_match::ANY),
    c("DEV_MATCH_PATH", dev_match::PATH),
    c("DEV_MATCH_TARGET", dev_match::TARGET),
    c("DEV_MATCH_LUN", dev_match::LUN),
    c("DEV_MATCH_INQUIRY", dev_match::INQUIRY),
    c("DEV_MATCH_DEVID", dev_match::DEVID),
    c("BUS_MATCH_ANY", bus_match::ANY),
    c("BUS_MATCH_PATH", bus_match::PATH),
    c("BUS_MATCH_NAME", bus_match::NAME),
    c("BUS_MATCH_UNIT", bus_match::UNIT),
    c("BUS_MATCH_BUS_ID", bus_match::BUS_ID),
    c("DEV_RESULT_UNCONFIGURED", DEV_RESULT_UNCONFIGURED),
    c("CAM_DEV_MATCH_LAST", dm_status::LAST),
    c("CAM_DEV_MATCH_MORE", dm_status::MORE),
    c("CAM_DEV_MATCH_LIST_CHANGED", dm_status::LIST_CHANGED),
    c("CAM_DEV_MATCH_SIZE_ERROR", dm_status::SIZE_ERROR),
    c("CAM_DEV_MATCH_ERROR", dm_status::ERROR),
    c("CAM_DEV_POS_NONE", dev_pos::NONE),
    c("CAM_DEV_POS_BUS", dev_pos::BUS),
    c("CAM_DEV_POS_TARGET", dev_pos::TARGET),
    c("CAM_DEV_POS_DEVICE", dev_pos::DEVICE),
    c("CAM_DEV_POS_PERIPH", dev_pos::PERIPH),
    c("CAM_DEV_POS_PDPTR", dev_pos::PDPTR),
    c("CAM_DEV_POS_EDT", dev_pos::EDT),
    c("CAM_DEV_POS_PDRV", dev_pos::PDRV),
    c("CAM_GDEVLIST_LAST_DEVICE", gdevlist::LAST_DEVICE),
    c("CAM_GDEVLIST_LIST_CHANGED", gdevlist::LIST_CHANGED),
    c("CAM_GDEVLIST_MORE_DEVS", gdevlist::MORE_DEVS),
    c("CAM_GDEVLIST_ERROR", gdevlist::ERROR),
    c("CDAI_FLAG_STORE", cdai::FLAG_STORE),
    c("CDAI_TYPE_SCSI_DEVID", cdai::TYPE_SCSI_DEVID),
    c("CDAI_TYPE_SERIAL_NUM", cdai::TYPE_SERIAL_NUM),
    c("CDAI_TYPE_PHYS_PATH", cdai::TYPE_PHYS_PATH),
    c("CDAI_TYPE_RCAPLONG", cdai::TYPE_RCAPLONG),
    c("CDAI_TYPE_EXT_INQ", cdai::TYPE_EXT_INQ),
    c("CDAI_TYPE_NVME_CNTRL", cdai::TYPE_NVME_CNTRL),
    c("CDAI_TYPE_NVME_NS", cdai::TYPE_NVME_NS),
    c("CDAI_TYPE_MMC_PARAMS", cdai::TYPE_MMC_PARAMS),
    c("CTS_TYPE_CURRENT_SETTINGS", cts::TYPE_CURRENT_SETTINGS),
    c("CTS_TYPE_USER_SETTINGS", cts::TYPE_USER_SETTINGS),
    c("CTS_SCSI_VALID_TQ", cts::SCSI_VALID_TQ),
    c("CTS_ATA_VALID_TQ", cts::ATA_VALID_TQ),
    c("CTS_SATA_VALID_MODE", cts::SATA_VALID_MODE),
    c("CTS_SATA_VALID_BYTECOUNT", cts::SATA_VALID_BYTECOUNT),
    c("CTS_SATA_VALID_REVISION", cts::SATA_VALID_REVISION),
    c("CTS_SATA_VALID_PM", cts::SATA_VALID_PM),
    c("CTS_SATA_VALID_TAGS", cts::SATA_VALID_TAGS),
    c("CTS_SATA_VALID_ATAPI", cts::SATA_VALID_ATAPI),
    c("CTS_SATA_VALID_CAPS", cts::SATA_VALID_CAPS),
    c("CTS_SATA_CAPS_H_DMAAA", cts::SATA_CAPS_H_DMAAA),
    c("CTS_SATA_CAPS_H_AN", cts::SATA_CAPS_H_AN),
    c("CTS_NVME_VALID_SPEC", cts::NVME_VALID_SPEC),
    c("CTS_NVME_VALID_LINK", cts::NVME_VALID_LINK),
    h("CAM_ATAIO_48BIT", "cam/ata/ata_all.h", ataio::BIT48 as i64),
    h("CAM_ATAIO_FPDMA", "cam/ata/ata_all.h", ataio::FPDMA as i64),
    h(
        "CAM_ATAIO_CONTROL",
        "cam/ata/ata_all.h",
        ataio::CONTROL as i64,
    ),
    h(
        "CAM_ATAIO_NEEDRESULT",
        "cam/ata/ata_all.h",
        ataio::NEEDRESULT as i64,
    ),
    h("CAM_ATAIO_DMA", "cam/ata/ata_all.h", ataio::DMA as i64),
    c("DEV_IDLEN", 16),
];
