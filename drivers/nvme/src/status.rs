// SPDX-License-Identifier: BSD-2-Clause
//! Completion status and command printouts: which failed commands are
//! retried, and the lines FreeBSD prints for a failed command and its
//! completion.
//!
//! Behavior reference: sys/dev/nvme/nvme_qpair.c
//! (`nvme_completion_is_retry`, `nvme_qpair_print_command`,
//! `nvme_qpair_print_completion`), sys/dev/nvme/nvme_util.c (the names).

use core::fmt;

/// A completion's status field (completion dword 3 bits 31:17): status
/// code in bits 7:0, type in 10:8, CRD in 12:11, More in 13, Do Not
/// Retry in 14. 0 is success.
pub(crate) type Status = u16;

/// Status code types.
const SCT_GENERIC: u16 = 0;
const SCT_COMMAND_SPECIFIC: u16 = 1;
const SCT_MEDIA_ERROR: u16 = 2;
const SCT_PATH_RELATED: u16 = 3;
const SCT_VENDOR_SPECIFIC: u16 = 7;

/// Generic status codes the driver itself completes with.
pub(crate) const SC_ABORTED_BY_REQUEST: u16 = 0x07;
const SC_NAMESPACE_NOT_READY: u16 = 0x82;
/// Path-related.
const SC_INTERNAL_PATH_ERROR: u16 = 0x00;

pub(crate) fn sc(s: Status) -> u16 {
    s & 0xff
}

pub(crate) fn sct(s: Status) -> u16 {
    (s >> 8) & 0x7
}

fn crd(s: Status) -> u16 {
    (s >> 11) & 0x3
}

fn more(s: Status) -> u16 {
    (s >> 13) & 1
}

pub(crate) fn dnr(s: Status) -> bool {
    (s >> 14) & 1 != 0
}

/// A status the driver makes up (`nvme_qpair_make_status`).
pub(crate) fn make(sct: u16, sc: u16, dnr: bool) -> Status {
    (sc & 0xff) | (sct & 0x7) << 8 | u16::from(dnr) << 14
}

/// Whether a failed command may be sent again (`nvme_completion_is_retry`).
pub(crate) fn is_retry(s: Status) -> bool {
    match (sct(s), sc(s)) {
        (SCT_GENERIC, SC_ABORTED_BY_REQUEST | SC_NAMESPACE_NOT_READY)
        | (SCT_PATH_RELATED, SC_INTERNAL_PATH_ERROR) => !dnr(s),
        _ => false,
    }
}

/// What becomes of a completed command (`nvme_qpair_complete_tracker`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// It succeeded.
    Done,
    /// It failed and is sent again.
    Retry,
    /// It failed for good; `exhausted` if only because its retries ran
    /// out (`num_failures` counts those).
    Fail { exhausted: bool },
}

/// The outcome of status `s` for a command already retried `retries`
/// times, with `hw.nvme.retry_count` at `limit`.
pub(crate) fn outcome(s: Status, retries: u32, limit: i32) -> Outcome {
    if s == 0 {
        return Outcome::Done;
    }
    let retriable = is_retry(s);
    let more_allowed = i64::from(retries) < i64::from(limit);
    if retriable && more_allowed {
        Outcome::Retry
    } else {
        Outcome::Fail {
            exhausted: retriable,
        }
    }
}

fn generic(sc: u16) -> Option<&'static str> {
    Some(match sc {
        0x00 => "SUCCESS",
        0x01 => "INVALID_OPCODE",
        0x02 => "INVALID_FIELD",
        0x03 => "COMMAND_ID_CONFLICT",
        0x04 => "DATA_TRANSFER_ERROR",
        0x05 => "ABORTED_POWER_LOSS",
        0x06 => "INTERNAL_DEVICE_ERROR",
        0x07 => "ABORTED_BY_REQUEST",
        0x08 => "ABORTED_SQ_DELETION",
        0x09 => "ABORTED_FAILED_FUSED",
        0x0a => "ABORTED_MISSING_FUSED",
        0x0b => "INVALID_NAMESPACE_OR_FORMAT",
        0x0c => "COMMAND_SEQUENCE_ERROR",
        0x0d => "INVALID_SGL_SEGMENT_DESCR",
        0x0e => "INVALID_NUMBER_OF_SGL_DESCR",
        0x0f => "DATA_SGL_LENGTH_INVALID",
        0x10 => "METADATA_SGL_LENGTH_INVALID",
        0x11 => "SGL_DESCRIPTOR_TYPE_INVALID",
        0x12 => "INVALID_USE_OF_CMB",
        0x13 => "PRP_OFFET_INVALID",
        0x14 => "ATOMIC_WRITE_UNIT_EXCEEDED",
        0x15 => "OPERATION_DENIED",
        0x16 => "SGL_OFFSET_INVALID",
        0x18 => "HOST_ID_INCONSISTENT_FORMAT",
        0x19 => "KEEP_ALIVE_TIMEOUT_EXPIRED",
        0x1a => "KEEP_ALIVE_TIMEOUT_INVALID",
        0x1b => "ABORTED_DUE_TO_PREEMPT",
        0x1c => "SANITIZE_FAILED",
        0x1d => "SANITIZE_IN_PROGRESS",
        0x1e => "SGL_DATA_BLOCK_GRAN_INVALID",
        0x1f => "NOT_SUPPORTED_IN_CMB",
        0x20 => "NAMESPACE_IS_WRITE_PROTECTED",
        0x21 => "COMMAND_INTERRUPTED",
        0x22 => "TRANSIENT_TRANSPORT_ERROR",
        0x80 => "LBA_OUT_OF_RANGE",
        0x81 => "CAPACITY_EXCEEDED",
        0x82 => "NAMESPACE_NOT_READY",
        0x83 => "RESERVATION_CONFLICT",
        0x84 => "FORMAT_IN_PROGRESS",
        _ => return None,
    })
}

fn command_specific(sc: u16) -> Option<&'static str> {
    Some(match sc {
        0x00 => "COMPLETION_QUEUE_INVALID",
        0x01 => "INVALID_QUEUE_IDENTIFIER",
        0x02 => "MAXIMUM_QUEUE_SIZE_EXCEEDED",
        0x03 => "ABORT_COMMAND_LIMIT_EXCEEDED",
        0x05 => "ASYNC_EVENT_REQUEST_LIMIT_EXCEEDED",
        0x06 => "INVALID_FIRMWARE_SLOT",
        0x07 => "INVALID_FIRMWARE_IMAGE",
        0x08 => "INVALID_INTERRUPT_VECTOR",
        0x09 => "INVALID_LOG_PAGE",
        0x0a => "INVALID_FORMAT",
        0x0b => "FIRMWARE_REQUIRES_RESET",
        0x0c => "INVALID_QUEUE_DELETION",
        0x0d => "FEATURE_NOT_SAVEABLE",
        0x0e => "FEATURE_NOT_CHANGEABLE",
        0x0f => "FEATURE_NOT_NS_SPECIFIC",
        0x10 => "FW_ACT_REQUIRES_NVMS_RESET",
        0x11 => "FW_ACT_REQUIRES_RESET",
        0x12 => "FW_ACT_REQUIRES_TIME",
        0x13 => "FW_ACT_PROHIBITED",
        0x14 => "OVERLAPPING_RANGE",
        0x15 => "NS_INSUFFICIENT_CAPACITY",
        0x16 => "NS_ID_UNAVAILABLE",
        0x18 => "NS_ALREADY_ATTACHED",
        0x19 => "NS_IS_PRIVATE",
        0x1a => "NS_NOT_ATTACHED",
        0x1b => "THIN_PROV_NOT_SUPPORTED",
        0x1c => "CTRLR_LIST_INVALID",
        0x1d => "SELF_TEST_IN_PROGRESS",
        0x1e => "BOOT_PART_WRITE_PROHIB",
        0x1f => "INVALID_CTRLR_ID",
        0x20 => "INVALID_SEC_CTRLR_STATE",
        0x21 => "INVALID_NUM_OF_CTRLR_RESRC",
        0x22 => "INVALID_RESOURCE_ID",
        0x23 => "SANITIZE_PROHIBITED_WPMRE",
        0x24 => "ANA_GROUP_ID_INVALID",
        0x25 => "ANA_ATTACH_FAILED",
        0x80 => "CONFLICTING_ATTRIBUTES",
        0x81 => "INVALID_PROTECTION_INFO",
        0x82 => "ATTEMPTED_WRITE_TO_RO_PAGE",
        _ => return None,
    })
}

fn media_error(sc: u16) -> Option<&'static str> {
    Some(match sc {
        0x80 => "WRITE_FAULTS",
        0x81 => "UNRECOVERED_READ_ERROR",
        0x82 => "GUARD_CHECK_ERROR",
        0x83 => "APPLICATION_TAG_CHECK_ERROR",
        0x84 => "REFERENCE_TAG_CHECK_ERROR",
        0x85 => "COMPARE_FAILURE",
        0x86 => "ACCESS_DENIED",
        0x87 => "DEALLOCATED_OR_UNWRITTEN",
        _ => return None,
    })
}

fn path_related(sc: u16) -> Option<&'static str> {
    Some(match sc {
        0x00 => "INTERNAL_PATH_ERROR",
        0x01 => "ASYMMETRIC_ACCESS_PERSISTENT_LOSS",
        0x02 => "ASYMMETRIC_ACCESS_INACCESSIBLE",
        0x03 => "ASYMMETRIC_ACCESS_TRANSITION",
        0x60 => "CONTROLLER_PATHING_ERROR",
        0x70 => "HOST_PATHING_ERROR",
        0x71 => "COMMAND_ABORTED_BY_HOST",
        _ => return None,
    })
}

/// A status as `nvme_sc_sbuf` writes it: `ABORTED_BY_REQUEST (00/07)`.
pub(crate) struct Name(pub(crate) Status);

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (t, c) = (sct(self.0), sc(self.0));
        let (kind, name) = match t {
            SCT_GENERIC => (Some("GENERIC"), generic(c)),
            SCT_COMMAND_SPECIFIC => (Some("COMMAND SPECIFIC"), command_specific(c)),
            SCT_MEDIA_ERROR => (Some("MEDIA ERROR"), media_error(c)),
            SCT_PATH_RELATED => (Some("PATH RELATED"), path_related(c)),
            SCT_VENDOR_SPECIFIC => (Some("VENDOR SPECIFIC"), None),
            _ => (None, None),
        };
        let label = name.or(kind).unwrap_or("RESERVED");
        write!(f, "{label} ({t:02x}/{c:02x})")
    }
}

/// An I/O opcode as `nvme_opcode_sbuf` writes it: `READ (02)`.
pub(crate) struct Opcode(pub(crate) u8);

impl fmt::Display for Opcode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self.0 {
            0x00 => "FLUSH",
            0x01 => "WRITE",
            0x02 => "READ",
            0x04 => "WRITE_UNCORRECTABLE",
            0x05 => "COMPARE",
            0x08 => "WRITE_ZEROES",
            0x09 => "DATASET_MANAGEMENT",
            0x0c => "VERIFY",
            0x0d => "RESERVATION_REGISTER",
            0x0e => "RESERVATION_REPORT",
            0x11 => "RESERVATION_ACQUIRE",
            0x15 => "RESERVATION_RELEASE",
            0x19 => "COPY",
            _ => "NVM",
        };
        write!(f, "{name} ({:02x})", self.0)
    }
}

/// What `nvme_io_qpair_print_command` prints for an I/O command (after
/// `nvmeN: `).
pub(crate) struct IoCommand {
    pub(crate) opcode: u8,
    pub(crate) sqid: u16,
    pub(crate) cid: u16,
    pub(crate) nsid: u32,
    pub(crate) cdw10: u32,
    pub(crate) cdw11: u32,
    pub(crate) cdw12: u32,
}

impl fmt::Display for IoCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            opcode,
            sqid,
            cid,
            nsid,
            ..
        } = *self;
        write!(f, "{} sqid:{sqid} cid:{cid} nsid:{nsid}", Opcode(opcode))?;
        // Read, write, write uncorrectable, compare, write zeroes, verify.
        if matches!(opcode, 0x01 | 0x02 | 0x04 | 0x05 | 0x08 | 0x0c) {
            let lba = u64::from(self.cdw11) << 32 | u64::from(self.cdw10);
            write!(f, " lba:{lba} len:{}", (self.cdw12 & 0xffff) + 1)?;
        }
        Ok(())
    }
}

/// What `nvme_qpair_print_completion` prints (after `nvmeN: `).
pub(crate) struct Completion {
    pub(crate) status: Status,
    /// The phase tag it was posted with (0 for one the driver made up).
    pub(crate) phase: bool,
    pub(crate) sqid: u16,
    pub(crate) cid: u16,
    pub(crate) cdw0: u32,
}

impl fmt::Display for Completion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.status;
        write!(
            f,
            "{} crd:{:x} m:{:x} dnr:{:x} p:{} sqid:{} cid:{} cdw0:{:x}",
            Name(s),
            crd(s),
            more(s),
            u16::from(dnr(s)),
            u8::from(self.phase),
            self.sqid,
            self.cid,
            self.cdw0
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::format;

    #[test]
    fn retries_follow_nvme_completion_is_retry() {
        let aborted = make(SCT_GENERIC, SC_ABORTED_BY_REQUEST, false);
        assert!(is_retry(aborted));
        assert!(!is_retry(make(SCT_GENERIC, SC_ABORTED_BY_REQUEST, true)));
        assert!(is_retry(make(SCT_GENERIC, SC_NAMESPACE_NOT_READY, false)));
        assert!(is_retry(make(SCT_PATH_RELATED, 0, false)));
        assert!(!is_retry(make(SCT_PATH_RELATED, 0x01, false)));
        assert!(!is_retry(make(SCT_GENERIC, 0x04, false)), "data transfer");
        assert!(!is_retry(make(SCT_MEDIA_ERROR, 0x81, false)));
        assert_eq!(outcome(0, 0, 4), Outcome::Done);
        assert_eq!(outcome(aborted, 3, 4), Outcome::Retry);
        assert_eq!(outcome(aborted, 4, 4), Outcome::Fail { exhausted: true });
        assert_eq!(outcome(aborted, 0, 0), Outcome::Fail { exhausted: true });
        assert_eq!(outcome(aborted, 0, -1), Outcome::Fail { exhausted: true });
        assert_eq!(
            outcome(make(SCT_MEDIA_ERROR, 0x81, false), 0, 4),
            Outcome::Fail { exhausted: false }
        );
    }

    #[test]
    fn fields_unpack_from_completion_dword_3() {
        // DNR, More, CRD 2, type 2, code 0x81, as dword 3 bits 31:17.
        let dw3: u32 = (1 << 31) | (1 << 30) | (2 << 28) | (2 << 25) | (0x81 << 17) | (1 << 16);
        let s = (dw3 >> 17) as u16 & 0x7fff;
        assert_eq!(
            (sct(s), sc(s), crd(s), more(s), dnr(s)),
            (2, 0x81, 2, 1, true)
        );
    }

    #[test]
    fn printouts_match_freebsd() {
        let s = make(SCT_GENERIC, SC_ABORTED_BY_REQUEST, false);
        assert_eq!(format!("{}", Name(s)), "ABORTED_BY_REQUEST (00/07)");
        assert_eq!(
            format!("{}", Name(make(1, 0x04, false))),
            "COMMAND SPECIFIC (01/04)"
        );
        assert_eq!(
            format!("{}", Name(make(7, 0x10, false))),
            "VENDOR SPECIFIC (07/10)"
        );
        assert_eq!(
            format!("{}", Name(make(5, 0x10, false))),
            "RESERVED (05/10)"
        );
        let c = IoCommand {
            opcode: 0x02,
            sqid: 1,
            cid: 5,
            nsid: 1,
            cdw10: 2048,
            cdw11: 0,
            cdw12: 7,
        };
        assert_eq!(
            format!("{c}"),
            "READ (02) sqid:1 cid:5 nsid:1 lba:2048 len:8"
        );
        let flush = IoCommand { opcode: 0, ..c };
        assert_eq!(format!("{flush}"), "FLUSH (00) sqid:1 cid:5 nsid:1");
        let cpl = Completion {
            status: make(SCT_GENERIC, SC_ABORTED_BY_REQUEST, true),
            phase: false,
            sqid: 1,
            cid: 5,
            cdw0: 0,
        };
        assert_eq!(
            format!("{cpl}"),
            "ABORTED_BY_REQUEST (00/07) crd:0 m:0 dnr:1 p:0 sqid:1 cid:5 cdw0:0"
        );
    }
}
