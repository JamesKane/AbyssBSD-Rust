// SPDX-License-Identifier: BSD-2-Clause
//! Device statistics' kernel side: the pages `sys/storage` keeps its
//! entries in, the `kern.devstat` sysctls (`iostat`, `systat`), and
//! `/dev/devstat`, which maps the pages (`gstat`, `libgeom`'s
//! `geom_stats_open`).
//!
//! Behavior reference: sys/kern/subr_devstat.c.

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::sync::atomic::AtomicU64;

use abi::errno::Errno;
use sched::Sleepable;
use storage::devstat;
use sysctl::{Number, Value};
use vfs::{CharDevice, Uio};

/// The pages: 16, so 224 entries (a disk takes two, a partition one).
const ORDER: u8 = 4;

/// Gives `sys/storage` its pages and clock, and makes the sysctls. Before
/// the first disk is published.
pub(crate) fn init(s: &Sleepable) {
    let Some(order) = vm::Order::new(ORDER) else {
        return;
    };
    let pages = 1usize << ORDER;
    match crate::mm::alloc_pages(order) {
        Some(pa) => {
            let Some(va) = bootproto::layout::dmap(pa) else {
                return;
            };
            let words = pages * devstat::PAGE / 8;
            // SAFETY: the block is freshly allocated, `pages` pages of RAM
            // in the DMAP, kept for the life of the kernel (never freed),
            // and every access to it is atomic. Zeroed first: an
            // all-zero `AtomicU64` is 0.
            let words: &'static [AtomicU64] = unsafe {
                core::ptr::write_bytes(va.get() as *mut u8, 0, pages * devstat::PAGE);
                core::slice::from_raw_parts(va.get() as *const AtomicU64, words)
            };
            devstat::init(
                devstat::Region {
                    words,
                    pfn: pa.get() / 4096,
                },
                crate::time::now_ns,
                crate::geom_kern::provider_id,
            );
        }
        None => kprintln!("devstat: no pages for the statistics"),
    }
    crate::sysctl::with(s, |t| {
        let root = t.root();
        let Some(kern) = t.child(root, "kern") else {
            return;
        };
        let Ok(node) = t.add_node(kern, Number::Auto, "devstat", "Device Statistics") else {
            return;
        };
        let _ = t.add_value(
            node,
            Number::Auto,
            "all",
            "All devices in the devstat list",
            Value::Opaque {
                format: "S,devstat",
                read: Box::new(|out| out.extend(devstat::all())),
            },
        );
        let _ = t.add_value(
            node,
            Number::Auto,
            "numdevs",
            "Number of devices in the devstat list",
            Value::Int(Box::new(|| {
                i32::try_from(devstat::num_devs()).unwrap_or(i32::MAX)
            })),
        );
        let _ = t.add_value(
            node,
            Number::Auto,
            "generation",
            "Devstat list generation",
            Value::Long(Box::new(devstat::generation)),
        );
        let _ = t.add_value(
            node,
            Number::Auto,
            "version",
            "Devstat list version number",
            Value::int(abi::devstat::VERSION),
        );
    });
}

/// `/dev/devstat`.
struct DevstatDev;

impl CharDevice for DevstatDev {
    /// Its pages are the kernel's own: mapped only for reading
    /// (`devstat_mmap` refuses any other protection; here an open for
    /// writing is refused instead).
    fn instance_open(
        &self,
        _s: &Sleepable,
        _nonblock: bool,
        _read: bool,
        write: bool,
    ) -> Result<Option<Arc<dyn CharDevice>>, Errno> {
        if write {
            return Err(Errno::EPERM);
        }
        Ok(None)
    }

    fn read(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }

    fn write(&self, _: &Sleepable, _: &mut dyn Uio, _: u64, _: bool) -> Result<usize, Errno> {
        Err(Errno::ENODEV)
    }

    /// `DIOCGMEDIASIZE`: the bytes of the pages holding entries.
    fn ioctl(&self, _s: &Sleepable, req: u32, arg: &mut [u8]) -> Result<(), Errno> {
        if req != abi::ioctl::DIOCGMEDIASIZE {
            return Err(Errno::ENOTTY);
        }
        let v = devstat::mapped_bytes().to_ne_bytes();
        arg.get_mut(..8).ok_or(Errno::EINVAL)?.copy_from_slice(&v);
        Ok(())
    }

    /// `devstat_mmap`: the pages holding entries, in order.
    fn mmap(&self, page: u64, pages: u64) -> Result<Arc<vm::object::Object>, Errno> {
        let (pfn, used) = devstat::pages().ok_or(Errno::EINVAL)?;
        let end = page.checked_add(pages).ok_or(Errno::EINVAL)?;
        if end > u64::try_from(used).unwrap_or(0) {
            return Err(Errno::EINVAL);
        }
        Ok(vm::object::Object::device(
            crate::mm::space::page_ops(),
            vm::Pfn(pfn),
            end,
            vm::object::MemAttr::WriteBack,
        ))
    }
}

#[cfg(feature = "ktest")]
mod ktests {
    use ktest::ktest;

    use crate::devctl::ktests::cx;

    fn int(name: &str) -> i32 {
        crate::sysctl::read(&cx(), |t| {
            let oid = t.name2oid(name).unwrap_or_else(|| panic!("{name}"));
            let mut v = alloc::vec::Vec::new();
            t.read(&oid, &mut v).expect("read");
            i32::from_ne_bytes(v[..4].try_into().expect("an int"))
        })
    }

    ktest! {
        /// `kern.devstat`: every disk in the list, as `kern.devstat.all`
        /// gives it; and `hw.acpi.battery` with no batteries.
        fn devstat_lists_the_disks() {
            assert_eq!(int("kern.devstat.version"), 6);
            let n = int("kern.devstat.numdevs");
            assert!(n >= 1, "{n} devices");
            let all = storage::devstat::all();
            assert_eq!(all.len(), 8 + usize::try_from(n).unwrap() * abi::devstat::SIZE);
            assert!(storage::devstat::mapped_bytes() >= 4096);
            assert_eq!(int("hw.acpi.battery.units"), 0);
            assert_eq!(int("hw.acpi.battery.info_expire"), 5);
        }
    }
}

/// Makes `/dev/devstat`, `root:wheel` 0444.
pub(crate) fn make_dev() {
    crate::fs::devfs::add(
        abi::devstat::DEVICE_NAME.as_bytes(),
        0o444,
        0,
        0,
        Arc::new(DevstatDev),
    );
}
