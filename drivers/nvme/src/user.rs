// SPDX-License-Identifier: BSD-2-Clause
//! What userland sees of a controller: `/dev/nvmeN` and `/dev/nvmeNnsM`
//! (`nvmecontrol(8)`, smartmontools), and its CAM bus (`nvme_sim`), on
//! which each namespace is `ndaN` with a `pass` device beside it.
//!
//! - `/dev/nvmeN` (`root:wheel` 0600) takes `NVME_PASSTHROUGH_CMD` as
//!   an admin command (the namespace the caller names),
//!   `NVME_RESET_CONTROLLER`, `NVME_GET_NSID`, `NVME_GET_MAX_XFER_SIZE`,
//!   `NVME_GET_CONTROLLER_DATA`, and `DIOCGIDENT`.
//! - `/dev/nvmeNnM` (and its alias `nvmeNnsM`, `root:wheel` 0600) reads
//!   and writes the namespace in whole sectors, and takes
//!   `NVME_PASSTHROUGH_CMD` as an I/O command on the namespace,
//!   `NVME_GET_NSID`, `DIOCGIDENT`, `DIOCGMEDIASIZE`, and
//!   `DIOCGSECTORSIZE`. Opening it for writing needs securelevel 0 or
//!   below. `NVME_IO_TEST` and `NVME_BIO_TEST` (`nvmecontrol perftest`)
//!   are refused (`EINVAL`).
//! - A pass-through's data buffer is copied in (written to the
//!   controller) or out (`is_read`); a longer one than the controller's
//!   largest transfer fails with `EIO`, one that cannot be reached with
//!   `ENOMEM`, as on FreeBSD. The completion's dword 0 and status
//!   (without the phase bit) come back in `cpl`.
//! - The CAM bus is `nvme` unit N, numbered as CAM numbers buses; each
//!   namespace a device at target 0, LUN its ID, with its identify data
//!   for `XPT_DEV_ADVINFO`; `XPT_NVME_IO` and `XPT_NVME_ADMIN` from its
//!   pass device run as pass-through commands.
//!
//! Behavior reference: sys/dev/nvme/nvme_ctrlr.c (`nvme_ctrlr_ioctl`),
//! sys/dev/nvme/nvme_ns.c (`nvme_ns_ioctl`, `nvme_ns_construct`),
//! sys/dev/nvme/nvme_sim.c, sys/cam/nvme/nvme_xpt.c (`nvme_probe_done`),
//! sys/cam/nvme/nvme_da.c (`ndaregister`).

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::mem::offset_of;
use core::sync::atomic::Ordering;

use abi::cam::{PathinqNvme, TransSettingsNvme, cts, pim, proto, status as cam_status, xport};
use abi::errno::Errno;
use abi::ioctl::{DIOCGIDENT, DIOCGMEDIASIZE, DIOCGSECTORSIZE};
use abi::nvme::{
    NVME_BIO_TEST, NVME_GET_CONTROLLER_DATA, NVME_GET_MAX_XFER_SIZE, NVME_GET_NSID, NVME_IO_TEST,
    NVME_PASSTHROUGH_CMD, NVME_RESET_CONTROLLER, NvmeCompletion, NvmeGetNsid, NvmePtCommand,
};
use cam::ccb::{PathInq, TranSettings};
use cam::io::{Dir, NvmeIo};
use pci::bus::PciChild;
use sched::Sleepable;
use storage::{BioError, Disk};
use vfs::{CharDevice, Uio};

use crate::Controller;

/// The identify controller data's serial number (`sn`), model (`mn`),
/// and firmware revision (`fr`).
const SN: core::ops::Range<usize> = 4..24;
const MN: core::ops::Range<usize> = 24..64;
const FR: core::ops::Range<usize> = 64..72;
/// The identify namespace data's NGUID and EUI64.
const NGUID: core::ops::Range<usize> = 104..120;
const EUI64: core::ops::Range<usize> = 120..128;

/// `cam_strvis`: printable ASCII, with leading and trailing blanks and
/// NULs trimmed.
fn strvis(b: &[u8]) -> String {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    b[..n]
        .iter()
        .map(|&c| {
            if (0x20..0x7f).contains(&c) {
                char::from(c)
            } else {
                ' '
            }
        })
        .collect::<String>()
        .trim_matches(' ')
        .into()
}

/// `nvme_cdata_get_disk_ident`: the serial number, NUL-terminated, its
/// unprintable bytes made spaces.
fn disk_ident(cdata: &[u8; 4096], arg: &mut [u8]) {
    arg.fill(0);
    let sn = &cdata[SN];
    let n = sn.iter().position(|&c| c == 0).unwrap_or(sn.len());
    for (d, &c) in arg.iter_mut().zip(&sn[..n]) {
        *d = if (0x20..0x80).contains(&c) { c } else { b' ' };
    }
}

/// `NVME_GET_NSID`'s answer.
fn get_nsid(arg: &mut [u8], ctrl: &str, nsid: u32) -> Result<(), Errno> {
    if arg.len() != size_of::<NvmeGetNsid>() {
        return Err(Errno::EINVAL);
    }
    arg.fill(0);
    let n = ctrl.len().min(abi::nvme::CDEV_NAME_LEN - 1);
    arg[offset_of!(NvmeGetNsid, cdev)..][..n].copy_from_slice(&ctrl.as_bytes()[..n]);
    arg[offset_of!(NvmeGetNsid, nsid)..][..4].copy_from_slice(&nsid.to_ne_bytes());
    Ok(())
}

/// `NVME_PASSTHROUGH_CMD`: the command in `arg` run as an admin command
/// (on the namespace it names) or, with `nsid`, as an I/O command on
/// that namespace (`nvme_ctrlr_passthrough_cmd`).
fn passthrough(
    ctrl: &Controller,
    s: &Sleepable,
    arg: &mut [u8],
    nsid: Option<u32>,
) -> Result<(), Errno> {
    type P = NvmePtCommand;
    if arg.len() != size_of::<P>() {
        return Err(Errno::EINVAL);
    }
    let mut cmd = cam::ccb::nvme_command(&arg[..size_of::<abi::nvme::NvmeCommand>()]);
    let u64_at = |o: usize| {
        let mut a = [0u8; 8];
        a.copy_from_slice(&arg[o..o + 8]);
        u64::from_ne_bytes(a)
    };
    let u32_ne = |o: usize| {
        let mut a = [0u8; 4];
        a.copy_from_slice(&arg[o..o + 4]);
        u32::from_ne_bytes(a)
    };
    let buf = u64_at(offset_of!(P, buf));
    let len = u32_ne(offset_of!(P, len));
    let read = u32_ne(offset_of!(P, is_read)) != 0;
    if let Some(n) = nsid {
        cmd.nsid = n;
    }
    let h = cam::host()?;
    let mut data = Vec::new();
    if len > 0 {
        let max = u32::try_from(ctrl.max_transfer).unwrap_or(u32::MAX);
        if len > max {
            ctrl.log(format_args!("len ({len}) exceeds max_xfer_size ({max})"));
            return Err(Errno::EIO);
        }
        data = vec![0u8; usize::try_from(len).map_err(|_| Errno::EIO)?];
        // The buffer must be there, and writable for a read, before the
        // command goes (`vm_fault_hold_pages`, which fails with ENOMEM).
        let held = if read {
            h.copy_out(buf, &data)
        } else {
            h.copy_in(&mut data, buf)
        };
        held.map_err(|_| Errno::ENOMEM)?;
    }
    let out = ctrl.passthrough(s, nsid.is_none(), &cmd, read, data)?;
    if len > 0 && read {
        h.copy_out(buf, &out.data)?;
    }
    let cpl = NvmeCompletion {
        cdw0: out.dw0,
        status: out.status << 1,
        ..NvmeCompletion::default()
    };
    cam::ccb::put_nvme_completion(&mut arg[offset_of!(P, cpl)..][..16], &cpl);
    Ok(())
}

/// `ndaioctl`: what `ndaN` answers besides GEOM's requests.
/// `NVME_PASSTHROUGH_CMD` runs as an I/O command with the namespace the
/// caller names, and fails with `EIO` if the command did.
pub(crate) fn nda_ioctl(
    ctrl: &Controller,
    nsid: u32,
    s: &Sleepable,
    req: u32,
    arg: &mut [u8],
) -> Result<(), Errno> {
    match req {
        NVME_GET_NSID => get_nsid(arg, &ctrl.name(), nsid),
        NVME_PASSTHROUGH_CMD => {
            if arg.len() != size_of::<NvmePtCommand>() {
                return Err(Errno::EINVAL);
            }
            let user_nsid = cam::ccb::nvme_command(&arg[..64]).nsid;
            passthrough(ctrl, s, arg, Some(user_nsid))?;
            let st = u16::from_le_bytes([
                arg[offset_of!(NvmePtCommand, cpl) + 14],
                arg[offset_of!(NvmePtCommand, cpl) + 15],
            ]);
            if st >> 1 & 0x7ff != 0 {
                return Err(Errno::EIO);
            }
            Ok(())
        }
        _ => Err(Errno::ENOTTY),
    }
}

/// `/dev/nvmeN`. An open of it outlives the controller: it fails with
/// `ENXIO` once the controller has gone.
pub(crate) struct CtrlDev {
    pub(crate) ctrl: Weak<Controller>,
}

impl CharDevice for CtrlDev {
    fn read(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }

    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }

    fn ioctl(&self, s: &Sleepable, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        let c = &self.ctrl.upgrade().ok_or(Errno::ENXIO)?;
        match req {
            NVME_RESET_CONTROLLER => {
                if !c.failed.load(Ordering::Acquire) {
                    c.reset(s);
                }
            }
            NVME_PASSTHROUGH_CMD => passthrough(c, s, arg, None)?,
            NVME_GET_NSID => get_nsid(arg, &c.name(), 0)?,
            NVME_GET_MAX_XFER_SIZE => {
                arg.copy_from_slice(&(c.max_transfer as u64).to_ne_bytes());
            }
            NVME_GET_CONTROLLER_DATA => arg.copy_from_slice(&*c.cdata),
            DIOCGIDENT => disk_ident(&c.cdata, arg),
            _ => return Err(Errno::ENOTTY),
        }
        Ok(())
    }
}

/// `/dev/nvmeNnM`: namespace `nsid`, through its disk.
pub(crate) struct NsDev {
    pub(crate) ctrl: Weak<Controller>,
    pub(crate) disk: Arc<Disk>,
    pub(crate) nsid: u32,
}

fn bio_errno(e: BioError) -> Errno {
    match e {
        BioError::Io => Errno::EIO,
        BioError::Gone => Errno::ENXIO,
        BioError::Invalid => Errno::EINVAL,
        BioError::ReadOnly => Errno::EROFS,
    }
}

/// Transfers at most this much per request.
const CHUNK: usize = 128 * 1024;

impl NsDev {
    /// The whole sectors a transfer of `len` at `offset` covers.
    fn range(&self, offset: u64, len: usize) -> Result<(u64, usize), Errno> {
        let ss = u64::from(self.disk.sector_size());
        if !offset.is_multiple_of(ss) || !(len as u64).is_multiple_of(ss) {
            return Err(Errno::EINVAL);
        }
        let n = usize::try_from(self.disk.bytes().saturating_sub(offset)).unwrap_or(usize::MAX);
        Ok((offset, len.min(n)))
    }
}

impl CharDevice for NsDev {
    fn dtype(&self) -> u32 {
        vfs::D_DISK
    }

    /// `nvme_ns_open`: writing needs securelevel 0 or below.
    fn instance_open(
        &self,
        _: &Sleepable,
        _: bool,
        _: bool,
        write: bool,
    ) -> Result<Option<Arc<dyn CharDevice>>, Errno> {
        if write {
            cam::host()?.securelevel_gt(0)?;
        }
        Ok(None)
    }

    fn read(&self, s: &Sleepable, uio: &mut dyn Uio, offset: u64, _: bool) -> Result<usize, Errno> {
        let (at, len) = self.range(offset, uio.resid())?;
        let mut done = 0;
        while done < len {
            let n = CHUNK.min(len - done);
            let data = self.disk.read(s, at + done as u64, n).map_err(bio_errno)?;
            let moved = uio.copy_out(&data)?;
            done += moved;
            if moved < n {
                break;
            }
        }
        Ok(done)
    }

    fn write(
        &self,
        s: &Sleepable,
        uio: &mut dyn Uio,
        offset: u64,
        _: bool,
    ) -> Result<usize, Errno> {
        let (at, len) = self.range(offset, uio.resid())?;
        if len == 0 && uio.resid() > 0 {
            return Err(Errno::ENOSPC);
        }
        let mut buf = vec![0u8; CHUNK.min(len)];
        let mut done = 0;
        while done < len {
            let n = CHUNK.min(len - done);
            let got = uio.copy_in(&mut buf[..n])?;
            self.disk
                .write(s, at + done as u64, &buf[..got])
                .map_err(bio_errno)?;
            done += got;
            if got < n {
                break;
            }
        }
        Ok(done)
    }

    fn ioctl(&self, s: &Sleepable, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        let c = &self.ctrl.upgrade().ok_or(Errno::ENXIO)?;
        match req {
            // `nvmecontrol perftest`'s kernel threads are not here.
            NVME_IO_TEST | NVME_BIO_TEST => return Err(Errno::EINVAL),
            NVME_PASSTHROUGH_CMD => passthrough(c, s, arg, Some(self.nsid))?,
            NVME_GET_NSID => get_nsid(arg, &c.name(), self.nsid)?,
            DIOCGIDENT => disk_ident(&c.cdata, arg),
            DIOCGMEDIASIZE => {
                let size = i64::try_from(self.disk.bytes()).unwrap_or(i64::MAX);
                arg.copy_from_slice(&size.to_ne_bytes());
            }
            DIOCGSECTORSIZE => arg.copy_from_slice(&self.disk.sector_size().to_ne_bytes()),
            _ => return Err(Errno::ENOTTY),
        }
        Ok(())
    }
}

/// The controller as CAM's SIM (`nvme_sim_action`). CAM holds it until
/// the bus goes; the controller itself it holds weakly.
pub(crate) struct NvmeSim {
    pub(crate) ctrl: Weak<Controller>,
    pub(crate) pci: Arc<PciChild>,
    /// The controller's unit, version, namespaces, and largest transfer.
    pub(crate) unit: u32,
    pub(crate) vs: u32,
    pub(crate) nn: u32,
    pub(crate) maxio: u32,
}

/// The PCI Express capability's link registers: flags, link
/// capabilities, link status.
fn pcie_link(pci: &PciChild) -> Option<(u16, u32, u16)> {
    let cap = pci.function.caps.iter().find(|c| c.id == 0x10)?;
    let o = u16::from(cap.offset);
    Some((
        pci.read16(o + 0x02),
        pci.read32(o + 0x0c),
        pci.read16(o + 0x12),
    ))
}

impl NvmeSim {
    /// `nvme_link_kBps`: the link's speed, in KB/s, its lanes'.
    fn link_kbps(&self) -> u32 {
        const KBPS: [u32; 5] = [1, 250_000, 500_000, 985_000, 1_970_000];
        let Some((_, _, sta)) = pcie_link(&self.pci) else {
            return 0;
        };
        let speed = usize::from(sta & 0xf);
        let lanes = u32::from((sta & 0x3f0) >> 4);
        KBPS.get(speed).copied().unwrap_or(1) * lanes
    }
}

impl cam::Sim for NvmeSim {
    fn path_inq(&self, lun: u64) -> PathInq {
        let f = &self.pci.function;
        let id = f.header.ident;
        let mut dev_name = [0u8; 52];
        let name = alloc::format!("nvme{}", self.unit);
        dev_name[..name.len()].copy_from_slice(name.as_bytes());
        let nvme = PathinqNvme {
            nsid: u32::try_from(lun).unwrap_or(0),
            domain: f.addr.domain,
            bus: f.addr.bus,
            slot: f.addr.slot,
            function: f.addr.function,
            extra: 0,
            dev_name,
        };
        PathInq {
            version_num: 1,
            hba_misc: pim::UNMAPPED | pim::NOSCAN,
            max_target: 0,
            max_lun: self.nn,
            maxio: self.maxio,
            sim_vid: "FreeBSD",
            hba_vid: "NVMe",
            dev_name: "nvme",
            unit_number: self.unit,
            bus_id: 0,
            base_transfer_speed: self.link_kbps(),
            transport: xport::NVME,
            transport_version: self.vs,
            protocol: proto::NVME,
            protocol_version: self.vs,
            xport_specific: Some(cam::ccb::pathinq_nvme(&nvme)),
            hba_vendor: id.vendor,
            hba_device: id.device,
            hba_subvendor: id.subvendor,
            hba_subdevice: id.subdevice,
            ..PathInq::default()
        }
    }

    fn trans_settings(&self, _: &Sleepable, _: u32, _: bool) -> TranSettings {
        let spec = self.vs;
        let mut x = TransSettingsNvme {
            valid: cts::NVME_VALID_SPEC,
            spec,
            ..TransSettingsNvme::default()
        };
        if let Some((flags, cap, sta)) = pcie_link(&self.pci)
            && flags & 0xf0 == 0
        {
            // An endpoint: its link.
            x.valid |= cts::NVME_VALID_LINK;
            x.speed = (sta & 0xf) as u8;
            x.lanes = ((sta & 0x3f0) >> 4) as u8;
            x.max_speed = (cap & 0xf) as u8;
            x.max_lanes = ((cap & 0x3f0) >> 4) as u8;
        }
        let p = TransSettingsNvme {
            valid: cts::NVME_VALID_SPEC,
            spec,
            ..TransSettingsNvme::default()
        };
        TranSettings {
            protocol: proto::NVME,
            protocol_version: spec,
            transport: xport::NVME,
            transport_version: spec,
            proto_specific: cam::ccb::trans_nvme(&p),
            xport_specific: cam::ccb::trans_nvme(&x),
        }
    }

    fn nvme_io(&self, s: &Sleepable, _: u64, admin: bool, io: &mut NvmeIo) -> u32 {
        let Some(c) = self.ctrl.upgrade() else {
            return cam_status::DEV_NOT_THERE;
        };
        if c.failed.load(Ordering::Acquire) {
            return cam_status::DEV_NOT_THERE;
        }
        // The opcode's low bits say which way data moves (host to
        // controller 01, controller to host 10) when the caller did not.
        let read = match io.dir {
            Dir::In => true,
            Dir::Out | Dir::None => false,
            Dir::Both => io.cmd.opc & 0x2 != 0,
        };
        let data = core::mem::take(&mut io.data);
        match c.passthrough(s, admin, &io.cmd, read, data) {
            Ok(out) => {
                io.data = out.data;
                io.cpl = NvmeCompletion {
                    cdw0: out.dw0,
                    sqid: out.sqid,
                    cid: out.cid,
                    status: out.status << 1,
                    ..NvmeCompletion::default()
                };
                if out.status & 0x7ff != 0 {
                    cam_status::NVME_STATUS_ERROR
                } else {
                    cam_status::REQ_CMP
                }
            }
            Err(_) => cam_status::RESRC_UNAVAIL,
        }
    }
}

/// A namespace's device identification page (`nvme_probe_done`): its
/// NGUID, else its EUI-64, as one binary LUN descriptor.
fn devid(nsdata: &[u8; 4096]) -> Vec<u8> {
    let id: &[u8] = if nsdata[NGUID].iter().any(|&b| b != 0) {
        &nsdata[NGUID]
    } else if nsdata[EUI64].iter().any(|&b| b != 0) {
        &nsdata[EUI64]
    } else {
        return Vec::new();
    };
    let len = 4 + id.len();
    // Peripheral 0, page 0x83, the descriptors' length; then the
    // descriptor: binary code set, LUN association, EUI-64 type.
    let mut p = vec![0, 0x83, 0, len as u8, 0x01, 0x02, 0, id.len() as u8];
    p.extend_from_slice(id);
    p
}

/// CAM's view of namespace `nsid`, `nda` unit `unit` (`nvme_probe_done`).
pub(crate) fn cam_device(
    ctrl: &Controller,
    nsid: u32,
    nsdata: &[u8; 4096],
    unit: u32,
) -> cam::DeviceInfo {
    let mut i = cam::DeviceInfo::new(0, u64::from(nsid), proto::NVME, ("nda", unit));
    i.protocol_version = ctrl.vs;
    i.transport = xport::NVME;
    i.transport_version = ctrl.vs;
    i.serial = strvis(&ctrl.cdata[SN]).into_bytes();
    i.devid = devid(nsdata);
    i.openings = ctrl.max_pending();
    i.nvme = Some(Box::new(cam::NvmeData {
        cdata: *ctrl.cdata,
        nsdata: *nsdata,
    }));
    i
}

/// The lines `ndaregister` prints for a namespace (before its size,
/// which the disk prints as it is published).
pub(crate) fn announce(ctrl: &Controller, unit: u32, scbus: u32, nsid: u32) {
    let m = bus::manager();
    let c = &ctrl.cdata;
    let (mn, fr, sn) = (strvis(&c[MN]), strvis(&c[FR]), strvis(&c[SN]));
    m.log(format_args!(
        "nda{unit} at nvme{} bus 0 scbus{scbus} target 0 lun {nsid}",
        ctrl.unit
    ));
    m.log(format_args!("nda{unit}: <{mn} {fr} {sn}>"));
    m.log(format_args!("nda{unit}: Serial Number {sn}"));
    m.log(format_args!(
        "nda{unit}: nvme version {}.{}",
        ctrl.vs >> 16,
        ctrl.vs >> 8 & 0xff
    ));
}
