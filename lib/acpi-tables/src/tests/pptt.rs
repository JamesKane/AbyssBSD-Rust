// SPDX-License-Identifier: BSD-2-Clause

extern crate std;
use std::vec::Vec;

use crate::pptt::*;
use crate::testutil::{B, Mem, one, torture};
use crate::{Error, Sdt};

fn node(b: B, flags: u32, parent: u32, id: u32, res: &[u32]) -> B {
    let len = u8::try_from(20 + 4 * res.len()).unwrap();
    let mut b = b
        .u8(0)
        .u8(len)
        .u16(0)
        .u32(flags)
        .u32(parent)
        .u32(id)
        .u32(u32::try_from(res.len()).unwrap());
    for r in res {
        b = b.u32(*r);
    }
    b
}

fn cache(b: B, next: u32, size: u32, id: u32) -> B {
    b.u8(1)
        .u8(28)
        .u16(0)
        .u32(0xff)
        .u32(next)
        .u32(size)
        .u32(size / 64 / 16)
        .u8(16)
        .u8(0x0b)
        .u16(64)
        .u32(id)
}

/// package(64) ─ cluster(88) ─ core0(108) ─ thread0(132, uid 0)
///            │                          └ thread1(152, uid 1)
///            └ core1(172, uid 2)
/// with an L3 (36) on the package and an L2 (192) on core0.
fn body() -> Vec<u8> {
    let b = cache(B::new(), 0, 32 << 20, 1);
    assert_eq!(36 + b.len(), 64);
    let b = node(b, 0x13, 0, 0, &[36]);
    assert_eq!(36 + b.len(), 88);
    let b = node(b, 0x10, 64, 0, &[]);
    let b = node(b, 0x10, 88, 0, &[192]);
    assert_eq!(36 + b.len(), 132);
    let b = node(b, 0xe, 108, 0, &[]);
    let b = node(b, 0xe, 108, 1, &[]);
    let b = node(b, 0xa, 64, 2, &[]);
    assert_eq!(36 + b.len(), 192);
    cache(b, 36, 1 << 20, 2).0
}

fn pptt(m: &Mem) -> Pptt<'_> {
    parse(&Sdt::read(&m, 0).unwrap()).unwrap()
}

#[test]
fn decodes_nodes() {
    let m = one(*b"PPTT", 3, &body());
    let p = pptt(&m);
    let n: Vec<_> = p.nodes().map(Result::unwrap).collect();
    assert_eq!(n.len(), 8);
    assert_eq!(
        n.iter().map(PpttNode::offset).collect::<Vec<_>>(),
        [36, 64, 88, 108, 132, 152, 172, 192]
    );
    let pkg = n[1].processor().unwrap();
    assert!(pkg.flags.physical_package() && pkg.flags.acpi_id_valid() && pkg.flags.identical());
    assert!(!pkg.flags.is_leaf() && !pkg.flags.is_thread());
    assert_eq!(pkg.parent(), None);
    assert_eq!(pkg.private_resources().collect::<Vec<_>>(), [36]);
    let t = n[5].processor().unwrap();
    assert!(t.flags.is_thread() && t.flags.is_leaf());
    assert_eq!((t.parent(), t.acpi_processor_id), (Some(108), 1));
    let PpttNode::Cache(l2) = n[7] else { panic!() };
    assert_eq!(
        l2,
        CacheNode {
            offset: 192,
            flags: CacheFlags(0xff),
            next_level: 36,
            size: 1 << 20,
            sets: 1024,
            associativity: 16,
            attributes: CacheAttributes(0x0b),
            line_size: 64,
            cache_id: Some(2),
        }
    );
    assert!(l2.flags.size_valid() && l2.flags.cache_id_valid() && l2.flags.line_size_valid());
    assert_eq!(l2.attributes.allocation(), Allocation::ReadWrite);
    assert_eq!(l2.attributes.cache_type(), CacheType::Unified);
    assert!(!l2.attributes.write_through());
    assert_eq!(CacheAttributes(0x14).cache_type(), CacheType::Instruction);
    assert_eq!(CacheAttributes(0x14).allocation(), Allocation::Read);
    assert!(CacheAttributes(0x14).write_through());
    assert_eq!(CacheAttributes(0x01).cache_type(), CacheType::Data);
    assert_eq!(CacheAttributes(0x01).allocation(), Allocation::Write);
    // The L2's next level is the L3.
    assert_eq!(p.cache_at(l2.next_level).map(|c| c.size), Some(32 << 20));
    // Revision 2 has no cache ID, even in a 28-byte structure.
    let m2 = one(*b"PPTT", 2, &body());
    assert_eq!(pptt(&m2).cache_at(192).unwrap().cache_id, None);
}

#[test]
fn lookups_and_topology() {
    let m = one(*b"PPTT", 3, &body());
    let p = pptt(&m);
    assert!(matches!(p.node_at(36), Some(PpttNode::Cache(_))));
    assert_eq!(p.node_at(37), None);
    assert_eq!(p.node_at(0), None);
    assert_eq!(p.node_at(u32::MAX), None);
    assert_eq!(p.processor_at(36), None);
    assert_eq!(p.cache_at(64), None);
    assert_eq!(p.processor(0).map(|n| n.offset), Some(132));
    assert_eq!(p.processor(2).map(|n| n.offset), Some(172));
    assert_eq!(p.processor(3), None);
    let t0 = p.processor(0).unwrap();
    let up: Vec<_> = p.ancestors(&t0).map(|n| n.unwrap().offset).collect();
    assert_eq!(up, [108, 88, 64]);

    let topo = p.topology(1).unwrap();
    assert!(topo.is_thread());
    assert_eq!(topo.leaf.offset, 152);
    assert_eq!(topo.core.offset, 108);
    assert_eq!(topo.cluster.map(|n| n.offset), Some(88));
    assert_eq!(topo.package.map(|n| n.offset), Some(64));
    let caches: Vec<_> = topo
        .core
        .private_resources()
        .filter_map(|o| p.cache_at(o))
        .collect();
    assert_eq!(caches.len(), 1);

    let topo = p.topology(2).unwrap();
    assert!(!topo.is_thread());
    assert_eq!(topo.core.offset, 172);
    assert_eq!(topo.cluster, None);
    assert_eq!(topo.package.map(|n| n.offset), Some(64));
    assert_eq!(p.topology(9), None);
}

#[test]
fn revision_1_leaves_are_found_without_the_leaf_flag() {
    // A core whose ID matches a thread's must not be taken for the leaf.
    let b = node(B::new(), 0x3, 0, 0, &[]);
    let b = node(b, 0x2, 36, 0, &[]);
    let b = node(b, 0x2 | 0x4, 56, 0, &[]);
    let m = one(*b"PPTT", 1, &b.0);
    let p = pptt(&m);
    let topo = p.topology(0).unwrap();
    assert_eq!(topo.leaf.offset, 76);
    assert_eq!(topo.core.offset, 56);
    assert_eq!(topo.cluster, None);
    assert_eq!(topo.package.map(|n| n.offset), Some(36));
}

#[test]
fn cycles_and_bad_parents_are_contained() {
    // Two nodes that are each other's parent.
    let b = node(B::new(), 0, 56, 0, &[]);
    let b = node(b, 0xa, 36, 0, &[]);
    let m = one(*b"PPTT", 3, &b.0);
    let p = pptt(&m);
    let leaf = p.processor(0).unwrap();
    let up: Vec<_> = p.ancestors(&leaf).collect();
    assert_eq!(up.len(), usize::try_from(MAX_DEPTH).unwrap() + 1);
    assert_eq!(
        up.last(),
        Some(&Err(Error::Malformed("PPTT hierarchy too deep")))
    );
    let topo = p.topology(0);
    assert_eq!(topo, None);

    // A node that is its own parent.
    let m = one(*b"PPTT", 3, &node(B::new(), 0xa, 36, 0, &[]).0);
    let p = pptt(&m);
    let leaf = p.processor(0).unwrap();
    assert_eq!(
        p.ancestors(&leaf).count(),
        usize::try_from(MAX_DEPTH).unwrap() + 1
    );

    // Parents in the middle of a node, at a cache, and past the end.
    for parent in [37, 56, 4000] {
        let b = node(B::new(), 0xa, parent, 0, &[]);
        let b = cache(b, 0, 0, 0);
        let m = one(*b"PPTT", 3, &b.0);
        let p = pptt(&m);
        let leaf = p.processor(0).unwrap();
        assert_eq!(
            p.ancestors(&leaf).collect::<Vec<_>>(),
            [Err(Error::Malformed("PPTT parent reference"))]
        );
        assert_eq!(p.topology(0), None);
    }
}

#[test]
fn malformed_nodes() {
    // Resource count larger than the node.
    let mut b = node(B::new(), 0xa, 0, 0, &[36]).0;
    b[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
    let b = node(B::new().bytes(&b), 0xa, 0, 1, &[]).0;
    let m = one(*b"PPTT", 3, &b);
    let p = pptt(&m);
    let n: Vec<_> = p.nodes().collect();
    assert_eq!(n[0], Err(Error::Malformed("PPTT node too short")));
    assert!(n[1].is_ok());
    // Helpers stop at the first error.
    assert_eq!(p.processor(1), None);
    // Zero length.
    let b = node(B::new(), 0xa, 0, 0, &[]).bytes(&[0, 0, 0, 0]).0;
    let m = one(*b"PPTT", 3, &b);
    let n: Vec<_> = pptt(&m).nodes().collect();
    assert_eq!(n.len(), 2);
    assert_eq!(n[1], Err(Error::Malformed("PPTT node length")));
    // Unknown types pass through.
    let b = B::new().u8(2).u8(4).u16(0).0;
    let m = one(*b"PPTT", 3, &b);
    let n: Vec<_> = pptt(&m).nodes().collect();
    assert_eq!(
        n,
        [Ok(PpttNode::Other {
            offset: 36,
            kind: 2,
            bytes: &[2, 4, 0, 0]
        })]
    );
    let m = one(*b"PPTX", 3, &b);
    assert_eq!(parse(&Sdt::read(&&m, 0).unwrap()), Err(Error::BadSignature));
}

#[test]
fn survives_corruption() {
    torture(*b"PPTT", 3, &body());
}
