// SPDX-License-Identifier: BSD-2-Clause
//! `/dev/efi`: UEFI variables, time, and configuration tables for user
//! space (`efivar(3)`, `efibootmgr(8)`, `efitable(8)`), through the
//! runtime services in [`crate::firmware`]. The node exists only where
//! runtime services do, `root:wheel` mode 0700, as FreeBSD's `efidev`
//! makes it.
//!
//! The requests are FreeBSD's, with its conventions: a variable or name
//! that does not fit the caller's buffer is not an error but a success
//! with the pointer (`data`, `name`) cleared and the size set to what is
//! needed; a name passed in must end in a NUL in its last character.
//!
//! FreeBSD allocates whatever size the caller passes. Here sizes are
//! bounded so that untrusted input cannot exhaust kernel memory:
//! - A variable's data is read into at most [`MAX_DATA`] bytes. A caller
//!   offering more gets the variable if it fits; one bigger than that
//!   (larger than any firmware's variable store holds) fails with
//!   `ENOMEM` instead of asking for a size it would be refused again.
//!   Setting more than [`MAX_DATA`] bytes is `EINVAL`, what firmware
//!   answers for a variable too big for it.
//! - A name is at most [`MAX_NAME`] bytes (`EINVAL` past that); for
//!   `EFIIOC_VAR_NEXT`, that much of a larger buffer is used, and the
//!   name in it must end within it.
//! - `EFIIOC_GET_TABLE` copies a table only from memory the kernel kept
//!   (runtime and ACPI memory): one in boot-services memory, which the
//!   kernel has reused, is `ENOENT`. It copies `table_len` bytes, where
//!   FreeBSD copies `buf_len` (past its buffer when `buf_len` is larger).
//!
//! See docs/compat-deviations.md.
//!
//! Behavior reference: sys/dev/efidev/efidev.c, sys/dev/efidev/efirt.c.

use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::efi::*;
use abi::errno::Errno;
use efi::raw::{RawGuid, Time};
use sched::Sleepable;
use vfs::{CharDevice, Uio};

use crate::firmware;

/// The most bytes of variable data read or written at once.
pub(crate) const MAX_DATA: usize = 1 << 20;
/// The longest name, in bytes with its NUL.
pub(crate) const MAX_NAME: usize = 64 << 10;

/// A zeroed buffer of `n` elements, or `ENOMEM`.
fn buffer<T: Copy + Default>(n: usize) -> Result<Vec<T>, Errno> {
    let mut v = Vec::new();
    v.try_reserve_exact(n).map_err(|_| Errno::ENOMEM)?;
    v.resize(n, T::default());
    Ok(v)
}

fn fault<T>(_: T) -> Errno {
    Errno::EFAULT
}

/// Copies `bytes` bytes of UCS-2 in from `addr`.
fn name_in(addr: u64, bytes: usize) -> Result<Vec<u16>, Errno> {
    let mut raw = buffer::<u8>(bytes)?;
    crate::uaccess::copy_in(&mut raw, addr).map_err(fault)?;
    let mut name = buffer::<u16>(bytes.div_ceil(2))?;
    for (c, b) in name.iter_mut().zip(raw.chunks(2)) {
        *c = u16::from_ne_bytes([b[0], b.get(1).copied().unwrap_or(0)]);
    }
    Ok(name)
}

/// The name `EFIIOC_VAR_GET` and `EFIIOC_VAR_SET` take: `namesize` bytes
/// whose last whole character is a NUL.
fn terminated_name(addr: u64, namesize: u64) -> Result<Vec<u16>, Errno> {
    let size = usize::try_from(namesize).map_err(|_| Errno::EINVAL)?;
    if !(2..=MAX_NAME).contains(&size) {
        return Err(Errno::EINVAL);
    }
    let name = name_in(addr, size)?;
    if name[size / 2 - 1] != 0 {
        return Err(Errno::EINVAL);
    }
    Ok(name)
}

fn guid(g: EfiGuid) -> RawGuid {
    RawGuid(g.to_bytes())
}

fn efi_tm(t: &Time) -> EfiTm {
    EfiTm {
        tm_year: t.year,
        tm_mon: t.month,
        tm_mday: t.day,
        tm_hour: t.hour,
        tm_min: t.minute,
        tm_sec: t.second,
        pad1: t.pad1,
        tm_nsec: t.nanosecond,
        tm_tz: t.time_zone,
        tm_dst: t.daylight,
        pad2: t.pad2,
    }
}

fn time(t: &EfiTm) -> Time {
    Time {
        year: t.tm_year,
        month: t.tm_mon,
        day: t.tm_mday,
        hour: t.tm_hour,
        minute: t.tm_min,
        second: t.tm_sec,
        pad1: t.pad1,
        nanosecond: t.tm_nsec,
        time_zone: t.tm_tz,
        daylight: t.tm_dst,
        pad2: t.pad2,
    }
}

/// `EFIIOC_VAR_GET`.
fn var_get(arg: &mut [u8]) -> Result<(), Errno> {
    let mut ev = EfiVarIoctl::from_bytes(arg).ok_or(Errno::EINVAL)?;
    let wanted = usize::try_from(ev.datasize).unwrap_or(usize::MAX);
    let mut data = buffer::<u8>(wanted.min(MAX_DATA))?;
    let name = terminated_name(ev.name, ev.namesize)?;
    let mut size = data.len();
    let mut attrib = ev.attrib;
    let r = firmware::get_variable(&name, &guid(ev.vendor), &mut attrib, &mut size, &mut data);
    ev.attrib = attrib;
    ev.datasize = u64::try_from(size).map_err(|_| Errno::EINVAL)?;
    match r {
        Ok(()) => {
            let out = data.get(..size).ok_or(Errno::EIO)?;
            crate::uaccess::copy_out(ev.data, out).map_err(fault)?;
        }
        // The caller's buffer was big enough, but more than this reads.
        Err(Errno::EOVERFLOW) if size <= wanted => return Err(Errno::ENOMEM),
        // The size needed goes back, and no data.
        Err(Errno::EOVERFLOW) => ev.data = 0,
        Err(e) => return Err(e),
    }
    ev.store(arg);
    Ok(())
}

/// `EFIIOC_VAR_NEXT`.
fn var_next(arg: &mut [u8]) -> Result<(), Errno> {
    let mut ev = EfiVarIoctl::from_bytes(arg).ok_or(Errno::EINVAL)?;
    let wanted = usize::try_from(ev.namesize).unwrap_or(usize::MAX);
    let room = wanted.min(MAX_NAME);
    let mut name = name_in(ev.name, room)?;
    // The previous name must end within the buffer, or firmware would
    // read past it (`EFI_INVALID_PARAMETER` in UEFI 2.7 and later).
    if !name[..room / 2].contains(&0) {
        return Err(Errno::EINVAL);
    }
    let mut size = room;
    let mut vendor = guid(ev.vendor);
    let r = firmware::next_variable_name(&mut size, &mut name, &mut vendor);
    ev.vendor = EfiGuid::from_bytes(&vendor.0);
    ev.namesize = u64::try_from(size).map_err(|_| Errno::EINVAL)?;
    match r {
        Ok(()) => {
            let n = name.get(..size.div_ceil(2)).ok_or(Errno::EIO)?;
            let mut out = buffer::<u8>(n.len() * 2)?;
            for (b, c) in out.chunks_mut(2).zip(n) {
                b.copy_from_slice(&c.to_ne_bytes());
            }
            out.truncate(size);
            crate::uaccess::copy_out(ev.name, &out).map_err(fault)?;
        }
        Err(Errno::EOVERFLOW) if size <= wanted => return Err(Errno::ENOMEM),
        Err(Errno::EOVERFLOW) => ev.name = 0,
        Err(e) => return Err(e),
    }
    ev.store(arg);
    Ok(())
}

/// `EFIIOC_VAR_SET`; no data deletes the variable.
fn var_set(arg: &[u8]) -> Result<(), Errno> {
    let ev = EfiVarIoctl::from_bytes(arg).ok_or(Errno::EINVAL)?;
    let size = usize::try_from(ev.datasize).map_err(|_| Errno::EINVAL)?;
    if size > MAX_DATA {
        return Err(Errno::EINVAL);
    }
    let mut data = buffer::<u8>(size)?;
    if size > 0 {
        crate::uaccess::copy_in(&mut data, ev.data).map_err(fault)?;
    }
    let name = terminated_name(ev.name, ev.namesize)?;
    firmware::set_variable(&name, &guid(ev.vendor), ev.attrib, &data)
}

/// `EFIIOC_GET_TABLE`: one of the tables FreeBSD knows the length of.
fn get_table(arg: &mut [u8]) -> Result<(), Errno> {
    let mut t = EfiGetTableIoctl::from_bytes(arg).ok_or(Errno::EINVAL)?;
    let kind = KnownTable::of(t.guid).ok_or(Errno::EINVAL)?;
    let pa = firmware::config_table(&t.guid.to_bytes())?;
    let mut header = [0u8; 16];
    let hlen = usize::try_from(kind.header_len()).map_err(|_| Errno::EINVAL)?;
    firmware::read_table(pa, &mut header[..hlen])?;
    let len = kind.length(&header)?;
    t.table_len = len;
    if t.buf != 0 {
        if t.buf_len < len {
            return Err(Errno::EINVAL);
        }
        // At most EFI_TABLE_ALLOC_MAX (`length` checks).
        let mut table = buffer::<u8>(usize::try_from(len).map_err(|_| Errno::ENOMEM)?)?;
        firmware::read_table(pa, &mut table)?;
        crate::uaccess::copy_out(t.buf, &table).map_err(fault)?;
    }
    t.store(arg);
    Ok(())
}

fn request(req: u32, arg: &mut [u8]) -> Result<(), Errno> {
    match req {
        EFIIOC_GET_TABLE => get_table(arg),
        EFIIOC_GET_TIME => {
            let t = firmware::get_time()?;
            arg.copy_from_slice(&efi_tm(&t).to_bytes());
            Ok(())
        }
        EFIIOC_SET_TIME => {
            let t = EfiTm::from_bytes(arg).ok_or(Errno::EINVAL)?;
            firmware::set_time(&time(&t))
        }
        EFIIOC_GET_WAKETIME => {
            let (enabled, pending, t) = firmware::get_wakeup_time()?;
            arg[..EFI_TM_LEN].copy_from_slice(&efi_tm(&t).to_bytes());
            arg[EFI_TM_LEN] = enabled;
            arg[EFI_TM_LEN + 1] = pending;
            Ok(())
        }
        EFIIOC_SET_WAKETIME => {
            let t = EfiTm::from_bytes(arg).ok_or(Errno::EINVAL)?;
            firmware::set_wakeup_time(arg[EFI_TM_LEN], &time(&t))
        }
        EFIIOC_VAR_GET => var_get(arg),
        EFIIOC_VAR_NEXT => var_next(arg),
        EFIIOC_VAR_SET => var_set(arg),
        _ => Err(Errno::ENOTTY),
    }
}

/// `/dev/efi`: requests only.
struct EfiDev;

impl CharDevice for EfiDev {
    /// No `d_read` (`enodev`).
    fn read(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }

    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }

    fn ioctl(&self, _: &Sleepable, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        request(req, arg)
    }
}

/// Adds `/dev/efi` if runtime services are available (`efidev_modevents`).
pub(crate) fn init() {
    if firmware::available() {
        crate::fs::devfs::add(b"efi", 0o700, 0, 0, Arc::new(EfiDev));
    }
}
