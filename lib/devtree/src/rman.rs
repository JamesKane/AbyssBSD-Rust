// SPDX-License-Identifier: BSD-2-Clause
//! FreeBSD's resource managers (`hw.bus.rman`) as `devinfo -u` and
//! `devinfo -r` read them, built from the device tree and what each node
//! holds (DRIVER-MODEL.md §8.1, §11.1).
//!
//! - An [`Rman`] is a resource space a bus keeps: the platform root's
//!   (`I/O memory addresses`, `Interrupts`), `acpi0`'s system-resource
//!   ranges, a host bridge's or PCI-PCI bridge's windows and bus numbers,
//!   a controller's own sub-ranges (AHCI channels).
//! - A [`Request`] is one range a node holds: a bus-described resource, a
//!   bridge's window, an installed interrupt. A request goes to the rman
//!   its bus names, else to the first rman of its kind that an ancestor
//!   owns and whose managed regions hold it (a function behind a
//!   PCI-PCI bridge lands in the bridge's window), else to the platform's
//!   (FreeBSD's requests walk up the tree to the bus that has an rman for
//!   them, `nexus0` last).
//! - [`build`] lists each rman as `rm_list` holds it: by address, with the
//!   free spans between allocations as resources of no device, and the
//!   holders of a shared range newest first (`r_sharehead`).
//! - The rmans are ordered by the node whose attach made them, in tree
//!   order (FreeBSD's `rman_head` is creation order, and its attach is
//!   one at a time, depth first), the platform's first.
//!
//! Behavior reference: sys/kern/subr_rman.c (`rman_manage_region`,
//! `rman_reserve_resource`, `sysctl_rman`), usr.sbin/devinfo.

use alloc::string::String;
use alloc::vec::Vec;
use core::mem::{offset_of, size_of};

use abi::bus::{RM_TEXTLEN, UResource, URman, rf, rman_type};

/// What an rman holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// Physical memory addresses.
    Memory,
    /// x86 I/O ports, or a PCI bus's I/O space behind a bridge.
    IoPort,
    /// Interrupt numbers.
    Irq,
    /// ISA DMA channels.
    Drq,
    /// PCI bus numbers in a domain.
    BusNumber(u32),
    /// A GPIO bus's pin interrupts.
    GpioIrq,
}

/// A node of the tree: its parent, by index into the slice [`build`] is
/// given, which lists the nodes in tree order (a parent before its
/// children, siblings in order).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreeNode {
    /// The parent's index; `None` for the root.
    pub parent: Option<usize>,
}

/// A resource manager.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rman {
    /// A handle unique among rmans (`rm_handle`), not 0.
    pub handle: usize,
    /// Its description (`rm_descr`).
    pub descr: String,
    /// What it holds.
    pub kind: Kind,
    /// The space it covers (`rm_start`, `rm_end`), inclusive.
    pub start: u64,
    /// See `start`.
    pub end: u64,
    /// The regions it manages (`rman_manage_region`), inclusive; adjacent
    /// ones merge.
    pub regions: Vec<(u64, u64)>,
    /// The node whose children allocate from it (the bus); `None` for the
    /// platform's, which take what no bus below has an rman for.
    pub owner: Option<usize>,
    /// The node whose attach made it, for ordering; `None` for the
    /// platform's, made before any device.
    pub anchor: Option<usize>,
    /// Each allocation from it adds its range to the managed regions (x86
    /// MSI interrupts: FreeBSD's `nexus_add_irq` manages each as its
    /// source is made).
    pub grows: bool,
}

/// A range a node holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Request {
    /// The holder, by index.
    pub dev: usize,
    /// What it is.
    pub kind: Kind,
    /// The first address.
    pub start: u64,
    /// The last address (inclusive).
    pub end: u64,
    /// `RF_*`.
    pub flags: u32,
    /// The rman it is in, by handle, when its bus named one.
    pub rman: Option<usize>,
}

/// One resource of an rman as `sysctl_rman` lists it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The first address.
    pub start: u64,
    /// The last address (inclusive).
    pub end: u64,
    /// The holder, by index; `None` for a free span.
    pub dev: Option<usize>,
    /// `RF_*` (0 for a free span).
    pub flags: u32,
}

/// An rman and its resources, in `rm_list` order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Table<'a> {
    /// The rman.
    pub rman: &'a Rman,
    /// Its resources.
    pub entries: Vec<Entry>,
}

/// Regions sorted and merged where they overlap or touch.
fn merge(mut regions: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    regions.retain(|r| r.0 <= r.1);
    regions.sort_unstable();
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(regions.len());
    for (s, e) in regions {
        match out.last_mut() {
            Some(last) if s <= last.1.saturating_add(1) => last.1 = last.1.max(e),
            _ => out.push((s, e)),
        }
    }
    out
}

fn holds(regions: &[(u64, u64)], start: u64, end: u64) -> bool {
    regions.iter().any(|&(s, e)| s <= start && end <= e)
}

/// The rman `req` lands in, by index into `rmans`.
fn route(nodes: &[TreeNode], rmans: &[Rman], req: &Request) -> Option<usize> {
    if let Some(h) = req.rman {
        return rmans.iter().position(|r| r.handle == h);
    }
    let fits = |r: &Rman| {
        r.kind == req.kind
            && req.start <= req.end
            && (holds(&r.regions, req.start, req.end)
                || (r.grows && r.start <= req.start && req.end <= r.end))
    };
    let mut at = nodes.get(req.dev).and_then(|n| n.parent);
    while let Some(a) = at {
        if let Some(i) = rmans.iter().position(|r| r.owner == Some(a) && fits(r)) {
            return Some(i);
        }
        at = nodes.get(a).and_then(|n| n.parent);
    }
    rmans.iter().position(|r| r.owner.is_none() && fits(r))
}

/// One rman's resources: each allocation in a managed region, by
/// address, the holders of one range newest first, and the free spans.
fn list(regions: &[(u64, u64)], mut allocs: Vec<Entry>) -> Vec<Entry> {
    allocs.retain(|a| holds(regions, a.start, a.end));
    // Stable: equal ranges stay in allocation order, then each run of
    // them is reversed (`LIST_INSERT_HEAD` on the sharing list).
    allocs.sort_by_key(|a| a.start);
    let mut i = 0;
    while i < allocs.len() {
        let mut j = i + 1;
        while j < allocs.len()
            && (allocs[j].start, allocs[j].end) == (allocs[i].start, allocs[i].end)
        {
            j += 1;
        }
        if j - i > 1 {
            allocs[i..j].reverse();
            for a in &mut allocs[i..j] {
                a.flags |= rf::SHAREABLE;
            }
        }
        i = j;
    }
    let free = |start: u64, end: u64| Entry {
        start,
        end,
        dev: None,
        flags: 0,
    };
    let mut out = Vec::with_capacity(allocs.len() * 2 + regions.len());
    let mut k = 0;
    for &(rs, re) in regions {
        // The next address not yet listed; `None` past the space's end.
        let mut cursor = Some(rs);
        while k < allocs.len() && allocs[k].start <= re {
            let a = allocs[k];
            if let Some(c) = cursor
                && a.start > c
            {
                out.push(free(c, a.start - 1));
            }
            out.push(a);
            let next = a.end.checked_add(1);
            cursor = match (cursor, next) {
                (Some(c), Some(n)) => Some(c.max(n)),
                _ => None,
            };
            k += 1;
        }
        if let Some(c) = cursor
            && c <= re
        {
            out.push(free(c, re));
        }
    }
    out
}

/// The rmans with their resources, in `rman_head` order: the platform's
/// first, then by the tree order of the node that made each, each node's
/// in the order it made them (`rmans` is in creation order).
#[must_use]
pub fn build<'a>(nodes: &[TreeNode], rmans: &'a [Rman], requests: &[Request]) -> Vec<Table<'a>> {
    let mut regions: Vec<Vec<(u64, u64)>> = rmans.iter().map(|r| r.regions.clone()).collect();
    let mut allocs: Vec<Vec<Entry>> = rmans.iter().map(|_| Vec::new()).collect();
    for req in requests {
        let Some(i) = route(nodes, rmans, req) else {
            continue;
        };
        if rmans[i].grows {
            regions[i].push((req.start, req.end));
        }
        allocs[i].push(Entry {
            start: req.start,
            end: req.end,
            dev: Some(req.dev),
            flags: req.flags,
        });
    }
    let mut order: Vec<usize> = (0..rmans.len()).collect();
    // Stable: creation order within an anchor.
    order.sort_by_key(|&i| rmans[i].anchor.map_or(0, |a| a + 1));
    order
        .into_iter()
        .map(|i| {
            let merged = merge(core::mem::take(&mut regions[i]));
            Table {
                rman: &rmans[i],
                entries: list(&merged, core::mem::take(&mut allocs[i])),
            }
        })
        .collect()
}

/// `size = end - start + 1`, as `sysctl_rman` computes it: 0 for the
/// whole 64-bit space.
#[must_use]
pub const fn size(start: u64, end: u64) -> u64 {
    end.wrapping_sub(start).wrapping_add(1)
}

/// `s` NUL-terminated in `RM_TEXTLEN` bytes, cut short as `strlcpy` and
/// `snprintf` cut it.
fn text(s: &str) -> [u8; RM_TEXTLEN] {
    let mut out = [0; RM_TEXTLEN];
    let n = s.len().min(RM_TEXTLEN - 1);
    out[..n].copy_from_slice(&s.as_bytes()[..n]);
    out
}

/// `struct u_rman` for `r`.
#[must_use]
pub fn urman(r: &Rman) -> URman {
    URman {
        rm_handle: r.handle,
        rm_descr: text(&r.descr),
        rm_start: r.start,
        rm_size: size(r.start, r.end),
        rm_type: rman_type::ARRAY,
    }
}

/// `struct u_resource` for `e` in the rman `parent`: its holder's
/// `dv_handle` and name and unit (`nomatch` when it has none, as
/// `sysctl_rman` says for a device without a name), or neither for a
/// free span.
#[must_use]
pub fn uresource(
    parent: usize,
    handle: usize,
    e: &Entry,
    device: Option<(usize, &str)>,
) -> UResource {
    let (r_device, r_devname) = match device {
        Some((h, "")) => (h, text("nomatch")),
        Some((h, name)) => (h, text(name)),
        None => (0, [0; RM_TEXTLEN]),
    };
    UResource {
        r_handle: handle,
        r_parent: parent,
        r_device,
        r_devname,
        r_start: e.start,
        r_size: size(e.start, e.end),
        r_flags: e.flags,
    }
}

/// `u`'s bytes as `struct u_rman` lays them out, padding zeroed.
pub fn urman_bytes(u: &URman, out: &mut Vec<u8>) {
    let base = out.len();
    out.resize(base + size_of::<URman>(), 0);
    let o = &mut out[base..];
    let mut put = |at: usize, b: &[u8]| o[at..at + b.len()].copy_from_slice(b);
    put(offset_of!(URman, rm_handle), &u.rm_handle.to_ne_bytes());
    put(offset_of!(URman, rm_descr), &u.rm_descr);
    put(offset_of!(URman, rm_start), &u.rm_start.to_ne_bytes());
    put(offset_of!(URman, rm_size), &u.rm_size.to_ne_bytes());
    put(offset_of!(URman, rm_type), &u.rm_type.to_ne_bytes());
}

/// `u`'s bytes as `struct u_resource` lays them out, padding zeroed.
pub fn uresource_bytes(u: &UResource, out: &mut Vec<u8>) {
    let base = out.len();
    out.resize(base + size_of::<UResource>(), 0);
    let o = &mut out[base..];
    let mut put = |at: usize, b: &[u8]| o[at..at + b.len()].copy_from_slice(b);
    put(offset_of!(UResource, r_handle), &u.r_handle.to_ne_bytes());
    put(offset_of!(UResource, r_parent), &u.r_parent.to_ne_bytes());
    put(offset_of!(UResource, r_device), &u.r_device.to_ne_bytes());
    put(offset_of!(UResource, r_devname), &u.r_devname);
    put(offset_of!(UResource, r_start), &u.r_start.to_ne_bytes());
    put(offset_of!(UResource, r_size), &u.r_size.to_ne_bytes());
    put(offset_of!(UResource, r_flags), &u.r_flags.to_ne_bytes());
}

/// `devinfo -u`'s text for `tables`, naming each holder with `name`
/// (empty for one with no name: `(unknown)`), as usr.sbin/devinfo prints
/// it: hexadecimal when the rman spans more than 1000 addresses (or the
/// whole space), a range only when it is longer than one.
pub fn render(tables: &[Table<'_>], name: &dyn Fn(usize) -> String, out: &mut String) {
    use core::fmt::Write;
    for t in tables {
        let _ = writeln!(out, "{}:", t.rman.descr);
        let s = size(t.rman.start, t.rman.end);
        let hex = s > 1000 || s == 0;
        for e in &t.entries {
            out.push_str("    ");
            let len = size(e.start, e.end);
            let _ = match (hex, len > 1) {
                (true, true) => write!(out, "{:#x}-{:#x}", e.start, e.end),
                (true, false) => write!(out, "{:#x}", e.start),
                (false, true) => write!(out, "{}-{}", e.start, e.end),
                (false, false) => write!(out, "{}", e.start),
            };
            match e.dev {
                Some(d) => {
                    let n = name(d);
                    let _ = writeln!(out, " ({})", if n.is_empty() { "unknown" } else { &n });
                }
                None => out.push_str(" ----\n"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;
    use alloc::vec;

    use super::*;

    fn rman(handle: usize, descr: &str, kind: Kind, end: u64, owner: Option<usize>) -> Rman {
        Rman {
            handle,
            descr: descr.into(),
            kind,
            start: 0,
            end,
            regions: vec![(0, end)],
            owner,
            anchor: owner,
            grows: false,
        }
    }

    fn req(dev: usize, kind: Kind, start: u64, end: u64) -> Request {
        Request {
            dev,
            kind,
            start,
            end,
            flags: rf::ALLOCATED | rf::ACTIVE,
            rman: None,
        }
    }

    /// QEMU's arm64 `virt` machine as FreeBSD 15.1 lists it
    /// (tests/corpus/freebsd/rman/qemu-aarch64/devinfo-u.txt), the
    /// devices AbyssBSD shares with it: the host bridge's windows, the
    /// root port's, and the functions behind each.
    #[test]
    fn qemu_arm64_pci() {
        // 0 nexus0, 1 acpi0, 2 uart0, 3 pcib0, 4 pci0, 5 nvme0, 6 pcib1,
        // 7 pci1, 8 virtio_pci0.
        let names = [
            "nexus0",
            "acpi0",
            "uart0",
            "pcib0",
            "pci0",
            "nvme0",
            "pcib1",
            "pci1",
            "virtio_pci0",
        ];
        let parent = [
            None,
            Some(0),
            Some(1),
            Some(1),
            Some(3),
            Some(4),
            Some(4),
            Some(6),
            Some(7),
        ];
        let nodes: Vec<TreeNode> = parent.iter().map(|&p| TreeNode { parent: p }).collect();
        let mut pf = rman(5, "pcib0 prefetch window", Kind::Memory, u64::MAX, Some(3));
        pf.regions.clear();
        let mut mem = rman(6, "pcib0 memory window", Kind::Memory, u64::MAX, Some(3));
        mem.regions = vec![(0x1000_0000, 0x3efe_ffff), (0x80_0000_0000, 0xff_ffff_ffff)];
        let io = rman(7, "pcib0 I/O port window", Kind::IoPort, 0xffff, Some(3));
        let mut domain = rman(8, "PCI domain 0 bus numbers", Kind::BusNumber(0), 255, None);
        domain.anchor = Some(4);
        let mut bus1 = rman(9, "pcib1 bus numbers", Kind::BusNumber(0), 255, Some(6));
        bus1.regions = vec![(1, 1)];
        let mut io1 = rman(10, "pcib1 I/O port window", Kind::IoPort, 0xffff, Some(6));
        io1.regions.clear();
        let mut mem1 = rman(11, "pcib1 memory window", Kind::Memory, u64::MAX, Some(6));
        mem1.regions = vec![(0x1000_0000, 0x101f_ffff)];
        let mut pf1 = rman(12, "pcib1 prefetch window", Kind::Memory, u64::MAX, Some(6));
        pf1.regions = vec![(0x80_0000_0000, 0x80_000f_ffff)];
        // Made in attach order: the root port's before the domain's
        // (which pci0 makes when it takes its bus), to show the order is
        // the tree's.
        let rmans = vec![
            rman(1, "I/O memory addresses", Kind::Memory, u64::MAX, None),
            rman(2, "Interrupts", Kind::Irq, u64::MAX, None),
            {
                let mut a = rman(3, "ACPI I/O ports", Kind::IoPort, u64::MAX, Some(1));
                a.regions.clear();
                a
            },
            {
                let mut a = rman(
                    4,
                    "ACPI I/O memory addresses",
                    Kind::Memory,
                    u64::MAX,
                    Some(1),
                );
                a.regions.clear();
                a
            },
            pf,
            mem,
            io,
            bus1,
            io1,
            mem1,
            pf1,
            domain,
        ];
        let requests = vec![
            req(2, Kind::Memory, 0x900_0000, 0x900_0fff),
            req(2, Kind::Irq, 0, 0),
            // The host bridge's CPU-side windows and ECAM.
            req(3, Kind::Memory, 0x1000_0000, 0x3efe_ffff),
            req(3, Kind::Memory, 0x3eff_0000, 0x3eff_ffff),
            req(3, Kind::Memory, 0x40_1000_0000, 0x40_1fff_ffff),
            req(3, Kind::Memory, 0x80_0000_0000, 0xff_ffff_ffff),
            req(4, Kind::BusNumber(0), 0, 0),
            req(5, Kind::Memory, 0x80_0010_c000, 0x80_0010_ffff),
            req(5, Kind::Irq, 0x2a, 0x2a),
            req(6, Kind::BusNumber(0), 1, 1),
            req(6, Kind::Memory, 0x1000_0000, 0x101f_ffff),
            req(6, Kind::Memory, 0x80_0000_0000, 0x80_000f_ffff),
            req(7, Kind::BusNumber(0), 1, 1),
            req(8, Kind::Memory, 0x1000_0000, 0x1000_0fff),
            req(8, Kind::Memory, 0x80_0000_0000, 0x80_0000_3fff),
        ];
        let tables = build(&nodes, &rmans, &requests);
        let mut text = String::new();
        render(&tables, &|d| names[d].to_string(), &mut text);
        let want = "\
I/O memory addresses:
    0x0-0x8ffffff ----
    0x9000000-0x9000fff (uart0)
    0x9001000-0xfffffff ----
    0x10000000-0x3efeffff (pcib0)
    0x3eff0000-0x3effffff (pcib0)
    0x3f000000-0x400fffffff ----
    0x4010000000-0x401fffffff (pcib0)
    0x4020000000-0x7fffffffff ----
    0x8000000000-0xffffffffff (pcib0)
    0x10000000000-0xffffffffffffffff ----
Interrupts:
    0x0 (uart0)
    0x1-0x29 ----
    0x2a (nvme0)
    0x2b-0xffffffffffffffff ----
ACPI I/O ports:
ACPI I/O memory addresses:
pcib0 prefetch window:
pcib0 memory window:
    0x10000000-0x101fffff (pcib1)
    0x10200000-0x3efeffff ----
    0x8000000000-0x80000fffff (pcib1)
    0x8000100000-0x800010bfff ----
    0x800010c000-0x800010ffff (nvme0)
    0x8000110000-0xffffffffff ----
pcib0 I/O port window:
    0x0-0xffff ----
PCI domain 0 bus numbers:
    0 (pci0)
    1 (pcib1)
    2-255 ----
pcib1 bus numbers:
    1 (pci1)
pcib1 I/O port window:
pcib1 memory window:
    0x10000000-0x10000fff (virtio_pci0)
    0x10001000-0x101fffff ----
pcib1 prefetch window:
    0x8000000000-0x8000003fff (virtio_pci0)
    0x8000004000-0x80000fffff ----
";
        assert_eq!(text, want);
    }

    /// x86's interrupt lines: the I/O APIC's pins are managed from the
    /// start, each MSI as it is allocated; two holders of a shared line
    /// are listed newest first (bhyve's `uart3`, then `uart1`).
    #[test]
    fn x86_irqs_grow_and_share() {
        let nodes = [
            TreeNode { parent: None },
            TreeNode { parent: Some(0) },
            TreeNode { parent: Some(0) },
            TreeNode { parent: Some(0) },
        ];
        let mut irq = rman(1, "Interrupt request lines", Kind::Irq, u64::MAX, None);
        irq.regions = vec![(0, 23)];
        irq.grows = true;
        let rmans = [irq];
        let requests = [
            req(1, Kind::Irq, 3, 3),
            req(2, Kind::Irq, 3, 3),
            req(3, Kind::Irq, 64, 64),
            req(3, Kind::Irq, 65, 65),
        ];
        let tables = build(&nodes, &rmans, &requests);
        let e = &tables[0].entries;
        assert_eq!(
            e.iter()
                .map(|e| (e.start, e.end, e.dev))
                .collect::<Vec<_>>(),
            vec![
                (0, 2, None),
                (3, 3, Some(2)),
                (3, 3, Some(1)),
                (4, 23, None),
                (64, 64, Some(3)),
                (65, 65, Some(3)),
            ]
        );
        assert!(e[1].flags & rf::SHAREABLE != 0 && e[0].flags == 0);
    }

    /// The whole 64-bit space has size 0, and a free span over all of it
    /// prints as its start alone (the board's `GPIO Interrupts`).
    #[test]
    fn whole_space() {
        let nodes = [TreeNode { parent: None }];
        let rmans = [rman(1, "GPIO Interrupts", Kind::GpioIrq, u64::MAX, Some(0))];
        let tables = build(&nodes, &rmans, &[]);
        let mut text = String::new();
        render(&tables, &|_| String::new(), &mut text);
        assert_eq!(text, "GPIO Interrupts:\n    0x0 ----\n");
        let u = urman(&rmans[0]);
        assert_eq!((u.rm_start, u.rm_size), (0, 0));
        assert_eq!(&u.rm_descr[..16], b"GPIO Interrupts\0");
    }

    /// A request no rman holds is dropped; one that only part of a region
    /// holds as well; regions that touch merge.
    #[test]
    fn regions_and_misses() {
        let nodes = [TreeNode { parent: None }, TreeNode { parent: Some(0) }];
        let mut a = rman(1, "ACPI I/O ports", Kind::IoPort, u64::MAX, Some(0));
        a.regions = vec![(0x72, 0x77), (0x61, 0x61), (0x60, 0x60), (0x400, 0x407)];
        let rmans = [a];
        let requests = [
            req(1, Kind::IoPort, 0x400, 0x403),
            req(1, Kind::IoPort, 0x406, 0x409),
            req(1, Kind::Memory, 0x400, 0x403),
        ];
        let tables = build(&nodes, &rmans, &requests);
        let mut text = String::new();
        render(&tables, &|_| "acpi_timer0".into(), &mut text);
        assert_eq!(
            text,
            "ACPI I/O ports:\n    0x60-0x61 ----\n    0x72-0x77 ----\n    0x400-0x403 (acpi_timer0)\n    0x404-0x407 ----\n"
        );
    }

    #[test]
    fn records() {
        let r = rman(
            7,
            "a description longer than thirty-one bytes",
            Kind::Memory,
            0xfff,
            None,
        );
        let u = urman(&r);
        assert_eq!(u.rm_descr[31], 0);
        assert_eq!(
            &u.rm_descr[..31],
            &b"a description longer than thirty-one bytes"[..31]
        );
        assert_eq!(u.rm_size, 0x1000);
        let mut b = Vec::new();
        urman_bytes(&u, &mut b);
        assert_eq!(b.len(), size_of::<URman>());
        let e = Entry {
            start: 0x10,
            end: 0x1f,
            dev: Some(3),
            flags: rf::ALLOCATED,
        };
        let r = uresource(7, 99, &e, Some((0x1_0000_0004, "")));
        assert_eq!(&r.r_devname[..8], b"nomatch\0");
        assert_eq!(r.r_size, 0x10);
        let free = uresource(7, 100, &e, None);
        assert_eq!((free.r_device, free.r_devname[0]), (0, 0));
        let mut b = Vec::new();
        uresource_bytes(&r, &mut b);
        assert_eq!(b.len(), size_of::<UResource>());
        assert_eq!(
            &b[offset_of!(UResource, r_start)..][..8],
            &0x10u64.to_ne_bytes()
        );
    }
}
