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
        f.table = taste(&m, "md0", &mut f.io(), true, 0, None);
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

    fn provider(&self, name: &str) -> Option<Provider> {
        let media = self.media();
        match name {
            "md0" => Some(Provider { media, part: None }),
            "md0p1" if self.table.is_some() => Some(Provider {
                media,
                part: Some((0, Scheme::Gpt, false)),
            }),
            "md0s1a" => Some(Provider {
                media,
                part: Some((1, Scheme::Bsd, true)),
            }),
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
    assert_eq!(t.scheme, Scheme::Gpt);
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
        f.gpart("create", &[("scheme", "apm")]),
        Err("22 scheme 'apm'".to_string())
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

/// The table a taste finds on `disk` (as a provider of `depth` in a
/// table of scheme `parent`).
fn taste_of(disk: &[u8], depth: u32, parent: Option<Scheme>) -> Option<Table> {
    let mut v = disk.to_vec();
    let mut log = Vec::new();
    let m = Media {
        mediasize: v.len() as u64,
        sectorsize: 512,
        ..Media::default()
    };
    taste(
        &m,
        "md0",
        &mut DiskIo {
            disk: &mut v,
            log: &mut log,
        },
        true,
        depth,
        parent,
    )
}

/// A field's value in a provider's configuration.
fn field<'a>(c: &'a [Field], name: &str) -> Vec<&'a str> {
    c.iter()
        .filter(|f| f.name == name)
        .map(|f| f.value.as_str())
        .collect()
}

#[test]
fn mbr_stick_writes_what_freebsd_writes() {
    let mut f = Fake::new(blank("mbr-fat"), 512);
    assert_eq!(
        f.gpart("create", &[("scheme", "MBR")]).unwrap(),
        "md0 created\n"
    );
    let t = f.table.clone().unwrap();
    assert_eq!(
        (t.scheme, t.first, t.last, t.entries),
        (Scheme::Mbr, 1, 131_071, 4)
    );
    assert_eq!(
        f.gpart(
            "add",
            &[("type", "fat32lba"), ("start", "1"), ("size", "131071")]
        )
        .unwrap(),
        "md0s1 added\n"
    );
    assert_eq!(
        f.gpart("set", &[("attrib", "active"), ("index", "1")])
            .unwrap(),
        "active set on md0s1\n"
    );
    // /boot/mbr, the whole file.
    let code = corpus("mbr-fat")[..512].to_vec();
    let a = alloc::vec![
        Arg::ascii("class", "PART"),
        Arg::ascii("verb", "bootcode"),
        Arg::ascii("flags", "C"),
        Arg::new("bootcode", GCTL_PARAM_RD, &code),
        Arg::new("skip_dsn", GCTL_PARAM_RD, &0i32.to_ne_bytes()),
        Arg::ascii("arg0", "md0"),
        Arg::new("output", GCTL_PARAM_WR | GCTL_PARAM_ASCII, &[0; 64]),
    ];
    assert_eq!(f.issue(a, "bootcode").unwrap(), "bootcode written to md0\n");
    assert_eq!(
        f.disk[..512],
        corpus("mbr-fat")[..512],
        "the MBR FreeBSD wrote"
    );
    // As FreeBSD's dumpconf shows it.
    let t = taste_of(&corpus("mbr-fat"), 0, None).unwrap();
    assert_eq!(
        (t.scheme, t.first, t.last, t.heads, t.sectors),
        (Scheme::Mbr, 1, 131_071, 255, 1)
    );
    let e = t.providers().next().unwrap();
    assert_eq!(e.name(), "s1");
    let (c, txt) = e.provider_config(&t, 512, 67_108_352);
    assert_eq!(field(&c, "type"), ["fat32lba"]);
    assert_eq!(field(&c, "rawtype"), ["12"]);
    assert_eq!(field(&c, "attrib"), ["active"]);
    assert_eq!(field(&c, "offset"), ["512"]);
    assert_eq!(field(&c, "efimedia"), ["HD(1,MBR,0x90909090,0x1,0x1ffff)"]);
    assert_eq!(txt, " i 1 o 512 ty fat32lba xs MBR xt 12");
    // The FAT volume inside is no MBR (it nests in one), nor is FreeBSD's
    // own boot sector with no entries.
    let slice = &corpus("mbr-fat")[512..];
    assert!(taste_of(slice, 1, Some(Scheme::Mbr)).is_none());
}

#[test]
fn mbr_edits_write_what_freebsd_writes() {
    let mut f = Fake::new(blank("mbr-edits"), 512);
    f.gpart("create", &[("scheme", "mbr")]).unwrap();
    for (args, out) in [
        (
            &[("type", "fat32"), ("start", "63"), ("size", "8192")][..],
            Ok("md0s1 added\n"),
        ),
        (
            &[("type", "ntfs"), ("start", "8255"), ("size", "8192")],
            Ok("md0s2 added\n"),
        ),
        (
            &[
                ("type", "linux-data"),
                ("index", "4"),
                ("start", "16447"),
                ("size", "4096"),
            ],
            Ok("md0s4 added\n"),
        ),
        (
            &[("type", "freebsd"), ("start", "20543"), ("size", "44993")],
            Ok("md0s3 added\n"),
        ),
        (
            &[("type", "linux-data"), ("start", "1"), ("size", "2")],
            Err("28 index '5'"),
        ),
    ] {
        assert_eq!(
            f.gpart("add", args),
            out.map(String::from).map_err(String::from),
            "{args:?}"
        );
    }
    f.gpart("delete", &[("index", "2")]).unwrap();
    assert_eq!(
        f.gpart(
            "add",
            &[
                ("type", "ms-basic-data"),
                ("index", "2"),
                ("start", "8255"),
                ("size", "2048")
            ]
        ),
        Err("22".to_string())
    );
    f.gpart("set", &[("attrib", "active"), ("index", "3")])
        .unwrap();
    f.gpart("unset", &[("attrib", "active"), ("index", "3")])
        .unwrap();
    f.gpart("set", &[("attrib", "active"), ("index", "4")])
        .unwrap();
    assert_eq!(
        f.gpart("resize", &[("index", "4"), ("size", "6144")]),
        Err("28 end '22590'".to_string())
    );
    same_as(&f.disk, "mbr-edits");
    // What MBR refuses.
    assert_eq!(
        f.gpart(
            "add",
            &[
                ("type", "fat16"),
                ("start", "8255"),
                ("size", "8"),
                ("label", "x")
            ]
        ),
        Err("22".to_string())
    );
    assert_eq!(
        f.gpart("set", &[("attrib", "bootme"), ("index", "1")]),
        Err("22 attrib 'bootme'".to_string())
    );
    assert_eq!(
        f.gpart("set", &[("attrib", "active")]),
        Err("19 attrib 'active'".to_string())
    );
    assert_eq!(
        f.gpart("recover", &[]).unwrap(),
        "md0 recovering is not needed\n"
    );
    let a = alloc::vec![
        Arg::ascii("class", "PART"),
        Arg::ascii("verb", "bootcode"),
        Arg::new("bootcode", GCTL_PARAM_RD, &[0u8; 446]),
        Arg::ascii("arg0", "md0"),
    ];
    assert_eq!(f.issue(a, "bootcode"), Err("19".to_string()));
    // Entries: exactly four.
    let mut g = Fake::new(blank("mbr-edits"), 512);
    assert_eq!(
        g.gpart("create", &[("scheme", "mbr"), ("entries", "8")]),
        Err("22 entries '8'".to_string())
    );
    // Types: by name, or a number.
    assert_eq!(mbr::parse_type(b"!12"), Ok(12));
    assert_eq!(mbr::parse_type(b"!0x83"), Ok(0x83));
    assert!(mbr::parse_type(b"!256").is_err() && mbr::parse_type(b"!0").is_err());
    assert_eq!(mbr::type_name(0x0f), "ebr");
    assert_eq!(mbr::type_name(0x99), "!153");
    // Destroyed: the first sector wiped.
    f.gpart("destroy", &[("force", "1")]).unwrap();
    assert!(f.disk[..512].iter().all(|&b| b == 0));
}

#[test]
fn bsd_labels_read_as_freebsd_reads_them() {
    let img = corpus("mbr-bsd");
    let t = taste_of(&img, 0, None).unwrap();
    assert_eq!(t.scheme, Scheme::Mbr);
    let s1 = t.providers().next().unwrap();
    assert_eq!(
        (s1.name(), s1.type_name(), s1.start, s1.end),
        ("s1".into(), "freebsd".into(), 1, 131_071)
    );
    // The slice: a label, its partitions relative to `c`'s offset.
    let slice = &img[512..];
    let b = taste_of(slice, 1, Some(Scheme::Mbr)).expect("the slice's label");
    assert_eq!(
        (b.scheme, b.entries, b.first, b.last, b.isleaf),
        (Scheme::Bsd, 8, 0, 131_070, true)
    );
    let parts: Vec<(String, String, u64, u64)> = b
        .providers()
        .map(|e| (e.name(), e.type_name(), e.start, e.end))
        .collect();
    assert_eq!(
        parts,
        [
            ("a".into(), "freebsd-ufs".into(), 0, 81_919),
            ("b".into(), "freebsd-swap".into(), 81_920, 98_303),
            ("d".into(), "freebsd-ufs".into(), 98_304, 131_070),
        ]
    );
    let d = b.providers().nth(2).unwrap();
    let (c, txt) = d.provider_config(&b, 512, 16_776_704);
    assert_eq!(field(&c, "rawtype"), ["7"]);
    assert_eq!(field(&c, "offset"), ["50331648"]);
    assert_eq!(txt, " i 4 o 50331648 ty freebsd-ufs xs BSD xt 7");
    // A label on a whole disk.
    let w = taste_of(&corpus("bsd-whole"), 0, None).unwrap();
    let parts: Vec<(String, u64, u64)> =
        w.providers().map(|e| (e.name(), e.start, e.end)).collect();
    assert_eq!(
        parts,
        [("a".into(), 0, 16_383), ("b".into(), 16_384, 32_767)]
    );
    // Read only: its verbs are refused, and none nests in it.
    let mut f = Fake::new(corpus("bsd-whole"), 512);
    assert_eq!(f.table.as_ref().map(|t| t.scheme), Some(Scheme::Bsd));
    assert_eq!(f.gpart("delete", &[("index", "2")]), Err("45".to_string()));
    assert_eq!(f.opens, 0);
    assert_eq!(
        f.gpart("create", &[("scheme", "bsd")]),
        Err("22 scheme 'bsd'".to_string())
    );
    let a = alloc::vec![
        Arg::ascii("class", "PART"),
        Arg::ascii("verb", "create"),
        Arg::ascii("scheme", "MBR"),
        Arg::ascii("arg0", "md0s1a"),
    ];
    assert_eq!(f.issue(a, "create"), Err("19 provider".to_string()));
}

/// A provider's bytes as [`Io`], without copying them.
struct SliceIo<'a>(&'a [u8]);

impl Io for SliceIo<'_> {
    fn read(&mut self, offset: u64, len: usize) -> Result<Vec<u8>, Errno> {
        let at = usize::try_from(offset).map_err(|_| Errno::EIO)?;
        self.0
            .get(at..at.checked_add(len).ok_or(Errno::EIO)?)
            .map(<[u8]>::to_vec)
            .ok_or(Errno::EIO)
    }

    fn write(&mut self, _: u64, _: &[u8]) -> Result<(), Errno> {
        Err(Errno::EROFS)
    }

    fn log(&mut self, _: &str) {}
}

/// One table a walk found: its provider's name, scheme, first and last
/// sectors, geometry, and how many providers it has.
type Found = (String, Scheme, u64, u64, (u32, u32), usize);

/// Every table on `img` and in its partitions, however deep, tasted as
/// sys/storage tastes them: oldest first (a queue, not a recursion), each
/// partition with its table's depth and scheme, and answering its table's
/// geometry (`g_part_start`). Stops at `limit` levels.
fn walk(img: &[u8], limit: u32) -> Vec<Found> {
    let mut out = Vec::new();
    let mut queue = VecDeque::new();
    queue.push_back((
        String::from("md0"),
        0usize,
        img.len(),
        0u32,
        None,
        (0u32, 0u32),
    ));
    while let Some((name, at, len, depth, parent, fw)) = queue.pop_front() {
        if depth >= limit {
            continue;
        }
        let bytes = &img[at..at + len];
        let m = Media {
            mediasize: len as u64,
            sectorsize: 512,
            fwheads: fw.0,
            fwsectors: fw.1,
            ..Media::default()
        };
        let Some(t) = taste(&m, &name, &mut SliceIo(bytes), true, depth, parent) else {
            continue;
        };
        out.push((
            name.clone(),
            t.scheme,
            t.first,
            t.last,
            (t.heads, t.sectors),
            t.providers().count(),
        ));
        let fw = if fw.0 != 0 && fw.1 != 0 {
            fw
        } else {
            (t.heads, t.sectors)
        };
        for e in t.providers() {
            let (Ok(s), Ok(n)) = (
                usize::try_from(e.start * 512),
                usize::try_from((e.end - e.start + 1) * 512),
            ) else {
                continue;
            };
            if s.checked_add(n).is_some_and(|end| end <= len) {
                queue.push_back((
                    std::format!("{name}{}", e.name()),
                    at + s,
                    n,
                    depth + 1,
                    Some(t.scheme),
                    fw,
                ));
            }
        }
    }
    out
}

#[test]
fn the_boards_disk_nests_as_on_freebsd() {
    // tests/corpus/gpart/board.txt: FreeBSD's gpart show of each level.
    let found = walk(&corpus("board"), 16);
    assert_eq!(
        found,
        [
            ("md0".into(), Scheme::Gpt, 40, 524_247, (64, 9), 5),
            // The vendor partition's FAT boot sector: an MBR with no
            // entries, with the disk table's geometry.
            ("md0p5".into(), Scheme::Mbr, 9, 114_607, (64, 9), 0),
        ]
    );
}

#[test]
fn nesting_stops_where_freebsd_stops() {
    // A slice with a disklabel: MBR, then BSD, then nothing in BSD's.
    let found = walk(&corpus("mbr-bsd"), 16);
    let schemes: Vec<(String, Scheme)> = found.iter().map(|f| (f.0.clone(), f.1)).collect();
    assert_eq!(
        schemes,
        [("md0".into(), Scheme::Mbr), ("md0s1".into(), Scheme::Bsd)]
    );
    // A slice whose first sector is the disk's MBR again (it starts at 1,
    // and sector 1 is a copy of sector 0): no MBR in an MBR.
    let mut img = corpus("mbr-fat");
    let s0 = img[..512].to_vec();
    img[512..1024].copy_from_slice(&s0);
    let found = walk(&img, 16);
    assert_eq!(found.len(), 1, "{found:?}");
    // A GPT inside a slice is not tasted (GPT is a disk's only).
    let gpt = corpus("basic");
    let mut img = alloc::vec![0u8; gpt.len() + 40 * 512];
    img[40 * 512..].copy_from_slice(&gpt);
    let mut f = Fake::new(alloc::vec![0u8; img.len()], 512);
    f.gpart("create", &[("scheme", "mbr")]).unwrap();
    let n = (gpt.len() / 512).to_string();
    f.gpart("add", &[("type", "freebsd"), ("start", "40"), ("size", &n)])
        .unwrap();
    img[..512].copy_from_slice(&f.disk[..512]);
    let found = walk(&img, 16);
    let schemes: Vec<Scheme> = found.iter().map(|f| f.1).collect();
    assert_eq!(schemes, [Scheme::Mbr], "{found:?}");
}

/// A deterministic generator for the garbage test.
fn xorshift(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

#[test]
fn garbage_never_panics_nor_nests_deep() {
    // Disks a taste must survive: random sectors, then random sectors
    // shaped like each scheme's (an MBR's magic and flags, BSD's magic
    // numbers and sector size, GPT's signature), at every depth under
    // every parent.
    let mut seed = 0x5eed_6e0d_u64;
    let mut seen = [0u32; 4];
    for round in 0..3000u32 {
        let sectors = 8 + usize::try_from(xorshift(&mut seed) % 2048).unwrap();
        let mut img = alloc::vec![0u8; sectors * 512];
        let fill = usize::try_from(xorshift(&mut seed) % 24)
            .unwrap()
            .min(sectors);
        for b in &mut img[..fill * 512] {
            *b = u8::try_from(xorshift(&mut seed) & 0xff).unwrap();
        }
        match round % 4 {
            1 => {
                img[510] = 0x55;
                img[511] = 0xaa;
                for i in 0..4 {
                    img[446 + 16 * i] = if xorshift(&mut seed).is_multiple_of(2) {
                        0
                    } else {
                        0x80
                    };
                }
            }
            2 => {
                img[512..516].copy_from_slice(&0x8256_4557u32.to_le_bytes());
                img[512 + 132..512 + 136].copy_from_slice(&0x8256_4557u32.to_le_bytes());
                img[512 + 40..512 + 44].copy_from_slice(&512u32.to_le_bytes());
                img[512 + 44..512 + 48].copy_from_slice(&63u32.to_le_bytes());
                img[512 + 48..512 + 52].copy_from_slice(&16u32.to_le_bytes());
                let n = u32::try_from(sectors).unwrap();
                img[512 + 60..512 + 64].copy_from_slice(&n.to_le_bytes());
                let parts = 8 + u16::try_from(xorshift(&mut seed) % 13).unwrap();
                img[512 + 138..512 + 140].copy_from_slice(&parts.to_le_bytes());
            }
            3 => img[512..520].copy_from_slice(b"EFI PART"),
            _ => {}
        }
        // At most a disk's table, its slices', and theirs.
        let found = walk(&img, 8);
        assert!(
            found.len() <= 1 + 4 + 4 * 20,
            "round {round}: {}",
            found.len()
        );
        for f in &found {
            seen[match f.1 {
                Scheme::Null => 0,
                Scheme::Gpt => 1,
                Scheme::Mbr => 2,
                Scheme::Bsd => 3,
            }] += 1;
        }
        for depth in 0..4 {
            for parent in [
                None,
                Some(Scheme::Gpt),
                Some(Scheme::Mbr),
                Some(Scheme::Bsd),
            ] {
                let m = Media {
                    mediasize: img.len() as u64,
                    sectorsize: 512,
                    ..Media::default()
                };
                let _ = taste(&m, "md0", &mut SliceIo(&img), true, depth, parent);
            }
        }
    }
    // The shaped rounds reached MBR's and BSD's readers (a random GPT
    // header never has its CRCs right).
    assert!(seen[2] > 0 && seen[3] > 0, "{seen:?}");
}
