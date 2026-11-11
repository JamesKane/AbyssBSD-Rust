// SPDX-License-Identifier: BSD-2-Clause
//! What `/dev/pci` reports: the list of PCI functions in the order the
//! buses found them (FreeBSD's `pci_devq`), each one's `struct pci_conf`,
//! `PCIOCGETCONF`'s patterns and paging, and its BARs and VPD.
//!
//! - A `pci` bus adds its functions to the list when it scans its bus, and
//!   takes them off when it stops; each change bumps the generation, so a
//!   caller paging through the list starts again.
//! - The driver's name and unit come from the device tree when asked; a
//!   bridge's bus numbers are read again, as a driver may have changed
//!   them.
//! - The device files themselves (copying in and out, permissions) are
//!   the kernel's.
//!
//! Behavior reference: sys/dev/pci/pci_user.c, sys/dev/pci/pci.c
//! (`pci_devq`, `pci_find_bar`, `pci_bar_enabled`, `pci_fetch_vpd_list`).

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use abi::pciio::{
    PCI_CONF_REPORTED_LEN, PCI_MAXNAMELEN, PciConf, PciConfIo, PciMatchConf, PciSel, flags, status,
};
use bus::DeviceId;
use sync::SpinLock;

use crate::bar::Map;
use crate::bus::PciChild;
use crate::config::Width;
use crate::header::{BUSES, CMD_IO, CMD_MEMORY, COMMAND, HeaderType};

/// A function on the list.
#[derive(Clone)]
pub struct Listed {
    /// Its node.
    pub id: DeviceId,
    /// What enumeration found, and its configuration space.
    pub child: Arc<PciChild>,
}

impl core::fmt::Debug for Listed {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Listed")
            .field("id", &self.id)
            .field("addr", &self.child.function.addr)
            .finish()
    }
}

/// The list and its generation (`pci_devq`, `pci_generation`).
static LIST: SpinLock<Vec<Listed>> = SpinLock::new(Vec::new());
static GENERATION: AtomicU32 = AtomicU32::new(0);

/// Puts a function a bus found at the end of the list.
pub(crate) fn add(id: DeviceId, child: Arc<PciChild>) {
    LIST.lock().push(Listed { id, child });
    // Relaxed: readers take the list's lock, which orders the two.
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// Takes bus `bus`'s functions off the list.
pub(crate) fn remove_bus(domain: u32, bus: u8) {
    let mut l = LIST.lock();
    let before = l.len();
    l.retain(|e| {
        let a = e.child.function.addr;
        a.domain != domain || a.bus != bus
    });
    if l.len() != before {
        GENERATION.fetch_add(1, Ordering::Relaxed);
    }
}

/// The list as it is now, and its generation.
#[must_use]
pub fn snapshot() -> (u32, Vec<Listed>) {
    let l = LIST.lock();
    (GENERATION.load(Ordering::Relaxed), l.clone())
}

/// The function at `sel` (`pci_find_dbsf`).
#[must_use]
pub fn find(sel: &PciSel) -> Option<Listed> {
    LIST.lock()
        .iter()
        .find(|e| {
            let a = e.child.function.addr;
            a.domain == sel.pc_domain
                && a.bus == sel.pc_bus
                && a.slot == sel.pc_dev
                && a.function == sel.pc_func
        })
        .cloned()
}

/// The header type byte, without the multi-function bit.
fn header_type(k: HeaderType) -> u8 {
    match k {
        HeaderType::Device => 0,
        HeaderType::Bridge => 1,
        HeaderType::CardBus => 2,
        HeaderType::Unknown(n) => n,
    }
}

/// `struct pci_conf` for a function whose driver is `name` (devclass and
/// unit), if it has one.
#[must_use]
pub fn conf(child: &PciChild, name: Option<(&str, u32)>) -> PciConf {
    let f = &child.function;
    let id = f.header.ident;
    let mut c = PciConf {
        pc_sel: sel_of(f.addr),
        pc_hdr: header_type(f.header.kind),
        pc_subvendor: id.subvendor,
        pc_subdevice: id.subdevice,
        pc_vendor: id.vendor,
        pc_device: id.device,
        pc_class: (id.class >> 16) as u8,
        pc_subclass: (id.class >> 8) as u8,
        pc_progif: id.class as u8,
        pc_revid: id.revision,
        pc_reported_len: PCI_CONF_REPORTED_LEN,
        ..PciConf::default()
    };
    if let Some((n, unit)) = name {
        let n = n.as_bytes();
        let len = n.len().min(PCI_MAXNAMELEN);
        c.pd_name[..len].copy_from_slice(&n[..len]);
        c.pd_unit = u64::from(unit);
    }
    // A bridge's bus numbers, as they are now (CardBus's are at the same
    // place).
    if matches!(f.header.kind, HeaderType::Bridge | HeaderType::CardBus) {
        let b = child.cfg.read32(f.addr, BUSES);
        c.pc_secbus = (b >> 8) as u8;
        c.pc_subbus = (b >> 16) as u8;
    }
    c
}

/// A function's selector.
#[must_use]
pub fn sel_of(a: crate::PciAddress) -> PciSel {
    PciSel {
        pc_domain: a.domain,
        pc_bus: a.bus,
        pc_dev: a.slot,
        pc_func: a.function,
        _pad0: 0,
    }
}

/// The name in a pattern or a `pci_conf`, up to its NUL.
fn name_of(n: &[u8; PCI_MAXNAMELEN + 1]) -> &[u8] {
    let end = n.iter().position(|&b| b == 0).unwrap_or(n.len());
    &n[..end]
}

/// Whether `c` matches any of `patterns` (`pci_conf_match_native`): a
/// pattern matches when every field its flags name matches. A pattern
/// with no flags matches nothing.
#[must_use]
pub fn matches(patterns: &[PciMatchConf], c: &PciConf) -> bool {
    patterns.iter().any(|p| {
        let f = p.flags;
        let want = |bit: u32, same: bool| f & bit == 0 || same;
        f != flags::NO_MATCH
            && want(
                flags::MATCH_DOMAIN,
                c.pc_sel.pc_domain == p.pc_sel.pc_domain,
            )
            && want(flags::MATCH_BUS, c.pc_sel.pc_bus == p.pc_sel.pc_bus)
            && want(flags::MATCH_DEV, c.pc_sel.pc_dev == p.pc_sel.pc_dev)
            && want(flags::MATCH_FUNC, c.pc_sel.pc_func == p.pc_sel.pc_func)
            && want(flags::MATCH_VENDOR, c.pc_vendor == p.pc_vendor)
            && want(flags::MATCH_DEVICE, c.pc_device == p.pc_device)
            && want(flags::MATCH_CLASS, c.pc_class == p.pc_class)
            && want(flags::MATCH_UNIT, c.pd_unit == p.pd_unit)
            && want(
                flags::MATCH_NAME,
                name_of(&p.pd_name) == name_of(&c.pd_name),
            )
    })
}

/// Why a `PCIOCGETCONF` failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GetconfError<E> {
    /// The patterns are malformed, or could not be read (`EINVAL`).
    Invalid,
    /// Writing a match failed.
    Write(E),
}

/// Runs a `PCIOCGETCONF` over `list` (of generation `generation`), as
/// `pci_ioctl` does: `io` is updated in place; `patterns(n)` reads the
/// caller's `n` patterns, `conf` makes an entry's `pci_conf`, and
/// `write(i, c)` stores the `i`th match.
///
/// # Errors
///
/// [`GetconfError::Invalid`] for patterns FreeBSD refuses, with `io`'s
/// status set to `PCI_GETCONF_ERROR` where FreeBSD sets it, and
/// [`GetconfError::Write`] when storing a match fails.
pub fn getconf<T, E>(
    io: &mut PciConfIo,
    generation: u32,
    list: &[T],
    patterns: impl FnOnce(usize) -> Option<Vec<PciMatchConf>>,
    conf: impl Fn(&T) -> PciConf,
    mut write: impl FnMut(usize, &PciConf) -> Result<(), E>,
) -> Result<(), GetconfError<E>> {
    io.num_matches = 0;
    let numdevs = u32::try_from(list.len()).unwrap_or(u32::MAX);
    if io.offset != 0 && io.generation != generation {
        io.status = status::LIST_CHANGED;
        return Ok(());
    }
    if io.offset >= numdevs {
        io.status = status::LAST_DEVICE;
        return Ok(());
    }
    let confsz = core::mem::size_of::<PciConf>();
    let room = usize::try_from(io.match_buf_len).unwrap_or(usize::MAX);
    let ionum = (room - room % confsz).min(list.len() * confsz) / confsz;
    let patterns = if io.num_patterns > 0 && io.num_patterns < numdevs && io.pat_buf_len > 0 {
        let n = usize::try_from(io.num_patterns).unwrap_or(usize::MAX);
        let len = n.checked_mul(core::mem::size_of::<PciMatchConf>());
        if len != usize::try_from(io.pat_buf_len).ok() {
            io.status = status::ERROR;
            return Err(GetconfError::Invalid);
        }
        Some(patterns(n).ok_or(GetconfError::Invalid)?)
    } else if io.num_patterns > 0 || io.pat_buf_len > 0 {
        io.status = status::ERROR;
        return Err(GetconfError::Invalid);
    } else {
        None
    };
    let start = usize::try_from(io.offset).unwrap_or(usize::MAX);
    let mut i = start;
    let mut result = Ok(());
    while i < list.len() {
        let c = conf(&list[i]);
        if patterns.as_ref().is_none_or(|p| matches(p, &c)) {
            if usize::try_from(io.num_matches).unwrap_or(usize::MAX) >= ionum {
                break;
            }
            if let Err(e) = write(usize::try_from(io.num_matches).unwrap_or(usize::MAX), &c) {
                result = Err(GetconfError::Write(e));
                break;
            }
            io.num_matches += 1;
        }
        i += 1;
    }
    io.offset = u32::try_from(i).unwrap_or(u32::MAX);
    io.generation = generation;
    io.status = if i >= list.len() {
        status::LAST_DEVICE
    } else {
        status::MORE_DEVS
    };
    result
}

/// Reads `width` bytes at register `reg`, aligned to `width`, of `child`
/// (`pci_read_config`, for `PCIOCREAD`): all-ones past the 4 KiB of
/// configuration space, as FreeBSD's accessors answer.
#[must_use]
pub fn read_config(child: &PciChild, reg: u32, width: Width) -> u32 {
    match u16::try_from(reg) {
        Ok(r) if r < 4096 => child.cfg.read_width(child.function.addr, r, width),
        _ => u32::MAX,
    }
}

/// Writes `width` bytes at register `reg` of `child` (`pci_write_config`,
/// for `PCIOCWRITE`), any register at all, as FreeBSD lets root; nothing
/// past configuration space.
pub fn write_config(child: &PciChild, reg: u32, width: Width, v: u32) {
    if let Ok(r) = u16::try_from(reg)
        && r < 4096
    {
        child.cfg.write_width(child.function.addr, r, width, v);
    }
}

/// FreeBSD's record of BAR register `reg` of `child` (`pci_find_bar`).
#[must_use]
pub fn find_bar(child: &PciChild, reg: i32) -> Option<Map> {
    child
        .function
        .maps
        .iter()
        .find(|m| i32::from(m.reg) == reg)
        .copied()
}

/// Whether the function decodes `m`'s space now (`pci_bar_enabled`).
#[must_use]
pub fn bar_enabled(child: &PciChild, m: &Map) -> bool {
    let cmd = child.cfg.read16(child.function.addr, COMMAND);
    if m.is_memory() {
        cmd & CMD_MEMORY != 0
    } else {
        cmd & CMD_IO != 0
    }
}

/// A function's VPD, read on first use and kept (`pci_fetch_vpd_list`);
/// `None` without a VPD capability or with no VPD FreeBSD keeps. `wait`
/// polls the device, as for [`crate::vpd::read_dword`].
pub fn vpd<'a>(
    child: &'a PciChild,
    wait: &mut dyn FnMut(u32) -> bool,
) -> Option<&'a crate::vpd::Vpd> {
    if let Some(v) = child.vpd.get() {
        return v.as_ref();
    }
    let cap = crate::caps::find(&child.function.caps, crate::vpd::CAP_ID)?;
    let (cfg, a, at) = (child.cfg, child.function.addr, u16::from(cap.offset));
    let v = crate::vpd::parse(|addr| crate::vpd::read_dword(cfg, a, at, addr, wait));
    // Another reader may have set it meanwhile; theirs is the same.
    let _ = child.vpd.set(v);
    child.vpd.get().and_then(Option::as_ref)
}

/// The bytes `PCIOCLISTVPD` copies out for `v` (`pci_list_vpd`): the
/// identifier, then each read-only keyword, then each read-write one,
/// each a `struct pci_vpd_element` followed by its data.
#[must_use]
pub fn vpd_elements(v: &crate::vpd::Vpd) -> Vec<u8> {
    use abi::pciio::{PVE_FLAG_IDENT, PVE_FLAG_RW};
    let mut out = Vec::new();
    let mut push = |kw: [u8; 2], fl: u8, data: &[u8]| {
        let len = data.len().min(255);
        out.extend_from_slice(&[kw[0], kw[1], fl, u8::try_from(len).unwrap_or(u8::MAX)]);
        out.extend_from_slice(&data[..len]);
    };
    // The identifier ends at its first NUL, as FreeBSD's string does.
    let ident = v.ident.split(|&b| b == 0).next().unwrap_or(&[]);
    push([0, 0], PVE_FLAG_IDENT, ident);
    for k in &v.read_only {
        push(k.keyword, 0, &k.value);
    }
    for k in &v.read_write {
        push(k.keyword, PVE_FLAG_RW, &k.value);
    }
    out
}
