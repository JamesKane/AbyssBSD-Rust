// SPDX-License-Identifier: BSD-2-Clause
//! The AbyssBSD UEFI loader.
//!
//! Reads the kernel from `/boot/kernel/kernel` on the first UFS2 file
//! system that has one, as FreeBSD's loader does (with the memory disk from
//! `/boot/abyss/md` there), or else `\EFI\ABYSS\KERNEL` (and
//! `\EFI\ABYSS\MD`) from the volume it was loaded from; the optional
//! command line comes from `\EFI\ABYSS\CMDLINE` there. It places the
//! kernel at a random 2 MiB-aligned slide, builds the initial page tables,
//! exits boot services, and jumps to the kernel with a `bootproto::BootInfo`.
//!
//! Built with the `ktest` feature it runs its in-firmware tests first.

#![no_std]
#![no_main]

extern crate alloc;

mod devices;
mod handoff;
mod heap;
mod kernel;
mod logring;
mod paging;
mod rng;
#[cfg(feature = "ktest")]
mod tests;
mod ufs;

use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicPtr, Ordering};

use addr::PhysAddr;
use bootproto::layout::KASLR_SLOTS;
use efi::{Handle, ResetKind, Status, SystemTable, raw};

/// The system table, for the panic handler. Cleared before boot services
/// are exited.
static SYSTEM_TABLE: AtomicPtr<raw::SystemTable> = AtomicPtr::new(core::ptr::null_mut());

const KERNEL_PATH: &str = "EFI/ABYSS/KERNEL";
const CMDLINE_PATH: &str = "EFI/ABYSS/CMDLINE";
const MD_PATH: &str = "EFI/ABYSS/MD";
const UFS_KERNEL_PATH: &[u8] = b"/boot/kernel/kernel";
const UFS_MD_PATH: &[u8] = b"/boot/abyss/md";

/// UEFI memory types the loader tags its allocations with, so the final
/// memory map tells the kernel what each region holds.
pub(crate) mod memtype {
    use efi::MemoryType;

    /// The kernel image.
    pub(crate) const KERNEL: MemoryType = MemoryType(0x8000_0001);
    /// Handoff data: `BootInfo`, the memory map, page tables, the kernel
    /// file (for its symbols), the command line.
    pub(crate) const BOOT_DATA: MemoryType = MemoryType(0x8000_0002);
    /// The persistent log ring.
    pub(crate) const LOG_RING: MemoryType = MemoryType(0x8000_0003);
}

/// Why booting failed.
#[derive(Debug)]
pub(crate) enum Error {
    Efi(&'static str, Status),
    Elf(elf::Error),
    Map(pt::MapError),
    Kernel(&'static str),
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Efi(what, status) => write!(f, "{what}: {status:?}"),
            Self::Elf(e) => write!(f, "kernel ELF: {e:?}"),
            Self::Map(e) => write!(f, "page tables: {e:?}"),
            Self::Kernel(what) => f.write_str(what),
        }
    }
}

const ARCH: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else {
    "arm64"
};

#[unsafe(no_mangle)]
extern "efiapi" fn efi_main(image: Handle, st: *mut raw::SystemTable) -> Status {
    // SAFETY: firmware passes the live system table to the entry point.
    let Some(system) = (unsafe { SystemTable::from_raw(st) }) else {
        return Status::ABORTED;
    };
    SYSTEM_TABLE.store(st, Ordering::Release);
    heap::enable(&system);

    let mut con = system.console();
    let fw = system.firmware();
    let rev = system.revision();
    let _ = writeln!(
        con,
        "\nAbyssBSD loader {} ({ARCH}), UEFI {}.{} firmware: {fw} rev {:#x}",
        env!("CARGO_PKG_VERSION"),
        rev >> 16,
        (rev & 0xFFFF) / 10,
        fw.revision
    );
    let _ = system.disable_watchdog();

    #[cfg(feature = "ktest")]
    let _ = ktest::run_tests(&mut con);

    let Err(e) = boot(image, system);
    // `boot` only returns before exiting boot services.
    // SAFETY: still the live table; `boot` consumed the previous wrapper
    // without exiting boot services.
    let system = unsafe { SystemTable::from_raw(st) }.expect("system table");
    let mut con = system.console();
    let _ = writeln!(con, "loader: boot failed: {e}");
    let _ = writeln!(con, "{}", ktest::FAIL_MARKER);
    system.stall(3_000_000);
    system.reset(ResetKind::Shutdown, Status::ABORTED)
}

/// Loads and starts the kernel. Returns only on failure, and only before
/// boot services are exited.
fn boot(image: Handle, st: SystemTable) -> Result<core::convert::Infallible, Error> {
    let mut con = st.console();
    let loaded = st
        .handle_protocol::<raw::LoadedImage>(image)
        .map_err(|s| Error::Efi("loaded image", s))?;
    // SAFETY: firmware returned our live LoadedImage protocol.
    let device = unsafe { (*loaded).device_handle };
    let root = st
        .open_volume(device)
        .map_err(|s| Error::Efi("open volume", s))?;

    let cmdline = match kernel::read_file(&st, &root, CMDLINE_PATH) {
        Ok(f) => f.bytes().trim_ascii().to_vec(),
        Err(_) => Vec::new(),
    };
    // The kernel from UFS2 if one has it, else from the ESP; the memory
    // disk (optional) from the same place.
    let mut cmdline = cmdline;
    let (file, md) = match ufs::read_file(&st, UFS_KERNEL_PATH) {
        Some((file, origin)) => {
            let _ = writeln!(con, "loader: kernel from UFS2 on {origin}");
            let md = ufs::read_file(&st, UFS_MD_PATH).map(|(f, _)| f);
            // The root is that file system's, unless the command line
            // names one.
            let named = cmdline
                .split(u8::is_ascii_whitespace)
                .any(|w| w.starts_with(b"vfs.root.mountfrom="));
            if !named {
                let root = ufs::root_mount(&st, origin);
                let _ = writeln!(con, "loader: root {root}");
                if !cmdline.is_empty() {
                    cmdline.push(b' ');
                }
                cmdline.extend_from_slice(b"vfs.root.mountfrom=");
                cmdline.extend_from_slice(root.as_bytes());
            }
            (file, md)
        }
        None => (
            kernel::read_file(&st, &root, KERNEL_PATH)?,
            kernel::read_file(&st, &root, MD_PATH).ok(),
        ),
    };
    let md = md.and_then(|f| bootproto::PhysRange::from_len(f.phys, f.len as u64));
    if let Some(md) = &md {
        let _ = writeln!(
            con,
            "loader: memory disk {} KiB at {:#x}",
            (md.end.get() - md.start.get()) / 1024,
            md.start
        );
    }
    let log = logring::setup(&st, &root);
    match &log {
        Some((ring, outcome)) => {
            let _ = writeln!(
                con,
                "loader: log ring at {:#x}, boot {}: {outcome}",
                ring.region.start, ring.boots
            );
        }
        None => {
            let _ = writeln!(con, "loader: no log ring");
        }
    }
    drop(root);

    let entropy = rng::gather(&st);
    let kaslr = !cmdline
        .split(|b| b.is_ascii_whitespace())
        .any(|w| w == b"kaslr=0");
    let slide = if kaslr {
        let r = u64::from_le_bytes(entropy.bytes[..8].try_into().unwrap_or_default());
        (r % KASLR_SLOTS) * bootproto::layout::KASLR_ALIGN
    } else {
        0
    };
    let kernel = kernel::load(&st, &file, slide)?;
    let _ = writeln!(
        con,
        "loader: kernel {} KiB at phys {:#x} virt {:#x} (slide {:#x}, entropy {:?})",
        kernel.image.size / 1024,
        kernel.image.phys,
        kernel.image.virt,
        slide,
        entropy.source
    );

    let rsdp = st
        .configuration_table(efi::tables::ACPI_20)
        .ok_or(Error::Kernel("no ACPI 2.0+ RSDP: AbyssBSD requires ACPI"))?;
    let smbios3 = st.configuration_table(efi::tables::SMBIOS3);
    let serial = match devices::serial(rsdp as u64) {
        Ok(s) => Some(s),
        Err(e) => {
            let _ = writeln!(con, "loader: no serial console ({e})");
            None
        }
    };
    let fb = devices::framebuffer(&st);
    let _ = writeln!(con, "loader: serial {serial:?}");
    if let Some(fb) = &fb {
        let _ = writeln!(
            con,
            "loader: framebuffer {}x{} at {:#x}",
            fb.width, fb.height, fb.phys
        );
    }

    let ram = paging::ram_ranges(&st)?;
    let tables = paging::build(&st, &ram, &kernel, serial, fb)?;

    // Room for the final map: everything now, plus slack for the
    // allocations still to come and firmware splitting regions.
    let map_bytes = st
        .memory_map_size()
        .map_err(|s| Error::Efi("memory map size", s))?
        + 16 * 1024;
    let handoff = handoff::prepare(&st, map_bytes / 40 + 64, &cmdline)?;
    let mut map_buf = alloc::vec![0u8; map_bytes];

    let firmware = bootproto::Firmware {
        rsdp: Some(PhysAddr::new(rsdp as u64)).into(),
        smbios3: smbios3.map(|p| PhysAddr::new(p as u64)).into(),
        uefi_system_table: PhysAddr::new(st.as_ptr() as u64),
        uefi_runtime_services: PhysAddr::new(st.runtime_services() as u64),
        uefi_revision: st.revision(),
        reserved: 0,
    };
    let _ = writeln!(con, "loader: exiting boot services");

    SYSTEM_TABLE.store(core::ptr::null_mut(), Ordering::Release);
    heap::disable();
    let Ok((_runtime, map)) = st.exit_boot_services(image, &mut map_buf) else {
        // Boot services may be half gone; nothing safe to do but stop.
        halt();
    };

    let info = handoff.finish(
        &map,
        handoff::Fields {
            kernel: kernel.image,
            serial: tables.serial,
            framebuffer: tables.framebuffer,
            firmware,
            symbols: kernel.symbols,
            entropy,
            log: log.map(|(ring, _)| ring),
            md,
        },
    );
    // SAFETY: the entry point follows the `bootproto` calling contract, the
    // image is loaded and relocated, and the tables map it.
    unsafe { jump(kernel.entry, info, tables.kernel_root, tables.identity_root) }
}

#[cfg(target_arch = "x86_64")]
type Entry = extern "sysv64" fn(u64, u64, u64) -> !;
#[cfg(target_arch = "aarch64")]
type Entry = extern "C" fn(u64, u64, u64) -> !;

/// Calls the kernel's physical entry point.
///
/// # Safety
///
/// `entry` must be the entry of a loaded kernel that honours the
/// `bootproto` contract.
unsafe fn jump(entry: PhysAddr, info: PhysAddr, kernel_root: PhysAddr, identity: PhysAddr) -> ! {
    // SAFETY: per the caller's contract `entry` is code with this
    // signature; the firmware identity map makes its physical address
    // callable.
    let f: Entry = unsafe { core::mem::transmute(entry.get() as *const ()) };
    f(info.get(), kernel_root.get(), identity.get())
}

fn halt() -> ! {
    loop {
        core::hint::spin_loop();
    }
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    let st = SYSTEM_TABLE.load(Ordering::Acquire);
    // SAFETY: the pointer is null or the live system table; it is cleared
    // before boot services are exited.
    if let Some(system) = unsafe { SystemTable::from_raw(st) } {
        ktest::report_panic(&mut system.console(), info);
        system.reset(ResetKind::Shutdown, Status::ABORTED);
    }
    halt()
}
