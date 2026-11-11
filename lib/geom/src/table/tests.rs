// SPDX-License-Identifier: BSD-2-Clause
//! The PART class's verbs against what FreeBSD 15.1 wrote: each scenario
//! in `tests/corpus/gpart/` is a disk FreeBSD's `gpart` changed (on a
//! vnode `md` on the build server; `NAME.txt` is its log), kept as its
//! nonzero sectors. The same requests (with the start and size gpart's
//! own autofill chose, and FreeBSD's UUIDs handed out in the order it
//! made them) must leave the same bytes: protective MBR, both headers,
//! both tables, their CRCs, and the sectors wiped.

extern crate std;

use alloc::collections::VecDeque;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use std::path::PathBuf;

use super::*;
use crate::ctl::{Arg, GCTL_PARAM_ASCII, GCTL_PARAM_RD, GCTL_PARAM_WR};

/// A disk image from `tests/corpus/gpart/NAME.sectors`.
fn corpus(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/corpus/gpart")
        .join(std::format!("{name}.sectors"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut lines = text.lines();
    let head = lines.next().unwrap();
    let bytes: usize = head
        .split(' ')
        .nth(2)
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("{head}"));
    let mut disk = alloc::vec![0u8; bytes];
    for l in lines {
        let (lba, hex) = l.split_once(' ').unwrap();
        let at = lba.parse::<usize>().unwrap() * 512;
        for (i, b) in disk[at..at + 512].iter_mut().enumerate() {
            *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap();
        }
    }
    disk
}

/// The GUID at byte `at` of `disk`.
fn guid(disk: &[u8], at: usize) -> Guid {
    guid_at(disk, at)
}

/// A disk with one PART geom at most, named `md0`.
struct Fake {
    disk: Vec<u8>,
    ss: u32,
    table: Option<Table>,
    /// The consumer's access count.
    opens: i32,
    uuids: VecDeque<Guid>,
    busy: Vec<u32>,
    changed: Vec<u32>,
    log: Vec<String>,
    debugflags: u32,
}

impl Fake {
    fn new(disk: Vec<u8>, ss: u32) -> Self {
        let mut f = Self {
            disk,
            ss,
            table: None,
            opens: 0,
            uuids: VecDeque::new(),
            busy: Vec::new(),
            changed: Vec::new(),
            log: Vec::new(),
            debugflags: 0,
        };
        let m = f.media();
        f.table = taste(&m, "md0", &mut f.io(), true);
        f
    }

    fn media(&self) -> Media {
        Media {
            mediasize: self.disk.len() as u64,
            sectorsize: self.ss,
            ..Media::default()
        }
    }

    fn io(&mut self) -> DiskIo<'_> {
        DiskIo {
            disk: &mut self.disk,
            log: &mut self.log,
        }
    }

    /// `gpart VERB ARGS... md0` as gpart sends it: flags `C` unless
    /// given, an output buffer, and the geom. Its output, or its error.
    fn gpart(&mut self, verb: &str, args: &[(&str, &str)]) -> Result<String, String> {
        let mut a = alloc::vec![Arg::ascii("class", "PART"), Arg::ascii("verb", verb)];
        a.push(Arg::new("version", GCTL_PARAM_RD, &0u32.to_ne_bytes()));
        if !args.iter().any(|(n, _)| *n == "flags") && !matches!(verb, "commit" | "undo") {
            a.push(Arg::ascii("flags", "C"));
        }
        for (n, v) in args {
            match *n {
                "index" | "entries" => {
                    let v: i64 = v.parse().unwrap();
                    a.push(Arg::new(n, GCTL_PARAM_RD, &v.to_ne_bytes()));
                }
                "force" => {
                    let v: i32 = v.parse().unwrap();
                    a.push(Arg::new(n, GCTL_PARAM_RD, &v.to_ne_bytes()));
                }
                _ => a.push(Arg::ascii(n, v)),
            }
        }
        a.push(Arg::ascii("arg0", "md0"));
        a.push(Arg::new(
            "output",
            GCTL_PARAM_WR | GCTL_PARAM_ASCII,
            &[0; 4096],
        ));
        self.issue(a, verb)
    }

    fn issue(&mut self, args: Vec<Arg>, verb: &str) -> Result<String, String> {
        let mut req = Request::new(args);
        ctlreq(&mut req, verb.as_bytes(), self);
        if let Some(m) = req.message() {
            return Err(m.to_string());
        }
        let out = req.args.iter().find(|a| a.name == b"output").unwrap();
        Ok(String::from_utf8_lossy(crate::ctl::c_str(&out.value)).into_owned())
    }
}

struct DiskIo<'a> {
    disk: &'a mut Vec<u8>,
    log: &'a mut Vec<String>,
}

impl Io for DiskIo<'_> {
    fn read(&mut self, offset: u64, len: usize) -> Result<Vec<u8>, Errno> {
        let at = usize::try_from(offset).unwrap();
        self.disk
            .get(at..at + len)
            .map(<[u8]>::to_vec)
            .ok_or(Errno::EIO)
    }

    fn write(&mut self, offset: u64, data: &[u8]) -> Result<(), Errno> {
        let at = usize::try_from(offset).unwrap();
        self.disk
            .get_mut(at..at + data.len())
            .ok_or(Errno::EIO)?
            .copy_from_slice(data);
        Ok(())
    }

    fn log(&mut self, line: &str) {
        self.log.push(line.to_string());
    }
}

impl Topology for Fake {
    fn table(&self, geom: &str) -> Option<Table> {
        (geom == "md0").then(|| self.table.clone()).flatten()
    }

    fn store(&mut self, geom: &str, table: Option<Table>) {
        if geom == "md0" {
            self.table = table;
        } else {
            assert!(table.is_none(), "only md0 has a table");
        }
    }

    fn provider(&self, name: &str) -> Option<(Media, Option<u32>)> {
        match name {
            "md0" => Some((self.media(), None)),
            "md0p1" if self.table.is_some() => Some((self.media(), Some(0))),
            _ => None,
        }
    }

    fn access(&mut self, _: &str, open: bool) -> Result<(), Errno> {
        self.opens += if open { 1 } else { -1 };
        assert!(self.opens >= 0);
        Ok(())
    }

    fn busy(&self, _: &str, index: u32) -> bool {
        self.busy.contains(&index)
    }

    fn media_changed(&mut self, _: &str, index: u32) {
        self.changed.push(index);
    }

    fn read(&mut self, _: &str, offset: u64, len: usize) -> Result<Vec<u8>, Errno> {
        self.io().read(offset, len)
    }

    fn write(&mut self, _: &str, offset: u64, data: &[u8]) -> Result<(), Errno> {
        self.io().write(offset, data)
    }

    fn uuidgen(&mut self) -> Guid {
        self.uuids.pop_front().unwrap_or(Guid::NIL)
    }

    fn debugflags(&self) -> u32 {
        self.debugflags
    }

    fn check_integrity(&self) -> bool {
        true
    }

    fn log(&mut self, line: &str) {
        self.log.push(line.to_string());
    }
}

/// Fails on the first sector that differs from FreeBSD's.
fn same_as(got: &[u8], name: &str) {
    let want = corpus(name);
    assert_eq!(got.len(), want.len());
    for (i, (g, w)) in got.chunks(512).zip(want.chunks(512)).enumerate() {
        assert!(
            g == w,
            "{name}: sector {i} differs:\n got {:02x?}\nwant {:02x?}",
            g,
            w
        );
    }
}

/// The disk GUID and the entry GUIDs (by index) of a FreeBSD image with
/// sectors of `ss` bytes.
fn uuids(name: &str, ss: usize) -> (Guid, Vec<Guid>) {
    let d = corpus(name);
    let disk = guid(&d, ss + 56);
    let entries = (0..8).map(|i| guid(&d, 2 * ss + 128 * i + 16)).collect();
    (disk, entries)
}

fn blank(name: &str) -> Vec<u8> {
    alloc::vec![0u8; corpus(name).len()]
}

#[test]
fn basic_sequence_writes_what_freebsd_writes() {
    let (disk, ents) = uuids("basic", 512);
    let mut f = Fake::new(blank("basic"), 512);
    assert!(f.table.is_none());
    f.uuids.extend([disk, ents[0], ents[1]]);
    assert_eq!(
        f.gpart("create", &[("scheme", "gpt")]).unwrap(),
        "md0 created\n"
    );
    let t = f.table.clone().unwrap();
    assert_eq!(
        (t.first, t.last, t.entries, t.opened),
        (40, 131_031, 128, false)
    );
    assert_eq!((t.heads, t.sectors), (255, 1));
    assert_eq!(f.opens, 0, "committed and closed");
    assert_eq!(
        f.gpart(
            "add",
            &[
                ("type", "efi"),
                ("start", "40"),
                ("size", "81920"),
                ("label", "efiboot")
            ]
        )
        .unwrap(),
        "md0p1 added\n"
    );
    assert_eq!(
        f.gpart(
            "add",
            &[
                ("type", "freebsd-ufs"),
                ("start", "81960"),
                ("size", "49072"),
                ("label", "test")
            ]
        )
        .unwrap(),
        "md0p2 added\n"
    );
    assert_eq!(
        f.gpart("set", &[("attrib", "bootme"), ("index", "2")])
            .unwrap(),
        "bootme set on md0p2\n"
    );
    assert_eq!(
        f.gpart("set", &[("attrib", "bootonce"), ("index", "1")])
            .unwrap(),
        "bootonce set on md0p1\n"
    );
    assert_eq!(
        f.gpart("unset", &[("attrib", "bootme"), ("index", "1")])
            .unwrap(),
        "bootme unset on md0p1\n"
    );
    // /boot/pmbr, as gpart reads it: the whole file.
    let pmbr = corpus("basic")[..512].to_vec();
    let mut a = alloc::vec![
        Arg::ascii("class", "PART"),
        Arg::ascii("verb", "bootcode"),
        Arg::ascii("flags", "C"),
        Arg::new("bootcode", GCTL_PARAM_RD, &pmbr),
        Arg::new("skip_dsn", GCTL_PARAM_RD, &0i32.to_ne_bytes()),
        Arg::ascii("arg0", "md0"),
    ];
    a.push(Arg::new(
        "output",
        GCTL_PARAM_WR | GCTL_PARAM_ASCII,
        &[0; 64],
    ));
    assert_eq!(f.issue(a, "bootcode").unwrap(), "bootcode written to md0\n");
    assert_eq!(
        f.gpart("set", &[("attrib", "active")]).unwrap(),
        "active set on md0\n"
    );
    same_as(&f.disk, "basic");
    // Modified entries are tasted again once committed.
    assert_eq!(f.changed, [2, 1, 1]);
    // And it reads back as it was written.
    let t = Fake::new(f.disk.clone(), 512).table.unwrap();
    let live: Vec<(u32, u64, u64, u64)> = t
        .live()
        .map(|e| (e.index, e.start, e.end, e.ent.attr))
        .collect();
    assert_eq!(
        live,
        [
            (1, 40, 81959, ATTR_BOOTONCE),
            (2, 81960, 131_031, ATTR_BOOTME)
        ]
    );
    assert_eq!(t.list[0].type_name(), "efi");
    assert_eq!(crate::label::utf16_name(&t.list[1].ent.name), "test");
    assert!(!t.corrupt);
}

#[test]
fn edits_write_what_freebsd_writes() {
    let (disk, ents) = uuids("edits", 512);
    let mut f = Fake::new(blank("edits"), 512);
    f.uuids.extend([disk, ents[0], ents[1], Guid::NIL, ents[2]]);
    f.gpart("create", &[("scheme", "GPT"), ("entries", "4")])
        .unwrap();
    let t = f.table.clone().unwrap();
    assert_eq!((t.first, t.last), (8, 9991));
    f.gpart(
        "add",
        &[("type", "freebsd-swap"), ("start", "100"), ("size", "1000")],
    )
    .unwrap();
    f.gpart(
        "add",
        &[
            ("type", "freebsd-ufs"),
            ("start", "2048"),
            ("size", "6144"),
            ("label", "data"),
        ],
    )
    .unwrap();
    assert_eq!(
        f.gpart(
            "add",
            &[
                ("type", "!0fc63daf-8483-4772-8e79-3d69d8477de4"),
                ("index", "4"),
                ("start", "1100"),
                ("size", "200")
            ]
        )
        .unwrap(),
        "md0p4 added\n"
    );
    assert_eq!(
        f.gpart(
            "add",
            &[("type", "freebsd-zfs"), ("start", "8192"), ("size", "1800")]
        )
        .unwrap(),
        "md0p3 added\n"
    );
    // Full: as FreeBSD says it.
    assert_eq!(
        f.gpart(
            "add",
            &[("type", "freebsd-zfs"), ("start", "1300"), ("size", "748")]
        ),
        Err("28 index '5'".to_string())
    );
    f.gpart("resize", &[("index", "1"), ("size", "500")])
        .unwrap();
    assert_eq!(
        f.gpart(
            "modify",
            &[
                ("index", "2"),
                ("label", "renamed label"),
                ("type", "freebsd-vinum")
            ]
        )
        .unwrap(),
        "md0p2 modified\n"
    );
    f.gpart("delete", &[("index", "4")]).unwrap();
    assert_eq!(
        f.gpart(
            "add",
            &[
                ("type", "ms-basic-data"),
                ("index", "3"),
                ("start", "1100"),
                ("size", "64")
            ]
        ),
        Err("17 index '3'".to_string())
    );
    f.gpart("resize", &[("index", "2"), ("size", "6144")])
        .unwrap();
    assert_eq!(
        f.gpart("set", &[("attrib", "lenovofix")]).unwrap(),
        "lenovofix set on md0\n"
    );
    same_as(&f.disk, "edits");
}

#[test]
fn four_kilobyte_sectors() {
    let (disk, ents) = uuids("4kn", 4096);
    let mut f = Fake::new(blank("4kn"), 4096);
    f.uuids.extend([disk, ents[0], ents[1]]);
    f.gpart("create", &[("scheme", "gpt")]).unwrap();
    let t = f.table.clone().unwrap();
    assert_eq!((t.first, t.last), (6, 16378));
    f.gpart(
        "add",
        &[("type", "freebsd-boot"), ("start", "6"), ("size", "128")],
    )
    .unwrap();
    f.gpart(
        "add",
        &[
            ("type", "freebsd-ufs"),
            ("start", "134"),
            ("size", "16245"),
            ("label", "root"),
        ],
    )
    .unwrap();
    same_as(&f.disk, "4kn");
}

#[test]
fn destroy_wipes_what_freebsd_wipes() {
    let mut f = Fake::new(blank("destroy"), 512);
    f.gpart("create", &[("scheme", "gpt")]).unwrap();
    f.gpart(
        "add",
        &[("type", "freebsd-ufs"), ("start", "40"), ("size", "16304")],
    )
    .unwrap();
    assert_eq!(f.gpart("destroy", &[("force", "0")]), Err("16".to_string()));
    // Open, it stays even with -F.
    f.busy.push(1);
    assert_eq!(f.gpart("destroy", &[("force", "1")]), Err("16".to_string()));
    f.busy.clear();
    assert_eq!(
        f.gpart("destroy", &[("force", "1")]).unwrap(),
        "md0 destroyed\n"
    );
    assert!(f.table.is_none(), "committed: the geom withered");
    assert_eq!(f.opens, 0);
    f.gpart("create", &[("scheme", "gpt")]).unwrap();
    f.gpart("destroy", &[("force", "0")]).unwrap();
    same_as(&f.disk, "destroy");
}

#[test]
fn recover_puts_back_what_freebsd_puts_back() {
    let mut f = Fake::new(corpus("recover-broken"), 512);
    let t = f.table.clone().expect("tasted from the primary");
    assert!(t.corrupt);
    assert_eq!(
        f.log,
        [
            "GEOM: md0: the secondary GPT table is corrupt or invalid.",
            "GEOM: md0: using the primary only -- recovery suggested."
        ]
    );
    assert_eq!(
        f.gpart(
            "add",
            &[
                ("type", "freebsd-swap"),
                ("start", "8232"),
                ("size", "8112")
            ]
        ),
        Err("1 table 'md0' is corrupt".to_string())
    );
    assert_eq!(f.opens, 0);
    assert_eq!(f.gpart("recover", &[]).unwrap(), "md0 recovered\n");
    assert!(!f.table.as_ref().unwrap().corrupt);
    same_as(&f.disk, "recover");
    assert_eq!(
        f.gpart("recover", &[]).unwrap(),
        "md0 recovering is not needed\n"
    );
}

#[test]
fn undo_and_commit_by_hand() {
    let (disk, ents) = uuids("undo", 512);
    let mut f = Fake::new(blank("undo"), 512);
    f.uuids.extend([disk, Guid::NIL, ents[0]]);
    f.gpart("create", &[("scheme", "gpt")]).unwrap();
    f.gpart(
        "add",
        &[
            ("type", "freebsd-ufs"),
            ("start", "40"),
            ("size", "2048"),
            ("flags", "x"),
        ],
    )
    .unwrap();
    let t = f.table.clone().unwrap();
    assert!(t.opened && t.list[0].created);
    assert_eq!(f.opens, 1, "held open until committed");
    // Nothing written yet.
    assert!(f.disk[2 * 512..3 * 512].iter().all(|&b| b == 0));
    f.gpart("undo", &[]).unwrap();
    let t = f.table.clone().unwrap();
    assert!(!t.opened && t.list.is_empty());
    assert_eq!(f.opens, 0);
    f.gpart(
        "add",
        &[
            ("type", "freebsd-ufs"),
            ("start", "40"),
            ("size", "4096"),
            ("label", "later"),
            ("flags", "x"),
        ],
    )
    .unwrap();
    f.gpart("commit", &[]).unwrap();
    assert_eq!(f.gpart("commit", &[]), Err("1".to_string()));
    same_as(&f.disk, "undo");
}

#[test]
fn refusals() {
    let mut f = Fake::new(blank("undo"), 512);
    // No table yet: the geom is not there.
    assert_eq!(
        f.gpart("add", &[("type", "efi"), ("start", "40"), ("size", "8")]),
        Err("22 arg0 'md0'".to_string())
    );
    assert_eq!(
        f.gpart("create", &[("scheme", "mbr")]),
        Err("22 scheme 'mbr'".to_string())
    );
    assert_eq!(f.gpart("frob", &[]), Err("22 verb 'frob'".to_string()));
    f.gpart("create", &[("scheme", "gpt")]).unwrap();
    assert_eq!(
        f.gpart("create", &[("scheme", "gpt")]),
        Err("17 geom 'md0'".to_string())
    );
    for (args, err) in [
        (
            &[("type", "efi"), ("start", "39"), ("size", "8")][..],
            "22 start '39'",
        ),
        (
            &[("type", "efi"), ("start", "40"), ("size", "0")],
            "22 size '0'",
        ),
        (
            &[("type", "efi"), ("start", "16343"), ("size", "2")],
            "22 size '2'",
        ),
        (
            &[("type", "efi"), ("start", "-1"), ("size", "2")],
            "22 start '-1'",
        ),
        (
            &[("type", "efi"), ("start", "4x"), ("size", "2")],
            "22 start '4x'",
        ),
        (&[("type", "fat32"), ("start", "40"), ("size", "2")], "22"),
        (
            &[
                ("type", "!00000000-0000-0000-0000-000000000000"),
                ("start", "40"),
                ("size", "2"),
            ],
            "22",
        ),
        (
            &[
                ("type", "efi"),
                ("start", "40"),
                ("size", "2"),
                ("index", "129"),
            ],
            "22 index '129'",
        ),
        (&[("type", "efi"), ("size", "2")], "87 param '200'"),
    ] {
        assert_eq!(f.gpart("add", args), Err(err.to_string()), "{args:?}");
    }
    assert_eq!(f.opens, 0, "closed after each failure");
    // Hexadecimal and octal, as strtoq reads them.
    f.gpart(
        "add",
        &[("type", "efi"), ("start", "0x28"), ("size", "010")],
    )
    .unwrap();
    for (args, err) in [
        (
            &[("type", "efi"), ("start", "47"), ("size", "2")][..],
            "28 start '47'",
        ),
        (
            &[("type", "efi"), ("start", "30"), ("size", "12")][..],
            "22 start '30'",
        ),
        (
            &[
                ("type", "efi"),
                ("start", "48"),
                ("size", "8"),
                ("index", "1"),
            ],
            "17 index '1'",
        ),
    ] {
        assert_eq!(f.gpart("add", args), Err(err.to_string()), "{args:?}");
    }
    f.gpart("add", &[("type", "efi"), ("start", "100"), ("size", "8")])
        .unwrap();
    assert_eq!(
        f.gpart("add", &[("type", "efi"), ("start", "90"), ("size", "30")]),
        Err("28 size '30'".to_string())
    );
    assert_eq!(
        f.gpart("add", &[("type", "efi"), ("start", "90"), ("size", "11")]),
        Err("28 end '100'".to_string())
    );
    // Busy partitions are not deleted, nor shrunk; grown, they are.
    f.busy.push(1);
    assert_eq!(f.gpart("delete", &[("index", "1")]), Err("16".to_string()));
    assert_eq!(
        f.gpart("resize", &[("index", "1"), ("size", "4")]),
        Err("16".to_string())
    );
    f.gpart("resize", &[("index", "1"), ("size", "16")])
        .unwrap();
    f.debugflags = G_F_FOOTSHOOTING;
    f.gpart("resize", &[("index", "1"), ("size", "4")]).unwrap();
    assert_eq!(
        f.gpart("resize", &[("index", "1"), ("size", "61")]),
        Err("28 end '100'".to_string())
    );
    assert_eq!(
        f.gpart("delete", &[("index", "9")]),
        Err("2 index '9'".to_string())
    );
    assert_eq!(
        f.gpart("set", &[("attrib", "frob"), ("index", "1")]),
        Err("22 attrib 'frob'".to_string())
    );
    assert_eq!(
        f.gpart("set", &[("attrib", "bootme")]),
        Err("19 attrib 'bootme'".to_string())
    );
    assert_eq!(
        f.gpart("set", &[("attrib", "active"), ("index", "1")]),
        Err("6 attrib 'active'".to_string())
    );
    assert_eq!(
        f.gpart("move", &[("index", "1")]),
        Err("78 verb 'move'".to_string())
    );
    let big = alloc::vec![0u8; 513];
    let a = alloc::vec![
        Arg::ascii("class", "PART"),
        Arg::ascii("verb", "bootcode"),
        Arg::new("bootcode", GCTL_PARAM_RD, &big),
        Arg::ascii("arg0", "md0"),
    ];
    assert_eq!(f.issue(a, "bootcode"), Err("27".to_string()));
    // GPT does not nest.
    let a = alloc::vec![
        Arg::ascii("class", "PART"),
        Arg::ascii("verb", "create"),
        Arg::ascii("scheme", "GPT"),
        Arg::ascii("arg0", "/dev/md0p1"),
    ];
    assert_eq!(f.issue(a, "create"), Err("6 provider".to_string()));
}

#[test]
fn labels_in_utf16() {
    let n = utf8_to_utf16("été".as_bytes());
    assert_eq!(&n[..4], &[0xe9, 0x74, 0xe9, 0]);
    let n = utf8_to_utf16("\u{1f600}".as_bytes());
    assert_eq!(&n[..3], &[0xd83d, 0xde00, 0]);
    // Malformed: a lone continuation byte, and a sequence cut short.
    assert_eq!(&utf8_to_utf16(b"\x80a")[..2], &[0xfffd, 0x61]);
    assert_eq!(&utf8_to_utf16(b"a\xc3")[..3], &[0x61, 0xfffd, 0]);
    // Truncated to 36 units; a pair that does not fit becomes a NUL.
    let long = "x".repeat(40);
    assert!(utf8_to_utf16(long.as_bytes()).iter().all(|&u| u == 0x78));
    let mut s = "x".repeat(35);
    s.push('\u{1f600}');
    assert_eq!(utf8_to_utf16(s.as_bytes())[35], 0);
}

#[test]
fn numbers_as_strtoq_reads_them() {
    assert_eq!(strtoq(b"40"), Some(40));
    assert_eq!(strtoq(b" +0x10"), Some(16));
    assert_eq!(strtoq(b"017"), Some(15));
    assert_eq!(strtoq(b"0"), Some(0));
    assert_eq!(strtoq(b"08"), None);
    assert_eq!(strtoq(b"0x"), None);
    assert_eq!(strtoq(b""), None);
    assert_eq!(strtoq(b"-5"), Some(-5));
    assert_eq!(strtoq(b"99999999999999999999"), Some(i64::MAX));
}
