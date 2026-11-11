// SPDX-License-Identifier: BSD-2-Clause
//! `/dev/pci` (`sys/pciio.h`): the requests `pciconf(8)` and libpciaccess
//! make, and the structures they pass.
//!
//! The padding C leaves between members is spelled out as `_pad` fields,
//! so every structure here is plain bytes the kernel can copy whole.
//! Pointers are user addresses, kept as `usize`.
//!
//! Behavior reference: sys/sys/pciio.h, sys/dev/pci/pci_user.c.

use crate::ioctl::{IOC_IN, IOC_OUT};
use crate::layout::{Arches, ConstValue};

/// `_PATH_DEVPCI`, under `/dev`.
pub const PATH_DEVPCI: &[u8] = b"pci";

/// `PCI_MAXNAMELEN`: a driver name's longest, without its NUL.
pub const PCI_MAXNAMELEN: usize = 16;

/// `pci_getconf_status`: how a `PCIOCGETCONF` ended.
pub mod status {
    /// `PCI_GETCONF_LAST_DEVICE`: the list's end was reached.
    pub const LAST_DEVICE: u32 = 0;
    /// `PCI_GETCONF_LIST_CHANGED`: the list changed since `generation`;
    /// start again from offset 0.
    pub const LIST_CHANGED: u32 = 1;
    /// `PCI_GETCONF_MORE_DEVS`: the buffer filled; ask again from `offset`.
    pub const MORE_DEVS: u32 = 2;
    /// `PCI_GETCONF_ERROR`: the patterns were malformed.
    pub const ERROR: u32 = 3;
}

/// `pci_getconf_flags`: which fields of a [`PciMatchConf`] must match.
pub mod flags {
    pub const NO_MATCH: u32 = 0x0000;
    pub const MATCH_DOMAIN: u32 = 0x0001;
    pub const MATCH_BUS: u32 = 0x0002;
    pub const MATCH_DEV: u32 = 0x0004;
    pub const MATCH_FUNC: u32 = 0x0008;
    pub const MATCH_NAME: u32 = 0x0010;
    pub const MATCH_UNIT: u32 = 0x0020;
    pub const MATCH_VENDOR: u32 = 0x0040;
    pub const MATCH_DEVICE: u32 = 0x0080;
    pub const MATCH_CLASS: u32 = 0x0100;
}

/// `struct pcisel`: a function's domain, bus, slot, and function.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PciSel {
    pub pc_domain: u32,
    pub pc_bus: u8,
    pub pc_dev: u8,
    pub pc_func: u8,
    pub _pad0: u8,
}

/// `struct pci_conf`: one function, as `PCIOCGETCONF` reports it.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciConf {
    pub pc_sel: PciSel,
    /// The header type, without the multi-function bit.
    pub pc_hdr: u8,
    pub _pad0: u8,
    pub pc_subvendor: u16,
    pub pc_subdevice: u16,
    pub pc_vendor: u16,
    pub pc_device: u16,
    pub pc_class: u8,
    pub pc_subclass: u8,
    pub pc_progif: u8,
    pub pc_revid: u8,
    /// The attached driver's name (its devclass), NUL-terminated; empty
    /// for none.
    pub pd_name: [u8; PCI_MAXNAMELEN + 1],
    pub _pad1: [u8; 1],
    /// Its unit (`u_long`).
    pub pd_unit: u64,
    pub pd_numa_domain: i32,
    pub _pad2: [u8; 4],
    /// How much of the structure the kernel filled: up to `pc_spare`.
    pub pc_reported_len: usize,
    pub pc_secbus: u8,
    pub pc_subbus: u8,
    pub pc_spare: [u8; 62],
}

impl Default for PciConf {
    fn default() -> Self {
        Self {
            pc_sel: PciSel::default(),
            pc_hdr: 0,
            _pad0: 0,
            pc_subvendor: 0,
            pc_subdevice: 0,
            pc_vendor: 0,
            pc_device: 0,
            pc_class: 0,
            pc_subclass: 0,
            pc_progif: 0,
            pc_revid: 0,
            pd_name: [0; PCI_MAXNAMELEN + 1],
            _pad1: [0; 1],
            pd_unit: 0,
            pd_numa_domain: 0,
            _pad2: [0; 4],
            pc_reported_len: 0,
            pc_secbus: 0,
            pc_subbus: 0,
            pc_spare: [0; 62],
        }
    }
}

/// `offsetof(struct pci_conf, pc_spare)`, the length FreeBSD reports.
pub const PCI_CONF_REPORTED_LEN: usize = core::mem::offset_of!(PciConf, pc_spare);

/// `struct pci_match_conf`: one pattern of a `PCIOCGETCONF`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciMatchConf {
    pub pc_sel: PciSel,
    pub pd_name: [u8; PCI_MAXNAMELEN + 1],
    pub _pad0: [u8; 7],
    pub pd_unit: u64,
    pub pc_vendor: u16,
    pub pc_device: u16,
    pub pc_class: u8,
    pub _pad1: [u8; 3],
    /// `pci_getconf_flags` ([`flags`]).
    pub flags: u32,
    pub _pad2: [u8; 4],
}

impl Default for PciMatchConf {
    fn default() -> Self {
        Self {
            pc_sel: PciSel::default(),
            pd_name: [0; PCI_MAXNAMELEN + 1],
            _pad0: [0; 7],
            pd_unit: 0,
            pc_vendor: 0,
            pc_device: 0,
            pc_class: 0,
            _pad1: [0; 3],
            flags: 0,
            _pad2: [0; 4],
        }
    }
}

/// `struct pci_conf_io`, `PCIOCGETCONF`'s argument.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PciConfIo {
    /// Bytes at `patterns`.
    pub pat_buf_len: u32,
    pub num_patterns: u32,
    /// `struct pci_match_conf *`.
    pub patterns: usize,
    /// Bytes at `matches`.
    pub match_buf_len: u32,
    /// Set: how many were written.
    pub num_matches: u32,
    /// `struct pci_conf *`.
    pub matches: usize,
    /// Where in the list to start; set to where it stopped.
    pub offset: u32,
    /// The list's generation, for a call with an offset.
    pub generation: u32,
    /// `pci_getconf_status` ([`status`]).
    pub status: u32,
    pub _pad0: [u8; 4],
}

/// `struct pci_io`, `PCIOCREAD`'s, `PCIOCWRITE`'s, and `PCIOCATTACHED`'s.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PciIo {
    pub pi_sel: PciSel,
    /// The register.
    pub pi_reg: i32,
    /// 1, 2, or 4 bytes.
    pub pi_width: i32,
    pub pi_data: u32,
}

/// `struct pci_bar_io`, `PCIOCGETBAR`'s.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PciBarIo {
    pub pbi_sel: PciSel,
    /// The BAR's register.
    pub pbi_reg: i32,
    pub pbi_enabled: i32,
    /// The BAR's value, with its type bits.
    pub pbi_base: u64,
    pub pbi_length: u64,
}

/// `struct pci_vpd_element`: a keyword's header, its data following.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PciVpdElement {
    pub pve_keyword: [u8; 2],
    pub pve_flags: u8,
    pub pve_datalen: u8,
}

/// `PVE_FLAG_IDENT`: the identifier string.
pub const PVE_FLAG_IDENT: u8 = 0x01;
/// `PVE_FLAG_RW`: a read-write keyword.
pub const PVE_FLAG_RW: u8 = 0x02;

/// `struct pci_list_vpd_io`, `PCIOCLISTVPD`'s.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PciListVpdIo {
    pub plvi_sel: PciSel,
    /// The data area's size; set to the size needed.
    pub plvi_len: usize,
    /// `struct pci_vpd_element *`.
    pub plvi_data: usize,
}

/// `struct pci_bar_mmap`, `PCIOCBARMMAP`'s.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PciBarMmap {
    /// In with `PCIIO_BAR_MMAP_FIXED`; out, where it was mapped.
    pub pbm_map_base: usize,
    /// Out: the mapping's length, whole pages.
    pub pbm_map_length: usize,
    /// Out: the BAR's.
    pub pbm_bar_length: u64,
    /// Out: where in the mapping the BAR starts.
    pub pbm_bar_off: i32,
    pub pbm_sel: PciSel,
    pub pbm_reg: i32,
    /// `PCIIO_BAR_MMAP_*`.
    pub pbm_flags: i32,
    /// A `vm_memattr_t`.
    pub pbm_memattr: i32,
}

/// `struct pci_bar_ioreq`, `PCIOCBARIO`'s.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PciBarIoreq {
    pub pbi_sel: PciSel,
    /// `PCIBARIO_READ` or `PCIBARIO_WRITE`.
    pub pbi_op: i32,
    /// The BAR's number.
    pub pbi_bar: u32,
    pub pbi_offset: u32,
    pub pbi_width: u32,
    pub pbi_value: u32,
}

pub const PCIBARIO_READ: i32 = 0x1;
pub const PCIBARIO_WRITE: i32 = 0x2;

pub const PCIIO_BAR_MMAP_FIXED: i32 = 0x01;
pub const PCIIO_BAR_MMAP_EXCL: i32 = 0x02;
pub const PCIIO_BAR_MMAP_RW: i32 = 0x04;
pub const PCIIO_BAR_MMAP_ACTIVATE: i32 = 0x08;

#[allow(
    clippy::cast_possible_truncation,
    reason = "every argument is under IOCPARM_MAX"
)]
const fn iowr(num: u32, len: usize) -> u32 {
    IOC_IN | IOC_OUT | ((len as u32) << 16) | (b'p' as u32) << 8 | num
}

pub const PCIOCREAD: u32 = iowr(2, size_of::<PciIo>());
pub const PCIOCWRITE: u32 = iowr(3, size_of::<PciIo>());
pub const PCIOCATTACHED: u32 = iowr(4, size_of::<PciIo>());
pub const PCIOCGETBAR: u32 = iowr(6, size_of::<PciBarIo>());
pub const PCIOCLISTVPD: u32 = iowr(7, size_of::<PciListVpdIo>());
pub const PCIOCBARMMAP: u32 = iowr(8, size_of::<PciBarMmap>());
pub const PCIOCBARIO: u32 = iowr(9, size_of::<PciBarIoreq>());
pub const PCIOCGETCONF: u32 = iowr(10, size_of::<PciConfIo>());

/// The `vm_memattr_t` values `PCIOCBARMMAP` takes (`machine/vm.h`), for
/// each architecture; [`memattr`] is the one built for.
pub mod memattr_amd64 {
    pub const UNCACHEABLE: i32 = 0;
    pub const WRITE_COMBINING: i32 = 1;
    pub const WRITE_THROUGH: i32 = 4;
    pub const WRITE_PROTECTED: i32 = 5;
    pub const WRITE_BACK: i32 = 6;
    /// `VM_MEMATTR_WEAK_UNCACHEABLE` (`PAT_UNCACHED`, UC-).
    pub const WEAK_UNCACHEABLE: i32 = 7;
    pub const DEVICE: i32 = UNCACHEABLE;
}

/// arm64's `vm_memattr_t` values.
pub mod memattr_arm64 {
    pub const DEVICE_NGNRNE: i32 = 0;
    pub const UNCACHEABLE: i32 = 1;
    pub const WRITE_BACK: i32 = 2;
    pub const WRITE_THROUGH: i32 = 3;
    pub const DEVICE_NGNRE: i32 = 4;
    pub const TAGGED: i32 = 5;
    pub const DEVICE: i32 = DEVICE_NGNRE;
    pub const WRITE_COMBINING: i32 = WRITE_THROUGH;
}

#[cfg(target_arch = "x86_64")]
pub use memattr_amd64 as memattr;
#[cfg(not(target_arch = "x86_64"))]
pub use memattr_arm64 as memattr;

const fn c(c_name: &'static str, value: i64) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/pciio.h",
        value,
        arches: Arches::All,
    }
}

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("PCI_MAXNAMELEN", PCI_MAXNAMELEN as i64),
    c("PCI_GETCONF_LAST_DEVICE", status::LAST_DEVICE as i64),
    c("PCI_GETCONF_LIST_CHANGED", status::LIST_CHANGED as i64),
    c("PCI_GETCONF_MORE_DEVS", status::MORE_DEVS as i64),
    c("PCI_GETCONF_ERROR", status::ERROR as i64),
    c("PCI_GETCONF_NO_MATCH", flags::NO_MATCH as i64),
    c("PCI_GETCONF_MATCH_DOMAIN", flags::MATCH_DOMAIN as i64),
    c("PCI_GETCONF_MATCH_BUS", flags::MATCH_BUS as i64),
    c("PCI_GETCONF_MATCH_DEV", flags::MATCH_DEV as i64),
    c("PCI_GETCONF_MATCH_FUNC", flags::MATCH_FUNC as i64),
    c("PCI_GETCONF_MATCH_NAME", flags::MATCH_NAME as i64),
    c("PCI_GETCONF_MATCH_UNIT", flags::MATCH_UNIT as i64),
    c("PCI_GETCONF_MATCH_VENDOR", flags::MATCH_VENDOR as i64),
    c("PCI_GETCONF_MATCH_DEVICE", flags::MATCH_DEVICE as i64),
    c("PCI_GETCONF_MATCH_CLASS", flags::MATCH_CLASS as i64),
    c("PVE_FLAG_IDENT", PVE_FLAG_IDENT as i64),
    c("PVE_FLAG_RW", PVE_FLAG_RW as i64),
    c("PCIBARIO_READ", PCIBARIO_READ as i64),
    c("PCIBARIO_WRITE", PCIBARIO_WRITE as i64),
    c("PCIIO_BAR_MMAP_FIXED", PCIIO_BAR_MMAP_FIXED as i64),
    c("PCIIO_BAR_MMAP_EXCL", PCIIO_BAR_MMAP_EXCL as i64),
    c("PCIIO_BAR_MMAP_RW", PCIIO_BAR_MMAP_RW as i64),
    c("PCIIO_BAR_MMAP_ACTIVATE", PCIIO_BAR_MMAP_ACTIVATE as i64),
    c("PCIOCREAD", PCIOCREAD as i64),
    c("PCIOCWRITE", PCIOCWRITE as i64),
    c("PCIOCATTACHED", PCIOCATTACHED as i64),
    c("PCIOCGETBAR", PCIOCGETBAR as i64),
    c("PCIOCLISTVPD", PCIOCLISTVPD as i64),
    c("PCIOCBARMMAP", PCIOCBARMMAP as i64),
    c("PCIOCBARIO", PCIOCBARIO as i64),
    c("PCIOCGETCONF", PCIOCGETCONF as i64),
    vm(
        "VM_MEMATTR_UNCACHEABLE",
        memattr_amd64::UNCACHEABLE,
        Arches::Amd64,
    ),
    vm(
        "VM_MEMATTR_WRITE_COMBINING",
        memattr_amd64::WRITE_COMBINING,
        Arches::Amd64,
    ),
    vm(
        "VM_MEMATTR_WRITE_THROUGH",
        memattr_amd64::WRITE_THROUGH,
        Arches::Amd64,
    ),
    vm(
        "VM_MEMATTR_WRITE_PROTECTED",
        memattr_amd64::WRITE_PROTECTED,
        Arches::Amd64,
    ),
    vm(
        "VM_MEMATTR_WRITE_BACK",
        memattr_amd64::WRITE_BACK,
        Arches::Amd64,
    ),
    vm(
        "VM_MEMATTR_WEAK_UNCACHEABLE",
        memattr_amd64::WEAK_UNCACHEABLE,
        Arches::Amd64,
    ),
    vm("VM_MEMATTR_DEVICE", memattr_amd64::DEVICE, Arches::Amd64),
    vm(
        "VM_MEMATTR_DEVICE_nGnRnE",
        memattr_arm64::DEVICE_NGNRNE,
        Arches::Arm64,
    ),
    vm(
        "VM_MEMATTR_UNCACHEABLE",
        memattr_arm64::UNCACHEABLE,
        Arches::Arm64,
    ),
    vm(
        "VM_MEMATTR_WRITE_BACK",
        memattr_arm64::WRITE_BACK,
        Arches::Arm64,
    ),
    vm(
        "VM_MEMATTR_WRITE_THROUGH",
        memattr_arm64::WRITE_THROUGH,
        Arches::Arm64,
    ),
    vm(
        "VM_MEMATTR_DEVICE_nGnRE",
        memattr_arm64::DEVICE_NGNRE,
        Arches::Arm64,
    ),
    vm("VM_MEMATTR_TAGGED", memattr_arm64::TAGGED, Arches::Arm64),
    vm("VM_MEMATTR_DEVICE", memattr_arm64::DEVICE, Arches::Arm64),
    vm(
        "VM_MEMATTR_WRITE_COMBINING",
        memattr_arm64::WRITE_COMBINING,
        Arches::Arm64,
    ),
];

const fn vm(c_name: &'static str, value: i32, arches: Arches) -> ConstValue {
    ConstValue {
        c_name,
        header: "vm/vm.h",
        value: value as i64,
        arches,
    }
}

const _: () = {
    assert!(size_of::<PciSel>() == 8);
    assert!(size_of::<PciConf>() == 128);
    assert!(size_of::<PciMatchConf>() == 56);
    assert!(size_of::<PciConfIo>() == 48);
    assert!(size_of::<PciIo>() == 20);
    assert!(size_of::<PciBarIo>() == 32);
    assert!(size_of::<PciListVpdIo>() == 24);
    assert!(size_of::<PciBarMmap>() == 48);
    assert!(size_of::<PciBarIoreq>() == 28);
};
