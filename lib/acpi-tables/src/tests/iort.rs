// SPDX-License-Identifier: BSD-2-Clause

use crate::iort::*;
use crate::testutil::{B, Mem, one, torture};
use crate::{Error, Sdt};

/// A node: type, revision, identifier, mappings (input, count - 1,
/// output, target offset, flags), and its type-specific data, with the
/// mappings after the data.
fn node(kind: u8, data: &B, maps: &[(u32, u32, u32, u32, u32)]) -> B {
    let map_off = 16 + data.len();
    let len = map_off + 20 * maps.len();
    let mut b = B::new()
        .u8(kind)
        .u16(u16::try_from(len).unwrap())
        .u8(0)
        .u32(0)
        .u32(u32::try_from(maps.len()).unwrap())
        .u32(u32::try_from(map_off).unwrap())
        .bytes(&data.0);
    for &(input, count, output, target, flags) in maps {
        b = b.u32(input).u32(count).u32(output).u32(target).u32(flags);
    }
    b
}

/// A root complex's data: memory access properties, ATS, segment.
fn rc(segment: u32) -> B {
    B::new().zeros(8).u32(0).u32(segment).zeros(4)
}

fn its_group(id: u32) -> B {
    B::new().u32(1).u32(id)
}

/// An IORT whose nodes start at 48 (header, count, offset, reserved).
fn table(nodes: &[B]) -> Mem {
    let mut body = B::new()
        .u32(u32::try_from(nodes.len()).unwrap())
        .u32(48)
        .u32(0);
    for n in nodes {
        body = body.bytes(&n.0);
    }
    one(*b"IORT", 3, &body.0)
}

#[test]
fn qemu_virt_maps_rids_identically() {
    let m = Mem(include_bytes!("../../../../tests/corpus/acpi/qemu-virt/iort.dat").to_vec());
    let t = parse(&Sdt::read(&&m, 0).unwrap()).unwrap();
    assert_eq!(t.its_groups(), 1);
    // virtio-rng behind the root port: bus 1, device 0.
    assert_eq!(
        t.its_for_pci(0, 0x0100),
        Some(ItsTarget {
            its: 0,
            device_id: 0x0100
        })
    );
    assert_eq!(t.its_for_pci(0, 0xffff).map(|t| t.device_id), Some(0xffff));
    assert_eq!(t.its_for_pci(1, 0x0100), None, "no segment 1");
}

#[test]
fn through_an_smmu() {
    // ITS group at 48 (24 bytes), SMMU at 72 (16 + 16 data + 20 map = 52),
    // root complex at 124.
    let its = node(0, &its_group(7), &[]);
    let smmu = node(4, &B::new().zeros(16), &[(0x1000, 0xfff, 0x9000, 48, 0)]);
    let root = node(2, &rc(2), &[(0, 0xffff, 0x1000, 72, 0)]);
    assert_eq!((its.len(), smmu.len()), (24, 52));
    let m = table(&[its, smmu, root]);
    let t = parse(&Sdt::read(&&m, 0).unwrap()).unwrap();
    // RID 0x10 -> stream 0x1010 -> device 0x9010.
    assert_eq!(
        t.its_for_pci(2, 0x10),
        Some(ItsTarget {
            its: 7,
            device_id: 0x9010
        })
    );
    // Out of the SMMU's input range: unmapped.
    assert_eq!(t.its_for_pci(2, 0xf000), None);
}

#[test]
fn single_mappings_and_loops_go_nowhere() {
    // A root complex whose only mapping is a single mapping, and one whose
    // mapping points at itself.
    let single = node(2, &rc(0), &[(0, 0xffff, 5, 48, 1)]);
    let m = table(&[node(0, &its_group(0), &[]), single]);
    let t = parse(&Sdt::read(&&m, 0).unwrap()).unwrap();
    assert_eq!(t.its_for_pci(0, 1), None);
    let looped = node(2, &rc(0), &[(0, 0xffff, 0, 48, 0)]);
    let m = table(&[looped]);
    let t = parse(&Sdt::read(&&m, 0).unwrap()).unwrap();
    assert_eq!(t.its_for_pci(0, 1), None);
}

#[test]
fn rejects_and_survives() {
    let m = one(*b"IORX", 3, &[0; 12]);
    assert_eq!(parse(&Sdt::read(&&m, 0).unwrap()), Err(Error::BadSignature));
    let m = one(*b"IORT", 3, &[0; 4]);
    assert_eq!(
        parse(&Sdt::read(&&m, 0).unwrap()),
        Err(Error::BadLength(*b"IORT"))
    );
    let its = node(0, &its_group(0), &[]);
    let root = node(2, &rc(0), &[(0, 0xffff, 0, 48, 0)]);
    let mut body = B::new().u32(2).u32(48).u32(0).bytes(&its.0).bytes(&root.0);
    torture(*b"IORT", 3, &body.0);
    // A node count far past the table: the walk stops at the end.
    body.0[0] = 0xff;
    let m = one(*b"IORT", 3, &body.0);
    let t = parse(&Sdt::read(&&m, 0).unwrap()).unwrap();
    assert_eq!(t.its_for_pci(0, 3).map(|t| t.device_id), Some(3));
}
