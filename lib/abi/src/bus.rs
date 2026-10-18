// SPDX-License-Identifier: BSD-2-Clause
//! The device tree as `devinfo(8)` and `devctl(8)` read it: `hw.bus.info`
//! (`struct u_businfo`) and `hw.bus.devices.<n>` (`struct u_device`), with
//! the device states and flags they report.

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
