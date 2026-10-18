// SPDX-License-Identifier: BSD-2-Clause
//! Fixed and general-purpose events: what raises the SCI on platforms
//! with ACPI hardware (ACPI 6.5 §4.8.3, §5.6.4). Hardware-reduced
//! platforms (arm64) signal events through GED devices instead.
//!
//! - ACPI mode is entered through `SMI_CMD` if firmware left it off.
//! - The power button, when it is a fixed feature, is enabled; a press
//!   asks the kernel to power off, as FreeBSD's
//!   `acpi_event_power_button_sleep` does.
//! - Every GPE with a `_Lxx` or `_Exx` method is enabled, except those
//!   that only wake the system (named by a device's `_PRW`).
//! - The SCI's filter claims what is pending: it clears fixed events and
//!   disables the GPEs that fired (so a level GPE stops asserting). Its
//!   thread runs their methods on the AML thread, then clears a level
//!   GPE's status and enables it again.
//!
//! Behavior reference: sys/dev/acpica/acpi.c (fixed events), and the ACPI
//! specification for the registers and the GPE protocol.

#![allow(
    clippy::cast_possible_truncation,
    reason = "the registers are 8 or 16 bits, read through 32-bit accessors, and GPE byte \
              indices fit a byte (blocks are at most 255 bytes)"
)]

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use aml::NodeId;
use bus::Resource;
use sched::Sleepable;

use crate::Ops;

/// Where a register block is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Space {
    /// I/O ports.
    Io,
    /// Physical memory.
    Memory,
}

/// A register block: an event block's status half then its enable half,
/// or a control register.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Block {
    /// Its address space.
    pub space: Space,
    /// Its address.
    pub address: u64,
    /// Its length in bytes.
    pub len: u8,
}

/// The fixed hardware the FADT describes.
#[derive(Clone, Copy, Debug)]
pub struct FixedHardware {
    /// The SCI.
    pub sci: Resource,
    /// The PM1 event blocks.
    pub pm1a: Block,
    /// See `pm1a`.
    pub pm1b: Option<Block>,
    /// The PM1 control blocks.
    pub pm1a_control: Option<Block>,
    /// See `pm1a_control`.
    pub pm1b_control: Option<Block>,
    /// The GPE blocks: GPE0 from event 0, GPE1 from its base.
    pub gpe0: Option<Block>,
    /// See `gpe0`.
    pub gpe1: Option<(Block, u8)>,
    /// The SMI command port and the value that enters ACPI mode.
    pub smi_command: Option<(u16, u8)>,
    /// The power button is a fixed feature (not a PNP0C0C device).
    pub power_button: bool,
}

/// PM1 status and enable bits.
const PWRBTN: u16 = 1 << 8;
/// Every PM1 status bit, cleared at start.
const PM1_ALL: u16 = 0xffff;
/// PM1 control: the SCI is enabled (ACPI mode).
const SCI_EN: u16 = 1;

fn read(ops: &dyn Ops, b: Block, offset: u8, bytes: u8) -> u32 {
    let at = b.address + u64::from(offset);
    match b.space {
        Space::Io => u16::try_from(at)
            .ok()
            .and_then(|p| ops.io_read(p, bytes))
            .unwrap_or(0),
        Space::Memory => ops
            .mem_read(at, bytes)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0),
    }
}

fn write(ops: &dyn Ops, b: Block, offset: u8, bytes: u8, v: u32) {
    let at = b.address + u64::from(offset);
    match b.space {
        Space::Io => {
            if let Ok(p) = u16::try_from(at) {
                let _ = ops.io_write(p, bytes, v);
            }
        }
        Space::Memory => {
            let _ = ops.mem_write(at, bytes, u64::from(v));
        }
    }
}

/// A GPE with a method.
#[derive(Clone, Copy, Debug)]
struct Gpe {
    number: u16,
    method: NodeId,
    level: bool,
}

/// The fixed and general-purpose events, owned by `acpi0`.
pub(crate) struct Events {
    hw: FixedHardware,
    ops: &'static dyn Ops,
    gpes: Vec<Gpe>,
    /// Fixed events the filter claimed, for the thread.
    fixed: AtomicU32,
    /// GPEs the filter claimed (bit per GPE number).
    pending: Vec<AtomicU64>,
}

impl core::fmt::Debug for Events {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Events")
            .field("gpes", &self.gpes.len())
            .finish_non_exhaustive()
    }
}

/// A GPE's register: the block, the byte in each half, and the bit.
fn locate(hw: &FixedHardware, n: u16) -> Option<(Block, u8, u8)> {
    if let Some(b) = hw.gpe0
        && n < u16::from(b.len / 2) * 8
    {
        return Some((b, (n / 8) as u8, (n % 8) as u8));
    }
    let (b, base) = hw.gpe1?;
    let i = n.checked_sub(u16::from(base))?;
    (i < u16::from(b.len / 2) * 8).then_some((b, (i / 8) as u8, (i % 8) as u8))
}

/// GPE methods under `\_GPE`: `_Lxx` (level) and `_Exx` (edge), by number.
fn gpe_methods(interp: &mut aml::Interpreter, host: &mut dyn aml::Host) -> Vec<Gpe> {
    let Some(scope) = aml::AmlPath::parse("\\_GPE")
        .ok()
        .and_then(|p| interp.lookup(&p))
    else {
        return Vec::new();
    };
    // GPEs a device wakes the system with (`_PRW`'s first element).
    let mut wake = Vec::new();
    for d in interp.devices() {
        if let Ok(Some(aml::Value::Package(p))) = interp.evaluate_child(d, "_PRW", Vec::new(), host)
            && let Some(aml::Value::Integer(n)) = p.first()
        {
            wake.push(*n);
        }
    }
    let mut out = Vec::new();
    for &c in interp.children(scope).to_vec().iter() {
        let Some(name) = interp.name(c) else { continue };
        let name = String::from(name.as_str());
        let bytes = name.as_bytes();
        let level = match bytes.first().zip(bytes.get(1)) {
            Some((b'_', b'L')) => true,
            Some((b'_', b'E')) => false,
            _ => continue,
        };
        let Ok(number) = u16::from_str_radix(&name[2..], 16) else {
            continue;
        };
        if !wake.contains(&u64::from(number)) {
            out.push(Gpe {
                number,
                method: c,
                level,
            });
        }
    }
    out
}

impl Events {
    /// Enters ACPI mode if needed, quiets every event, and enables the
    /// fixed power button and the GPEs with methods.
    pub(crate) fn start(
        ops: &'static dyn Ops,
        hw: FixedHardware,
        svc: &crate::service::Service,
        s: &Sleepable,
    ) -> Self {
        if let Some(cnt) = hw.pm1a_control
            && read(ops, cnt, 0, 2) as u16 & SCI_EN == 0
            && let Some((port, value)) = hw.smi_command
        {
            let _ = ops.io_write(port, 1, u32::from(value));
            for _ in 0..1000 {
                if read(ops, cnt, 0, 2) as u16 & SCI_EN != 0 {
                    break;
                }
                ops.sleep_ms(s, 1);
            }
        }
        // Everything off and clear.
        for b in [hw.gpe0, hw.gpe1.map(|(b, _)| b)].into_iter().flatten() {
            let half = b.len / 2;
            for i in 0..half {
                write(ops, b, half + i, 1, 0);
                write(ops, b, i, 1, 0xff);
            }
        }
        for b in [Some(hw.pm1a), hw.pm1b].into_iter().flatten() {
            let half = b.len / 2;
            write(ops, b, half, 2, 0);
            write(ops, b, 0, 2, u32::from(PM1_ALL));
        }
        if hw.power_button {
            for b in [Some(hw.pm1a), hw.pm1b].into_iter().flatten() {
                write(ops, b, b.len / 2, 2, u32::from(PWRBTN));
            }
            ops.log(format_args!("acpi0: Power Button (fixed)"));
        }
        let gpes: Vec<Gpe> = svc
            .run(s, gpe_methods)
            .into_iter()
            .filter(|g| locate(&hw, g.number).is_some())
            .collect();
        let numbers = gpes
            .iter()
            .map(|g| usize::from(g.number) + 1)
            .max()
            .unwrap_or(0);
        let ev = Self {
            hw,
            ops,
            gpes,
            fixed: AtomicU32::new(0),
            pending: (0..numbers.div_ceil(64))
                .map(|_| AtomicU64::new(0))
                .collect(),
        };
        for g in &ev.gpes {
            ev.set_enabled(g.number, true);
        }
        ev
    }

    /// The SCI.
    pub(crate) fn sci(&self) -> Resource {
        self.hw.sci
    }

    /// GPEs enabled (diagnostics).
    pub(crate) fn gpe_count(&self) -> usize {
        self.gpes.len()
    }

    fn set_enabled(&self, n: u16, on: bool) {
        let Some((b, byte, bit)) = locate(&self.hw, n) else {
            return;
        };
        let at = b.len / 2 + byte;
        let v = read(self.ops, b, at, 1);
        let v = if on { v | 1 << bit } else { v & !(1 << bit) };
        write(self.ops, b, at, 1, v);
    }

    fn clear_status(&self, n: u16) {
        if let Some((b, byte, bit)) = locate(&self.hw, n) {
            write(self.ops, b, byte, 1, 1 << bit);
        }
    }

    /// In interrupt context: claims pending events. Returns whether any
    /// were (the SCI may be shared).
    pub(crate) fn filter(&self) -> bool {
        let mut claimed = false;
        for b in [Some(self.hw.pm1a), self.hw.pm1b].into_iter().flatten() {
            let sts = read(self.ops, b, 0, 2) as u16;
            let en = read(self.ops, b, b.len / 2, 2) as u16;
            let active = sts & en;
            if active != 0 {
                write(self.ops, b, 0, 2, u32::from(active));
                self.fixed.fetch_or(u32::from(active), Ordering::AcqRel);
                claimed = true;
            }
        }
        for g in &self.gpes {
            let Some((b, byte, bit)) = locate(&self.hw, g.number) else {
                continue;
            };
            let sts = read(self.ops, b, byte, 1);
            let en = read(self.ops, b, b.len / 2 + byte, 1);
            if sts & en & (1 << bit) != 0 {
                self.set_enabled(g.number, false);
                if !g.level {
                    self.clear_status(g.number);
                }
                let n = usize::from(g.number);
                self.pending[n / 64].fetch_or(1 << (n % 64), Ordering::AcqRel);
                claimed = true;
            }
        }
        claimed
    }

    /// In the SCI's thread: handles what the filter claimed.
    pub(crate) fn run(&self, s: &Sleepable) {
        let fixed = self.fixed.swap(0, Ordering::AcqRel) as u16;
        if fixed & PWRBTN != 0 {
            self.ops.power_button();
        }
        let Some(svc) = crate::service() else { return };
        for g in &self.gpes {
            let n = usize::from(g.number);
            let bit = 1u64 << (n % 64);
            if self.pending[n / 64].fetch_and(!bit, Ordering::AcqRel) & bit == 0 {
                continue;
            }
            let method = g.method;
            svc.run(s, move |interp, host| {
                let _ = interp.evaluate_node(method, Vec::new(), host);
            });
            if g.level {
                self.clear_status(g.number);
            }
            self.set_enabled(g.number, true);
        }
    }
}
