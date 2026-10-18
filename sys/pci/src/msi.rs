// SPDX-License-Identifier: BSD-2-Clause
//! Message-signalled interrupts: MSI and MSI-X as an interrupt domain
//! (docs/DRIVER-MODEL.md §8.3).
//!
//! One domain, [`PCI_MSI`], serves every function. Its inputs index a
//! table of message sources: an MSI-X table entry, or a function's MSI
//! capability (one message). Connecting an input asks the platform's
//! [`MsiPlatform`] for a message (on amd64, a LAPIC vector at the target
//! CPU) and writes it into the source, masked. Enabling MSI or MSI-X on a
//! function is the bus's job ([`enable_msix`], [`enable_msi`]): drivers
//! get interrupt lines, never the capability.

use alloc::vec::Vec;

use devtree::strings::PciAddress;
use hal::CpuId;
use irq::{IrqDomain, IrqError, Polarity, Trigger};
use sync::{Once, SpinLock};

use crate::caps::{self, Capability};
use crate::config::ConfigSpace;
use crate::enumerate::Function;
use crate::header::{CMD_INTX_DISABLE, COMMAND};

/// What message-signalled interrupts need from the platform.
pub trait MsiPlatform: Sync {
    /// A message that interrupts `cpu`: (address, data, the root id
    /// dispatch will see), with the root allocated.
    fn compose(&self, cpu: CpuId) -> Option<(u64, u32, u32)>;
    /// Frees a root from [`MsiPlatform::compose`].
    fn release(&self, cpu: CpuId, root: u32);
    /// Maps `len` bytes of device memory at `pa` uncached.
    fn map(&self, pa: u64, len: u64) -> Option<usize>;
}

static PLATFORM: Once<&'static dyn MsiPlatform> = Once::new();

/// Installs the platform's message composer. Without one (arm64 until
/// the ITS driver), functions use INTx.
pub fn set_platform(p: &'static dyn MsiPlatform) {
    let _ = PLATFORM.set(p);
}

/// Whether message-signalled interrupts are available.
#[must_use]
pub fn available() -> bool {
    PLATFORM.get().is_some()
}

/// One message source.
#[derive(Clone, Copy)]
enum Source {
    /// MSI-X table entry `entry`, whose table is mapped at `table`.
    MsiX { table: usize, entry: u16 },
    /// A function's MSI capability.
    Msi {
        cfg: &'static (dyn ConfigSpace + Sync),
        addr: PciAddress,
        cap: u8,
    },
}

/// The PCI MSI domain.
pub struct MsiDomain {
    sources: SpinLock<Vec<Option<Source>>>,
}

/// The one domain for every function's messages.
pub static PCI_MSI: MsiDomain = MsiDomain {
    sources: SpinLock::new(Vec::new()),
};

impl core::fmt::Debug for MsiDomain {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MsiDomain").finish_non_exhaustive()
    }
}

/// MSI-X table entry layout: address low, address high, data, control.
const ENTRY: usize = 16;
/// Vector control: masked.
const ENTRY_MASKED: u32 = 1;

/// MSI-X message control: enable, function mask.
const MSIX_ENABLE: u16 = 1 << 15;
const MSIX_FUNCTION_MASK: u16 = 1 << 14;
/// MSI message control: enable, 64-bit address, per-vector masking.
const MSI_ENABLE: u16 = 1;
const MSI_64BIT: u16 = 1 << 7;
const MSI_MASKABLE: u16 = 1 << 8;

fn entry_ptr(table: usize, entry: u16, dword: usize) -> *mut u32 {
    core::ptr::with_exposed_provenance_mut(table + usize::from(entry) * ENTRY + dword * 4)
}

impl Source {
    fn write_message(self, addr: u64, data: u32) {
        match self {
            Self::MsiX { table, entry } => {
                // SAFETY: the table was mapped for its entries by
                // `enable_msix`, and the entry is ours.
                unsafe {
                    entry_ptr(table, entry, 0).write_volatile(addr as u32);
                    entry_ptr(table, entry, 1).write_volatile((addr >> 32) as u32);
                    entry_ptr(table, entry, 2).write_volatile(data);
                }
            }
            Self::Msi { cfg, addr: a, cap } => {
                let at = u16::from(cap);
                let ctl = cfg.read16(a, at + 2);
                cfg.write32(a, at + 4, addr as u32);
                if ctl & MSI_64BIT != 0 {
                    cfg.write32(a, at + 8, (addr >> 32) as u32);
                    cfg.write16(a, at + 12, data as u16);
                } else {
                    cfg.write16(a, at + 8, data as u16);
                }
            }
        }
    }

    fn set_masked(self, masked: bool) {
        match self {
            Self::MsiX { table, entry } => {
                let p = entry_ptr(table, entry, 3);
                // SAFETY: as in `write_message`.
                unsafe {
                    let v = p.read_volatile();
                    p.write_volatile(if masked {
                        v | ENTRY_MASKED
                    } else {
                        v & !ENTRY_MASKED
                    });
                }
            }
            Self::Msi { cfg, addr: a, cap } => {
                let at = u16::from(cap);
                let ctl = cfg.read16(a, at + 2);
                if ctl & MSI_MASKABLE != 0 {
                    let mask_at = at + if ctl & MSI_64BIT != 0 { 16 } else { 12 };
                    let v = cfg.read32(a, mask_at);
                    cfg.write32(a, mask_at, if masked { v | 1 } else { v & !1 });
                }
            }
        }
    }
}

impl MsiDomain {
    fn source(&self, hw: u32) -> Option<Source> {
        *self.sources.lock().get(hw as usize)?
    }

    fn add(&self, s: Source) -> u32 {
        let mut v = self.sources.lock();
        let i = match v.iter().position(Option::is_none) {
            Some(i) => i,
            None => {
                v.push(None);
                v.len() - 1
            }
        };
        v[i] = Some(s);
        u32::try_from(i).unwrap_or(u32::MAX)
    }

    /// Forgets inputs (after their handlers are removed).
    pub fn remove(&self, hws: &[u32]) {
        let mut v = self.sources.lock();
        for &h in hws {
            if let Some(slot) = v.get_mut(h as usize) {
                *slot = None;
            }
        }
    }
}

impl IrqDomain for MsiDomain {
    fn name(&self) -> &'static str {
        "pci-msi"
    }

    fn connect(&self, hw: u32, _: Trigger, _: Polarity, cpu: CpuId) -> Result<u32, IrqError> {
        let src = self.source(hw).ok_or(IrqError::NoSuchInput)?;
        let p = PLATFORM.get().ok_or(IrqError::NoSuchInput)?;
        let (addr, data, root) = p.compose(cpu).ok_or(IrqError::NoVector)?;
        src.set_masked(true);
        src.write_message(addr, data);
        Ok(root)
    }

    fn disconnect(&self, hw: u32, cpu: CpuId, root: u32) {
        if let Some(src) = self.source(hw) {
            src.set_masked(true);
            src.write_message(0, 0);
        }
        if let Some(p) = PLATFORM.get() {
            p.release(cpu, root);
        }
    }

    fn mask(&self, hw: u32) {
        if let Some(s) = self.source(hw) {
            s.set_masked(true);
        }
    }

    fn unmask(&self, hw: u32) {
        if let Some(s) = self.source(hw) {
            s.set_masked(false);
        }
    }
}

/// Why message-signalled interrupts could not be enabled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MsiError {
    /// No platform composer (arm64 before the ITS).
    Unavailable,
    /// The function lacks the capability.
    NoCapability,
    /// The table's BAR is missing or too small.
    BadTable,
    /// The table could not be mapped.
    MapFailed,
}

/// The MSI-X vectors a function supports, if it has the capability.
#[must_use]
pub fn msix_count(cfg: &dyn ConfigSpace, f: &Function) -> Option<u16> {
    let cap = caps::find(&f.caps, caps::id::MSIX)?;
    Some((cfg.read16(f.addr, u16::from(cap.offset) + 2) & 0x7ff) + 1)
}

/// Enables MSI-X on `f` with `want` vectors (at most its table size),
/// every entry masked and INTx disabled. Returns the domain inputs, one per
/// vector, in table order.
pub fn enable_msix(
    cfg: &'static (dyn ConfigSpace + Sync),
    f: &Function,
    want: u16,
) -> Result<Vec<u32>, MsiError> {
    let p = PLATFORM.get().ok_or(MsiError::Unavailable)?;
    let cap: Capability = caps::find(&f.caps, caps::id::MSIX).ok_or(MsiError::NoCapability)?;
    let a = f.addr;
    let at = u16::from(cap.offset);
    let ctl = cfg.read16(a, at + 2);
    let size = (ctl & 0x7ff) + 1;
    let n = want.min(size).max(1);
    let table = cfg.read32(a, at + 4);
    let (bir, offset) = ((table & 7) as u8, u64::from(table & !7));
    let bar = f
        .bars
        .iter()
        .find(|b| b.index == bir)
        .ok_or(MsiError::BadTable)?;
    let len = u64::from(n) * ENTRY as u64;
    if offset + len > bar.size {
        return Err(MsiError::BadTable);
    }
    let va = p.map(bar.base + offset, len).ok_or(MsiError::MapFailed)?;
    // Function mask on while the entries are set up; INTx off.
    cfg.write16(a, at + 2, ctl | MSIX_ENABLE | MSIX_FUNCTION_MASK);
    let cmd = cfg.read16(a, COMMAND);
    cfg.write16(a, COMMAND, cmd | CMD_INTX_DISABLE);
    let hws = (0..n)
        .map(|entry| {
            let s = Source::MsiX { table: va, entry };
            s.set_masked(true);
            PCI_MSI.add(s)
        })
        .collect();
    cfg.write16(a, at + 2, (ctl | MSIX_ENABLE) & !MSIX_FUNCTION_MASK);
    Ok(hws)
}

/// Turns MSI-X off on `f` and INTx back on, once its vectors are
/// removed from [`PCI_MSI`].
pub fn disable_msix(cfg: &dyn ConfigSpace, f: &Function) {
    let Some(cap) = caps::find(&f.caps, caps::id::MSIX) else {
        return;
    };
    let at = u16::from(cap.offset);
    let ctl = cfg.read16(f.addr, at + 2);
    cfg.write16(f.addr, at + 2, ctl & !(MSIX_ENABLE | MSIX_FUNCTION_MASK));
    let cmd = cfg.read16(f.addr, COMMAND);
    cfg.write16(f.addr, COMMAND, cmd & !CMD_INTX_DISABLE);
}

/// Enables MSI on `f` with one message, masked where the function allows,
/// INTx disabled. Returns the domain input.
pub fn enable_msi(cfg: &'static (dyn ConfigSpace + Sync), f: &Function) -> Result<u32, MsiError> {
    if PLATFORM.get().is_none() {
        return Err(MsiError::Unavailable);
    }
    let cap = caps::find(&f.caps, caps::id::MSI).ok_or(MsiError::NoCapability)?;
    let a = f.addr;
    let at = u16::from(cap.offset);
    let s = Source::Msi {
        cfg,
        addr: a,
        cap: cap.offset,
    };
    s.set_masked(true);
    // One message (multiple-message enable 0), then enable.
    let ctl = cfg.read16(a, at + 2) & !(7 << 4);
    cfg.write16(a, at + 2, ctl | MSI_ENABLE);
    let cmd = cfg.read16(a, COMMAND);
    cfg.write16(a, COMMAND, cmd | CMD_INTX_DISABLE);
    Ok(PCI_MSI.add(s))
}
