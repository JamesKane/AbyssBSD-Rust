// SPDX-License-Identifier: BSD-2-Clause
//! A CAM control block as the bytes `CAMIOCOMMAND` copies in and out,
//! read and written in place at the offsets `lib/abi`'s structures give.
//!
//! Every member a request uses is reached through an accessor here, so
//! the marshalling is one place, checked on the host against those
//! structures (and they, by `abi-check`, against FreeBSD's headers).

use core::mem::offset_of;

use abi::cam::{
    AtaCmd, AtaRes, CCB_SIZE, CcbAtaio, CcbDevAdvinfo, CcbDevMatch, CcbDevPosition, CcbGetdev,
    CcbGetdevlist, CcbGetdevstats, CcbHdr, CcbNvmeio, CcbPathinq, CcbScsiio, CcbTransSettings,
};
use abi::nvme::{NvmeCommand, NvmeCompletion};

/// A CCB, by value.
#[derive(Clone, PartialEq, Eq)]
pub struct Ccb {
    b: [u8; CCB_SIZE],
}

impl core::fmt::Debug for Ccb {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Ccb")
            .field("func_code", &self.func_code())
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

impl Default for Ccb {
    fn default() -> Self {
        Self { b: [0; CCB_SIZE] }
    }
}

/// Offsets of the header's members.
mod hdr {
    use super::{CcbHdr, offset_of};
    pub(super) const PINFO: usize = offset_of!(CcbHdr, pinfo);
    pub(super) const RETRY_COUNT: usize = offset_of!(CcbHdr, retry_count);
    pub(super) const FUNC_CODE: usize = offset_of!(CcbHdr, func_code);
    pub(super) const STATUS: usize = offset_of!(CcbHdr, status);
    pub(super) const PATH_ID: usize = offset_of!(CcbHdr, path_id);
    pub(super) const TARGET_ID: usize = offset_of!(CcbHdr, target_id);
    pub(super) const TARGET_LUN: usize = offset_of!(CcbHdr, target_lun);
    pub(super) const FLAGS: usize = offset_of!(CcbHdr, flags);
    pub(super) const PERIPH_PRIV: usize = offset_of!(CcbHdr, periph_priv);
    pub(super) const TIMEOUT: usize = offset_of!(CcbHdr, timeout);
    pub(super) const SIZE: usize = size_of::<CcbHdr>();
}

/// The user's view of a header after the kernel ran it: the members a
/// peripheral driver may set kept (`xpt_merge_ccb`), the path the
/// kernel's, its pointers not shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Path {
    /// The bus.
    pub path_id: u32,
    /// The target.
    pub target_id: u32,
    /// The LUN.
    pub target_lun: u64,
}

macro_rules! rw {
    ($get:ident, $set:ident, $t:ty) => {
        fn $get(&self, o: usize) -> $t {
            let mut a = [0u8; size_of::<$t>()];
            a.copy_from_slice(&self.b[o..o + size_of::<$t>()]);
            <$t>::from_ne_bytes(a)
        }

        fn $set(&mut self, o: usize, v: $t) {
            self.b[o..o + size_of::<$t>()].copy_from_slice(&v.to_ne_bytes());
        }
    };
}

/// Generates a getter and setter for a member at a fixed offset.
macro_rules! member {
    ($get:ident, $set:ident, $t:ty, $off:expr, $rd:ident, $wr:ident) => {
        #[doc = concat!("`", stringify!($get), "`.")]
        #[must_use]
        pub fn $get(&self) -> $t {
            self.$rd($off)
        }

        #[doc = concat!("Sets `", stringify!($get), "`.")]
        pub fn $set(&mut self, v: $t) {
            self.$wr($off, v);
        }
    };
}

impl Ccb {
    /// The CCB in `arg`, if it is a whole one.
    #[must_use]
    pub fn from_bytes(arg: &[u8]) -> Option<Self> {
        let b: [u8; CCB_SIZE] = arg.try_into().ok()?;
        Some(Self { b })
    }

    /// Its bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.b
    }

    rw!(rd8, wr8, u8);
    rw!(rd16, wr16, u16);
    rw!(rd32, wr32, u32);
    rw!(rd64, wr64, u64);

    fn bytes(&self, o: usize, n: usize) -> &[u8] {
        &self.b[o..o + n]
    }

    fn put(&mut self, o: usize, n: usize, v: &[u8]) {
        let n = n.min(v.len());
        self.b[o..o + n].copy_from_slice(&v[..n]);
    }

    fn put_zeroed(&mut self, o: usize, n: usize, v: &[u8]) {
        self.b[o..o + n].fill(0);
        self.put(o, n, v);
    }

    // The header.
    member!(func_code, set_func_code, u32, hdr::FUNC_CODE, rd32, wr32);
    member!(status, set_status, u32, hdr::STATUS, rd32, wr32);
    member!(flags, set_flags, u32, hdr::FLAGS, rd32, wr32);
    member!(path_id, set_path_id, u32, hdr::PATH_ID, rd32, wr32);
    member!(target_id, set_target_id, u32, hdr::TARGET_ID, rd32, wr32);
    member!(target_lun, set_target_lun, u64, hdr::TARGET_LUN, rd64, wr64);
    member!(timeout, set_timeout, u32, hdr::TIMEOUT, rd32, wr32);
    member!(
        retry_count,
        set_retry_count,
        u16,
        hdr::RETRY_COUNT,
        rd16,
        wr16
    );

    /// The header as the kernel hands it back after running the CCB on
    /// `path` (`xpt_setup_ccb` then `xpt_merge_ccb`): the user's
    /// retry count, function code, timeout, flags, priority, and
    /// peripheral-private words; the path's IDs; nothing of the kernel's
    /// queues and pointers.
    pub fn set_path(&mut self, path: Path) {
        let mut h = [0u8; hdr::SIZE];
        // The priority (pinfo's first word) and the periph's own words.
        h[hdr::PINFO..hdr::PINFO + 4].copy_from_slice(self.bytes(hdr::PINFO, 4));
        h[hdr::PERIPH_PRIV..hdr::PERIPH_PRIV + 16]
            .copy_from_slice(self.bytes(hdr::PERIPH_PRIV, 16));
        for o in [
            hdr::RETRY_COUNT,
            hdr::FUNC_CODE,
            hdr::STATUS,
            hdr::FLAGS,
            hdr::TIMEOUT,
        ] {
            let n = if o == hdr::RETRY_COUNT { 2 } else { 4 };
            h[o..o + n].copy_from_slice(self.bytes(o, n));
        }
        self.b[..hdr::SIZE].copy_from_slice(&h);
        self.set_path_id(path.path_id);
        self.set_target_id(path.target_id);
        self.set_target_lun(path.target_lun);
    }

    /// The header's path.
    #[must_use]
    pub fn path(&self) -> Path {
        Path {
            path_id: self.path_id(),
            target_id: self.target_id(),
            target_lun: self.target_lun(),
        }
    }

    // `struct ccb_getdev`.
    member!(
        cgd_protocol,
        set_cgd_protocol,
        u32,
        offset_of!(CcbGetdev, protocol),
        rd32,
        wr32
    );

    /// Sets `XPT_GDEV_TYPE`'s data.
    pub fn set_cgd(&mut self, inq: &[u8], ident: &[u8], inq_flags: u8, serial: &[u8]) {
        self.put_zeroed(offset_of!(CcbGetdev, inq_data), 256, inq);
        self.put_zeroed(offset_of!(CcbGetdev, ident_data), 512, ident);
        self.wr8(offset_of!(CcbGetdev, inq_flags), inq_flags);
        let n = serial.len().min(252);
        self.put(offset_of!(CcbGetdev, serial_num), n, serial);
        self.wr8(
            offset_of!(CcbGetdev, serial_num_len),
            u8::try_from(n).unwrap_or(u8::MAX),
        );
    }

    // `struct ccb_getdevlist`.
    member!(
        cgdl_unit,
        set_cgdl_unit,
        u32,
        offset_of!(CcbGetdevlist, unit_number),
        rd32,
        wr32
    );
    member!(
        cgdl_generation,
        set_cgdl_generation,
        u32,
        offset_of!(CcbGetdevlist, generation),
        rd32,
        wr32
    );
    member!(
        cgdl_index,
        set_cgdl_index,
        u32,
        offset_of!(CcbGetdevlist, index),
        rd32,
        wr32
    );
    member!(
        cgdl_status,
        set_cgdl_status,
        u32,
        offset_of!(CcbGetdevlist, status),
        rd32,
        wr32
    );

    /// `periph_name`, up to its NUL.
    #[must_use]
    pub fn cgdl_name(&self) -> &[u8] {
        cstr(self.bytes(offset_of!(CcbGetdevlist, periph_name), 16))
    }

    /// Sets `periph_name` (NUL-padded).
    pub fn set_cgdl_name(&mut self, name: &[u8]) {
        let n = name.len().min(15);
        self.put_zeroed(offset_of!(CcbGetdevlist, periph_name), 16, &name[..n]);
    }

    /// Sets the whole of `XPT_PATH_INQ`'s answer.
    pub fn set_cpi(&mut self, p: &PathInq) {
        type P = CcbPathinq;
        self.wr8(offset_of!(P, version_num), p.version_num);
        self.wr8(offset_of!(P, hba_inquiry), p.hba_inquiry);
        self.wr16(offset_of!(P, target_sprt), p.target_sprt);
        self.wr32(offset_of!(P, hba_misc), p.hba_misc);
        self.wr16(offset_of!(P, hba_eng_cnt), 0);
        self.wr32(offset_of!(P, max_target), p.max_target);
        self.wr32(offset_of!(P, max_lun), p.max_lun);
        self.wr32(offset_of!(P, initiator_id), p.initiator_id);
        self.put_zeroed(offset_of!(P, sim_vid), 16, p.sim_vid.as_bytes());
        self.put_zeroed(offset_of!(P, hba_vid), 16, p.hba_vid.as_bytes());
        self.put_zeroed(offset_of!(P, dev_name), 16, p.dev_name.as_bytes());
        self.wr32(offset_of!(P, unit_number), p.unit_number);
        self.wr32(offset_of!(P, bus_id), p.bus_id);
        self.wr32(offset_of!(P, base_transfer_speed), p.base_transfer_speed);
        self.wr32(offset_of!(P, protocol), p.protocol);
        self.wr32(offset_of!(P, protocol_version), p.protocol_version);
        self.wr32(offset_of!(P, transport), p.transport);
        self.wr32(offset_of!(P, transport_version), p.transport_version);
        if let Some(x) = &p.xport_specific {
            self.put_zeroed(offset_of!(P, xport_specific), 128, x);
        }
        self.wr32(offset_of!(P, maxio), p.maxio);
        self.wr16(offset_of!(P, hba_vendor), p.hba_vendor);
        self.wr16(offset_of!(P, hba_device), p.hba_device);
        self.wr16(offset_of!(P, hba_subvendor), p.hba_subvendor);
        self.wr16(offset_of!(P, hba_subdevice), p.hba_subdevice);
    }

    // `struct ccb_trans_settings`.
    member!(
        cts_type,
        set_cts_type,
        u32,
        offset_of!(CcbTransSettings, type_),
        rd32,
        wr32
    );

    /// Sets `XPT_GET_TRAN_SETTINGS`'s answer.
    pub fn set_cts(&mut self, t: &TranSettings) {
        type T = CcbTransSettings;
        self.wr32(offset_of!(T, protocol), t.protocol);
        self.wr32(offset_of!(T, protocol_version), t.protocol_version);
        self.wr32(offset_of!(T, transport), t.transport);
        self.wr32(offset_of!(T, transport_version), t.transport_version);
        self.put_zeroed(offset_of!(T, proto_specific), 64, &t.proto_specific);
        self.put_zeroed(offset_of!(T, xport_specific), 32, &t.xport_specific);
    }

    // `struct ccb_dev_match`.
    member!(
        cdm_status,
        set_cdm_status,
        u32,
        offset_of!(CcbDevMatch, status),
        rd32,
        wr32
    );
    member!(
        cdm_num_patterns,
        set_cdm_num_patterns,
        u32,
        offset_of!(CcbDevMatch, num_patterns),
        rd32,
        wr32
    );
    member!(
        cdm_pattern_buf_len,
        set_cdm_pattern_buf_len,
        u32,
        offset_of!(CcbDevMatch, pattern_buf_len),
        rd32,
        wr32
    );
    member!(
        cdm_patterns,
        set_cdm_patterns,
        u64,
        offset_of!(CcbDevMatch, patterns),
        rd64,
        wr64
    );
    member!(
        cdm_num_matches,
        set_cdm_num_matches,
        u32,
        offset_of!(CcbDevMatch, num_matches),
        rd32,
        wr32
    );
    member!(
        cdm_match_buf_len,
        set_cdm_match_buf_len,
        u32,
        offset_of!(CcbDevMatch, match_buf_len),
        rd32,
        wr32
    );
    member!(
        cdm_matches,
        set_cdm_matches,
        u64,
        offset_of!(CcbDevMatch, matches),
        rd64,
        wr64
    );

    /// `pos`.
    #[must_use]
    pub fn cdm_pos(&self) -> Position {
        type D = CcbDevPosition;
        let base = offset_of!(CcbDevMatch, pos);
        let mut p = Position::default();
        for (i, g) in p.generations.iter_mut().enumerate() {
            *g = self.rd32(base + offset_of!(D, generations) + 4 * i);
        }
        p.position_type = self.rd32(base + offset_of!(D, position_type));
        for (i, c) in p.cookie.iter_mut().enumerate() {
            *c = self.rd64(base + offset_of!(D, cookie) + 8 * i);
        }
        p
    }

    /// Sets `pos`.
    pub fn set_cdm_pos(&mut self, p: &Position) {
        type D = CcbDevPosition;
        let base = offset_of!(CcbDevMatch, pos);
        self.b[base..base + size_of::<D>()].fill(0);
        for (i, g) in p.generations.iter().enumerate() {
            self.wr32(base + offset_of!(D, generations) + 4 * i, *g);
        }
        self.wr32(base + offset_of!(D, position_type), p.position_type);
        for (i, c) in p.cookie.iter().enumerate() {
            self.wr64(base + offset_of!(D, cookie) + 8 * i, *c);
        }
    }

    // `struct ccb_scsiio`.
    member!(
        csio_data_ptr,
        set_csio_data_ptr,
        u64,
        offset_of!(CcbScsiio, data_ptr),
        rd64,
        wr64
    );
    member!(
        csio_dxfer_len,
        set_csio_dxfer_len,
        u32,
        offset_of!(CcbScsiio, dxfer_len),
        rd32,
        wr32
    );
    member!(
        csio_sense_len,
        set_csio_sense_len,
        u8,
        offset_of!(CcbScsiio, sense_len),
        rd8,
        wr8
    );
    member!(
        csio_cdb_len,
        set_csio_cdb_len,
        u8,
        offset_of!(CcbScsiio, cdb_len),
        rd8,
        wr8
    );
    member!(
        csio_scsi_status,
        set_csio_scsi_status,
        u8,
        offset_of!(CcbScsiio, scsi_status),
        rd8,
        wr8
    );
    member!(
        csio_sense_resid,
        set_csio_sense_resid,
        u8,
        offset_of!(CcbScsiio, sense_resid),
        rd8,
        wr8
    );
    member!(
        csio_resid,
        set_csio_resid,
        u32,
        offset_of!(CcbScsiio, resid),
        rd32,
        wr32
    );
    member!(
        csio_cdb_ptr,
        set_csio_cdb_ptr,
        u64,
        offset_of!(CcbScsiio, cdb_io),
        rd64,
        wr64
    );

    /// The CDB's bytes, when they are in the CCB (not `CAM_CDB_POINTER`).
    #[must_use]
    pub fn csio_cdb_bytes(&self) -> [u8; 16] {
        let mut c = [0u8; 16];
        c.copy_from_slice(self.bytes(offset_of!(CcbScsiio, cdb_io), 16));
        c
    }

    /// Sets the CDB's bytes.
    pub fn set_csio_cdb_bytes(&mut self, cdb: &[u8]) {
        self.put_zeroed(offset_of!(CcbScsiio, cdb_io), 16, cdb);
    }

    /// Sets the sense data (the rest of `sense_data` left as it was).
    pub fn set_csio_sense(&mut self, sense: &[u8]) {
        let n = sense.len().min(252);
        self.put(offset_of!(CcbScsiio, sense_data), n, sense);
    }

    /// The sense data.
    #[must_use]
    pub fn csio_sense(&self) -> &[u8] {
        self.bytes(offset_of!(CcbScsiio, sense_data), 252)
    }

    // `struct ccb_ataio`.
    member!(
        ataio_data_ptr,
        set_ataio_data_ptr,
        u64,
        offset_of!(CcbAtaio, data_ptr),
        rd64,
        wr64
    );
    member!(
        ataio_dxfer_len,
        set_ataio_dxfer_len,
        u32,
        offset_of!(CcbAtaio, dxfer_len),
        rd32,
        wr32
    );
    member!(
        ataio_resid,
        set_ataio_resid,
        u32,
        offset_of!(CcbAtaio, resid),
        rd32,
        wr32
    );
    member!(
        ataio_aux,
        set_ataio_aux,
        u32,
        offset_of!(CcbAtaio, aux),
        rd32,
        wr32
    );
    member!(
        ataio_ata_flags,
        set_ataio_ata_flags,
        u8,
        offset_of!(CcbAtaio, ata_flags),
        rd8,
        wr8
    );

    /// The ATA command's registers.
    #[must_use]
    pub fn ataio_cmd(&self) -> AtaCmd {
        let b = self.bytes(offset_of!(CcbAtaio, cmd), size_of::<AtaCmd>());
        AtaCmd {
            flags: b[offset_of!(AtaCmd, flags)],
            command: b[offset_of!(AtaCmd, command)],
            features: b[offset_of!(AtaCmd, features)],
            lba_low: b[offset_of!(AtaCmd, lba_low)],
            lba_mid: b[offset_of!(AtaCmd, lba_mid)],
            lba_high: b[offset_of!(AtaCmd, lba_high)],
            device: b[offset_of!(AtaCmd, device)],
            lba_low_exp: b[offset_of!(AtaCmd, lba_low_exp)],
            lba_mid_exp: b[offset_of!(AtaCmd, lba_mid_exp)],
            lba_high_exp: b[offset_of!(AtaCmd, lba_high_exp)],
            features_exp: b[offset_of!(AtaCmd, features_exp)],
            sector_count: b[offset_of!(AtaCmd, sector_count)],
            sector_count_exp: b[offset_of!(AtaCmd, sector_count_exp)],
            control: b[offset_of!(AtaCmd, control)],
        }
    }

    /// Sets the ATA command's registers.
    pub fn set_ataio_cmd(&mut self, c: &AtaCmd) {
        let o = offset_of!(CcbAtaio, cmd);
        for (f, v) in [
            (offset_of!(AtaCmd, flags), c.flags),
            (offset_of!(AtaCmd, command), c.command),
            (offset_of!(AtaCmd, features), c.features),
            (offset_of!(AtaCmd, lba_low), c.lba_low),
            (offset_of!(AtaCmd, lba_mid), c.lba_mid),
            (offset_of!(AtaCmd, lba_high), c.lba_high),
            (offset_of!(AtaCmd, device), c.device),
            (offset_of!(AtaCmd, lba_low_exp), c.lba_low_exp),
            (offset_of!(AtaCmd, lba_mid_exp), c.lba_mid_exp),
            (offset_of!(AtaCmd, lba_high_exp), c.lba_high_exp),
            (offset_of!(AtaCmd, features_exp), c.features_exp),
            (offset_of!(AtaCmd, sector_count), c.sector_count),
            (offset_of!(AtaCmd, sector_count_exp), c.sector_count_exp),
            (offset_of!(AtaCmd, control), c.control),
        ] {
            self.wr8(o + f, v);
        }
    }

    /// The ATA result registers.
    #[must_use]
    pub fn ataio_res(&self) -> AtaRes {
        let b = self.bytes(offset_of!(CcbAtaio, res), size_of::<AtaRes>());
        AtaRes {
            flags: b[offset_of!(AtaRes, flags)],
            status: b[offset_of!(AtaRes, status)],
            error: b[offset_of!(AtaRes, error)],
            lba_low: b[offset_of!(AtaRes, lba_low)],
            lba_mid: b[offset_of!(AtaRes, lba_mid)],
            lba_high: b[offset_of!(AtaRes, lba_high)],
            device: b[offset_of!(AtaRes, device)],
            lba_low_exp: b[offset_of!(AtaRes, lba_low_exp)],
            lba_mid_exp: b[offset_of!(AtaRes, lba_mid_exp)],
            lba_high_exp: b[offset_of!(AtaRes, lba_high_exp)],
            sector_count: b[offset_of!(AtaRes, sector_count)],
            sector_count_exp: b[offset_of!(AtaRes, sector_count_exp)],
        }
    }

    /// Sets the ATA result registers.
    pub fn set_ataio_res(&mut self, r: &AtaRes) {
        let o = offset_of!(CcbAtaio, res);
        for (f, v) in [
            (offset_of!(AtaRes, flags), r.flags),
            (offset_of!(AtaRes, status), r.status),
            (offset_of!(AtaRes, error), r.error),
            (offset_of!(AtaRes, lba_low), r.lba_low),
            (offset_of!(AtaRes, lba_mid), r.lba_mid),
            (offset_of!(AtaRes, lba_high), r.lba_high),
            (offset_of!(AtaRes, device), r.device),
            (offset_of!(AtaRes, lba_low_exp), r.lba_low_exp),
            (offset_of!(AtaRes, lba_mid_exp), r.lba_mid_exp),
            (offset_of!(AtaRes, lba_high_exp), r.lba_high_exp),
            (offset_of!(AtaRes, sector_count), r.sector_count),
            (offset_of!(AtaRes, sector_count_exp), r.sector_count_exp),
        ] {
            self.wr8(o + f, v);
        }
    }

    // `struct ccb_nvmeio`.
    member!(
        nvmeio_data_ptr,
        set_nvmeio_data_ptr,
        u64,
        offset_of!(CcbNvmeio, data_ptr),
        rd64,
        wr64
    );
    member!(
        nvmeio_dxfer_len,
        set_nvmeio_dxfer_len,
        u32,
        offset_of!(CcbNvmeio, dxfer_len),
        rd32,
        wr32
    );

    /// The NVMe command.
    #[must_use]
    pub fn nvmeio_cmd(&self) -> NvmeCommand {
        let o = offset_of!(CcbNvmeio, cmd);
        nvme_command(&self.b[o..o + size_of::<NvmeCommand>()])
    }

    /// Sets the NVMe command.
    pub fn set_nvmeio_cmd(&mut self, c: &NvmeCommand) {
        let o = offset_of!(CcbNvmeio, cmd);
        put_nvme_command(&mut self.b[o..o + size_of::<NvmeCommand>()], c);
    }

    /// The NVMe completion.
    #[must_use]
    pub fn nvmeio_cpl(&self) -> NvmeCompletion {
        let o = offset_of!(CcbNvmeio, cpl);
        nvme_completion(&self.b[o..o + size_of::<NvmeCompletion>()])
    }

    /// Sets the NVMe completion.
    pub fn set_nvmeio_cpl(&mut self, c: &NvmeCompletion) {
        let o = offset_of!(CcbNvmeio, cpl);
        put_nvme_completion(&mut self.b[o..o + size_of::<NvmeCompletion>()], c);
    }

    // `struct ccb_dev_advinfo`.
    member!(
        cdai_flags,
        set_cdai_flags,
        u32,
        offset_of!(CcbDevAdvinfo, flags),
        rd32,
        wr32
    );
    member!(
        cdai_buftype,
        set_cdai_buftype,
        u32,
        offset_of!(CcbDevAdvinfo, buftype),
        rd32,
        wr32
    );
    member!(
        cdai_bufsiz,
        set_cdai_bufsiz,
        u64,
        offset_of!(CcbDevAdvinfo, bufsiz),
        rd64,
        wr64
    );
    member!(
        cdai_provsiz,
        set_cdai_provsiz,
        u64,
        offset_of!(CcbDevAdvinfo, provsiz),
        rd64,
        wr64
    );
    member!(
        cdai_buf,
        set_cdai_buf,
        u64,
        offset_of!(CcbDevAdvinfo, buf),
        rd64,
        wr64
    );

    /// Sets `XPT_GDEV_STATS`'s answer: openings, and the tag limits.
    pub fn set_cgds(&mut self, openings: i32, maxtags: i32, mintags: i32) {
        type G = CcbGetdevstats;
        let base = hdr::SIZE;
        self.b[base..size_of::<G>()].fill(0);
        for (o, v) in [
            (offset_of!(G, dev_openings), openings),
            (offset_of!(G, maxtags), maxtags),
            (offset_of!(G, mintags), mintags),
        ] {
            self.wr32(o, v.cast_unsigned());
        }
    }
}

/// `pos`, as the user's CCB holds it between `XPT_DEV_MATCH` calls.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Position {
    /// Bus, target, device, and peripheral generations.
    pub generations: [u32; 4],
    /// `CAM_DEV_POS_*`.
    pub position_type: u32,
    /// Where it stopped: bus, target, device, peripheral, peripheral
    /// driver (opaque numbers; zero for none).
    pub cookie: [u64; 5],
}

/// What a SIM answers `XPT_PATH_INQ` with.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PathInq {
    pub version_num: u8,
    pub hba_inquiry: u8,
    pub target_sprt: u16,
    pub hba_misc: u32,
    pub max_target: u32,
    pub max_lun: u32,
    pub initiator_id: u32,
    pub sim_vid: &'static str,
    pub hba_vid: &'static str,
    pub dev_name: &'static str,
    pub unit_number: u32,
    pub bus_id: u32,
    pub base_transfer_speed: u32,
    pub protocol: u32,
    pub protocol_version: u32,
    pub transport: u32,
    pub transport_version: u32,
    /// The transport's part (`xport_specific`), if the SIM sets it.
    pub xport_specific: Option<[u8; 128]>,
    pub maxio: u32,
    pub hba_vendor: u16,
    pub hba_device: u16,
    pub hba_subvendor: u16,
    pub hba_subdevice: u16,
}

/// What a SIM answers `XPT_GET_TRAN_SETTINGS` with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranSettings {
    pub protocol: u32,
    pub protocol_version: u32,
    pub transport: u32,
    pub transport_version: u32,
    /// `proto_specific`'s bytes.
    pub proto_specific: [u8; 64],
    /// `xport_specific`'s bytes.
    pub xport_specific: [u8; 32],
}

impl Default for TranSettings {
    fn default() -> Self {
        Self {
            protocol: 0,
            protocol_version: 0,
            transport: 0,
            transport_version: 0,
            proto_specific: [0; 64],
            xport_specific: [0; 32],
        }
    }
}

/// The bytes of `struct ccb_pathinq_settings_nvme`, as `xport_specific`.
#[must_use]
pub fn pathinq_nvme(p: &abi::cam::PathinqNvme) -> [u8; 128] {
    use abi::cam::PathinqNvme as N;
    let mut b = [0u8; 128];
    b[offset_of!(N, nsid)..][..4].copy_from_slice(&p.nsid.to_ne_bytes());
    b[offset_of!(N, domain)..][..4].copy_from_slice(&p.domain.to_ne_bytes());
    b[offset_of!(N, bus)] = p.bus;
    b[offset_of!(N, slot)] = p.slot;
    b[offset_of!(N, function)] = p.function;
    b[offset_of!(N, extra)] = p.extra;
    b[offset_of!(N, dev_name)..][..52].copy_from_slice(&p.dev_name);
    b
}

/// The bytes of `struct ccb_trans_settings_sata`.
#[must_use]
pub fn trans_sata(t: &abi::cam::TransSettingsSata) -> [u8; 32] {
    use abi::cam::TransSettingsSata as S;
    let mut b = [0u8; 32];
    for (o, v) in [
        (offset_of!(S, valid), t.valid),
        (offset_of!(S, mode), t.mode.cast_unsigned()),
        (offset_of!(S, bytecount), t.bytecount),
        (offset_of!(S, revision), t.revision.cast_unsigned()),
        (offset_of!(S, pm_present), t.pm_present),
        (offset_of!(S, tags), t.tags),
        (offset_of!(S, atapi), t.atapi),
        (offset_of!(S, caps), t.caps),
    ] {
        b[o..o + 4].copy_from_slice(&v.to_ne_bytes());
    }
    b
}

/// The bytes of `struct ccb_trans_settings_nvme`, at the start of a
/// union of `N` bytes.
#[must_use]
pub fn trans_nvme<const N: usize>(t: &abi::cam::TransSettingsNvme) -> [u8; N] {
    use abi::cam::TransSettingsNvme as T;
    let mut b = [0u8; N];
    for (o, v) in [
        (offset_of!(T, valid), t.valid),
        (offset_of!(T, spec), t.spec),
        (offset_of!(T, max_xfer), t.max_xfer),
        (offset_of!(T, caps), t.caps),
    ] {
        b[o..o + 4].copy_from_slice(&v.to_ne_bytes());
    }
    b[offset_of!(T, lanes)] = t.lanes;
    b[offset_of!(T, speed)] = t.speed;
    b[offset_of!(T, max_lanes)] = t.max_lanes;
    b[offset_of!(T, max_speed)] = t.max_speed;
    b
}

/// The bytes of `struct ccb_trans_settings_scsi` (or `_ata`), as
/// `proto_specific`.
#[must_use]
pub fn trans_scsi(t: &abi::cam::TransSettingsScsi) -> [u8; 64] {
    use abi::cam::TransSettingsScsi as T;
    let mut b = [0u8; 64];
    b[offset_of!(T, valid)..][..4].copy_from_slice(&t.valid.to_ne_bytes());
    b[offset_of!(T, flags)..][..4].copy_from_slice(&t.flags.to_ne_bytes());
    b
}

/// The `valid` word starting a transfer-settings union.
#[must_use]
pub fn union_valid(b: &[u8]) -> u32 {
    let mut a = [0u8; 4];
    a.copy_from_slice(&b[..4]);
    u32::from_ne_bytes(a)
}

/// An NVMe command from its 64 bytes.
#[must_use]
pub fn nvme_command(b: &[u8]) -> NvmeCommand {
    type C = NvmeCommand;
    let u32_at = |o: usize| {
        let mut a = [0u8; 4];
        a.copy_from_slice(&b[o..o + 4]);
        u32::from_le_bytes(a)
    };
    let u64_at = |o: usize| {
        let mut a = [0u8; 8];
        a.copy_from_slice(&b[o..o + 8]);
        u64::from_le_bytes(a)
    };
    NvmeCommand {
        opc: b[offset_of!(C, opc)],
        fuse: b[offset_of!(C, fuse)],
        cid: u16::from_le_bytes([b[offset_of!(C, cid)], b[offset_of!(C, cid) + 1]]),
        nsid: u32_at(offset_of!(C, nsid)),
        rsvd2: u32_at(offset_of!(C, rsvd2)),
        rsvd3: u32_at(offset_of!(C, rsvd3)),
        mptr: u64_at(offset_of!(C, mptr)),
        prp1: u64_at(offset_of!(C, prp1)),
        prp2: u64_at(offset_of!(C, prp2)),
        cdw10: u32_at(offset_of!(C, cdw10)),
        cdw11: u32_at(offset_of!(C, cdw11)),
        cdw12: u32_at(offset_of!(C, cdw12)),
        cdw13: u32_at(offset_of!(C, cdw13)),
        cdw14: u32_at(offset_of!(C, cdw14)),
        cdw15: u32_at(offset_of!(C, cdw15)),
    }
}

/// Writes an NVMe command's 64 bytes.
pub fn put_nvme_command(b: &mut [u8], c: &NvmeCommand) {
    type C = NvmeCommand;
    b[offset_of!(C, opc)] = c.opc;
    b[offset_of!(C, fuse)] = c.fuse;
    b[offset_of!(C, cid)..][..2].copy_from_slice(&c.cid.to_le_bytes());
    for (o, v) in [
        (offset_of!(C, nsid), c.nsid),
        (offset_of!(C, rsvd2), c.rsvd2),
        (offset_of!(C, rsvd3), c.rsvd3),
        (offset_of!(C, cdw10), c.cdw10),
        (offset_of!(C, cdw11), c.cdw11),
        (offset_of!(C, cdw12), c.cdw12),
        (offset_of!(C, cdw13), c.cdw13),
        (offset_of!(C, cdw14), c.cdw14),
        (offset_of!(C, cdw15), c.cdw15),
    ] {
        b[o..o + 4].copy_from_slice(&v.to_le_bytes());
    }
    for (o, v) in [
        (offset_of!(C, mptr), c.mptr),
        (offset_of!(C, prp1), c.prp1),
        (offset_of!(C, prp2), c.prp2),
    ] {
        b[o..o + 8].copy_from_slice(&v.to_le_bytes());
    }
}

/// An NVMe completion from its 16 bytes.
#[must_use]
pub fn nvme_completion(b: &[u8]) -> NvmeCompletion {
    type P = NvmeCompletion;
    let u16_at = |o: usize| u16::from_le_bytes([b[o], b[o + 1]]);
    let u32_at = |o: usize| {
        let mut a = [0u8; 4];
        a.copy_from_slice(&b[o..o + 4]);
        u32::from_le_bytes(a)
    };
    NvmeCompletion {
        cdw0: u32_at(offset_of!(P, cdw0)),
        rsvd1: u32_at(offset_of!(P, rsvd1)),
        sqhd: u16_at(offset_of!(P, sqhd)),
        sqid: u16_at(offset_of!(P, sqid)),
        cid: u16_at(offset_of!(P, cid)),
        status: u16_at(offset_of!(P, status)),
    }
}

/// Writes an NVMe completion's 16 bytes.
pub fn put_nvme_completion(b: &mut [u8], c: &NvmeCompletion) {
    type P = NvmeCompletion;
    b[offset_of!(P, cdw0)..][..4].copy_from_slice(&c.cdw0.to_le_bytes());
    b[offset_of!(P, rsvd1)..][..4].copy_from_slice(&c.rsvd1.to_le_bytes());
    for (o, v) in [
        (offset_of!(P, sqhd), c.sqhd),
        (offset_of!(P, sqid), c.sqid),
        (offset_of!(P, cid), c.cid),
        (offset_of!(P, status), c.status),
    ] {
        b[o..o + 2].copy_from_slice(&v.to_le_bytes());
    }
}

/// `b` up to its first NUL.
#[must_use]
pub fn cstr(b: &[u8]) -> &[u8] {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    &b[..n]
}
