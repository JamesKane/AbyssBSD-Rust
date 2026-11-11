// SPDX-License-Identifier: BSD-2-Clause
//! The kernel environment (`kern_environment.c`): the loader's variables,
//! from the command line, which `kenv(2)` reads and changes. init reads
//! `init_exec`, `init_script`, and `init_chroot` from it, the kernel
//! `init_path`.

use abi::errno::Errno;
use alloc::vec::Vec;
use security::Priv;
use sync::SpinLock;

use crate::uaccess;

/// `KENV_GET`.
const KENV_GET: u64 = 0;
/// `KENV_SET`.
const KENV_SET: u64 = 1;
/// `KENV_UNSET`.
const KENV_UNSET: u64 = 2;
/// `KENV_DUMP`.
const KENV_DUMP: u64 = 3;
/// `KENV_DUMP_LOADER`.
const KENV_DUMP_LOADER: u64 = 4;
/// `KENV_DUMP_STATIC`.
const KENV_DUMP_STATIC: u64 = 5;
/// `KENV_MNAMELEN`: the longest name `kenv(2)` takes.
const MNAMELEN: usize = 128;
/// `KENV_MVALLEN` (the `kenv_mvallen` tunable's default).
const MVALLEN: usize = 128;
/// `KENV_SIZE`: the most variables there may be.
const SIZE: usize = 512;

/// The variables, `name=value` each, in the order set.
static ENV: SpinLock<Vec<Vec<u8>>> = SpinLock::new(Vec::new());

/// Fills the environment from the loader's: the variables it set itself
/// (SMBIOS's), then each `name=value` word of the command line (bare
/// words are kernel flags, not variables), a later setting replacing an
/// earlier one. They are kept sorted by name, as FreeBSD's loader keeps
/// its environment (stand/libsa/environment.c), which is the order the
/// kernel's static environment, and so `kenv`, lists them in.
pub(crate) fn init() {
    let mut env = ENV.lock();
    let loader = crate::boot::env()
        .split(|&b| b == 0)
        .filter(|e| e.iter().position(|&b| b == b'=').is_some_and(|i| i > 0));
    let cmdline = crate::boot::cmdline()
        .split_ascii_whitespace()
        .map(str::as_bytes)
        .filter(|w| w.iter().position(|&b| b == b'=').is_some_and(|i| i > 0));
    for w in loader.chain(cmdline) {
        let name = &w[..w.iter().position(|&b| b == b'=').unwrap_or(0)];
        env.retain(|e| !is(e, name));
        if env.len() < SIZE {
            env.push(w.to_vec());
        }
    }
    env.sort_by(|a, b| {
        let key = |e: &[u8]| e.split(|&c| c == b'=').next().unwrap_or(&[]).to_vec();
        key(a).cmp(&key(b))
    });
}

/// Whether `entry` (`name=value`) is the variable `name`.
fn is(entry: &[u8], name: &[u8]) -> bool {
    entry
        .strip_prefix(name)
        .is_some_and(|r| r.first() == Some(&b'='))
}

/// `kern_getenv`: the value of `name`.
pub(crate) fn get(name: &[u8]) -> Option<Vec<u8>> {
    ENV.lock()
        .iter()
        .find(|e| is(e, name))
        .map(|e| e[name.len() + 1..].to_vec())
}

/// `kern_setenv`: false when full or too long, as its -1.
fn set(name: &[u8], value: &[u8]) -> bool {
    if name.len() + 1 + value.len() > MNAMELEN + 1 + MVALLEN || name.contains(&b'=') {
        return false;
    }
    let mut entry = Vec::with_capacity(name.len() + 1 + value.len());
    entry.extend_from_slice(name);
    entry.push(b'=');
    entry.extend_from_slice(value);
    let mut env = ENV.lock();
    if let Some(e) = env.iter_mut().find(|e| is(e, name)) {
        *e = entry;
        return true;
    }
    if env.len() >= SIZE {
        return false;
    }
    env.push(entry);
    true
}

/// `kern_unsetenv`: whether `name` was there.
fn unset(name: &[u8]) -> bool {
    let mut env = ENV.lock();
    let before = env.len();
    env.retain(|e| !is(e, name));
    env.len() != before
}

/// `kenv(what, name, value, len)`, as `sys_kenv`.
pub(crate) fn kenv(what: u64, name: u64, value: u64, len: u64) -> crate::syscall::SysResult {
    let len = i32::try_from(len.cast_signed()).map_err(|_| Errno::EINVAL)?;
    match what {
        KENV_DUMP => return dump(value, len),
        // Without `PRESERVE_EARLY_KENV` (not in GENERIC).
        KENV_DUMP_LOADER | KENV_DUMP_STATIC => return Err(Errno::ENOENT),
        KENV_SET => current()?.priv_check(Priv::KenvSet)?,
        KENV_UNSET => current()?.priv_check(Priv::KenvUnset)?,
        _ => {}
    }
    let name = uaccess::copy_in_str_vec(name, MNAMELEN + 1).map_err(str_errno)?;
    match what {
        KENV_GET => {
            let v = get(&name).ok_or(Errno::ENOENT)?;
            let n = (v.len() + 1).min(usize::try_from(len).unwrap_or(0));
            let mut out = v;
            out.push(0);
            uaccess::copy_out(value, &out[..n]).map_err(|_| Errno::EFAULT)?;
            Ok(n as u64)
        }
        KENV_SET => {
            if len < 1 {
                return Err(Errno::EINVAL);
            }
            let max = usize::try_from(len).unwrap_or(0).min(MVALLEN + 1);
            let v = uaccess::copy_in_str_vec(value, max).map_err(str_errno)?;
            set(&name, &v);
            Ok(0)
        }
        KENV_UNSET => {
            if unset(&name) {
                Ok(0)
            } else {
                Err(Errno::ENOENT)
            }
        }
        _ => Err(Errno::EINVAL),
    }
}

/// `kenv_dump`: as many whole or cut variables as fit in `len` bytes at
/// `value`; returns 0 when they all fit, else the size they need.
fn dump(value: u64, len: i32) -> crate::syscall::SysResult {
    let room = usize::try_from(len)
        .unwrap_or(0)
        .min(SIZE * (MNAMELEN + MVALLEN + 2));
    let mut out = Vec::new();
    let mut needed = 0;
    for e in ENV.lock().iter() {
        needed += e.len() + 1;
        if value != 0 {
            let n = (e.len() + 1).min(room - out.len());
            out.extend(e.iter().copied().chain(core::iter::once(0)).take(n));
        }
    }
    if value != 0 && len > 0 {
        uaccess::copy_out(value, &out).map_err(|_| Errno::EFAULT)?;
    }
    Ok(if out.len() == needed {
        0
    } else {
        needed as u64
    })
}

/// `copyinstr`'s errors.
fn str_errno(e: uaccess::StrError) -> Errno {
    match e {
        uaccess::StrError::Fault => Errno::EFAULT,
        uaccess::StrError::TooLong => Errno::ENAMETOOLONG,
    }
}

/// The calling process.
fn current() -> Result<alloc::sync::Arc<crate::proc::Process>, Errno> {
    crate::proc::Process::current().ok_or(Errno::EINVAL)
}

#[cfg(feature = "ktest")]
mod ktests {
    use ktest::ktest;

    use super::*;

    fn val(name: &str) -> alloc::string::String {
        get(name.as_bytes())
            .map(|v| alloc::string::String::from_utf8_lossy(&v).into_owned())
            .unwrap_or_default()
    }

    ktest! {
        /// The loader's SMBIOS variables, as FreeBSD's loader sets them;
        /// in QEMU, the strings xtask gives it (`-smbios`).
        fn kenv_has_the_loaders_smbios_variables() {
            let hint = val("hint.smbios.0.mem");
            assert!(hint.starts_with("0x") && hint.len() > 2, "hint {hint:?}");
            assert!(
                matches!(val("smbios.entry_point_type").as_str(), "v3 (64-bit)" | "v2.1 (32-bit)"),
                "{:?}",
                val("smbios.entry_point_type")
            );
            assert!(!val("smbios.version").is_empty());
            if val("smbios.system.maker") == "AbyssBSD Project" {
                assert_eq!(val("smbios.system.product"), "Abyss Test Machine");
                assert_eq!(val("smbios.system.version"), "1.0");
                assert_eq!(val("smbios.system.serial"), "ABYSS-0001");
                // SMBIOS 2.6 and later: the first three fields little-endian,
                // which FreeBSD prints as written.
                assert_eq!(val("smbios.system.uuid"), "4abe6a8f-1c2d-4e5f-8a9b-0c1d2e3f4a5b");
                assert_eq!(val("smbios.system.sku"), "T1");
                assert_eq!(val("smbios.system.family"), "Test");
                assert_eq!(val("smbios.bios.vendor"), "AbyssBSD Firmware");
                assert_eq!(val("smbios.bios.version"), "7.1");
                assert_eq!(val("smbios.bios.reldate"), "11/10/2026");
                assert_eq!(val("smbios.bios.revision"), "7.1");
                assert_eq!(val("smbios.planar.product"), "Abyss Test Board");
                assert_eq!(val("smbios.chassis.version"), "3.0");
            }
            // Sorted by name, as the loader keeps them.
            let env = ENV.lock();
            let names: alloc::vec::Vec<&[u8]> = env
                .iter()
                .map(|e| e.split(|&c| c == b'=').next().unwrap_or(&[]))
                .collect();
            assert!(names.windows(2).all(|w| w[0] < w[1]));
        }
    }
}
