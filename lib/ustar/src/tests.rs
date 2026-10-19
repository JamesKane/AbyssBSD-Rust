// SPDX-License-Identifier: BSD-2-Clause
//! `testdata/sample.tar` was written by FreeBSD 15.1's bsdtar
//! (`tar --format ustar --uid 0 --gid 0`): a file with two hard links to
//! it, a symlink, directories, and a path long enough to need the prefix
//! field.

extern crate std;

use std::vec::Vec;

use super::*;

const SAMPLE: &[u8] = include_bytes!("testdata/sample.tar");
const DEEP: &[u8] = b"a/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/\
cccccccccccccccccccccccccccccccccccccccccccccccccc/file.txt";

fn path(e: &Entry<'_>) -> Vec<u8> {
    let mut p = Vec::new();
    e.path_parts(|s| p.extend_from_slice(s));
    p
}

#[test]
fn reads_bsdtar_output() {
    let a = Archive::new(SAMPLE);
    let entries: Vec<_> = a.entries().collect::<Result<_, _>>().unwrap();
    let paths: Vec<_> = entries.iter().map(path).collect();
    assert_eq!(
        paths[..5],
        [
            b"rescue".to_vec(),
            b"rescue/sh".to_vec(),
            b"rescue/ls".to_vec(),
            b"rescue/rescue".to_vec(),
            b"shell".to_vec(),
        ]
    );
    assert_eq!(entries[0].kind, Kind::Directory);
    let sh = &entries[1];
    assert_eq!(sh.kind, Kind::File);
    assert_eq!(sh.data, b"#!/bin/sh\necho crunched\n");
    assert_eq!((sh.mode, sh.uid, sh.gid), (0o555, 0, 0));
    assert!(sh.mtime > 1_700_000_000);
    assert_eq!(entries[2].kind, Kind::HardLink(b"rescue/sh"));
    assert_eq!(entries[4].kind, Kind::Symlink(b"rescue/sh"));
    assert!(entries.last().unwrap().is(DEEP));
    assert_eq!(entries.last().unwrap().data, b"deep\n");
}

#[test]
fn find_follows_hard_links() {
    let a = Archive::new(SAMPLE);
    for name in [&b"rescue/sh"[..], b"/rescue/ls", b"./rescue/rescue"] {
        let e = a.find(name).unwrap().unwrap();
        assert_eq!(
            e.kind,
            Kind::File,
            "{}",
            std::string::String::from_utf8_lossy(name)
        );
        assert_eq!(e.data, b"#!/bin/sh\necho crunched\n");
    }
    assert_eq!(a.find(b"rescue/cat").unwrap(), None);
    assert_eq!(
        a.find(b"shell").unwrap().unwrap().kind,
        Kind::Symlink(b"rescue/sh")
    );
    assert_eq!(a.find(b"rescue/").unwrap().unwrap().kind, Kind::Directory);
    assert_eq!(a.find(DEEP).unwrap().unwrap().data, b"deep\n");
}

/// Writes a ustar header (and data) the way the format says.
fn header(name: &[u8], kind: u8, link: &[u8], data: &[u8]) -> Vec<u8> {
    let mut h = [0u8; BLOCK];
    h[..name.len()].copy_from_slice(name);
    h[100..108].copy_from_slice(b"0000644\0");
    h[108..116].copy_from_slice(b"0000000\0");
    h[116..124].copy_from_slice(b"0000000\0");
    h[124..136].copy_from_slice(std::format!("{:011o}\0", data.len()).as_bytes());
    h[136..148].copy_from_slice(b"00000000000\0");
    h[156] = kind;
    h[157..157 + link.len()].copy_from_slice(link);
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    let sum: u32 = h
        .iter()
        .enumerate()
        .map(|(i, &b)| {
            if (148..156).contains(&i) {
                32
            } else {
                u32::from(b)
            }
        })
        .sum();
    h[148..156].copy_from_slice(std::format!("{sum:06o}\0 ").as_bytes());
    let mut out = h.to_vec();
    out.extend_from_slice(data);
    out.resize(out.len().next_multiple_of(BLOCK), 0);
    out
}

fn archive(parts: &[Vec<u8>]) -> Vec<u8> {
    let mut a: Vec<u8> = parts.concat();
    a.extend_from_slice(&[0; 2 * BLOCK]);
    a
}

#[test]
fn malformed_archives_are_errors() {
    let good = archive(&[header(b"f", b'0', b"", b"hello")]);
    assert_eq!(
        Archive::new(&good).find(b"f").unwrap().unwrap().data,
        b"hello"
    );

    let mut bad_sum = good.clone();
    bad_sum[0] = b'g';
    assert_eq!(
        Archive::new(&bad_sum).entries().next(),
        Some(Err(Error::BadChecksum { offset: 0 }))
    );

    // The entries end where the end blocks start.
    assert_eq!(Archive::new(&good).entries_len(), Ok(2 * BLOCK));
    assert_eq!(
        Archive::new(&bad_sum).entries_len(),
        Err(Error::BadChecksum { offset: 0 })
    );

    // Data past the end.
    let short = &good[..BLOCK + 2];
    assert_eq!(
        Archive::new(short).entries().next(),
        Some(Err(Error::Truncated))
    );
    // No end blocks: the iterator reports the truncation, once.
    let mut it = Archive::new(&good[..2 * BLOCK]).entries();
    assert!(matches!(it.next(), Some(Ok(_))));
    assert_eq!(it.next(), Some(Err(Error::Truncated)));
    assert_eq!(it.next(), None);

    // A link to nothing, and a loop.
    let dangling = archive(&[header(b"l", b'1', b"nowhere", b"")]);
    assert_eq!(Archive::new(&dangling).find(b"l"), Err(Error::BadLink));
    let looped = archive(&[header(b"a", b'1', b"b", b""), header(b"b", b'1', b"a", b"")]);
    assert_eq!(Archive::new(&looped).find(b"a"), Err(Error::BadLink));

    // Not ustar.
    let mut v7 = header(b"f", b'0', b"", b"");
    v7[257..263].fill(0);
    let sum: u32 = v7
        .iter()
        .enumerate()
        .map(|(i, &b)| {
            if (148..156).contains(&i) {
                32
            } else {
                u32::from(b)
            }
        })
        .sum();
    v7[148..156].copy_from_slice(std::format!("{sum:06o}\0 ").as_bytes());
    assert_eq!(
        Archive::new(&archive(&[v7])).entries().next(),
        Some(Err(Error::BadHeader { offset: 0 }))
    );
}

#[test]
fn garbage_never_panics() {
    // Deterministic noise over a valid header and over zeros.
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let base = SAMPLE.to_vec();
    for round in 0..2000 {
        let mut a = base.clone();
        for _ in 0..(round % 16 + 1) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let i = usize::try_from(x % a.len() as u64).unwrap();
            a[i] = u8::try_from(x >> 56).unwrap();
        }
        let ar = Archive::new(&a);
        for e in ar.entries() {
            let _ = e;
        }
        let _ = ar.find(b"rescue/ls");
    }
}
