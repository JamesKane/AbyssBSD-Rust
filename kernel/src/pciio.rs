// SPDX-License-Identifier: BSD-2-Clause
//! `/dev/pci`, the PCI user interface `pciconf(8)` and libpciaccess use:
//! the list of functions with their drivers' names (`PCIOCGETCONF`),
//! configuration reads and writes (`PCIOCREAD`, `PCIOCWRITE`), BARs
//! (`PCIOCGETBAR`, `PCIOCBARMMAP`, `PCIOCBARIO`), whether a driver is
//! attached (`PCIOCATTACHED`), and VPD (`PCIOCLISTVPD`).
//!
//! - The node is `root:wheel` 0644, as FreeBSD makes it. Opening it for
//!   writing needs securelevel 0 or below; a descriptor opened read-only
//!   may only list functions, BARs, and VPD (`EPERM` otherwise), FreeBSD's
//!   promise that it changes nothing.
//! - The list, the patterns, and VPD are `sys/pci`'s ([`pci::user`]);
//!   this is the copying in and out, and the mappings.
//! - `PCIOCBARIO` reaches a BAR no driver has: one of a function with a
//!   driver attached is refused (`ENOENT`, FreeBSD's failed
//!   `bus_alloc_resource`), as is an I/O BAR on arm64, which has no port
//!   space here.
//!
//! Behavior reference: sys/dev/pci/pci_user.c, sys/dev/pci/pci.c
//! (`pci_modevent`).

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::mem::size_of;

use abi::errno::Errno;
use abi::fcntl::{FREAD, FWRITE};
use abi::pciio::*;
use addr::{PhysAddr, VirtAddr};
use pci::config::Width;
use pci::user::{self, GetconfError, Listed};
use sched::Sleepable;
use vfs::{CharDevice, Uio};

use crate::mm::space::{AddressSpace, Anon, AnonKind};
use crate::uaccess::{Plain, bytes_of, bytes_of_mut};

/// The request's argument as a `T` (copied in already, the length the
/// request number encodes).
fn take<T: Plain + Default>(arg: &[u8]) -> Result<T, Errno> {
    let mut v = T::default();
    let b = bytes_of_mut(&mut v);
    if arg.len() != b.len() {
        return Err(Errno::EINVAL);
    }
    b.copy_from_slice(arg);
    Ok(v)
}

/// Puts `v` back as the argument, to be copied out.
fn put<T: Plain>(arg: &mut [u8], v: &T) {
    arg.copy_from_slice(bytes_of(v));
}

/// The function `sel` names, if its node is still in the tree.
fn find(s: &Sleepable, sel: &PciSel) -> Result<Listed, Errno> {
    user::find(sel)
        .filter(|e| bus::manager().state(e.id, s).is_some())
        .ok_or(Errno::ENODEV)
}

/// Its driver's name and unit (`device_get_name`, `device_get_unit`).
fn name_of(s: &Sleepable, e: &Listed) -> Option<(&'static str, u32)> {
    bus::manager().class_unit(e.id, s).map(|(c, u, _)| (c, u))
}

fn fault<T>(_: T) -> Errno {
    Errno::EFAULT
}

/// `PCIOCGETCONF`.
fn getconf(s: &Sleepable, arg: &mut [u8]) -> Result<(), Errno> {
    let mut io: PciConfIo = take(arg)?;
    let m = bus::manager();
    let (generation, list) = user::snapshot();
    // A function whose node was deleted has left the list.
    let list: Vec<Listed> = list
        .into_iter()
        .filter(|e| m.state(e.id, s).is_some())
        .collect();
    let user_addr = |a: usize| u64::try_from(a).unwrap_or(u64::MAX);
    let (patterns_at, matches_at) = (user_addr(io.patterns), user_addr(io.matches));
    let patterns = |n: usize| -> Option<Vec<PciMatchConf>> {
        let mut v = Vec::new();
        v.try_reserve_exact(n).ok()?;
        for i in 0..n {
            let mut p = PciMatchConf::default();
            let at = patterns_at.checked_add(u64::try_from(i * size_of::<PciMatchConf>()).ok()?)?;
            crate::uaccess::copy_in(bytes_of_mut(&mut p), at).ok()?;
            v.push(p);
        }
        Some(v)
    };
    let write = |i: usize, c: &PciConf| {
        let at = u64::try_from(i * size_of::<PciConf>())
            .ok()
            .and_then(|off| matches_at.checked_add(off))
            .ok_or(Errno::EFAULT)?;
        crate::uaccess::copy_out(at, bytes_of(c)).map_err(fault)
    };
    let conf = |e: &Listed| user::conf(&e.child, name_of(s, e));
    match user::getconf(&mut io, generation, &list, patterns, conf, write) {
        Ok(()) => {
            put(arg, &io);
            Ok(())
        }
        Err(GetconfError::Invalid) => Err(Errno::EINVAL),
        Err(GetconfError::Write(e)) => Err(e),
    }
}

/// `PCIOCREAD` and `PCIOCWRITE`.
fn config(s: &Sleepable, arg: &mut [u8], write: bool) -> Result<(), Errno> {
    let mut io: PciIo = take(arg)?;
    let width = Width::of(io.pi_width).ok_or(Errno::EINVAL)?;
    // Not negative, and aligned to the width.
    let reg = u32::try_from(io.pi_reg).map_err(|_| Errno::EINVAL)?;
    if !reg.is_multiple_of(u32::from(width.bytes())) {
        return Err(Errno::EINVAL);
    }
    let e = find(s, &io.pi_sel)?;
    if write {
        user::write_config(&e.child, reg, width, io.pi_data);
    } else {
        io.pi_data = user::read_config(&e.child, reg, width);
    }
    put(arg, &io);
    Ok(())
}

/// `PCIOCGETBAR`.
fn getbar(s: &Sleepable, arg: &mut [u8]) -> Result<(), Errno> {
    let mut b: PciBarIo = take(arg)?;
    let e = find(s, &b.pbi_sel)?;
    let m = user::find_bar(&e.child, b.pbi_reg).ok_or(Errno::EINVAL)?;
    b.pbi_base = m.value;
    b.pbi_length = m.size();
    b.pbi_enabled = i32::from(user::bar_enabled(&e.child, &m));
    put(arg, &b);
    Ok(())
}

/// `PCIOCATTACHED`.
fn attached(s: &Sleepable, arg: &mut [u8]) -> Result<(), Errno> {
    let mut io: PciIo = take(arg)?;
    let e = find(s, &io.pi_sel)?;
    let on = bus::manager().state(e.id, s) == Some(bus::NodeState::Attached);
    io.pi_data = u32::from(on);
    put(arg, &io);
    Ok(())
}

/// FreeBSD polls a VPD read a million times a microsecond apart.
const VPD_POLLS: u32 = 1_000_000;

/// `PCIOCLISTVPD`.
fn list_vpd(s: &Sleepable, arg: &mut [u8]) -> Result<(), Errno> {
    let mut lv: PciListVpdIo = take(arg)?;
    let e = find(s, &lv.plvi_sel)?;
    let m = bus::manager();
    let mut wait = |n: u32| {
        m.delay_us(1);
        n < VPD_POLLS
    };
    let v = user::vpd(&e.child, &mut wait).ok_or(Errno::ENXIO)?;
    let bytes = user::vpd_elements(v);
    let len = bytes.len();
    if lv.plvi_len == 0 {
        lv.plvi_len = len;
        put(arg, &lv);
        return Ok(());
    }
    if lv.plvi_len < len {
        return Err(Errno::ENOMEM);
    }
    let to = u64::try_from(lv.plvi_data).map_err(|_| Errno::EFAULT)?;
    crate::uaccess::copy_out(to, &bytes).map_err(fault)?;
    lv.plvi_len = len;
    put(arg, &lv);
    Ok(())
}

/// What a `pbm_memattr` maps as. Write-through and write-protected
/// (amd64), and nGnRnE (arm64), have no mapping of their own
/// here.
fn memattr(attr: i32) -> Option<vm::object::MemAttr> {
    use vm::object::MemAttr;
    #[cfg(target_arch = "x86_64")]
    {
        use abi::pciio::memattr_amd64 as a;
        match attr {
            a::UNCACHEABLE | a::WEAK_UNCACHEABLE => Some(MemAttr::Device),
            a::WRITE_COMBINING => Some(MemAttr::WriteCombining),
            a::WRITE_BACK => Some(MemAttr::WriteBack),
            _ => None,
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        use abi::pciio::memattr_arm64 as a;
        match attr {
            a::DEVICE_NGNRE => Some(MemAttr::Device),
            // Normal non-cacheable, which is how write-combining is
            // mapped here.
            a::UNCACHEABLE => Some(MemAttr::WriteCombining),
            a::WRITE_BACK => Some(MemAttr::WriteBack),
            _ => None,
        }
    }
}

/// `PCIOCBARMMAP` (`pci_bar_mmap`): the BAR's pages, shared, into the
/// caller's address space.
fn bar_mmap(s: &Sleepable, arg: &mut [u8], fflag: u32) -> Result<(), Errno> {
    let mut pbm: PciBarMmap = take(arg)?;
    if fflag & FWRITE == 0 && pbm.pbm_flags & PCIIO_BAR_MMAP_RW != 0 {
        return Err(Errno::EPERM);
    }
    let e = find(s, &pbm.pbm_sel)?;
    let known =
        PCIIO_BAR_MMAP_FIXED | PCIIO_BAR_MMAP_EXCL | PCIIO_BAR_MMAP_RW | PCIIO_BAR_MMAP_ACTIVATE;
    if pbm.pbm_flags & !known != 0 {
        return Err(Errno::EINVAL);
    }
    let attr = memattr(pbm.pbm_memattr).ok_or(Errno::EINVAL)?;
    let m = user::find_bar(&e.child, pbm.pbm_reg).ok_or(Errno::EINVAL)?;
    if !user::bar_enabled(&e.child, &m) {
        return Err(Errno::EBUSY);
    }
    if !m.is_memory() {
        return Err(Errno::EIO);
    }
    let page = vm::PAGE_SIZE;
    let membase = m.base();
    let pbase = membase & !(page - 1);
    let end = membase
        .checked_add(m.size())
        .and_then(|e| addr::align_up(e, page))
        .ok_or(Errno::EINVAL)?;
    let plen = end - pbase;
    let prot = if pbm.pbm_flags & PCIIO_BAR_MMAP_RW != 0 {
        vm::map::Prot::RW
    } else {
        vm::map::Prot::READ
    };
    let fixed = pbm.pbm_flags & PCIIO_BAR_MMAP_FIXED != 0;
    let at = u64::try_from(pbm.pbm_map_base).map_err(|_| Errno::EINVAL)?;
    if fixed && !at.is_multiple_of(page) {
        return Err(Errno::EINVAL);
    }
    let object = vm::object::Object::device(
        crate::mm::space::page_ops(),
        vm::Pfn::containing(PhysAddr::new(pbase)),
        plen / page,
        attr,
    );
    let hint =
        crate::proc::Process::with_current(crate::proc::Process::mmap_base).ok_or(Errno::EINVAL)?;
    let a = Anon {
        fixed: fixed.then_some(VirtAddr::new(at)),
        exclusive: pbm.pbm_flags & PCIIO_BAR_MMAP_EXCL != 0,
        hint: VirtAddr::new(hint),
        below: None,
        len: plen,
        align: page,
        prot,
        max_prot: prot,
        inherit: vm::map::Inherit::Share,
        kind: AnonKind::Memory,
    };
    let va = AddressSpace::with_current(|sp| sp.map_object(s, &a, object, 0, false))
        .ok_or(Errno::EINVAL)?
        .map_err(|e| match e {
            crate::mm::space::SpaceError::OutOfMemory
            | crate::mm::space::SpaceError::Map(vm::map::MapError::NoSpace) => Errno::ENOMEM,
            _ => Errno::EINVAL,
        })?;
    pbm.pbm_map_base = usize::try_from(va.get()).map_err(|_| Errno::EINVAL)?;
    pbm.pbm_map_length = usize::try_from(plen).map_err(|_| Errno::EINVAL)?;
    pbm.pbm_bar_off = i32::try_from(membase - pbase).map_err(|_| Errno::EINVAL)?;
    pbm.pbm_bar_length = m.size();
    put(arg, &pbm);
    Ok(())
}

/// `PCIOCBARIO` (`pci_bar_io`): one access to a BAR no driver has.
fn bar_io(s: &Sleepable, arg: &mut [u8]) -> Result<(), Errno> {
    let mut pbi: PciBarIoreq = take(arg)?;
    if pbi.pbi_op != PCIBARIO_READ && pbi.pbi_op != PCIBARIO_WRITE {
        return Err(Errno::EINVAL);
    }
    let e = find(s, &pbi.pbi_sel)?;
    let reg = pbi
        .pbi_bar
        .checked_mul(4)
        .and_then(|r| r.checked_add(0x10))
        .and_then(|r| i32::try_from(r).ok())
        .ok_or(Errno::EINVAL)?;
    let m = user::find_bar(&e.child, reg).ok_or(Errno::EINVAL)?;
    let (offset, width) = (pbi.pbi_offset, pbi.pbi_width);
    let end = offset.checked_add(width).ok_or(Errno::EINVAL)?;
    if m.size() < u64::from(end) {
        return Err(Errno::EINVAL);
    }
    // A driver's BARs are its own; an unassigned BAR has nothing to reach.
    let mgr = bus::manager();
    if mgr.state(e.id, s) == Some(bus::NodeState::Attached) || m.base() == 0 {
        return Err(Errno::ENOENT);
    }
    let write = pbi.pbi_op == PCIBARIO_WRITE;
    if m.is_memory() {
        // Aligned: a misaligned access to device memory faults on arm64.
        if !matches!(width, 1 | 2 | 4 | 8) || !offset.is_multiple_of(width) {
            return Err(Errno::EINVAL);
        }
        let pa = m.base() + u64::from(offset);
        let va = crate::mm::map_device(PhysAddr::new(pa), u64::from(width))
            .map_err(|_| Errno::ENOENT)?;
        let v = mmio(va, width, write.then_some(pbi.pbi_value));
        crate::mm::unmap_device(va, u64::from(width));
        if let Some(v) = v {
            pbi.pbi_value = v;
        }
    } else {
        let port = u16::try_from(m.base() + u64::from(offset)).map_err(|_| Errno::ENOENT)?;
        let v = port_io(port, width, write.then_some(pbi.pbi_value))?;
        if let Some(v) = v {
            pbi.pbi_value = v;
        }
    }
    put(arg, &pbi);
    Ok(())
}

/// One access of `width` bytes to the device memory mapped at `va`: a
/// write of `value`, or a read whose value it returns (the low 32 bits,
/// as `pbi_value` holds them).
#[allow(
    clippy::cast_possible_truncation,
    reason = "the value is `width` bytes wide; pbi_value holds 32 bits"
)]
fn mmio(va: VirtAddr, width: u32, value: Option<u32>) -> Option<u32> {
    let p = usize::try_from(va.get()).ok()?;
    // SAFETY: `va` maps `width` bytes of a BAR as device memory, just
    // mapped for this access and aligned to `width` (the caller checked
    // the offset; BARs are page aligned); no driver has the BAR.
    unsafe {
        match (width, value) {
            (1, Some(v)) => core::ptr::with_exposed_provenance_mut::<u8>(p).write_volatile(v as u8),
            (2, Some(v)) => {
                core::ptr::with_exposed_provenance_mut::<u16>(p).write_volatile(v as u16)
            }
            (4, Some(v)) => core::ptr::with_exposed_provenance_mut::<u32>(p).write_volatile(v),
            (_, Some(v)) => {
                core::ptr::with_exposed_provenance_mut::<u64>(p).write_volatile(u64::from(v))
            }
            (1, None) => {
                return Some(u32::from(
                    core::ptr::with_exposed_provenance::<u8>(p).read_volatile(),
                ));
            }
            (2, None) => {
                return Some(u32::from(
                    core::ptr::with_exposed_provenance::<u16>(p).read_volatile(),
                ));
            }
            (4, None) => return Some(core::ptr::with_exposed_provenance::<u32>(p).read_volatile()),
            (_, None) => {
                return Some(core::ptr::with_exposed_provenance::<u64>(p).read_volatile() as u32);
            }
        }
    }
    None
}

/// One access to I/O port `port`.
#[cfg(target_arch = "x86_64")]
#[allow(
    clippy::cast_possible_truncation,
    reason = "the value is `width` bytes wide"
)]
fn port_io(port: u16, width: u32, value: Option<u32>) -> Result<Option<u32>, Errno> {
    use crate::arch::cpu::{inb, inl, inw, outb, outl, outw};
    // SAFETY: the port is in an I/O BAR the function decodes and no driver
    // has (checked by the caller), as FreeBSD's `bus_alloc_resource` lets
    // root reach it.
    unsafe {
        match (width, value) {
            (1, Some(v)) => outb(port, v as u8),
            (2, Some(v)) => outw(port, v as u16),
            (4, Some(v)) => outl(port, v),
            (1, None) => return Ok(Some(u32::from(inb(port)))),
            (2, None) => return Ok(Some(u32::from(inw(port)))),
            (4, None) => return Ok(Some(inl(port))),
            _ => return Err(Errno::EINVAL),
        }
    }
    Ok(None)
}

/// No port space on arm64 here: the resource cannot be had.
#[cfg(not(target_arch = "x86_64"))]
fn port_io(_port: u16, _width: u32, _value: Option<u32>) -> Result<Option<u32>, Errno> {
    Err(Errno::ENOENT)
}

/// `pci_ioctl`.
fn request(s: &Sleepable, req: u32, arg: &mut [u8], fflag: u32) -> Result<(), Errno> {
    // A read-only descriptor changes nothing.
    if fflag & FWRITE == 0 && !matches!(req, PCIOCGETCONF | PCIOCGETBAR | PCIOCLISTVPD) {
        return Err(Errno::EPERM);
    }
    match req {
        PCIOCGETCONF => getconf(s, arg),
        PCIOCREAD => config(s, arg, false),
        PCIOCWRITE => config(s, arg, true),
        PCIOCGETBAR => getbar(s, arg),
        PCIOCATTACHED => attached(s, arg),
        PCIOCLISTVPD => list_vpd(s, arg),
        PCIOCBARMMAP => bar_mmap(s, arg, fflag),
        PCIOCBARIO => bar_io(s, arg),
        _ => Err(Errno::ENOTTY),
    }
}

/// `/dev/pci` (`pcicdev`).
struct DevPci;

impl CharDevice for DevPci {
    /// `pci_open`: writing needs securelevel 0 or below.
    fn instance_open(
        &self,
        _s: &Sleepable,
        _nonblock: bool,
        _read: bool,
        write: bool,
    ) -> Result<Option<Arc<dyn CharDevice>>, Errno> {
        if write {
            crate::fs::cred().securelevel_gt(0)?;
        }
        Ok(None)
    }

    /// No `d_read`.
    fn read(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }

    /// No `d_write`.
    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }

    /// Without the open file's flags: as read-only.
    fn ioctl(&self, s: &Sleepable, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        request(s, req, arg, FREAD)
    }

    fn ioctl_file(
        &self,
        s: &Sleepable,
        req: u32,
        arg: &mut [u8],
        fflag: u32,
    ) -> Result<usize, Errno> {
        request(s, req, arg, fflag).map(|()| 0)
    }
}

/// Adds `/dev/pci`, `root:wheel` 0644 (`pci_modevent`).
pub(crate) fn init() {
    crate::fs::devfs::add(PATH_DEVPCI, 0o644, 0, 0, Arc::new(DevPci));
}

#[cfg(feature = "ktest")]
pub(crate) mod ktests {

    use ktest::ktest;

    use super::*;
    use crate::devctl::ktests::cx;

    fn open(s: &Sleepable) -> Arc<dyn CharDevice> {
        let v = crate::fs::devfs::vnode_of(s, PATH_DEVPCI).expect("/dev/pci");
        let d = v.device(s).unwrap().ops;
        d.instance_open(s, false, true, true).unwrap().unwrap_or(d)
    }

    fn ask<T: Plain>(
        d: &Arc<dyn CharDevice>,
        s: &Sleepable,
        req: u32,
        v: &mut T,
        fflag: u32,
    ) -> Result<(), Errno> {
        let mut arg = bytes_of(v).to_vec();
        d.ioctl_file(s, req, &mut arg, fflag)?;
        bytes_of_mut(v).copy_from_slice(&arg);
        Ok(())
    }

    ktest! {
        fn dev_pci_lists_and_reads_functions() {
            let s = cx();
            let d = open(&s);
            let rw = FREAD | FWRITE;
            // Every function, in a buffer the kernel fills (a kernel
            // address is not a user one: listing goes through the list
            // itself here).
            let (_, list) = pci::user::snapshot();
            assert!(!list.is_empty(), "no PCI functions listed");
            let first = &list[0];
            let sel = pci::user::sel_of(first.child.function.addr);
            // The vendor and device register, as enumeration found them.
            let mut io = PciIo { pi_sel: sel, pi_reg: 0, pi_width: 4, ..PciIo::default() };
            ask(&d, &s, PCIOCREAD, &mut io, rw).unwrap();
            let id = first.child.function.header.ident;
            assert_eq!(io.pi_data, u32::from(id.vendor) | u32::from(id.device) << 16);
            // Narrower reads, and what FreeBSD refuses.
            io.pi_width = 2;
            io.pi_reg = 2;
            ask(&d, &s, PCIOCREAD, &mut io, rw).unwrap();
            assert_eq!(io.pi_data, u32::from(id.device));
            io.pi_reg = 1;
            assert_eq!(ask(&d, &s, PCIOCREAD, &mut io, rw), Err(Errno::EINVAL));
            io.pi_reg = -4;
            io.pi_width = 4;
            assert_eq!(ask(&d, &s, PCIOCREAD, &mut io, rw), Err(Errno::EINVAL));
            io.pi_width = 3;
            io.pi_reg = 0;
            assert_eq!(ask(&d, &s, PCIOCREAD, &mut io, rw), Err(Errno::EINVAL));
            // Past configuration space: all-ones.
            io.pi_width = 4;
            io.pi_reg = 0x1000;
            ask(&d, &s, PCIOCREAD, &mut io, rw).unwrap();
            assert_eq!(io.pi_data, u32::MAX);
            // Read-only: listing only.
            io.pi_reg = 0;
            assert_eq!(ask(&d, &s, PCIOCREAD, &mut io, FREAD), Err(Errno::EPERM));
            assert_eq!(ask(&d, &s, PCIOCATTACHED, &mut io, FREAD), Err(Errno::EPERM));
            // No such function.
            let mut none = PciIo { pi_sel: PciSel { pc_bus: 255, pc_dev: 31, pc_func: 7, ..sel }, pi_width: 4, ..PciIo::default() };
            assert_eq!(ask(&d, &s, PCIOCREAD, &mut none, rw), Err(Errno::ENODEV));
            // Whether a driver is attached, as the tree says.
            let m = bus::manager();
            for e in &list {
                let mut io = PciIo { pi_sel: pci::user::sel_of(e.child.function.addr), ..PciIo::default() };
                ask(&d, &s, PCIOCATTACHED, &mut io, rw).unwrap();
                let on = m.state(e.id, &s) == Some(bus::NodeState::Attached);
                assert_eq!(io.pi_data, u32::from(on));
            }
            // Every recorded BAR, read-only too.
            for e in &list {
                for map in &e.child.function.maps {
                    let mut b = PciBarIo {
                        pbi_sel: pci::user::sel_of(e.child.function.addr),
                        pbi_reg: i32::from(map.reg),
                        ..PciBarIo::default()
                    };
                    ask(&d, &s, PCIOCGETBAR, &mut b, FREAD).unwrap();
                    assert_eq!((b.pbi_base, b.pbi_length), (map.value, map.size()));
                }
            }
            let mut b = PciBarIo { pbi_sel: sel, pbi_reg: 0x0c, ..PciBarIo::default() };
            assert_eq!(ask(&d, &s, PCIOCGETBAR, &mut b, FREAD), Err(Errno::EINVAL));
            // A function without VPD.
            let mut lv = PciListVpdIo { plvi_sel: sel, ..PciListVpdIo::default() };
            if pci::caps::find(&first.child.function.caps, pci::vpd::CAP_ID).is_none() {
                assert_eq!(ask(&d, &s, PCIOCLISTVPD, &mut lv, FREAD), Err(Errno::ENXIO));
            }
            // Patterns that do not add up, and what is not a request.
            let mut io = PciConfIo { num_patterns: 1, ..PciConfIo::default() };
            assert_eq!(ask(&d, &s, PCIOCGETCONF, &mut io, FREAD), Err(Errno::EINVAL));
            assert_eq!(d.ioctl_file(&s, abi::ioctl::FIONREAD, &mut [0; 4], rw), Err(Errno::ENOTTY));
            // Names come from the tree: every attached function has one.
            for e in &list {
                let c = pci::user::conf(&e.child, name_of(&s, e));
                if m.state(e.id, &s) == Some(bus::NodeState::Attached) {
                    assert_ne!(c.pd_name[0], 0, "{:?}", e);
                }
            }
        }
    }
}
