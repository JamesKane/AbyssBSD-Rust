// SPDX-License-Identifier: BSD-2-Clause
//! Register offsets and fields (xHCI 1.2 §5), and the rules for writing
//! them that are easy to get wrong: `PORTSC`'s mix of write-1-to-clear
//! and preserved bits, and the extended capability list.

use crate::trb::byte;

/// Capability registers, from the controller's base.
pub mod cap {
    /// `CAPLENGTH` (8 bits): where the operational registers start.
    pub const CAPLENGTH: usize = 0x00;
    /// `HCIVERSION` (16 bits, at byte 2).
    pub const HCIVERSION: usize = 0x02;
    /// `HCSPARAMS1`.
    pub const HCSPARAMS1: usize = 0x04;
    /// `HCSPARAMS2`.
    pub const HCSPARAMS2: usize = 0x08;
    /// `HCCPARAMS1`.
    pub const HCCPARAMS1: usize = 0x10;
    /// `DBOFF`.
    pub const DBOFF: usize = 0x14;
    /// `RTSOFF`.
    pub const RTSOFF: usize = 0x18;
}

/// Operational registers, from `CAPLENGTH`.
pub mod op {
    /// `USBCMD`.
    pub const USBCMD: usize = 0x00;
    /// `USBSTS`.
    pub const USBSTS: usize = 0x04;
    /// `PAGESIZE`.
    pub const PAGESIZE: usize = 0x08;
    /// `DNCTRL`.
    pub const DNCTRL: usize = 0x14;
    /// `CRCR` (64 bits).
    pub const CRCR: usize = 0x18;
    /// `DCBAAP` (64 bits).
    pub const DCBAAP: usize = 0x30;
    /// `CONFIG`.
    pub const CONFIG: usize = 0x38;

    /// `PORTSC` of port `port` (from 1).
    #[must_use]
    pub fn portsc(port: u8) -> usize {
        0x400 + 0x10 * (usize::from(port.max(1)) - 1)
    }

    /// `USBCMD` bits.
    pub mod cmd {
        /// Run/stop.
        pub const RUN: u32 = 1 << 0;
        /// Host controller reset.
        pub const HCRST: u32 = 1 << 1;
        /// Interrupter enable.
        pub const INTE: u32 = 1 << 2;
        /// Host system error enable.
        pub const HSEE: u32 = 1 << 3;
    }

    /// `USBSTS` bits.
    pub mod sts {
        /// Halted.
        pub const HCH: u32 = 1 << 0;
        /// Host system error.
        pub const HSE: u32 = 1 << 2;
        /// Event interrupt (write 1 to clear).
        pub const EINT: u32 = 1 << 3;
        /// Port change detect (write 1 to clear).
        pub const PCD: u32 = 1 << 4;
        /// Controller not ready.
        pub const CNR: u32 = 1 << 11;
        /// Host controller error.
        pub const HCE: u32 = 1 << 12;
    }

    /// `CRCR` bits.
    pub mod crcr {
        /// Ring cycle state.
        pub const RCS: u64 = 1 << 0;
        /// Command stop.
        pub const CS: u64 = 1 << 1;
        /// Command abort.
        pub const CA: u64 = 1 << 2;
        /// Command ring running.
        pub const CRR: u64 = 1 << 3;
    }
}

/// Runtime registers, from `RTSOFF`.
pub mod rt {
    /// `MFINDEX`.
    pub const MFINDEX: usize = 0x00;

    /// The base of interrupter `n`'s registers.
    #[must_use]
    pub fn interrupter(n: u16) -> usize {
        0x20 + 0x20 * usize::from(n)
    }

    /// `IMAN`, from an interrupter's base.
    pub const IMAN: usize = 0x00;
    /// `IMOD`.
    pub const IMOD: usize = 0x04;
    /// `ERSTSZ`.
    pub const ERSTSZ: usize = 0x08;
    /// `ERSTBA` (64 bits).
    pub const ERSTBA: usize = 0x10;
    /// `ERDP` (64 bits).
    pub const ERDP: usize = 0x18;

    /// `IMAN`: interrupt pending (write 1 to clear).
    pub const IMAN_IP: u32 = 1 << 0;
    /// `IMAN`: interrupt enable.
    pub const IMAN_IE: u32 = 1 << 1;
    /// `ERDP`: event handler busy (write 1 to clear).
    pub const ERDP_EHB: u64 = 1 << 3;
}

/// What `HCSPARAMS1`, `HCSPARAMS2`, and `HCCPARAMS1` say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Params {
    /// Device slots.
    pub max_slots: u8,
    /// Interrupters.
    pub max_interrupters: u16,
    /// Root hub ports.
    pub max_ports: u8,
    /// Log2 of the event ring segment table's maximum size.
    pub erst_max: u8,
    /// Scratchpad buffers the controller needs.
    pub scratchpads: u16,
    /// 64-bit addressing.
    pub ac64: bool,
    /// 64-byte contexts.
    pub csz: bool,
    /// Port power control.
    pub ppc: bool,
    /// Primary stream array size (log2, minus one), 0 without streams.
    pub max_psa: u8,
    /// The extended capability list, as a byte offset (0: none).
    pub xecp: usize,
}

impl Params {
    /// Decodes the three registers.
    #[must_use]
    pub fn decode(hcs1: u32, hcs2: u32, hcc1: u32) -> Self {
        let sp_hi = (hcs2 >> 21) & 0x1f;
        let sp_lo = (hcs2 >> 27) & 0x1f;
        Self {
            max_slots: byte(hcs1),
            max_interrupters: u16::try_from((hcs1 >> 8) & 0x7ff).unwrap_or(0),
            max_ports: byte(hcs1 >> 24),
            erst_max: byte((hcs2 >> 4) & 0xf),
            scratchpads: u16::try_from((sp_hi << 5) | sp_lo).unwrap_or(0),
            ac64: hcc1 & 1 != 0,
            csz: hcc1 & (1 << 2) != 0,
            ppc: hcc1 & (1 << 3) != 0,
            max_psa: byte((hcc1 >> 12) & 0xf),
            xecp: usize::from(u16::try_from(hcc1 >> 16).unwrap_or(0)) * 4,
        }
    }
}

/// `PORTSC` fields (xHCI 1.2 §5.4.8).
pub mod portsc {
    /// Current connect status (read only).
    pub const CCS: u32 = 1 << 0;
    /// Port enabled (write 1 to *disable*: never write it back).
    pub const PED: u32 = 1 << 1;
    /// Over-current active.
    pub const OCA: u32 = 1 << 3;
    /// Port reset (write 1 to start).
    pub const PR: u32 = 1 << 4;
    /// Port link state, bits 8:5.
    pub const PLS_SHIFT: u32 = 5;
    /// Port power.
    pub const PP: u32 = 1 << 9;
    /// Port speed, bits 13:10.
    pub const SPEED_SHIFT: u32 = 10;
    /// Port indicator control, bits 15:14.
    pub const PIC: u32 = 3 << 14;
    /// Link state write strobe.
    pub const LWS: u32 = 1 << 16;
    /// Connect status change (write 1 to clear).
    pub const CSC: u32 = 1 << 17;
    /// Enable change.
    pub const PEC: u32 = 1 << 18;
    /// Warm reset change.
    pub const WRC: u32 = 1 << 19;
    /// Over-current change.
    pub const OCC: u32 = 1 << 20;
    /// Reset change.
    pub const PRC: u32 = 1 << 21;
    /// Link state change.
    pub const PLC: u32 = 1 << 22;
    /// Config error change.
    pub const CEC: u32 = 1 << 23;
    /// Wake on connect, disconnect, and over-current enables.
    pub const WAKE: u32 = 7 << 25;
    /// Warm port reset (USB 3; write 1 to start).
    pub const WPR: u32 = 1 << 31;
    /// Every change bit.
    pub const CHANGES: u32 = CSC | PEC | WRC | OCC | PRC | PLC | CEC;
    /// Bits a write must carry over unchanged.
    pub const PRESERVE: u32 = PP | PIC | WAKE;

    /// The link state.
    #[must_use]
    pub fn link_state(v: u32) -> u8 {
        super::byte((v >> PLS_SHIFT) & 0xf)
    }

    /// The speed ID.
    #[must_use]
    pub fn speed(v: u32) -> u8 {
        super::byte((v >> SPEED_SHIFT) & 0xf)
    }

    /// The value to write to make `action` happen to a port that reads
    /// `current`, without disturbing anything else: the preserved bits,
    /// plus `action` (change bits to clear, `PR`, `WPR`, `PP`, or a link
    /// state with `LWS`). Any other bit of `action` is ignored, so `PED`
    /// and unrequested change bits are never written as 1.
    #[must_use]
    pub fn write(current: u32, action: u32) -> u32 {
        let allowed = CHANGES | PR | WPR | PP | LWS | (0xf << PLS_SHIFT);
        (current & PRESERVE) | (action & allowed)
    }

    /// Link states (bits 8:5).
    pub mod pls {
        /// U0: active.
        pub const U0: u8 = 0;
        /// U3: suspended.
        pub const U3: u8 = 3;
        /// Disabled.
        pub const DISABLED: u8 = 4;
        /// RxDetect: USB 3, nothing connected.
        pub const RX_DETECT: u8 = 5;
        /// Inactive: USB 3 link error, needs a warm reset.
        pub const INACTIVE: u8 = 6;
        /// Polling.
        pub const POLLING: u8 = 7;
        /// Compliance mode: needs a warm reset.
        pub const COMPLIANCE: u8 = 10;
        /// Resume.
        pub const RESUME: u8 = 15;
    }
}

/// The doorbell value that rings endpoint `dci` (stream `stream`); 0 on
/// doorbell 0 rings the command ring.
#[must_use]
pub fn doorbell(dci: u8, stream: u16) -> u32 {
    u32::from(dci) | (u32::from(stream) << 16)
}

/// Extended capability IDs (xHCI 1.2 Table 7-1).
pub mod xcap {
    /// USB legacy support.
    pub const LEGACY: u8 = 1;
    /// Supported protocol.
    pub const PROTOCOL: u8 = 2;

    /// `USBLEGSUP`: BIOS owns the controller.
    pub const BIOS_OWNED: u32 = 1 << 16;
    /// `USBLEGSUP`: the OS owns the controller.
    pub const OS_OWNED: u32 = 1 << 24;
    /// `USBLEGCTLSTS` (the next word): the SMI enables to clear.
    pub const SMI_ENABLES: u32 = (1 << 0) | (1 << 4) | (1 << 13) | (1 << 14) | (1 << 15);
    /// `USBLEGCTLSTS`: the SMI status bits (write 1 to clear).
    pub const SMI_STATUS: u32 = 7 << 29;
}

/// Walks the extended capability list: yields `(byte offset, id)` from
/// `first` (the [`Params::xecp`] offset), reading words through `read`.
/// Stops at the end of the list, at an offset past `limit`, or after 64
/// entries, so a bogus list cannot loop.
pub fn ext_caps<F: Fn(usize) -> u32>(first: usize, limit: usize, read: F) -> ExtCaps<F> {
    ExtCaps {
        next: first,
        limit,
        left: 64,
        read,
    }
}

/// The iterator [`ext_caps`] returns.
#[derive(Debug)]
pub struct ExtCaps<F> {
    next: usize,
    limit: usize,
    left: u8,
    read: F,
}

impl<F: Fn(usize) -> u32> Iterator for ExtCaps<F> {
    type Item = (usize, u8);

    fn next(&mut self) -> Option<(usize, u8)> {
        if self.next == 0 || self.left == 0 || self.next.saturating_add(4) > self.limit {
            return None;
        }
        self.left -= 1;
        let at = self.next;
        let w = (self.read)(at);
        let step = usize::from(byte(w >> 8)) * 4;
        self.next = if step == 0 { 0 } else { at + step };
        Some((at, byte(w)))
    }
}

/// Extended capability IDs as FreeBSD's `xhci_get_xecp` prints them:
/// `PROTO,LEGACY`, with `RES(b)` for reserved IDs and `VEND(c0)` for
/// vendor ones. IDs it has no name for (0, 7 to 9) print as nothing
/// between their commas, as FreeBSD's do.
#[derive(Clone, Copy, Debug)]
pub struct XecpNames<'a>(pub &'a [u8]);

impl core::fmt::Display for XecpNames<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        for (i, &id) in self.0.iter().enumerate() {
            if i > 0 {
                f.write_str(",")?;
            }
            match id {
                11..=16 | 19..=191 => write!(f, "RES({id:x})")?,
                192.. => write!(f, "VEND({id:x})")?,
                1 => f.write_str("LEGACY")?,
                2 => f.write_str("PROTO")?,
                3 => f.write_str("POWER")?,
                4 => f.write_str("VIRT")?,
                5 => f.write_str("MSG IRQ")?,
                6 => f.write_str("LOCAL MEM")?,
                10 => f.write_str("DEBUG")?,
                17 => f.write_str("EXT MSI")?,
                18 => f.write_str("TUN")?,
                _ => {}
            }
        }
        Ok(())
    }
}

/// A supported-protocol capability (xHCI 1.2 §7.2): which root ports
/// speak which USB major version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Protocol {
    /// The USB major revision (2 or 3).
    pub major: u8,
    /// The minor revision, as BCD (`0x10` for 3.1).
    pub minor: u8,
    /// The first port (from 1).
    pub first_port: u8,
    /// Ports.
    pub count: u8,
    /// The slot type for Enable Slot.
    pub slot_type: u8,
}

impl Protocol {
    /// Decodes the capability's first four words.
    #[must_use]
    pub fn decode(w: [u32; 4]) -> Self {
        Self {
            major: byte(w[0] >> 24),
            minor: byte(w[0] >> 16),
            first_port: byte(w[2]),
            count: byte(w[2] >> 8),
            slot_type: byte(w[3] & 0x1f),
        }
    }

    /// Whether root port `port` is one of this protocol's.
    #[must_use]
    pub fn has(&self, port: u8) -> bool {
        port >= self.first_port
            && u16::from(port) < u16::from(self.first_port) + u16::from(self.count)
    }
}
