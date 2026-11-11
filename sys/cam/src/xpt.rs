// SPDX-License-Identifier: BSD-2-Clause
//! `/dev/xpt0` and `/dev/passN`: the CCBs userland sends.
//!
//! - `xpt0` (`root:operator` 0600, opened read-write and blocking only)
//!   takes what concerns the table or a bus: `XPT_DEV_MATCH`,
//!   `XPT_PATH_INQ`, the scans and bus resets (which find nothing new:
//!   the drivers register what they find), and `CAMGETPASSTHRU` from a
//!   peripheral's name.
//! - `passN` (the same) takes what concerns its device: `XPT_GDEV_TYPE`,
//!   `XPT_GDEVLIST`, `XPT_PATH_INQ`, the transfer settings,
//!   `XPT_DEV_ADVINFO`, `XPT_GDEV_STATS`, and the I/O requests, which go
//!   to the SIM's own submission path with the data in a kernel buffer.
//!   Requests only the transport layer takes fail with `ENODEV`.
//! - A data buffer longer than the SIM's largest transfer fails with
//!   `E2BIG`; one not given by virtual address with `EINVAL`.
//!
//! Behavior reference: sys/cam/cam_xpt.c (`xptopen`, `xptdoioctl`,
//! `xpt_action_default`), sys/cam/scsi/scsi_pass.c (`passopen`,
//! `passdoioctl`, `passsendccb`), sys/cam/cam_periph.c
//! (`cam_periph_mapmem`, `cam_periph_ioctl`), sys/cam/ata/ata_xpt.c and
//! sys/cam/scsi/scsi_xpt.c (`*_get_transfer_settings`,
//! `*_dev_advinfo`).

use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

use abi::cam::{
    CAM_LUN_WILDCARD, CAM_TARGET_WILDCARD, CAMGETPASSTHRU, CAMIOCOMMAND, cdai, cts, dm_status,
    flags, func, gdevlist, proto, status, xport,
};
use abi::errno::Errno;
use sched::Sleepable;
use vfs::{CharDevice, Uio};

use crate::ccb::{Ccb, Path, PathInq, TranSettings, union_valid};
use crate::edt::{self, DeviceNode, Edt, Sim};
use crate::io::{AtaIo, Dir, NvmeIo, ScsiIo};
use crate::matching::{self, PATTERN_SIZE, Pattern, RESULT_SIZE};

/// `maxphys`: the most a request may map.
const MAXPHYS: u64 = 1024 * 1024;
/// `DFLTPHYS`: a pass device's largest transfer when its SIM names none.
const DFLTPHYS: u32 = 64 * 1024;
/// `SID_CmdQue`.
const SID_CMDQUE: u8 = 0x02;
/// The peripheral drivers FreeBSD's GENERIC has (`periph_drivers`).
const DRIVERS: &[&str] = &[
    "da", "ada", "nda", "pass", "xpt", "cd", "sa", "ses", "ch", "pt", "sg", "targ", "enc",
];

/// The user's memory and credentials, as the kernel reaches them.
pub trait Host: Sync {
    /// Copies `dst.len()` bytes from user address `src`.
    ///
    /// # Errors
    ///
    /// `EFAULT`.
    fn copy_in(&self, dst: &mut [u8], src: u64) -> Result<(), Errno>;
    /// Copies `src` to user address `dst`.
    ///
    /// # Errors
    ///
    /// `EFAULT`.
    fn copy_out(&self, dst: u64, src: &[u8]) -> Result<(), Errno>;
    /// `securelevel_gt(cred, level)`: `EPERM` if the caller's securelevel
    /// is above `level`.
    ///
    /// # Errors
    ///
    /// `EPERM`.
    fn securelevel_gt(&self, level: i32) -> Result<(), Errno>;
}

static HOST: sync::Once<&'static dyn Host> = sync::Once::new();

/// Installs the kernel's user-memory access and credentials, once.
pub fn set_host(h: &'static dyn Host) {
    let _ = HOST.set(h);
}

/// The kernel's user-memory access.
///
/// # Errors
///
/// `ENXIO` before the kernel installed it.
pub fn host() -> Result<&'static dyn Host, Errno> {
    HOST.get().copied().ok_or(Errno::ENXIO)
}

/// The transport layer's own bus answering `XPT_PATH_INQ` (`xptaction`).
fn xpt_path_inq() -> PathInq {
    PathInq {
        version_num: 1,
        sim_vid: "FreeBSD",
        hba_vid: "",
        dev_name: "xpt",
        protocol: proto::UNSPECIFIED,
        protocol_version: abi::cam::VERSION_UNSPECIFIED,
        transport: xport::UNSPECIFIED,
        transport_version: abi::cam::VERSION_UNSPECIFIED,
        ..PathInq::default()
    }
}

fn path_inq(ops: Option<&Arc<dyn Sim>>, lun: u64) -> PathInq {
    ops.map_or_else(xpt_path_inq, |o| o.path_inq(lun))
}

/// Reads a CCB argument.
fn take(arg: &[u8]) -> Result<Ccb, Errno> {
    Ccb::from_bytes(arg).ok_or(Errno::EINVAL)
}

fn put(arg: &mut [u8], ccb: &Ccb) {
    arg.copy_from_slice(ccb.as_bytes());
}

/// `/dev/xpt0`.
#[derive(Debug)]
pub(crate) struct XptDev;

impl CharDevice for XptDev {
    fn instance_open(
        &self,
        _: &Sleepable,
        nonblock: bool,
        read: bool,
        write: bool,
    ) -> Result<Option<Arc<dyn CharDevice>>, Errno> {
        if !read || !write {
            return Err(Errno::EPERM);
        }
        if nonblock {
            return Err(Errno::ENODEV);
        }
        Ok(None)
    }

    fn read(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }

    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }

    fn ioctl(&self, _: &Sleepable, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        match req {
            CAMIOCOMMAND => xpt_command(arg),
            CAMGETPASSTHRU => get_passthru(arg),
            _ => Err(Errno::ENOTTY),
        }
    }
}

/// `CAMIOCOMMAND` on `xpt0`.
fn xpt_command(arg: &mut [u8]) -> Result<(), Errno> {
    let mut ccb = take(arg)?;
    if ccb.flags() & flags::UNLOCKED != 0 {
        return Err(Errno::EINVAL);
    }
    let path = ccb.path();
    let ops = edt::with(|e| e.bus(path.path_id).map(|b| b.ops.clone())).ok_or(Errno::EINVAL)?;
    let fc = ccb.func_code();
    let wildcard = path.target_id == CAM_TARGET_WILDCARD && path.target_lun == CAM_LUN_WILDCARD;
    match fc {
        func::SCAN_BUS | func::RESET_BUS if !wildcard => return Err(Errno::EINVAL),
        func::SCAN_TGT
            if path.target_id == CAM_TARGET_WILDCARD || path.target_lun != CAM_LUN_WILDCARD =>
        {
            return Err(Errno::EINVAL);
        }
        _ => {}
    }
    match fc {
        // Every device a SIM has is in the table already: a scan finds
        // nothing new, and the drivers recover their own buses.
        func::SCAN_BUS | func::RESET_BUS | func::SCAN_LUN | func::SCAN_TGT => {
            ccb.set_status(status::REQ_CMP);
        }
        func::PATH_INQ => {
            ccb.set_cpi(&path_inq(ops.as_ref(), path.target_lun));
            ccb.set_status(status::REQ_CMP);
        }
        func::ENG_INQ => ccb.set_status(status::REQ_INVALID),
        // GENERIC has no CAMDEBUG.
        func::DEBUG => ccb.set_status(status::FUNC_NOTAVAIL),
        func::DEV_MATCH => dev_match(&mut ccb)?,
        _ => return Err(Errno::ENOTSUP),
    }
    ccb.set_path(path);
    put(arg, &ccb);
    Ok(())
}

/// `XPT_DEV_MATCH`: the patterns copied in, the results out.
fn dev_match(ccb: &mut Ccb) -> Result<(), Errno> {
    if ccb.flags() & flags::DATA_MASK != flags::DATA_VADDR {
        return Err(Errno::EINVAL);
    }
    let (plen, mlen) = (ccb.cdm_pattern_buf_len(), ccb.cdm_match_buf_len());
    if mlen == 0 {
        return Err(Errno::EINVAL);
    }
    if u64::from(plen) > MAXPHYS || u64::from(mlen) > MAXPHYS {
        return Err(Errno::E2BIG);
    }
    let h = host()?;
    let mut raw = vec![0u8; usize::try_from(plen).unwrap_or(0)];
    if plen > 0 {
        h.copy_in(&mut raw, ccb.cdm_patterns())
            .map_err(|_| Errno::EACCES)?;
    }
    let n = usize::try_from(ccb.cdm_num_patterns()).unwrap_or(usize::MAX);
    let patterns: Option<Vec<Pattern>> = n
        .checked_mul(PATTERN_SIZE)
        .filter(|&bytes| bytes <= raw.len())
        .and_then(|_| {
            let (chunks, _) = raw.as_chunks::<PATTERN_SIZE>();
            chunks.iter().take(n).map(|c| Pattern::parse(c)).collect()
        });
    let Some(patterns) = patterns else {
        // More patterns than their buffer holds.
        ccb.set_cdm_num_matches(0);
        ccb.set_cdm_status(dm_status::ERROR);
        ccb.set_status(status::REQ_CMP_ERR);
        return Ok(());
    };
    let room = usize::try_from(mlen).unwrap_or(0) / RESULT_SIZE;
    let m = edt::with(|e: &mut Edt| matching::dev_match(e, &patterns, room, ccb.cdm_pos()));
    let at = ccb.cdm_matches();
    for (i, r) in m.results.iter().enumerate() {
        let off = u64::try_from(i * RESULT_SIZE).map_err(|_| Errno::EFAULT)?;
        h.copy_out(at.checked_add(off).ok_or(Errno::EFAULT)?, &r.encode())?;
    }
    ccb.set_cdm_num_matches(u32::try_from(m.results.len()).unwrap_or(u32::MAX));
    ccb.set_cdm_status(m.status);
    if m.status == dm_status::MORE {
        ccb.set_cdm_pos(&m.pos);
    }
    ccb.set_status(if m.status == dm_status::ERROR {
        status::REQ_CMP_ERR
    } else {
        status::REQ_CMP
    });
    Ok(())
}

/// Fills `cgdl` with device `d`'s `pass` peripheral, if it has one
/// (`CAMGETPASSTHRU`'s answer).
fn fill_passthru(ccb: &mut Ccb, path_id: u32, target: u32, d: &DeviceNode) -> bool {
    let Some(i) = d.periphs.iter().position(|p| p.name == "pass") else {
        return false;
    };
    ccb.set_cgdl_name(b"pass");
    ccb.set_cgdl_unit(d.periphs[i].unit);
    ccb.set_cgdl_status(if i + 1 < d.periphs.len() {
        gdevlist::MORE_DEVS
    } else {
        gdevlist::LAST_DEVICE
    });
    ccb.set_cgdl_generation(d.generation);
    ccb.set_cgdl_index(u32::try_from(i).unwrap_or(0));
    ccb.set_path(Path {
        path_id,
        target_id: target,
        target_lun: d.lun,
    });
    ccb.set_status(status::REQ_CMP);
    true
}

/// `CAMGETPASSTHRU` on `xpt0`: the `pass` device of the peripheral
/// named in `cgdl`.
fn get_passthru(arg: &mut [u8]) -> Result<(), Errno> {
    let mut ccb = take(arg)?;
    let name = ccb.cgdl_name().to_vec();
    if name.is_empty() {
        return Err(Errno::EINVAL);
    }
    let unit = ccb.cgdl_unit();
    let found = DRIVERS.iter().any(|d| d.as_bytes() == name.as_slice())
        && edt::with(|e| {
            let Some((b, t, d, _)) = e
                .periphs()
                .find(|(_, _, _, p)| p.name.as_bytes() == name.as_slice() && p.unit == unit)
            else {
                return false;
            };
            let path_id = b.path_id;
            fill_passthru(&mut ccb, path_id, t, d)
        });
    if !found {
        ccb.set_status(status::REQ_CMP_ERR);
        ccb.set_cgdl_status(gdevlist::ERROR);
        ccb.set_cgdl_name(b"");
        ccb.set_cgdl_unit(0);
        put(arg, &ccb);
        return Err(Errno::ENOENT);
    }
    put(arg, &ccb);
    Ok(())
}

/// `/dev/passN`: device `id` of the table.
#[derive(Debug)]
pub(crate) struct PassDev {
    pub(crate) id: u64,
}

/// What a pass device needs of its device for one request.
struct Target {
    path: Path,
    ops: Option<Arc<dyn Sim>>,
    info: Option<edt::DeviceInfo>,
    unconfigured: bool,
    generation: u32,
    periphs: Vec<edt::Periph>,
}

impl PassDev {
    fn target(&self) -> Result<Target, Errno> {
        edt::with(|e| {
            let (b, t, d) = e.device(self.id)?;
            Some(Target {
                path: Path {
                    path_id: b.path_id,
                    target_id: t,
                    target_lun: d.lun,
                },
                ops: b.ops.clone(),
                info: d.info.clone(),
                unconfigured: d.unconfigured,
                generation: d.generation,
                periphs: d.periphs.clone(),
            })
        })
        .ok_or(Errno::ENXIO)
    }
}

impl CharDevice for PassDev {
    fn instance_open(
        &self,
        _: &Sleepable,
        nonblock: bool,
        read: bool,
        write: bool,
    ) -> Result<Option<Arc<dyn CharDevice>>, Errno> {
        self.target()?;
        host()?.securelevel_gt(1)?;
        if !read || !write {
            return Err(Errno::EPERM);
        }
        if nonblock {
            return Err(Errno::EINVAL);
        }
        Ok(None)
    }

    fn read(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }

    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }

    fn ioctl(&self, s: &Sleepable, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        match req {
            CAMIOCOMMAND => self.command(s, arg),
            CAMGETPASSTHRU => {
                let mut ccb = take(arg)?;
                let t = self.target()?;
                let found = edt::with(|e| {
                    e.device(self.id).is_some_and(|(_, _, d)| {
                        fill_passthru(&mut ccb, t.path.path_id, t.path.target_id, d)
                    })
                });
                if !found {
                    ccb.set_cgdl_name(b"");
                    ccb.set_cgdl_unit(0);
                }
                put(arg, &ccb);
                Ok(())
            }
            _ => Err(Errno::ENOTTY),
        }
    }
}

impl PassDev {
    /// `CAMIOCOMMAND` on a pass device (`passsendccb`).
    fn command(&self, s: &Sleepable, arg: &mut [u8]) -> Result<(), Errno> {
        let mut ccb = take(arg)?;
        if ccb.flags() & flags::UNLOCKED != 0 {
            return Err(Errno::EINVAL);
        }
        let fc = ccb.func_code();
        if fc & func::FC_XPT_ONLY != 0 {
            return Err(Errno::ENODEV);
        }
        let t = self.target()?;
        let st = match fc {
            func::GDEV_TYPE => match &t.info {
                Some(i) if !t.unconfigured => {
                    ccb.set_cgd_protocol(i.protocol);
                    ccb.set_cgd(&*i.inq_data, &*i.ident_data, i.inq_flags, &i.serial);
                    status::REQ_CMP
                }
                _ => status::DEV_NOT_THERE,
            },
            func::GDEVLIST => gdevlist(&mut ccb, &t),
            func::PATH_INQ => {
                ccb.set_cpi(&path_inq(t.ops.as_ref(), t.path.target_lun));
                status::REQ_CMP
            }
            func::GET_TRAN_SETTINGS => {
                ccb.set_cts(&trans_settings(s, &t, ccb.cts_type()));
                status::REQ_CMP
            }
            func::SET_TRAN_SETTINGS | func::NOOP | func::REL_SIMQ | func::TERM_IO => {
                status::REQ_CMP
            }
            func::PATH_STATS => status::REQ_CMP,
            func::GDEV_STATS => {
                let n = t.info.as_ref().map_or(1, |i| i.openings);
                let n = i32::try_from(n).unwrap_or(i32::MAX);
                // A queueing device's tag limits, as CAM sets them (at
                // least two tags).
                let (min, max) = if n > 1 { (2, n) } else { (0, 0) };
                ccb.set_cgds(n, max, min);
                status::REQ_CMP
            }
            func::DEV_ADVINFO => advinfo(&mut ccb, &t)?,
            func::RESET_DEV => t
                .ops
                .as_ref()
                .map_or(status::REQ_INVALID, |o| o.reset_dev(s)),
            func::SCSI_IO | func::ATA_IO | func::NVME_IO | func::NVME_ADMIN => {
                run_io(s, &mut ccb, &t)?
            }
            _ => status::REQ_INVALID,
        };
        // A queued request comes back through `xpt_done`, which records
        // its time in the SIM.
        let st = if fc & func::FC_QUEUED != 0 {
            st | status::QOS_VALID
        } else {
            st
        };
        ccb.set_status(st);
        ccb.set_path(t.path);
        put(arg, &ccb);
        Ok(())
    }
}

/// `XPT_GDEVLIST`: the device's peripherals, one per call.
fn gdevlist(ccb: &mut Ccb, t: &Target) -> u32 {
    let index = ccb.cgdl_index();
    if index != 0 && ccb.cgdl_generation() != t.generation {
        ccb.set_cgdl_status(gdevlist::LIST_CHANGED);
        return ccb.status();
    }
    let Some(p) = usize::try_from(index).ok().and_then(|i| t.periphs.get(i)) else {
        ccb.set_cgdl_status(gdevlist::ERROR);
        return ccb.status();
    };
    ccb.set_cgdl_name(p.name.as_bytes());
    ccb.set_cgdl_unit(p.unit);
    let last = usize::try_from(index).ok().map(|i| i + 1) >= Some(t.periphs.len());
    ccb.set_cgdl_status(if last {
        gdevlist::LAST_DEVICE
    } else {
        gdevlist::MORE_DEVS
    });
    ccb.set_cgdl_index(index + 1);
    ccb.set_cgdl_generation(t.generation);
    status::REQ_CMP
}

/// `XPT_GET_TRAN_SETTINGS`: the SIM's settings, completed from the
/// device as the protocol's transport code does.
fn trans_settings(s: &Sleepable, t: &Target, ty: u32) -> TranSettings {
    let user = ty == cts::TYPE_USER_SETTINGS;
    let mut c = t
        .ops
        .as_ref()
        .map(|o| o.trans_settings(s, t.path.target_id, user))
        .unwrap_or_default();
    let Some(i) = &t.info else {
        return c;
    };
    if c.protocol == proto::UNKNOWN || c.protocol == proto::UNSPECIFIED {
        c.protocol = i.protocol;
        c.protocol_version = i.protocol_version;
    }
    if (c.protocol == proto::ATA || c.protocol == proto::SCSI)
        && union_valid(&c.proto_specific) & cts::SCSI_VALID_TQ == 0
    {
        let tag = user || i.inq_flags & SID_CMDQUE != 0;
        let s = abi::cam::TransSettingsScsi {
            valid: union_valid(&c.proto_specific) | cts::SCSI_VALID_TQ,
            flags: u32::from(tag),
        };
        c.proto_specific = crate::ccb::trans_scsi(&s);
    }
    if c.transport == xport::UNKNOWN || c.transport == xport::UNSPECIFIED {
        c.transport = i.transport;
        c.transport_version = i.transport_version;
    }
    c
}

/// `XPT_DEV_ADVINFO`: the device's identification, serial number, and
/// NVMe identify data, as much as the buffer holds.
fn advinfo(ccb: &mut Ccb, t: &Target) -> Result<u32, Errno> {
    let Some(i) = &t.info else {
        return Ok(status::REQ_INVALID);
    };
    let nvme = i.protocol == proto::NVME;
    let empty: &[u8] = &[];
    let data: &[u8] = match ccb.cdai_buftype() {
        cdai::TYPE_SCSI_DEVID => &i.devid,
        cdai::TYPE_SERIAL_NUM => &i.serial,
        cdai::TYPE_PHYS_PATH => empty,
        cdai::TYPE_RCAPLONG | cdai::TYPE_EXT_INQ if i.protocol == proto::SCSI => empty,
        cdai::TYPE_NVME_CNTRL if nvme => i.nvme.as_ref().map_or(empty, |n| &n.cdata),
        cdai::TYPE_NVME_NS if nvme => i.nvme.as_ref().map_or(empty, |n| &n.nsdata),
        _ => return Ok(status::REQ_INVALID),
    };
    if ccb.cdai_flags() & cdai::FLAG_STORE != 0 {
        // Only the physical path is stored, and none is kept here.
        return Ok(if ccb.cdai_buftype() == cdai::TYPE_PHYS_PATH {
            status::REQ_CMP
        } else {
            status::REQ_INVALID
        });
    }
    let bufsiz = ccb.cdai_bufsiz();
    if bufsiz > MAXPHYS {
        return Err(Errno::E2BIG);
    }
    ccb.set_cdai_provsiz(data.len() as u64);
    let amt = data.len().min(usize::try_from(bufsiz).unwrap_or(0));
    if amt > 0 {
        host()?.copy_out(ccb.cdai_buf(), &data[..amt])?;
    }
    Ok(status::REQ_CMP)
}

/// The I/O requests: data copied in, the SIM's submission, data and
/// results copied out (`cam_periph_mapmem`, `cam_periph_runccb`).
fn run_io(s: &Sleepable, ccb: &mut Ccb, t: &Target) -> Result<u32, Errno> {
    let fc = ccb.func_code();
    let Some(ops) = &t.ops else {
        return Ok(status::REQ_INVALID);
    };
    if t.unconfigured {
        return Ok(status::DEV_NOT_THERE);
    }
    let dir = Dir::of(ccb.flags());
    let (ptr, len) = match fc {
        func::SCSI_IO => (ccb.csio_data_ptr(), ccb.csio_dxfer_len()),
        func::ATA_IO => (ccb.ataio_data_ptr(), ccb.ataio_dxfer_len()),
        _ => (ccb.nvmeio_data_ptr(), ccb.nvmeio_dxfer_len()),
    };
    let h = host()?;
    let mut data = Vec::new();
    if dir != Dir::None {
        if ccb.flags() & flags::DATA_MASK != flags::DATA_VADDR {
            return Err(Errno::EINVAL);
        }
        let maxio = match ops.path_inq(t.path.target_lun).maxio {
            0 => DFLTPHYS,
            m => m.min(u32::try_from(MAXPHYS).unwrap_or(u32::MAX)),
        };
        if len > maxio {
            return Err(Errno::E2BIG);
        }
        data = vec![0u8; usize::try_from(len).map_err(|_| Errno::E2BIG)?];
        // A buffer that cannot be reached is `EACCES`
        // (`cam_periph_mapmem`).
        if dir.copies_in() && !data.is_empty() {
            h.copy_in(&mut data, ptr).map_err(|_| Errno::EACCES)?;
        }
    }
    let qfrzdis = ccb.flags() & flags::DEV_QFRZDIS != 0;
    let (st, data) = match fc {
        func::SCSI_IO => {
            let cdb_len = usize::from(ccb.csio_cdb_len());
            let cdb = if ccb.flags() & flags::CDB_POINTER != 0 {
                let mut c = vec![0u8; cdb_len];
                h.copy_in(&mut c, ccb.csio_cdb_ptr())?;
                c
            } else {
                ccb.csio_cdb_bytes()[..cdb_len.min(16)].to_vec()
            };
            let mut io = ScsiIo::new(cdb, dir, data, ccb.csio_sense_len());
            io.timeout_ms = ccb.timeout();
            let retries = if ccb.flags() & flags::PASS_ERR_RECOVER != 0 {
                ccb.retry_count()
            } else {
                0
            };
            let mut st = ops.scsi_io(s, t.path.target_id, t.path.target_lun, &mut io);
            // With error recovery, a unit attention is tried again.
            for _ in 0..retries {
                let ua = st & status::MASK == status::SCSI_STATUS_ERROR
                    && st & status::AUTOSNS_VALID != 0
                    && sense_key(&io.sense) == Some(0x6);
                if !ua {
                    break;
                }
                io.sense.clear();
                st = ops.scsi_io(s, t.path.target_id, t.path.target_lun, &mut io);
            }
            ccb.set_csio_scsi_status(io.scsi_status);
            ccb.set_csio_resid(io.resid);
            if st & status::AUTOSNS_VALID != 0 {
                let n = io.sense.len().min(usize::from(ccb.csio_sense_len()));
                ccb.set_csio_sense(&io.sense[..n]);
                ccb.set_csio_sense_resid(ccb.csio_sense_len() - u8::try_from(n).unwrap_or(0));
            }
            (st, io.data)
        }
        func::ATA_IO => {
            let mut io = AtaIo {
                cmd: ccb.ataio_cmd(),
                dir,
                data,
                timeout_ms: ccb.timeout(),
                res: abi::cam::AtaRes::default(),
                resid: 0,
            };
            let st = ops.ata_io(s, t.path.target_id, &mut io);
            ccb.set_ataio_res(&io.res);
            ccb.set_ataio_resid(io.resid);
            (st, io.data)
        }
        _ => {
            let mut io = NvmeIo {
                cmd: ccb.nvmeio_cmd(),
                dir,
                data,
                cpl: abi::nvme::NvmeCompletion::default(),
            };
            let st = ops.nvme_io(s, t.path.target_lun, fc == func::NVME_ADMIN, &mut io);
            ccb.set_nvmeio_cpl(&io.cpl);
            (st, io.data)
        }
    };
    if dir.copies_out() && !data.is_empty() {
        h.copy_out(ptr, &data)?;
    }
    // A failed command freezes its device's queue (released at once
    // here, there being no queue), unless the caller asked otherwise.
    let freezes = matches!(
        st & status::MASK,
        status::SCSI_STATUS_ERROR
            | status::ATA_STATUS_ERROR
            | status::CMD_TIMEOUT
            | status::REQ_CMP_ERR
    );
    Ok(if freezes && !qfrzdis {
        st | status::DEV_QFRZN
    } else {
        st
    })
}

/// The sense key of fixed or descriptor sense data.
fn sense_key(b: &[u8]) -> Option<u8> {
    match b.first()? & 0x7f {
        0x70 | 0x71 => Some(b.get(2)? & 0xf),
        0x72 | 0x73 => Some(b.get(1)? & 0xf),
        _ => None,
    }
}

/// Makes `/dev/xpt0`, `root:operator` 0600 (`xpt_periph_init`).
pub(crate) fn make_xpt0() {
    let _ = vfs::devices::make_dev(b"xpt0", 0o600, 0, crate::GID_OPERATOR, Arc::new(XptDev));
}
