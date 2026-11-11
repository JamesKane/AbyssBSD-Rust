// SPDX-License-Identifier: BSD-2-Clause
//! `/dev/geom.ctl`: GEOM's control requests (`GEOM_CTL`), which libgeom's
//! `gctl_issue` sends for `gpart`, `geom`, and `glabel`.
//!
//! The request (`struct gctl_req`) and its arguments are copied in, run
//! on GEOM's event thread (`g_waitfor_event`), and the values a class
//! wrote copied back out; the first error a class records is the text
//! the caller gets, and the request fails with `EINVAL` (or the error it
//! names). A request names its class: `getxml` (a geom and its parents'
//! part of `kern.geom.confxml`) is every class's; the PART class's verbs
//! go to [`geom::table::ctlreq`], with the disks of `sys/storage` as its
//! topology. Classes without verbs say so ("Class takes no requests").
//!
//! Behavior reference: sys/geom/geom_ctl.c, sys/geom/part/g_part.c,
//! lib/libgeom/geom_ctl.c.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

use abi::errno::Errno;
use abi::geom::{GCTL_VERSION, GEOM_CTL, GEOM_CTL_ARG_MAX};
use geom::ctl::{Arg, Request, Show};
use geom::table::{Media, Provider, Table, Topology};
use sched::Sleepable;
use storage::Disk;
use sync::SpinLock;
use vfs::{CharDevice, Uio};

/// FreeBSD's `GID_OPERATOR`, which owns the node.
const GID_OPERATOR: u32 = 5;
/// A name's longest length, its NUL included (`SPECNAMELEN`).
const SPECNAMELEN: u32 = 255;
/// `sizeof(struct gctl_req_arg)`.
const ARG_SIZE: usize = 48;

/// `/dev/geom.ctl` (`g_ctl_cdevsw`): requests only.
struct GeomCtl;

impl CharDevice for GeomCtl {
    fn read(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }

    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }

    fn ioctl(&self, s: &Sleepable, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        if req != GEOM_CTL {
            return Err(Errno::ENOTTY);
        }
        ioctl_ctl(s, arg)
    }
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4)
        .and_then(|c| <[u8; 4]>::try_from(c).ok())
        .map_or(0, u32::from_ne_bytes)
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    b.get(at..at + 8)
        .and_then(|c| <[u8; 8]>::try_from(c).ok())
        .map_or(0, u64::from_ne_bytes)
}

/// `len` bytes from user address `at`, or the request's error.
fn copy_in(req: &mut Request, at: u64, len: usize) -> Option<Vec<u8>> {
    let mut v = Vec::new();
    if v.try_reserve_exact(len).is_err() {
        req.nerror = Some(Errno::ENOMEM);
        return None;
    }
    v.resize(len, 0);
    if crate::uaccess::copy_in(&mut v, at).is_err() {
        req.nerror = Some(Errno::EFAULT);
        return None;
    }
    Some(v)
}

/// `gctl_copyin`: the arguments, each name and readable value copied in;
/// the user addresses of the values, to copy written ones back to.
fn copy_args(req: &mut Request, narg: u32, at: u64) -> Vec<u64> {
    if narg > GEOM_CTL_ARG_MAX {
        req.error(format_args!("too many arguments"));
        return Vec::new();
    }
    let n = usize::try_from(narg).unwrap_or(0);
    let Some(raw) = copy_in(req, at, n * ARG_SIZE) else {
        req.error(format_args!("bad control request"));
        return Vec::new();
    };
    let mut values = Vec::new();
    for (i, a) in raw.as_chunks::<ARG_SIZE>().0.iter().enumerate() {
        let a = a.as_slice();
        let (nlen, name, flag, len, value) = (
            u32_at(a, 0),
            u64_at(a, 8),
            u32_at(a, 24),
            i32::from_ne_bytes(u32_at(a, 28).to_ne_bytes()),
            u64_at(a, 32),
        );
        if !(1..=SPECNAMELEN).contains(&nlen) {
            req.error(format_args!("wrong param name length {i}: {nlen}"));
            break;
        }
        let Some(mut name) = copy_in(req, name, usize::try_from(nlen).unwrap_or(0)) else {
            break;
        };
        if name.pop() != Some(0) {
            req.error(format_args!("unterminated param name"));
            break;
        }
        let Ok(len) = usize::try_from(len) else {
            req.error(format_args!("negative param length"));
            break;
        };
        if len == 0 {
            req.error(format_args!("negative param length"));
            break;
        }
        // The flags the caller may set (`GCTL_PARAM_RD`, `_WR`, `_ASCII`).
        let flag = flag & 7;
        let v = if flag & geom::ctl::GCTL_PARAM_RD != 0 {
            let Some(v) = copy_in(req, value, len) else {
                break;
            };
            if flag & geom::ctl::GCTL_PARAM_ASCII != 0 && v.last() != Some(&0) {
                req.error(format_args!("unterminated param value"));
                break;
            }
            v
        } else {
            let mut v = Vec::new();
            if v.try_reserve_exact(len).is_err() {
                req.nerror = Some(Errno::ENOMEM);
                break;
            }
            v.resize(len, 0);
            v
        };
        req.args.push(Arg {
            name,
            flag,
            value: v,
            changed: false,
        });
        values.push(value);
    }
    values
}

/// `g_ctl_ioctl_ctl`.
fn ioctl_ctl(s: &Sleepable, arg: &[u8]) -> Result<(), Errno> {
    let (version, narg, args, lerror, error) = (
        u32_at(arg, 0),
        u32_at(arg, 8),
        u64_at(arg, 16),
        u32_at(arg, 24),
        u64_at(arg, 32),
    );
    // It is an error if no error text can be returned.
    if lerror < 2 {
        return Err(Errno::EINVAL);
    }
    let mut req = Request::default();
    if version == GCTL_VERSION {
        let values = copy_args(&mut req, narg, args);
        if req.nerror.is_none() {
            let shared = Arc::new(SpinLock::new(Some(req)));
            let job = Arc::clone(&shared);
            crate::geom_kern::run(
                s,
                Box::new(move |s| {
                    let taken = job.lock().take();
                    if let Some(mut r) = taken {
                        dispatch(s, &mut r);
                        *job.lock() = Some(r);
                    }
                }),
            );
            req = shared.lock().take().unwrap_or_default();
            // `gctl_copyout`.
            if req.nerror.is_none() {
                for (a, &to) in req.args.iter().zip(&values) {
                    if a.changed && crate::uaccess::copy_out(to, &a.value).is_err() {
                        req.nerror = Some(Errno::EFAULT);
                        break;
                    }
                }
            }
        }
    } else {
        req.error(format_args!("kernel and libgeom version mismatch."));
    }
    if let Some(text) = req.message() {
        let mut out = String::from(text).into_bytes();
        out.push(0);
        out.truncate(usize::try_from(lerror).unwrap_or(usize::MAX));
        if crate::uaccess::copy_out(error, &out).is_err() && req.nerror.is_none() {
            req.nerror = Some(Errno::EFAULT);
        }
    }
    req.nerror.map_or(Ok(()), Err)
}

/// `g_ctl_req`: the request's class and verb.
fn dispatch(s: &Sleepable, req: &mut Request) {
    let Some(class) = req.ascii("class") else {
        req.error(format_args!("Missing class argument"));
        return;
    };
    let Some(class) = crate::geom_kern::class_named(&class) else {
        req.error(format_args!("Class not found: \"{}\"", Show(&class)));
        return;
    };
    let Some(verb) = req.param("verb").map(|v| geom::ctl::c_str(v).to_vec()) else {
        req.error(format_args!("Verb missing"));
        return;
    };
    if verb == b"getxml" {
        getxml(req, class);
    } else if class == "PART" {
        geom::table::ctlreq(req, &verb, &mut Topo { s });
    } else {
        req.error(format_args!("Class takes no requests"));
    }
}

/// `g_ctl_getxml`: the geom `arg0` names (or all) of `class`, and with
/// `parents` its parents', as `confxml` shows them.
fn getxml(req: &mut Request, class: &str) {
    let parents = match req.param("parents").map(<[u8; 4]>::try_from) {
        Some(Ok(p)) => i32::from_ne_bytes(p),
        _ => {
            req.error(format_args!("Wrong length parents argument"));
            return;
        }
    };
    let name = req
        .ascii("arg0")
        .map(|n| String::from_utf8_lossy(&n).into_owned());
    let Some(xml) = crate::geom_kern::build().getxml(class, name.as_deref(), parents != 0) else {
        return;
    };
    let Some(out) = req
        .args
        .iter_mut()
        .find(|a| a.name == b"output" && a.flag & geom::ctl::GCTL_PARAM_WR != 0)
    else {
        req.error(format_args!("output parameter missing"));
        return;
    };
    // `sbuf_new(.., SBUF_FIXEDLEN | SBUF_INCLUDENUL)`: as much as fits,
    // its NUL included.
    let room = out.value.len();
    let bytes = xml.as_bytes();
    let n = bytes.len().min(room.saturating_sub(1));
    out.value[..n].copy_from_slice(&bytes[..n]);
    if let Some(nul) = out.value.get_mut(n) {
        *nul = 0;
    }
    out.changed = true;
    if bytes.len() + 1 > room {
        req.error(format_args!("output buffer overflow"));
    }
}

/// The disks as the PART class's topology: a geom is named as the disk
/// its table is on.
struct Topo<'a> {
    s: &'a Sleepable,
}

/// The disk or partition named `name` (a geom's name is its provider's;
/// tables nest in partitions).
fn whole(name: &str) -> Option<Arc<Disk>> {
    storage::disk::all().into_iter().find(|d| d.name() == name)
}

/// Partition `index` (1-based) of `disk`.
fn partition(disk: &Arc<Disk>, index: u32) -> Option<Arc<Disk>> {
    storage::partitions(disk)
        .into_iter()
        .find(|p| p.partition().is_some_and(|(_, e)| e.index + 1 == index))
}

/// A disk's media as its provider shows it: a memory disk's geometry is
/// its own.
fn media(d: &Arc<Disk>) -> Media {
    let mut m = d.media();
    if let Some(md) = crate::mdisk::info(d) {
        m.fwheads = md.fwheads;
        m.fwsectors = md.fwsectors;
    }
    m
}

impl Topology for Topo<'_> {
    fn table(&self, geom: &str) -> Option<Table> {
        whole(geom)?.table().map(|t| t.part)
    }

    fn store(&mut self, geom: &str, table: Option<Table>) {
        if let Some(d) = whole(geom) {
            storage::set_table(self.s, &d, table);
        }
    }

    fn provider(&self, name: &str) -> Option<Provider> {
        let d = storage::find(name)?;
        // A partition's provider answers `PART::depth`, `PART::scheme`,
        // and `PART::isleaf` with its table's.
        let part = d
            .partition()
            .and_then(|(parent, _)| parent.table())
            .map(|t| (t.part.depth, t.part.scheme, t.part.isleaf));
        Some(Provider {
            media: media(&d),
            part,
        })
    }

    fn access(&mut self, geom: &str, open: bool) -> Result<(), Errno> {
        let d = whole(geom).ok_or(Errno::ENXIO)?;
        if open {
            crate::geom_kern::table_access(&d)?;
        }
        Ok(())
    }

    fn busy(&self, geom: &str, index: u32) -> bool {
        whole(geom)
            .and_then(|d| partition(&d, index))
            .is_some_and(|p| crate::geom_kern::busy(&p))
    }

    fn media_changed(&mut self, geom: &str, index: u32) {
        if let Some(p) = whole(geom).and_then(|d| partition(&d, index)) {
            storage::relabel(self.s, &p);
        }
    }

    fn read(&mut self, geom: &str, offset: u64, len: usize) -> Result<Vec<u8>, Errno> {
        let d = whole(geom).ok_or(Errno::ENXIO)?;
        d.read(self.s, offset, len).map_err(storage::bio_errno)
    }

    fn write(&mut self, geom: &str, offset: u64, data: &[u8]) -> Result<(), Errno> {
        let d = whole(geom).ok_or(Errno::ENXIO)?;
        d.write(self.s, offset, data).map_err(storage::bio_errno)
    }

    fn uuidgen(&mut self) -> guid::Guid {
        let mut out = [[0u8; 16]];
        crate::uuid::generate(&mut out);
        let u = out[0];
        let mut node = [0u8; 8];
        node.copy_from_slice(&u[8..16]);
        // `struct uuid`'s fields, in the machine's order.
        guid::Guid::from_fields(
            u32::from_ne_bytes([u[0], u[1], u[2], u[3]]),
            u16::from_ne_bytes([u[4], u[5]]),
            u16::from_ne_bytes([u[6], u[7]]),
            node,
        )
    }

    fn debugflags(&self) -> u32 {
        crate::geom_kern::debugflags()
    }

    fn check_integrity(&self) -> bool {
        // A setting; nothing is ordered after it.
        storage::CHECK_INTEGRITY.load(core::sync::atomic::Ordering::Relaxed)
    }

    fn log(&mut self, line: &str) {
        bus::manager().log(format_args!("{line}"));
    }
}

/// Makes `/dev/geom.ctl` (`g_ctl_init`): root, group operator, 0640.
pub(crate) fn init() {
    crate::fs::devfs::add(
        abi::geom::PATH_GEOM_CTL.as_bytes(),
        0o640,
        0,
        GID_OPERATOR,
        Arc::new(GeomCtl),
    );
}
