// SPDX-License-Identifier: BSD-2-Clause
//! A channel as CAM's SIM (`ahciaction`): its path inquiry and transfer
//! settings, and `XPT_ATA_IO` from its drive's pass device, which runs
//! on the port beside the disk's requests.
//!
//! - A pass-through command waits until the port is idle, takes slot 0,
//!   and nothing else is issued until it ends (as a flush): its FIS is
//!   the command's registers (`ahci_setup_fis`), with the tag in the
//!   sector count for an FPDMA command.
//! - It ends with the task file's status and error and the received
//!   D2H register FIS's other registers, when it failed or asked for
//!   them (`CAM_ATAIO_NEEDRESULT`), and the bytes it did not move.
//! - One that never completes is ended (`CAM_CMD_TIMEOUT`) by restarting
//!   the port's command engine.
//! - `XPT_SCSI_IO` (ATAPI) and control commands (soft resets) are
//!   refused: there are no ATAPI devices here.
//!
//! Behavior reference: sys/dev/ahci/ahci.c (`ahciaction`,
//! `ahci_setup_fis`, `ahci_end_transaction`), sys/cam/ata/ata_xpt.c
//! (`probedone`, `ata_device_transport`), sys/cam/ata/ata_all.c
//! (`ata_param_fixup`, `ata_version`).

use alloc::boxed::Box;
use alloc::sync::{Arc, Weak};

use abi::cam::{AtaCmd, AtaRes, TransSettingsSata, ataio, cts, pi, pim, proto, status, xport};
use cam::ccb::{PathInq, TranSettings};
use cam::io::{AtaIo, Dir};
use sched::{Sleepable, WaitQueue};
use sync::SpinLock;
use vm::object::IoBuf;

use crate::{
    CAP_SNCQ, CMD_ST, Data, Drive, InFlight, MAX_TRANSFER, PRDS, PX_CI, PX_CMD, PX_SACT, PX_SSTS,
    Port, Req, TFD_ERR, sleep_ms,
};

/// CAP.SPM: port multipliers.
const CAP_SPM: u32 = 1 << 17;
/// A command's timeout when the caller gives none (CAM's default).
const DEFAULT_TIMEOUT_MS: u32 = 30_000;
/// `ATA_UDMA0`, and `ATA_PIO4`.
const ATA_UDMA0: i32 = 0x40;
const ATA_PIO4: i32 = 0x0c;
/// `SID_CmdQue`.
const SID_CMDQUE: u8 = 0x02;

/// How a pass-through command ended.
pub(crate) struct Done {
    /// Whether the port reported no error.
    pub(crate) ok: bool,
    /// PxTFD: status, and error above it.
    pub(crate) tfd: u32,
    /// The received D2H register FIS.
    pub(crate) rfis: [u8; 20],
    /// The bytes it moved (PRDBC).
    pub(crate) bytecount: u32,
    /// Its data buffer.
    pub(crate) buf: IoBuf,
}

/// Where a pass-through command's sender waits.
pub(crate) struct Waiter {
    done: SpinLock<Option<Done>>,
    wq: WaitQueue,
}

impl Waiter {
    /// Ends the command (from the interrupt thread).
    pub(crate) fn finish(&self, d: Done) {
        *self.done.lock() = Some(d);
        self.wq.wake_all();
    }
}

/// An H2D register FIS for `c` in slot `tag` (`ahci_setup_fis`).
fn fis_of(c: &AtaCmd, tag: u8) -> [u8; 20] {
    let mut f = [0u8; 20];
    f[0] = 0x27;
    f[1] = 0x80;
    f[2] = c.command;
    f[3] = c.features;
    f[4] = c.lba_low;
    f[5] = c.lba_mid;
    f[6] = c.lba_high;
    f[7] = c.device;
    f[8] = c.lba_low_exp;
    f[9] = c.lba_mid_exp;
    f[10] = c.lba_high_exp;
    f[11] = c.features_exp;
    f[12] = c.sector_count;
    if c.flags & ataio::FPDMA != 0 {
        f[12] = (f[12] & 0x07) | tag << 3;
    }
    f[13] = c.sector_count_exp;
    // ATA_A_4BIT.
    f[15] = 0x08;
    f
}

impl Port {
    /// Runs `io` (`XPT_ATA_IO`) on the port: the CAM status.
    pub(crate) fn ata_pass(&self, s: &Sleepable, io: &mut AtaIo) -> u32 {
        if io.cmd.flags & ataio::CONTROL != 0 {
            return status::REQ_INVALID;
        }
        let data = core::mem::take(&mut io.data);
        let len = u32::try_from(data.len()).unwrap_or(u32::MAX);
        let write = matches!(io.dir, Dir::Out | Dir::Both);
        let data = if data.is_empty() || io.dir == Dir::None {
            Data::None
        } else if write {
            match self.tag.map(IoBuf::Owned(data)) {
                Ok(m) => Data::Write(m),
                Err(_) => return status::RESRC_UNAVAIL,
            }
        } else {
            match self.tag.map(IoBuf::Owned(data)) {
                Ok(m) => Data::Read(m),
                Err(_) => return status::RESRC_UNAVAIL,
            }
        };
        let segs = match &data {
            Data::None => 0,
            Data::Read(d) => d.segments().len(),
            Data::Write(d) => d.segments().len(),
        };
        if segs > PRDS {
            io.data = data.unmap().into_vec();
            return status::REQ_TOO_BIG;
        }
        let fpdma = io.cmd.flags & ataio::FPDMA != 0;
        let w = Arc::new(Waiter {
            done: SpinLock::new(None),
            wq: WaitQueue::new(),
        });
        let mut f = Some(InFlight {
            req: Req::Pass(Arc::clone(&w)),
            data,
        });
        // Alone on the port: once what is outstanding has drained.
        loop {
            let mut st = self.state.lock(s);
            if st.drive.is_none() {
                if let Some(f) = f.take() {
                    io.data = f.data.unmap().into_vec();
                }
                return status::DEV_NOT_THERE;
            }
            if st.issued == 0
                && st.slots.iter().all(Option::is_none)
                && let Some(inflight) = f.take()
            {
                let segs: &[vm::dma::Segment] = match &inflight.data {
                    Data::None => &[],
                    Data::Read(d) => d.segments(),
                    Data::Write(d) => d.segments(),
                };
                Self::build(&st, 0, &fis_of(&io.cmd, 0), segs, write);
                st.slots[0] = Some(inflight);
                st.issued |= 1;
                if fpdma {
                    self.write(PX_SACT, 1);
                }
                self.write(PX_CI, 1);
                break;
            }
            drop(st);
            sleep_ms(s, 1);
        }
        let ms = if io.timeout_ms == 0 {
            DEFAULT_TIMEOUT_MS
        } else {
            io.timeout_ms
        };
        let mut ended = || w.done.lock().is_some();
        bus::manager().wait_timeout(s, &w.wq, &mut ended, u64::from(ms) * 1_000_000);
        let done = w.done.lock().take();
        let st = match done {
            Some(d) => {
                let failed = !d.ok || d.tfd & TFD_ERR != 0;
                if failed || io.cmd.flags & ataio::NEEDRESULT != 0 {
                    io.res = AtaRes {
                        flags: 0,
                        status: d.tfd as u8,
                        error: (d.tfd >> 8) as u8,
                        lba_low: d.rfis[4],
                        lba_mid: d.rfis[5],
                        lba_high: d.rfis[6],
                        device: d.rfis[7],
                        lba_low_exp: d.rfis[8],
                        lba_mid_exp: d.rfis[9],
                        lba_high_exp: d.rfis[10],
                        sector_count: d.rfis[12],
                        sector_count_exp: d.rfis[13],
                    };
                }
                if !fpdma && io.dir != Dir::None {
                    io.resid = len.saturating_sub(d.bytecount);
                }
                io.data = d.buf.into_vec();
                match (d.ok, d.tfd & TFD_ERR != 0) {
                    (true, false) => status::REQ_CMP,
                    (_, true) => status::ATA_STATUS_ERROR,
                    (false, false) => status::REQ_CMP_ERR,
                }
            }
            None => {
                // It never completed: stop the engine, which ends it.
                let mut st = self.state.lock(s);
                let held =
                    st.slots[0].take_if(|f| matches!(&f.req, Req::Pass(p) if Arc::ptr_eq(p, &w)));
                if held.is_some() {
                    self.write(PX_CMD, self.read(PX_CMD) & !CMD_ST);
                    self.wait_cmd_clear(s, crate::CMD_CR, 500);
                    st.issued = 0;
                    self.start_engine();
                }
                drop(st);
                match held {
                    Some(f) => io.data = f.data.unmap().into_vec(),
                    // It ended as it was taken.
                    None => {
                        if let Some(d) = w.done.lock().take() {
                            io.data = d.buf.into_vec();
                        }
                    }
                }
                status::CMD_TIMEOUT
            }
        };
        // What waited behind it goes now.
        let disk = self.disk.lock().clone();
        if let Some(d) = disk {
            d.resume(s, 0);
        }
        st
    }
}

/// The channel's SIM. CAM holds it until the bus goes; the port (and
/// with it the controller's registers) it holds weakly.
pub(crate) struct ChannelSim {
    pub(crate) port: Weak<Port>,
    pub(crate) unit: u32,
    /// The controller's capabilities and PCI IDs.
    pub(crate) cap: u32,
    pub(crate) ident: devtree::ids::PciIdent,
}

/// Identify word `i`.
fn word(id: &[u8; 512], i: usize) -> u16 {
    u16::from_le_bytes([id[2 * i], id[2 * i + 1]])
}

/// `ata_max_umode`, else `ata_max_pmode`'s: the transfer mode set.
fn mode(id: &[u8; 512]) -> i32 {
    // Word 53 bit 2: word 88 is valid.
    let udma = word(id, 88) & 0x7f;
    if word(id, 53) & 0x04 != 0 && udma != 0 {
        return ATA_UDMA0 + (15 - i32::try_from(udma.leading_zeros()).unwrap_or(15));
    }
    ATA_PIO4
}

impl cam::Sim for ChannelSim {
    fn path_inq(&self, _: u64) -> PathInq {
        let (cap, id) = (self.cap, self.ident);
        let mut inq = pi::SDTR_ABLE;
        if cap & CAP_SNCQ != 0 {
            inq |= pi::TAG_ABLE;
        }
        if cap & CAP_SPM != 0 {
            inq |= pi::SATAPM;
        }
        PathInq {
            version_num: 1,
            hba_inquiry: inq,
            hba_misc: pim::SEQSCAN | pim::UNMAPPED | pim::ATA_EXT,
            max_target: if cap & CAP_SPM != 0 { 15 } else { 0 },
            max_lun: 0,
            initiator_id: 0,
            bus_id: 0,
            base_transfer_speed: 150_000,
            sim_vid: "FreeBSD",
            hba_vid: "AHCI",
            dev_name: "ahcich",
            unit_number: self.unit,
            transport: xport::SATA,
            transport_version: abi::cam::VERSION_UNSPECIFIED,
            protocol: proto::ATA,
            protocol_version: abi::cam::VERSION_UNSPECIFIED,
            maxio: MAX_TRANSFER as u32,
            hba_vendor: id.vendor,
            hba_device: id.device,
            hba_subvendor: id.subvendor,
            hba_subdevice: id.subdevice,
            ..PathInq::default()
        }
    }

    fn trans_settings(&self, s: &Sleepable, _: u32, user: bool) -> TranSettings {
        let Some(port) = self.port.upgrade() else {
            return TranSettings::default();
        };
        let drive = port.state.lock(s).drive.clone();
        let mut x = TransSettingsSata {
            valid: cts::SATA_VALID_MODE
                | cts::SATA_VALID_BYTECOUNT
                | cts::SATA_VALID_PM
                | cts::SATA_VALID_TAGS
                | cts::SATA_VALID_ATAPI
                | cts::SATA_VALID_CAPS,
            ..TransSettingsSata::default()
        };
        // The link's generation, while it is up.
        let spd = (port.read(PX_SSTS) & 0xf0) >> 4;
        if !user && spd != 0 {
            x.revision = i32::try_from(spd).unwrap_or(0);
            x.valid |= cts::SATA_VALID_REVISION;
        }
        if let Some(d) = &drive {
            x.mode = mode(&d.ident);
            // PIO multi-sector transfers, at most 8 KiB.
            let multi = u32::from(word(&d.ident, 47) & 0xff).max(1);
            x.bytecount = (multi * 512).min(8192);
            x.tags = d.ncq.map_or(0, |n| u32::try_from(n).unwrap_or(0));
        }
        if self.cap & CAP_SNCQ != 0 {
            x.caps |= cts::SATA_CAPS_H_DMAAA;
        }
        x.caps |= cts::SATA_CAPS_H_AN;
        TranSettings {
            protocol: proto::UNSPECIFIED,
            protocol_version: abi::cam::VERSION_UNSPECIFIED,
            transport: xport::SATA,
            transport_version: abi::cam::VERSION_UNSPECIFIED,
            proto_specific: [0; 64],
            xport_specific: cam::ccb::trans_sata(&x),
        }
    }

    fn ata_io(&self, s: &Sleepable, target: u32, io: &mut AtaIo) -> u32 {
        let Some(port) = self.port.upgrade() else {
            return status::DEV_NOT_THERE;
        };
        if target != 0 {
            return status::SEL_TIMEOUT;
        }
        port.ata_pass(s, io)
    }
}

/// `ata_bswap`, `ata_btrim`, and `ata_bpack` on an identify string.
fn fix_string(b: &mut [u8]) {
    for w in b.as_chunks_mut::<2>().0 {
        w.swap(0, 1);
    }
    for c in b.iter_mut() {
        if *c == 0 || *c == b'_' {
            *c = b' ';
        }
    }
    for c in b.iter_mut().rev() {
        if *c != b' ' {
            break;
        }
        *c = 0;
    }
    // Runs of blanks become one; a leading one goes.
    let src = b.to_vec();
    let (mut j, mut blank) = (0, false);
    for (i, &c) in src.iter().enumerate() {
        if blank && c == b' ' {
            continue;
        }
        if blank && c != b' ' {
            b[j] = c;
            j += 1;
            blank = false;
            continue;
        }
        if c == b' ' {
            blank = true;
            if i == 0 {
                continue;
            }
        }
        b[j] = c;
        j += 1;
    }
    b[j..].fill(0);
}

/// `ata_param_fixup`: the identify data as CAM keeps it, its strings in
/// order and trimmed.
fn fixup(raw: &[u8; 512]) -> Box<[u8; 512]> {
    let mut b = Box::new(*raw);
    // Model numbers these vendors send unswapped (`FX`, `NEC`,
    // `Pioneer`, `SHARP`), as swapped bytes.
    let model = &raw[54..94];
    let unswapped = [&b"FX"[..], b"NEC", b"Pioneer", b"SHARP"]
        .iter()
        .any(|p| model.starts_with(p));
    for r in [54..94, 46..54, 20..40] {
        if unswapped {
            // Swap twice: only trimmed.
            for w in b[r.clone()].as_chunks_mut::<2>().0 {
                w.swap(0, 1);
            }
        }
        fix_string(&mut b[r]);
    }
    b
}

/// `ata_version`: the highest major version word 80 claims.
fn version(id: &[u8; 512]) -> u32 {
    let v = word(id, 80);
    if v == 0 || v == 0xffff {
        return 0;
    }
    (1..16).rev().find(|&i| v & (1 << i) != 0).unwrap_or(0)
}

/// CAM's view of `drive`, `ada` unit `unit` (`probedone`).
pub(crate) fn device_info(drive: &Drive, unit: u32) -> cam::DeviceInfo {
    let mut i = cam::DeviceInfo::new(0, 0, proto::ATA, ("ada", unit));
    i.protocol_version = version(&drive.ident);
    i.transport = xport::SATA;
    i.ident_data = fixup(&drive.ident);
    let serial = &i.ident_data[20..40];
    let n = serial.iter().position(|&c| c == 0).unwrap_or(serial.len());
    i.serial = serial[..n].to_vec();
    if let Some(depth) = drive.ncq {
        i.inq_flags |= SID_CMDQUE;
        i.openings = u32::try_from(depth).unwrap_or(1);
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identify_strings_as_cam_keeps_them() {
        // QEMU's model as the drive sends it: byte-swapped words,
        // space-padded.
        let mut raw = [0u8; 512];
        let model = b"QEMU HARDDISK                           ";
        for (i, w) in model.as_chunks::<2>().0.iter().enumerate() {
            raw[54 + 2 * i] = w[1];
            raw[54 + 2 * i + 1] = w[0];
        }
        let f = fixup(&raw);
        assert_eq!(&f[54..67], b"QEMU HARDDISK");
        assert!(f[67..94].iter().all(|&c| c == 0));
        // Swapped, underscores blanked, trailing blanks dropped, runs of
        // blanks made one and a leading one dropped.
        let mut b = *b"  A  B__C   ";
        fix_string(&mut b);
        assert_eq!(&b, b"AB C\0\0\0\0\0\0\0\0");
    }
}
