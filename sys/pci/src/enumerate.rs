// SPDX-License-Identifier: BSD-2-Clause
//! Enumeration: every function under a host bridge, depth first by bus,
//! slot, and function, following bridges to the buses firmware gave them.

use alloc::vec::Vec;

use devtree::strings::PciAddress;

use crate::bar::{self, Bar, BarError};
use crate::caps::{self, Capability, ExtCapability};
use crate::config::ConfigSpace;
use crate::header::{self, BridgeBuses, Header, HeaderType};

/// A function found by [`enumerate`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Function {
    /// Where it is.
    pub addr: PciAddress,
    /// Its header.
    pub header: Header,
    /// Its implemented BARs.
    pub bars: Vec<Bar>,
    /// BARs that failed validation, by number.
    pub bad_bars: Vec<(u8, BarError)>,
    /// Its standard capabilities.
    pub caps: Vec<Capability>,
    /// Its extended capabilities (PCIe functions).
    pub ext_caps: Vec<ExtCapability>,
    /// For a bridge: its bus numbers.
    pub bridge: Option<BridgeBuses>,
    /// How many bridges are above it (0 on the host bridge's root bus).
    pub depth: u8,
}

/// Why a bridge's buses were not followed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BridgeProblem {
    /// Its secondary bus is outside the host bridge's range, not above its
    /// own bus, or already seen (a loop).
    BadBus(PciAddress),
}

/// The functions under a host bridge whose root bus is `buses.start()` in
/// `segment`, and any bridges whose bus numbers could not be followed.
pub fn enumerate(
    cfg: &dyn ConfigSpace,
    segment: u32,
    buses: core::ops::RangeInclusive<u8>,
) -> (Vec<Function>, Vec<BridgeProblem>) {
    let mut out = Vec::new();
    let mut problems = Vec::new();
    let mut seen = [0u64; 4];
    scan_bus(
        cfg,
        segment,
        *buses.start(),
        &buses,
        0,
        &mut seen,
        &mut out,
        &mut problems,
    );
    (out, problems)
}

/// The functions on one bus, by slot and function, without following
/// bridges: a bridge's own driver scans the bus behind it, as FreeBSD's
/// `pcib`/`pci` pairs do.
pub fn scan(cfg: &dyn ConfigSpace, segment: u32, bus: u8, depth: u8) -> Vec<Function> {
    let mut out = Vec::new();
    for slot in 0..32 {
        let f0 = PciAddress {
            domain: segment,
            bus,
            slot,
            function: 0,
        };
        let Some(h0) = header::read(cfg, f0) else {
            continue;
        };
        let functions = if h0.multifunction { 8 } else { 1 };
        for function in 0..functions {
            let addr = PciAddress { function, ..f0 };
            let h = if function == 0 {
                h0
            } else {
                match header::read(cfg, addr) {
                    Some(h) => h,
                    None => continue,
                }
            };
            if let HeaderType::Unknown(_) = h.kind {
                continue;
            }
            out.push(probe(cfg, addr, h, depth));
        }
    }
    out
}

/// Sizes a present function's BARs and reads its capabilities.
fn probe(cfg: &dyn ConfigSpace, addr: PciAddress, h: Header, depth: u8) -> Function {
    let mut bars = Vec::new();
    let mut bad_bars = Vec::new();
    for b in bar::read_all(cfg, addr, h.kind.bars()) {
        match b {
            Ok(b) => bars.push(b),
            Err(e) => bad_bars.push(e),
        }
    }
    let caps = caps::list(cfg, addr);
    let ext_caps = if caps::find(&caps, caps::id::PCIE).is_some() {
        caps::ext_list(cfg, addr)
    } else {
        Vec::new()
    };
    let bridge = (h.kind == HeaderType::Bridge).then(|| header::bridge_buses(cfg, addr));
    Function {
        addr,
        header: h,
        bars,
        bad_bars,
        caps,
        ext_caps,
        bridge,
        depth,
    }
}

#[allow(clippy::too_many_arguments)]
fn scan_bus(
    cfg: &dyn ConfigSpace,
    segment: u32,
    bus: u8,
    range: &core::ops::RangeInclusive<u8>,
    depth: u8,
    seen: &mut [u64; 4],
    out: &mut Vec<Function>,
    problems: &mut Vec<BridgeProblem>,
) {
    seen[usize::from(bus / 64)] |= 1 << (bus % 64);
    for f in scan(cfg, segment, bus, depth) {
        let bridge = f.bridge;
        let addr = f.addr;
        out.push(f);
        if let Some(b) = bridge {
            let next = b.secondary;
            let fresh = seen[usize::from(next / 64)] & (1 << (next % 64)) == 0;
            if next > bus && range.contains(&next) && fresh {
                scan_bus(cfg, segment, next, range, depth + 1, seen, out, problems);
            } else if next != 0 {
                // Secondary 0: firmware left the bridge unconfigured.
                problems.push(BridgeProblem::BadBus(addr));
            }
        }
    }
}
