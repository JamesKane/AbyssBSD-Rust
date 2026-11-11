// SPDX-License-Identifier: BSD-2-Clause
//! The device tree as `devinfo(8)` and `devctl(8)` read it: `hw.bus.info`
//! (`struct u_businfo`) and `hw.bus.devices.<n>` (`struct u_device`), with
//! the device states and flags they report, the resource managers of
//! `hw.bus.rman.<n>` (`struct u_rman`, `struct u_resource`), and
//! `/dev/devctl2`'s requests (`struct devreq`, `DEV_*`).

use crate::ioctl::{IOC_IN, IOC_OUT};

/// `struct u_businfo`, the value of `hw.bus.info`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UBusinfo {
    /// The interface version, [`BUS_USER_VERSION`].
    pub ub_version: i32,
    /// Bumped on every change to the tree.
    pub ub_generation: i32,
}

/// `BUS_USER_VERSION`.
pub const BUS_USER_VERSION: i32 = 2;
/// `BUS_USER_BUFFER`: room for a device's NUL-separated text fields.
pub const BUS_USER_BUFFER: usize = 3 * 1024;

/// `struct u_device`, the value of `hw.bus.devices.<n>`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct UDevice {
    /// An opaque handle identifying the device.
    pub dv_handle: usize,
    /// The parent's handle (0 for the root).
    pub dv_parent: usize,
    /// Driver-defined flags (`device_get_flags`).
    pub dv_devflags: u32,
    /// `DF_*` state flags.
    pub dv_flags: u16,
    /// `DS_*` (`device_state_t`).
    pub dv_state: i32,
    /// `name\0desc\0driver\0pnpinfo\0location\0`, then zeros.
    pub dv_fields: [u8; BUS_USER_BUFFER],
}

/// `device_state_t` values.
pub mod ds {
    /// Not probed, or no driver.
    pub const NOTPRESENT: i32 = 10;
    /// Probed, not attached.
    pub const ALIVE: i32 = 20;
    /// Attaching.
    pub const ATTACHING: i32 = 25;
    /// Attached.
    pub const ATTACHED: i32 = 30;
}

/// `DF_*` device flags.
pub mod df {
    /// Should be probed and attached.
    pub const ENABLED: u16 = 0x01;
    /// The devclass was fixed at creation.
    pub const FIXEDCLASS: u16 = 0x02;
    /// The unit was originally a wildcard.
    pub const WILDCARD: u16 = 0x04;
    /// The description was allocated.
    pub const DESCMALLOCED: u16 = 0x08;
    /// No verbose attach message.
    pub const QUIET: u16 = 0x10;
    /// The nomatch event has been sent.
    pub const DONENOMATCH: u16 = 0x20;
    /// The softc is not the framework's.
    pub const EXTERNALSOFTC: u16 = 0x40;
    /// Suspended.
    pub const SUSPENDED: u16 = 0x100;
    /// Children default to quiet.
    pub const QUIET_CHILDREN: u16 = 0x200;
    /// Attached at least once.
    pub const ATTACHED_ONCE: u16 = 0x400;
    /// A nomatch event is pending.
    pub const NEEDNOMATCH: u16 = 0x800;
}

/// `RM_TEXTLEN`: an rman's description and a resource's device name.
pub const RM_TEXTLEN: usize = 32;

/// `struct u_rman`, `hw.bus.rman.<generation>.<rman>.-1`: a resource
/// manager (sys/rman.h).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct URman {
    /// An opaque handle identifying the rman.
    pub rm_handle: usize,
    /// Its description, NUL-terminated (`I/O memory addresses`).
    pub rm_descr: [u8; RM_TEXTLEN],
    /// The base of the managed region (`rman_res_t`).
    pub rm_start: u64,
    /// Its size: `rm_end - rm_start + 1`, which is 0 for the whole
    /// 64-bit space.
    pub rm_size: u64,
    /// `RMAN_*` (`enum rman_type`).
    pub rm_type: i32,
}

/// `struct u_resource`, `hw.bus.rman.<generation>.<rman>.<n>`: one
/// resource of an rman, allocated or free.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UResource {
    /// An opaque handle identifying the resource.
    pub r_handle: usize,
    /// Its rman's handle.
    pub r_parent: usize,
    /// The owning device's `dv_handle`, 0 for a free span.
    pub r_device: usize,
    /// The owner's name and unit (`nomatch` without a name), empty for a
    /// free span.
    pub r_devname: [u8; RM_TEXTLEN],
    /// The first address.
    pub r_start: u64,
    /// The size: `r_end - r_start + 1`.
    pub r_size: u64,
    /// `RF_*`.
    pub r_flags: u32,
}

/// `enum rman_type`.
pub mod rman_type {
    /// Not set up.
    pub const UNINIT: i32 = 0;
    /// A gauge (unused by FreeBSD's buses).
    pub const GAUGE: i32 = 1;
    /// An array of addresses: what every bus's rman is.
    pub const ARRAY: i32 = 2;
}

/// `RF_*` resource flags.
pub mod rf {
    /// Reserved.
    pub const ALLOCATED: u32 = 0x0001;
    /// Activated (mapped, or its interrupt set up).
    pub const ACTIVE: u32 = 0x0002;
    /// May be shared.
    pub const SHAREABLE: u32 = 0x0004;
    /// The first of a sharing list.
    pub const FIRSTSHARE: u32 = 0x0020;
    /// Prefetchable memory.
    pub const PREFETCHABLE: u32 = 0x0040;
}

/// `struct devreq_buffer`: a caller's buffer and its length.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DevreqBuffer {
    /// The buffer (a user address).
    pub buffer: usize,
    /// Its length; `DEV_GET_PATH` sets it to the length needed.
    pub length: usize,
}

/// `struct devreq`, the argument of every `/dev/devctl2` request. The
/// union `dr_dru` is its buffer form here; its other member, `dr_data`
/// (a pointer: `DEV_SET_DRIVER`'s driver name), is `dr_buffer.buffer`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Devreq {
    /// The device: a name (`vtnet0`) or a bus's locator (`pci0:0:3:0`).
    pub dr_name: [u8; 128],
    /// The request's `DEVF_*` flags.
    pub dr_flags: i32,
    /// `dr_dru.dru_buffer` (`dr_buffer`), or `dr_dru.dru_data`.
    pub dr_buffer: DevreqBuffer,
}

/// `sizeof(struct devreq)`, which the requests encode.
pub const DEVREQ_LEN: u32 = 152;
const _: () = assert!(size_of::<Devreq>() == DEVREQ_LEN as usize);
const fn dev_iow(num: u32) -> u32 {
    IOC_IN | (DEVREQ_LEN << 16) | (b'D' as u32) << 8 | num
}

pub const DEV_ATTACH: u32 = dev_iow(1);
pub const DEV_DETACH: u32 = dev_iow(2);
pub const DEV_ENABLE: u32 = dev_iow(3);
pub const DEV_DISABLE: u32 = dev_iow(4);
pub const DEV_SUSPEND: u32 = dev_iow(5);
pub const DEV_RESUME: u32 = dev_iow(6);
pub const DEV_SET_DRIVER: u32 = dev_iow(7);
pub const DEV_CLEAR_DRIVER: u32 = dev_iow(8);
pub const DEV_RESCAN: u32 = dev_iow(9);
pub const DEV_DELETE: u32 = dev_iow(10);
pub const DEV_FREEZE: u32 = dev_iow(11);
pub const DEV_THAW: u32 = dev_iow(12);
pub const DEV_RESET: u32 = dev_iow(13);
pub const DEV_GET_PATH: u32 = dev_iow(14) | IOC_OUT;

/// `DEV_DETACH` and `DEV_DISABLE`: don't ask the driver first.
pub const DEVF_FORCE_DETACH: i32 = 0x1;
/// `DEV_SET_DRIVER`: detach the driver attached now.
pub const DEVF_SET_DRIVER_DETACH: i32 = 0x1;
/// `DEV_CLEAR_DRIVER`: detach the driver attached now.
pub const DEVF_CLEAR_DRIVER_DETACH: i32 = 0x1;
/// `DEV_DELETE`: delete a device its bus says is present.
pub const DEVF_FORCE_DELETE: i32 = 0x1;
/// `DEV_RESET`: detach the drivers rather than suspend the device.
pub const DEVF_RESET_DETACH: i32 = 0x1;

/// `DEV_GET_PATH`'s locators (`BUS_LOCATOR_*`).
pub const BUS_LOCATOR_ACPI: &str = "ACPI";
pub const BUS_LOCATOR_FREEBSD: &str = "FreeBSD";
pub const BUS_LOCATOR_UEFI: &str = "UEFI";
