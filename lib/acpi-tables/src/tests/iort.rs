// SPDX-License-Identifier: BSD-2-Clause

extern crate std;

use std::vec;
use std::vec::Vec;

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
fn single_mappings_and_loops() {
    // A single mapping gives its one output ID for every input it covers
    // (FreeBSD's `iort_entry_lookup`); a mapping that points at its own
    // node goes nowhere.
    let single = node(2, &rc(0), &[(0, 0xff, 5, 48, 1)]);
    let m = table(&[node(0, &its_group(0), &[]), single]);
    let t = parse(&Sdt::read(&&m, 0).unwrap()).unwrap();
    assert_eq!(t.its_for_pci(0, 1).map(|t| t.device_id), Some(5));
    assert_eq!(t.its_for_pci(0, 0x100), None, "outside the input range");
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

/// An SMMUv3's data: base, flags, VATOS, model, the four GSIVs (event,
/// PRI, gerror, sync), proximity domain, ID mapping index.
fn smmu_v3(base: u64, gsivs: [u32; 4], index: u32) -> B {
    B::new()
        .u64(base)
        .u32(1)
        .u32(0)
        .u64(0)
        .u32(0)
        .u32(gsivs[0])
        .u32(gsivs[1])
        .u32(gsivs[2])
        .u32(gsivs[3])
        .u32(0)
        .u32(index)
}

/// A node as [`node`] makes it, with revision `rev`.
fn node_rev(kind: u8, rev: u8, data: &B, maps: &[(u32, u32, u32, u32, u32)]) -> B {
    let mut b = node(kind, data, maps);
    b.0[3] = rev;
    b
}

#[test]
fn an_smmu_msi_mapping_is_skipped_only_when_in_use() {
    // ITS at 48 (24 bytes); SMMU at 72 (16 + 52 + 2 maps = 108): mapping 0
    // is its own MSI (single, to device 0x77), mapping 1 passes streams on;
    // root complex at 180.
    let maps = [(0, 0, 0x77, 48, 1), (0, 0xffff, 0x1000, 48, 0)];
    for (gsivs, msi) in [([0, 0, 0, 0], true), ([40, 41, 42, 43], false)] {
        let its = node(0, &its_group(3), &[]);
        let smmu = node_rev(4, 2, &smmu_v3(0x1000_0000, gsivs, 0), &maps);
        assert_eq!(smmu.len(), 108);
        let root = node(2, &rc(0), &[(0, 0xffff, 0, 72, 0)]);
        let m = table(&[its, smmu, root]);
        let t = parse(&Sdt::read(&&m, 0).unwrap()).unwrap();
        assert_eq!(
            t.stream_for_pci(0, 0),
            Some(Stream {
                smmu: 72,
                stream: 0
            })
        );
        // With MSIs in use, mapping 0 is the SMMU's own and stream 0 takes
        // mapping 1; wired, mapping 0 (single) catches stream 0.
        let want = if msi { 0x1000 } else { 0x77 };
        assert_eq!(t.its_for_pci(0, 0).map(|t| t.device_id), Some(want));
        let (_, s) = t.smmu_v3s().next().unwrap();
        assert_eq!(s.wired(), !msi);
        assert_eq!(s.id_mapping_index, Some(0));
    }
    // Revision 0 has no index field.
    let smmu = node_rev(4, 0, &smmu_v3(0, [0; 4], 0), &[]);
    let m = table(&[smmu]);
    let t = parse(&Sdt::read(&&m, 0).unwrap()).unwrap();
    assert_eq!(t.smmu_v3s().next().unwrap().1.id_mapping_index, None);
}

#[test]
fn truncated_nodes_fail_cleanly() {
    // An SMMUv3 whose data is cut short, a named component without its
    // NUL, and an RMR whose ranges run past the node.
    let short = node(4, &B::new().u64(0x1000), &[]);
    let unnamed = node(1, &B::new().zeros(13).bytes(b"\\_SB_.X"), &[]);
    let rmr = node(6, &B::new().u32(1).u32(4).u32(28), &[]);
    let m = table(&[short, unnamed, rmr]);
    let t = parse(&Sdt::read(&&m, 0).unwrap()).unwrap();
    assert_eq!(t.nodes().count(), 3);
    assert_eq!(t.smmu_v3s().count(), 0);
    assert_eq!(t.named_components().count(), 0);
    assert_eq!(t.rmrs().count(), 0);
    // A mapping count past the node's end: only the mappings that fit.
    let mut n = node(2, &rc(0), &[(0, 0xff, 0, 48, 0)]);
    n.0[8] = 9;
    let m = table(&[node(0, &its_group(0), &[]), n]);
    let t = parse(&Sdt::read(&&m, 0).unwrap()).unwrap();
    assert_eq!(t.nodes().nth(1).unwrap().mappings().count(), 1);
    assert_eq!(t.its_for_pci(0, 7).map(|t| t.device_id), Some(7));
}

#[test]
fn opi6p() {
    let m = Mem(include_bytes!("../../../../tests/corpus/acpi/opi6p/iort.dat").to_vec());
    let sdt = Sdt::read(&&m, 0).unwrap();
    assert_eq!(sdt.revision(), 5);
    let t = parse(&sdt).unwrap();
    assert_eq!(t.nodes().count(), 19);
    assert_eq!(t.its_groups(), 1);
    assert_eq!(t.root_complexes().count(), 1);
    assert_eq!(t.named_components().count(), 14);
    assert_eq!(t.rmrs().count(), 1);
    // Both SMMUs, in table order: base, coherent, generic, and the GSIVs.
    let smmus: Vec<(usize, SmmuV3)> = t.smmu_v3s().collect();
    assert_eq!(smmus.len(), 2);
    let [(s0, a), (s1, b)] = [smmus[0], smmus[1]];
    assert_eq!((s0, s1), (0x48, 0xdc));
    assert_eq!(a.base, 0xb01_0000);
    assert_eq!(b.base, 0xb1b_0000);
    for (s, gsivs) in [(a, [107, 116, 112, 108]), (b, [141, 150, 146, 142])] {
        assert!(s.coherent() && s.wired());
        assert_eq!(s.flags, 1);
        assert_eq!(s.model, SmmuV3::MODEL_GENERIC);
        assert_eq!([s.event_gsiv, s.pri_gsiv, s.gerr_gsiv, s.sync_gsiv], gsivs);
    }
    // The root complex: coherent, ATS, no address size limit, and RIDs to
    // SMMU 0 as stream IDs, then on to the ITS as device IDs.
    let (_, rc) = t.root_complexes().next().unwrap();
    assert_eq!((rc.cca, rc.access_flags, rc.ats), (1, 3, 1));
    assert_eq!((rc.segment, rc.address_limit), (0, 0));
    for rid in [0, 0xc000, 0xc100, 0x3100, 0xffff] {
        assert_eq!(
            t.stream_for_pci(0, rid),
            Some(Stream {
                smmu: s0,
                stream: u32::from(rid)
            })
        );
        assert_eq!(
            t.its_for_pci(0, rid),
            Some(ItsTarget {
                its: 0,
                device_id: u32::from(rid)
            })
        );
    }
    assert_eq!(t.stream_for_pci(1, 0xc100), None);
    // Every named component's streams on SMMU 1, and none on SMMU 0.
    let streams =
        |at: usize, smmu: usize| -> Vec<u32> { t.node_streams(at, smmu).flatten().collect() };
    let named: Vec<(&str, Vec<u32>)> = t
        .named_components()
        .map(|(at, c)| {
            assert!(streams(at, s0).is_empty());
            assert_eq!((c.cca, c.address_limit), (1, 40));
            (c.name, streams(at, s1))
        })
        .collect();
    let want: Vec<(&str, Vec<u32>)> = vec![
        ("\\_SB_.DPU0", vec![0, 1, 3, 4, 5]),
        ("\\_SB_.AEU0", vec![2]),
        ("\\_SB_.DPU1", vec![6, 7, 9, 10, 11]),
        ("\\_SB_.AEU1", vec![8]),
        ("\\_SB_.DPU2", vec![0xc, 0xd, 0xf, 0x10, 0x11]),
        ("\\_SB_.AEU2", vec![0xe]),
        ("\\_SB_.DPU3", vec![0x12, 0x13, 0x15, 0x16, 0x17]),
        ("\\_SB_.AEU3", vec![0x14]),
        ("\\_SB_.DPU4", vec![0x18, 0x19, 0x1b, 0x1c, 0x1d]),
        ("\\_SB_.AEU4", vec![0x1a]),
        ("\\_SB_.NPU0", vec![0x1e]),
        ("\\_SB_.ISPM", vec![0x1f]),
        ("\\_SB_.CBD0", vec![0x21]),
        ("\\_SB_.CBD2", vec![0x22]),
    ];
    assert_eq!(named, want);
    // The RMR: the firmware framebuffer, remappable, Normal non-cacheable,
    // for two streams of each display controller.
    let (at, rmr) = t.rmrs().next().unwrap();
    assert_eq!(t.node(at).unwrap().revision(), 3);
    assert_eq!(rmr.flags, 0x11);
    assert!(rmr.remap_permitted());
    assert_eq!(rmr.attributes(), 4);
    assert_eq!(
        rmr.ranges().collect::<Vec<_>>(),
        [MemRange {
            base: 0x8480_0000,
            len: 8 << 20
        }]
    );
    assert_eq!(
        streams(at, s1),
        [0x0, 0x1, 0x6, 0x7, 0xc, 0xd, 0x12, 0x13, 0x18, 0x19]
    );
    assert!(streams(at, s0).is_empty());
}

/// QEMU 10.2's `virt,iommu=smmuv3`: an SMMUv3 in front of buses 0 and 1
/// only; the rest of the RID space goes straight to the ITS.
#[test]
fn qemu_virt_smmuv3() {
    let m = Mem(include_bytes!("../../../../tests/corpus/acpi/qemu-virt-smmuv3/iort.dat").to_vec());
    let t = parse(&Sdt::read(&&m, 0).unwrap()).unwrap();
    assert_eq!(t.nodes().count(), 3);
    let smmus: Vec<(usize, SmmuV3)> = t.smmu_v3s().collect();
    assert_eq!(smmus.len(), 1);
    let (at, s) = smmus[0];
    assert_eq!(s.base, 0x905_0000);
    assert!(s.coherent() && s.wired());
    assert_eq!(s.model, SmmuV3::MODEL_GENERIC);
    assert_eq!(
        [s.event_gsiv, s.pri_gsiv, s.gerr_gsiv, s.sync_gsiv],
        [106, 107, 109, 108]
    );
    assert_eq!(
        t.stream_for_pci(0, 0x0100),
        Some(Stream {
            smmu: at,
            stream: 0x100
        })
    );
    assert_eq!(t.stream_for_pci(0, 0x0200), None, "bypasses the SMMU");
    for rid in [0x8, 0x100, 0x1ff, 0x200, 0xffff] {
        assert_eq!(
            t.its_for_pci(0, rid),
            Some(ItsTarget {
                its: 0,
                device_id: u32::from(rid)
            })
        );
    }
}

#[test]
fn the_corpora_survive_the_torture() {
    for bytes in [
        &include_bytes!("../../../../tests/corpus/acpi/opi6p/iort.dat")[..],
        &include_bytes!("../../../../tests/corpus/acpi/qemu-virt-smmuv3/iort.dat")[..],
    ] {
        torture(*b"IORT", bytes[8], &bytes[crate::HEADER_LEN..]);
    }
}
