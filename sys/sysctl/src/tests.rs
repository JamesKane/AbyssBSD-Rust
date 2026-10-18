// SPDX-License-Identifier: BSD-2-Clause

extern crate std;

use alloc::vec;
use alloc::vec::Vec;

use abi::sysctl::{CTL_AUTO_START, ctlflag, ctltype, hw, kern};

use super::*;

fn sample() -> Tree {
    let mut t = Tree::new();
    let k = t.child(t.root(), "kern").unwrap();
    t.add_value(
        k,
        Number::Fixed(kern::OSTYPE),
        "ostype",
        "Operating system type",
        Value::text("FreeBSD"),
    )
    .unwrap();
    t.add_value(
        k,
        Number::Fixed(kern::OSRELDATE),
        "osreldate",
        "Kernel release date",
        Value::int(1_501_000),
    )
    .unwrap();
    let h = t.child(t.root(), "hw").unwrap();
    t.add_value(
        h,
        Number::Fixed(hw::NCPU),
        "ncpu",
        "Number of active CPUs",
        Value::int(8),
    )
    .unwrap();
    t
}

fn read(t: &Tree, oid: &[i32]) -> Result<Vec<u8>, ReadError> {
    let mut out = Vec::new();
    t.read(oid, &mut out).map(|()| out)
}

#[test]
fn fixed_numbers_and_names() {
    let t = sample();
    assert_eq!(t.name2oid("kern.ostype"), Some(vec![1, 1]));
    assert_eq!(t.name2oid("kern.osreldate"), Some(vec![1, 24]));
    assert_eq!(t.name2oid("hw.ncpu"), Some(vec![6, 3]));
    assert_eq!(t.name(&[6, 3]).as_deref(), Some("hw.ncpu"));
    assert_eq!(t.name2oid("kern.nope"), None);
    assert_eq!(read(&t, &[1, 1]).unwrap(), b"FreeBSD\0");
    assert_eq!(read(&t, &[1, 24]).unwrap(), 1_501_000i32.to_ne_bytes());
    assert_eq!(read(&t, &[1]), Err(ReadError::IsNode));
    assert_eq!(read(&t, &[1, 99]), Err(ReadError::NotFound));
}

#[test]
fn formats_and_descriptions() {
    let t = sample();
    let (kind, fmt) = t.format(&[1, 1]).unwrap();
    assert_eq!(kind & ctltype::MASK, ctltype::STRING);
    assert_ne!(kind & ctlflag::RD, 0);
    assert_eq!(fmt, "A");
    assert_eq!(t.format(&[6, 3]).unwrap().1, "I");
    assert_eq!(t.format(&[1]).unwrap(), (ctltype::NODE | ctlflag::RD, "N"));
    assert_eq!(t.description(&[6, 3]), Some("Number of active CPUs"));
}

#[test]
fn auto_numbers() {
    let mut t = sample();
    let d = t
        .add_node(t.root(), Number::Auto, "dev", "Device tree")
        .unwrap();
    assert_eq!(t.oid(d), Some(vec![CTL_AUTO_START]));
    let s = t
        .add_node(t.root(), Number::Auto, "security", "Security")
        .unwrap();
    assert_eq!(t.oid(s), Some(vec![CTL_AUTO_START + 1]));
    // A removed auto number is not reused.
    assert!(t.remove(s));
    let c = t
        .add_node(t.root(), Number::Auto, "compat", "Compatibility")
        .unwrap();
    assert_eq!(t.oid(c), Some(vec![CTL_AUTO_START + 2]));
    // Fixed numbers below the auto range coexist.
    assert_eq!(
        t.add_node(t.root(), Number::Fixed(1), "other", ""),
        Err(AddError::NumberTaken)
    );
    assert_eq!(
        t.add_node(d, Number::Auto, "dev", ""),
        Ok(t.child(d, "dev").unwrap())
    );
}

#[test]
fn rejects_bad_additions() {
    let mut t = sample();
    let k = t.child(t.root(), "kern").unwrap();
    assert_eq!(
        t.add_node(k, Number::Auto, "ostype", ""),
        Err(AddError::NameTaken)
    );
    assert_eq!(
        t.add_node(k, Number::Auto, "a.b", ""),
        Err(AddError::BadName)
    );
    assert_eq!(t.add_node(k, Number::Auto, "", ""), Err(AddError::BadName));
    let leaf = t.lookup(&[1, 1]).unwrap();
    assert_eq!(
        t.add_node(leaf, Number::Auto, "x", ""),
        Err(AddError::NoParent)
    );
    let mut deep = k;
    for i in 1..CTL_MAXNAME {
        deep = t
            .add_node(deep, Number::Auto, &alloc::format!("d{i}"), "")
            .unwrap();
    }
    assert_eq!(
        t.add_node(deep, Number::Auto, "x", ""),
        Err(AddError::TooDeep)
    );
}

#[test]
fn listing_order() {
    let mut t = sample();
    let d = t.add_node(t.root(), Number::Auto, "dev", "").unwrap();
    let nvme = t.add_node(d, Number::Auto, "nvme", "").unwrap();
    let unit = t.add_node(nvme, Number::Fixed(0), "0", "").unwrap();
    t.add_value(
        unit,
        Number::Auto,
        "%desc",
        "",
        Value::text("Generic NVMe Device"),
    )
    .unwrap();
    let hidden = t
        .add(
            t.child(t.root(), "debug").unwrap(),
            Spec {
                number: Number::Auto,
                name: "hidden",
                description: "",
                flags: ctlflag::SKIP,
                value: Some(Value::int(1)),
            },
        )
        .unwrap();
    let mut seen = Vec::new();
    let mut cur = Vec::new();
    while let Some(next) = t.next(&cur, true) {
        seen.push(t.name(&next).unwrap());
        cur = next;
    }
    assert_eq!(
        seen,
        [
            "kern.ostype",
            "kern.osreldate",
            "hw.ncpu",
            "dev.nvme.0.%desc"
        ]
    );
    // Without honoring SKIP the hidden leaf appears, in number order.
    let mut cur = Vec::new();
    let mut all = Vec::new();
    while let Some(next) = t.next(&cur, false) {
        all.push(next.clone());
        cur = next;
    }
    assert!(all.contains(&t.oid(hidden).unwrap()));
    // Starting from an interior node yields its first leaf.
    assert_eq!(t.next(&[6], true), Some(vec![6, 3]));
    assert_eq!(t.next(&[1, 24], true), Some(vec![6, 3]));
}

#[test]
fn removal_invalidates_ids() {
    let mut t = sample();
    let d = t.add_node(t.root(), Number::Auto, "dev", "").unwrap();
    let n = t.add_node(d, Number::Auto, "nvme", "").unwrap();
    let leaf = t
        .add_value(n, Number::Auto, "x", "", Value::int(3))
        .unwrap();
    assert!(t.remove(n));
    assert!(t.child(d, "nvme").is_none());
    assert!(!t.remove(leaf), "removed with its parent");
    // The slot is reused, but the old id does not reach the new node.
    let fresh = t.add_node(d, Number::Auto, "ahci", "").unwrap();
    assert!(t.oid(n).is_none());
    assert!(t.oid(fresh).is_some());
    assert!(!t.remove(t.root()));
}
