// SPDX-License-Identifier: BSD-2-Clause
//! What `netstat -m` reads through `libmemstat`: `vm.zone_stats` with the
//! mbuf zones, and `kern.malloc_stats` with the one malloc type it asks
//! for. Zones count in totals here, so the streams hold no per-CPU records.
//!
//! Behavior reference: sys/vm/uma_core.c (`sysctl_vm_zone_stats`),
//! sys/kern/kern_malloc.c (`sysctl_kern_malloc_stats`),
//! lib/libmemstat/memstat_uma.c.

use alloc::boxed::Box;
use alloc::vec::Vec;

use abi::memstat::{
    MALLOC_TYPE_STREAM_VERSION, MallocTypeHeader, MallocTypeStreamHeader, NAME_MAX, Sfstat,
    UMA_STREAM_VERSION, UmaStreamHeader, UmaTypeHeader,
};
use mbuf::zone;
use sysctl::{Number, Tree, Value};

use crate::uaccess::bytes_of;

fn name_of(n: &str) -> [u8; NAME_MAX] {
    let mut b = [0; NAME_MAX];
    b[..n.len()].copy_from_slice(n.as_bytes());
    b
}

/// One record: zone `name`'s counts (`mbuf_packet`, made of an mbuf and a
/// cluster at once, has none here: nothing allocates that pair).
fn record(name: &str, s: zone::Stats) -> UmaTypeHeader {
    UmaTypeHeader {
        uth_name: name_of(name),
        uth_size: s.size,
        uth_rsize: s.size,
        uth_limit: u32::try_from(s.limit).unwrap_or(0),
        uth_allocs: s.used,
        uth_fails: s.fails,
        ..UmaTypeHeader::default()
    }
}

/// The mbuf zones, then the heap's (ADR-0024: `Files`, `pipe`, ...), each
/// with its objects out of its slabs as the allocations.
fn zones() -> Vec<UmaTypeHeader> {
    let r = |z: &zone::Zone| record(z.name(), z.stats());
    let mut v = alloc::vec![
        r(&zone::MBUF),
        r(&zone::CLUSTER),
        record("mbuf_packet", zone::Stats::default()),
        r(&zone::JUMBOP),
        r(&zone::JUMBO9),
        r(&zone::JUMBO16),
    ];
    for z in crate::mm::zone_stats() {
        v.push(UmaTypeHeader {
            uth_name: name_of(z.name),
            uth_size: u32::try_from(z.size).unwrap_or(0),
            uth_rsize: u32::try_from(z.size).unwrap_or(0),
            uth_allocs: z.used,
            ..UmaTypeHeader::default()
        });
    }
    v
}

fn zone_stats(out: &mut Vec<u8>) {
    let z = zones();
    let h = UmaStreamHeader {
        ush_version: UMA_STREAM_VERSION,
        ush_count: u32::try_from(z.len()).unwrap_or(0),
        ..UmaStreamHeader::default()
    };
    out.extend_from_slice(bytes_of(&h));
    for r in &z {
        out.extend_from_slice(bytes_of(r));
    }
}

fn malloc_stats(out: &mut Vec<u8>) {
    let h = MallocTypeStreamHeader {
        mtsh_version: MALLOC_TYPE_STREAM_VERSION,
        mtsh_count: 1,
        ..MallocTypeStreamHeader::default()
    };
    out.extend_from_slice(bytes_of(&h));
    // No `m_tag`s here: `mbuf_tag`, with nothing in it.
    let t = MallocTypeHeader {
        mth_name: name_of("mbuf_tag"),
    };
    out.extend_from_slice(bytes_of(&t));
}

/// `vm.zone_count`, `vm.zone_stats`, `vm.malloc.*`, `kern.malloc_count`,
/// `kern.malloc_stats`, and `kern.ipc.sfstat`.
pub(crate) fn nodes(t: &mut Tree, vm: sysctl::NodeId, kern: sysctl::NodeId, ipc: sysctl::NodeId) {
    t.add_value(
        vm,
        Number::Auto,
        "zone_count",
        "Number of UMA zones",
        Value::Int(Box::new(|| i32::try_from(zones().len()).unwrap_or(0))),
    )
    .expect("vm.zone_count");
    t.add_value(
        vm,
        Number::Auto,
        "zone_stats",
        "Zone Stats",
        Value::Opaque {
            format: "s,uma_type_header",
            read: Box::new(zone_stats),
        },
    )
    .expect("vm.zone_stats");
    let m = t
        .add_node(vm, Number::Auto, "malloc", "Malloc information")
        .expect("vm.malloc");
    t.add_value(
        m,
        Number::Auto,
        "zone_count",
        "Number of malloc zones",
        Value::int(0),
    )
    .expect("vm.malloc.zone_count");
    t.add_value(
        m,
        Number::Auto,
        "zone_sizes",
        "Zone sizes used by malloc",
        Value::Opaque {
            format: "S",
            read: Box::new(|_| {}),
        },
    )
    .expect("vm.malloc.zone_sizes");
    t.add_value(
        kern,
        Number::Auto,
        "malloc_count",
        "Count of kernel malloc types",
        Value::int(1),
    )
    .expect("kern.malloc_count");
    t.add_value(
        kern,
        Number::Auto,
        "malloc_stats",
        "Return malloc types",
        Value::Opaque {
            format: "s,malloc_type_ustats",
            read: Box::new(malloc_stats),
        },
    )
    .expect("kern.malloc_stats");
    t.add_value(
        ipc,
        Number::Auto,
        "sfstat",
        "sendfile statistics",
        Value::Opaque {
            format: "S,sfstat",
            read: Box::new(|out| {
                let s: Sfstat = crate::syscall::sendfile::stats();
                out.extend_from_slice(bytes_of(&s));
            }),
        },
    )
    .expect("kern.ipc.sfstat");
}
