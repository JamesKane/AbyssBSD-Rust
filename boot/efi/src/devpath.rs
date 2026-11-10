// SPDX-License-Identifier: BSD-2-Clause
//! Device paths (UEFI 2.10 §10): a handle's place in the machine, as a
//! sequence of nodes from the root bridge down, ended by an End Entire
//! node. A partition's path is its disk's with a media node added, which is
//! how the loader finds the disk it was loaded from (FreeBSD's
//! `efiblk_get_pdinfo_by_handle` matches them the same way).

/// A device path's nodes, without the end node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DevicePath<'a> {
    bytes: &'a [u8],
}

/// Node header: type, sub-type, and a little-endian 16-bit length that
/// counts the header.
const HEADER: usize = 4;
const END_TYPE: u8 = 0x7f;
const END_ENTIRE: u8 = 0xff;
/// No real path comes near this; it bounds a walk over a broken one.
const MAX_BYTES: usize = 64 * 1024;

impl<'a> DevicePath<'a> {
    /// Reads a path from `bytes`, which hold it and its end node (and may
    /// run on). `None` when a node is shorter than its header, overruns
    /// `bytes`, or no End Entire node comes.
    #[must_use]
    pub fn parse(bytes: &'a [u8]) -> Option<Self> {
        let mut at = 0;
        loop {
            let h = bytes.get(at..at + HEADER)?;
            if h[0] == END_TYPE && h[1] == END_ENTIRE {
                return Some(Self {
                    bytes: &bytes[..at],
                });
            }
            let len = usize::from(u16::from_le_bytes([h[2], h[3]]));
            if len < HEADER {
                return None;
            }
            at = at.checked_add(len).filter(|&n| n <= bytes.len())?;
        }
    }

    /// Reads the path at `p` in firmware memory.
    ///
    /// # Safety
    ///
    /// `p` points to a device path the firmware returned, which stays put
    /// while boot services run.
    #[must_use]
    pub unsafe fn from_ptr(p: *const u8) -> Option<DevicePath<'static>> {
        let mut at = 0usize;
        loop {
            if at + HEADER > MAX_BYTES {
                return None;
            }
            // SAFETY: the caller vouches for a well-formed path, so a node
            // header lies at each node boundary up to the end node.
            let h: [u8; HEADER] = unsafe { p.add(at).cast::<[u8; HEADER]>().read_unaligned() };
            if h[0] == END_TYPE && h[1] == END_ENTIRE {
                // SAFETY: the nodes walked so far are `at` bytes of the
                // firmware's path.
                let bytes = unsafe { core::slice::from_raw_parts(p, at) };
                return Some(DevicePath { bytes });
            }
            let len = usize::from(u16::from_le_bytes([h[2], h[3]]));
            if len < HEADER {
                return None;
            }
            at += len;
        }
    }

    /// The path's bytes, without the end node.
    #[must_use]
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// The number of nodes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.nodes().count()
    }

    /// Whether the path has no nodes (the root).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Each node's type, sub-type and data.
    pub fn nodes(&self) -> impl Iterator<Item = (u8, u8, &'a [u8])> + 'a {
        let bytes = self.bytes;
        let mut at = 0;
        core::iter::from_fn(move || {
            let h = bytes.get(at..at + HEADER)?;
            let len = usize::from(u16::from_le_bytes([h[2], h[3]]));
            let node = (h[0], h[1], &bytes[at + HEADER..at + len]);
            at += len;
            Some(node)
        })
    }

    /// The unique GUID of the GPT partition this path ends in (a Hard
    /// Drive media node with a GUID signature, UEFI 2.10 §10.3.5.1), in
    /// on-disk byte order.
    #[must_use]
    pub fn gpt_partition(&self) -> Option<[u8; 16]> {
        let (kind, sub, data) = self.nodes().last()?;
        // Partition number (4), start (8), size (8), signature (16), MBR
        // type (1), signature type (1; 2 is a GUID).
        if kind != 4 || sub != 1 || data.len() < 38 || data[37] != 2 {
            return None;
        }
        data[20..36].try_into().ok()
    }

    /// Whether `ancestor`'s nodes begin this path: the device `self` names
    /// is `ancestor` or lies below it.
    #[must_use]
    pub fn starts_with(&self, ancestor: &DevicePath<'_>) -> bool {
        // Both were walked node by node, so equal leading bytes end on a
        // node boundary of each.
        self.bytes.starts_with(ancestor.bytes)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use std::vec::Vec;

    use super::*;

    fn node(kind: u8, sub: u8, data: &[u8]) -> Vec<u8> {
        let len = u16::try_from(HEADER + data.len()).unwrap();
        let mut v = std::vec![kind, sub];
        v.extend_from_slice(&len.to_le_bytes());
        v.extend_from_slice(data);
        v
    }

    fn end() -> Vec<u8> {
        node(END_TYPE, END_ENTIRE, &[])
    }

    #[test]
    fn a_partition_lies_below_its_disk() {
        // PciRoot(0)/Pci(1,0)/USB(2,0) and HD(1,GPT,...) below it.
        let pci_root = node(2, 1, &[0x41, 0xd0, 0x0a, 0x03, 0, 0, 0, 0]);
        let pci = node(1, 1, &[0, 1]);
        let usb = node(3, 5, &[2, 0]);
        let hd = node(4, 1, &[0; 38]);
        let disk: Vec<u8> = [&pci_root[..], &pci, &usb, &end()].concat();
        let part: Vec<u8> = [&pci_root[..], &pci, &usb, &hd, &end()].concat();
        let other: Vec<u8> = [&pci_root[..], &node(1, 1, &[0, 2]), &end()].concat();
        let (disk, part_path, other) = (
            DevicePath::parse(&disk).unwrap(),
            DevicePath::parse(&part).unwrap(),
            DevicePath::parse(&other).unwrap(),
        );
        assert_eq!(disk.len(), 3);
        // SAFETY: `part` holds a whole path, end node included.
        let raw = unsafe { DevicePath::from_ptr(part.as_ptr()) };
        assert_eq!(raw, Some(part_path));
        let part = part_path;
        assert_eq!(part.len(), 4);
        assert!(part.starts_with(&disk));
        assert!(disk.starts_with(&disk));
        assert!(!disk.starts_with(&part));
        assert!(!part.starts_with(&other));
        assert_eq!(part.nodes().last().unwrap().0, 4);
        // That HD node has an MBR signature type (0), so no GUID.
        assert_eq!(part.gpt_partition(), None);
        assert_eq!(disk.gpt_partition(), None);
        let mut hd = std::vec![0u8; 38];
        hd[0] = 1;
        hd[20..36].copy_from_slice(&[0xab; 16]);
        hd[36] = 2;
        hd[37] = 2;
        let esp: Vec<u8> = [&pci_root[..], &pci, &usb, &node(4, 1, &hd), &end()].concat();
        let esp = DevicePath::parse(&esp).unwrap();
        assert_eq!(esp.gpt_partition(), Some([0xab; 16]));
    }

    #[test]
    fn broken_paths_are_refused() {
        assert_eq!(DevicePath::parse(&[]), None);
        // A node shorter than its header.
        assert_eq!(DevicePath::parse(&[1, 1, 2, 0]), None);
        // A node running past the bytes.
        assert_eq!(DevicePath::parse(&[1, 1, 9, 0, 0]), None);
        // No end node.
        assert_eq!(DevicePath::parse(&node(1, 1, &[0, 1])), None);
        assert!(DevicePath::parse(&end()).unwrap().is_empty());
    }
}
