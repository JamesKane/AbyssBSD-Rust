// SPDX-License-Identifier: BSD-2-Clause
//! The mesh: classes, their geoms, and the geoms' consumers and
//! providers, as `kern.geom.confxml`, `conftxt`, and `confdot` show it.
//!
//! It is built in the order FreeBSD creates things. Every list here is
//! one GEOM inserts at its head (`g_load_class`, `g_new_geomf`,
//! `g_new_consumer`, `g_new_providerf`, and a provider's consumers in
//! `g_attach`), so each comes out newest first. Attaching ranks the
//! consumer's geom above the provider's (`g_attach`).
//!
//! Access counts are given where they start (an open device, a mounted
//! file system: [`Mesh::open`]) and passed down to the providers below as
//! each class's `access` method passes them ([`Kind`]); a provider's
//! counts are the sum of its consumers'.
//!
//! Behavior reference: sys/geom/geom_dump.c, sys/geom/geom_subr.c,
//! sys/geom/geom_slice.c (`g_slice_access`), sys/geom/part/g_part.c
//! (`g_part_access`).

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::{self, Write};

use crate::label::escape;

/// Access counts: readers, writers, and exclusive holders.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Access {
    pub r: u32,
    pub w: u32,
    pub e: u32,
}

impl Access {
    /// No access.
    pub const NONE: Self = Self { r: 0, w: 0, e: 0 };

    /// Counts of `r`, `w`, and `e`.
    #[must_use]
    pub const fn new(r: u32, w: u32, e: u32) -> Self {
        Self { r, w, e }
    }

    /// Whether nothing holds it.
    #[must_use]
    pub const fn is_none(self) -> bool {
        self.r == 0 && self.w == 0 && self.e == 0
    }

    fn plus(self, o: Self) -> Self {
        Self {
            r: self.r.saturating_add(o.r),
            w: self.w.saturating_add(o.w),
            e: self.e.saturating_add(o.e),
        }
    }
}

/// `r1w1e1`, as GEOM writes a mode.
impl fmt::Display for Access {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "r{}w{}e{}", self.r, self.w, self.e)
    }
}

/// What a geom's class does with access and configuration: whether it
/// has a `dumpconf` method (so its geoms, consumers, and providers show a
/// `<config>`), and how opens of its providers reach its consumer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// DISK: a driver's disk.
    Disk,
    /// MD: a memory disk.
    Md,
    /// PART: a partition table. Its consumer takes its providers' counts,
    /// and an exclusive count for every writer (`g_part_access`).
    Part,
    /// LABEL: another name for a provider (a slice of all of it). Its
    /// consumer takes its providers' counts, and one exclusive count more
    /// while anything is open (`g_slice_access`).
    Label,
    /// DEV: a provider's `/dev` node; its consumer's counts are the
    /// node's opens.
    Dev,
    /// VFS: a mounted file system; its consumer's counts are the mount's.
    Vfs,
}

impl Kind {
    fn dumpconf(self) -> bool {
        matches!(self, Self::Disk | Self::Md | Self::Part | Self::Label)
    }
}

/// A `dumpconf` line: `<name>value</name>` (the value escaped).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub name: &'static str,
    pub value: String,
}

impl Field {
    /// A line.
    pub fn new(name: &'static str, value: impl fmt::Display) -> Self {
        let mut v = String::new();
        let _ = write!(v, "{value}");
        Self { name, value: v }
    }
}

/// A provider's size and alignment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Media {
    pub mediasize: u64,
    pub sectorsize: u32,
    pub stripesize: u64,
    pub stripeoffset: u64,
}

/// A class in a [`Mesh`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClassRef(usize);
/// A geom in a [`Mesh`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GeomRef(usize);
/// A consumer in a [`Mesh`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConsumerRef(usize);
/// A provider in a [`Mesh`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProviderRef(usize);

#[derive(Debug)]
struct Class {
    id: u64,
    name: &'static str,
    geoms: Vec<usize>,
}

#[derive(Debug)]
struct Geom {
    id: u64,
    class: usize,
    name: String,
    rank: u32,
    kind: Kind,
    config: Vec<Field>,
    consumers: Vec<usize>,
    providers: Vec<usize>,
}

#[derive(Debug)]
struct Consumer {
    id: u64,
    geom: usize,
    provider: usize,
    /// The counts opened through it, for [`Kind::Dev`] and [`Kind::Vfs`].
    own: Access,
}

#[derive(Debug)]
struct Provider {
    id: u64,
    geom: usize,
    name: String,
    aliases: Vec<String>,
    media: Media,
    config: Vec<Field>,
    /// What `dumpconf` adds to the provider's `conftxt` line.
    txt: String,
    consumers: Vec<usize>,
}

/// GEOM's mesh, built in creation order.
#[derive(Debug, Default)]
pub struct Mesh {
    classes: Vec<Class>,
    geoms: Vec<Geom>,
    consumers: Vec<Consumer>,
    providers: Vec<Provider>,
}

impl Mesh {
    /// An empty mesh.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Loads class `name`, with address `id`.
    pub fn add_class(&mut self, id: u64, name: &'static str) -> ClassRef {
        self.classes.insert(
            0,
            Class {
                id,
                name,
                geoms: Vec::new(),
            },
        );
        // Earlier references stay valid: they are stored by position in
        // load order, below.
        ClassRef(self.classes.len() - 1)
    }

    /// The class at `c`, which [`Self::add_class`] numbered in load order.
    fn class_mut(&mut self, c: ClassRef) -> &mut Class {
        let i = self.classes.len() - 1 - c.0;
        &mut self.classes[i]
    }

    /// The class named `name`.
    #[must_use]
    pub fn class(&self, name: &str) -> Option<ClassRef> {
        let n = self.classes.len();
        self.classes
            .iter()
            .position(|c| c.name == name)
            .map(|i| ClassRef(n - 1 - i))
    }

    /// A new geom of class `c`, of rank 1.
    pub fn add_geom(&mut self, c: ClassRef, id: u64, name: &str, kind: Kind) -> GeomRef {
        let g = self.geoms.len();
        self.geoms.push(Geom {
            id,
            class: c.0,
            name: String::from(name),
            rank: 1,
            kind,
            config: Vec::new(),
            consumers: Vec::new(),
            providers: Vec::new(),
        });
        self.class_mut(c).geoms.insert(0, g);
        GeomRef(g)
    }

    /// Sets what the geom's `dumpconf` shows of the geom itself.
    pub fn set_geom_config(&mut self, g: GeomRef, config: Vec<Field>) {
        self.geoms[g.0].config = config;
    }

    /// A new provider of geom `g`.
    pub fn add_provider(&mut self, g: GeomRef, id: u64, name: &str, media: Media) -> ProviderRef {
        let p = self.providers.len();
        self.providers.push(Provider {
            id,
            geom: g.0,
            name: String::from(name),
            aliases: Vec::new(),
            media,
            config: Vec::new(),
            txt: String::new(),
            consumers: Vec::new(),
        });
        self.geoms[g.0].providers.insert(0, p);
        ProviderRef(p)
    }

    /// Gives provider `p` another name (`g_provider_add_alias`).
    pub fn add_alias(&mut self, p: ProviderRef, alias: &str) {
        self.providers[p.0].aliases.insert(0, String::from(alias));
    }

    /// Sets what the provider's geom's `dumpconf` shows of it: its
    /// `confxml` lines and its `conftxt` words.
    pub fn set_provider_config(&mut self, p: ProviderRef, config: Vec<Field>, txt: String) {
        let pp = &mut self.providers[p.0];
        pp.config = config;
        pp.txt = txt;
    }

    /// A new consumer of geom `g`, attached to provider `p`.
    pub fn attach(&mut self, g: GeomRef, id: u64, p: ProviderRef) -> ConsumerRef {
        let c = self.consumers.len();
        self.consumers.push(Consumer {
            id,
            geom: g.0,
            provider: p.0,
            own: Access::NONE,
        });
        self.geoms[g.0].consumers.insert(0, c);
        self.providers[p.0].consumers.insert(0, c);
        let below = self.geoms[self.providers[p.0].geom].rank;
        let geom = &mut self.geoms[g.0];
        if below >= geom.rank {
            geom.rank = below + 1;
        }
        ConsumerRef(c)
    }

    /// Opens consumer `c` (a DEV or VFS geom's) with `a` more.
    pub fn open(&mut self, c: ConsumerRef, a: Access) {
        let cp = &mut self.consumers[c.0];
        cp.own = cp.own.plus(a);
    }

    /// The provider named `name`.
    #[must_use]
    pub fn provider(&self, name: &str) -> Option<ProviderRef> {
        self.providers
            .iter()
            .position(|p| p.name == name)
            .map(ProviderRef)
    }

    /// Provider `p`'s counts: its consumers'.
    #[must_use]
    pub fn provider_access(&self, p: ProviderRef) -> Access {
        self.providers[p.0]
            .consumers
            .iter()
            .fold(Access::NONE, |a, &c| a.plus(self.consumer_access(c)))
    }

    /// Consumer `c`'s counts, as its geom's `access` method passes its
    /// providers' down.
    fn consumer_access(&self, c: usize) -> Access {
        let cp = &self.consumers[c];
        let g = &self.geoms[cp.geom];
        let above = || {
            g.providers.iter().fold(Access::NONE, |a, &p| {
                a.plus(self.provider_access(ProviderRef(p)))
            })
        };
        match g.kind {
            Kind::Part => {
                let a = above();
                cp.own.plus(Access::new(a.r, a.w, a.w.saturating_add(a.e)))
            }
            Kind::Label => {
                let a = above();
                let extra = u32::from(!a.is_none());
                cp.own
                    .plus(Access::new(a.r, a.w, a.e.saturating_add(extra)))
            }
            Kind::Disk | Kind::Md | Kind::Dev | Kind::Vfs => cp.own,
        }
    }

    /// `kern.geom.confxml` (`g_conf_specific`).
    #[must_use]
    pub fn confxml(&self) -> String {
        let mut sb = String::from("<mesh>\n");
        for c in &self.classes {
            let _ = writeln!(sb, "  <class id=\"{:#x}\">", c.id);
            sb.push_str("    <name>");
            escape(&mut sb, c.name);
            sb.push_str("</name>\n");
            for &g in &c.geoms {
                self.xml_geom(&mut sb, g);
            }
            sb.push_str("  </class>\n");
        }
        sb.push_str("</mesh>\n");
        sb
    }

    fn class_id(&self, class: usize) -> u64 {
        self.classes[self.classes.len() - 1 - class].id
    }

    fn class_name(&self, class: usize) -> &'static str {
        self.classes[self.classes.len() - 1 - class].name
    }

    fn xml_fields(sb: &mut String, indent: &str, fields: &[Field]) {
        for f in fields {
            let _ = write!(sb, "{indent}<{}>", f.name);
            escape(sb, &f.value);
            let _ = writeln!(sb, "</{}>", f.name);
        }
    }

    fn xml_geom(&self, sb: &mut String, g: usize) {
        let gp = &self.geoms[g];
        let _ = writeln!(sb, "    <geom id=\"{:#x}\">", gp.id);
        let _ = writeln!(sb, "      <class ref=\"{:#x}\"/>", self.class_id(gp.class));
        sb.push_str("      <name>");
        escape(sb, &gp.name);
        sb.push_str("</name>\n");
        let _ = writeln!(sb, "      <rank>{}</rank>", gp.rank);
        if gp.kind.dumpconf() {
            sb.push_str("      <config>\n");
            Self::xml_fields(sb, "\t", &gp.config);
            sb.push_str("      </config>\n");
        }
        for &c in &gp.consumers {
            let cp = &self.consumers[c];
            let _ = writeln!(sb, "\t<consumer id=\"{:#x}\">", cp.id);
            let _ = writeln!(sb, "\t  <geom ref=\"{:#x}\"/>", gp.id);
            let _ = writeln!(
                sb,
                "\t  <provider ref=\"{:#x}\"/>",
                self.providers[cp.provider].id
            );
            let _ = writeln!(sb, "\t  <mode>{}</mode>", self.consumer_access(c));
            if gp.kind.dumpconf() {
                // No class here shows anything of a consumer.
                sb.push_str("\t  <config>\n\t  </config>\n");
            }
            sb.push_str("\t</consumer>\n");
        }
        for &p in &gp.providers {
            let pp = &self.providers[p];
            let _ = writeln!(sb, "\t<provider id=\"{:#x}\">", pp.id);
            let _ = writeln!(sb, "\t  <geom ref=\"{:#x}\"/>", gp.id);
            let _ = writeln!(
                sb,
                "\t  <mode>{}</mode>",
                self.provider_access(ProviderRef(p))
            );
            sb.push_str("\t  <name>");
            escape(sb, &pp.name);
            sb.push_str("</name>\n");
            for a in &pp.aliases {
                sb.push_str("\t  <alias>");
                escape(sb, a);
                sb.push_str("</alias>\n");
            }
            let m = &pp.media;
            let _ = writeln!(sb, "\t  <mediasize>{}</mediasize>", m.mediasize);
            let _ = writeln!(sb, "\t  <sectorsize>{}</sectorsize>", m.sectorsize);
            let _ = writeln!(sb, "\t  <stripesize>{}</stripesize>", m.stripesize);
            let _ = writeln!(sb, "\t  <stripeoffset>{}</stripeoffset>", m.stripeoffset);
            if gp.kind.dumpconf() {
                sb.push_str("\t  <config>\n");
                Self::xml_fields(sb, "\t    ", &pp.config);
                sb.push_str("\t  </config>\n");
            }
            sb.push_str("\t</provider>\n");
        }
        sb.push_str("    </geom>\n");
    }

    /// `kern.geom.conftxt` (`g_conftxt`): the DISK and MD classes' geoms,
    /// each provider on a line with the geoms that consume it below,
    /// indented a level each.
    #[must_use]
    pub fn conftxt(&self) -> String {
        let mut sb = String::new();
        for c in &self.classes {
            if c.name == "DISK" || c.name == "MD" {
                for &g in &c.geoms {
                    self.txt_geom(&mut sb, g, 0);
                }
            }
        }
        sb
    }

    fn txt_geom(&self, sb: &mut String, g: usize, level: u32) {
        let gp = &self.geoms[g];
        for &p in &gp.providers {
            let pp = &self.providers[p];
            let _ = write!(
                sb,
                "{level} {} {} {} {}",
                self.class_name(gp.class),
                pp.name,
                pp.media.mediasize,
                pp.media.sectorsize
            );
            sb.push_str(&pp.txt);
            sb.push('\n');
            for &c in &pp.consumers {
                self.txt_geom(sb, self.consumers[c].geom, level + 1);
            }
        }
    }

    /// `kern.geom.confdot` (`g_confdot`): the mesh for graphviz.
    #[must_use]
    pub fn confdot(&self) -> String {
        let mut sb = String::from("digraph geom {\n");
        for c in &self.classes {
            for &g in &c.geoms {
                let gp = &self.geoms[g];
                let _ = writeln!(
                    sb,
                    "z{:#x} [shape=box,label=\"{}\\n{}\\nr#{}\"];",
                    gp.id,
                    self.class_name(gp.class),
                    gp.name,
                    gp.rank
                );
                for &cc in &gp.consumers {
                    let cp = &self.consumers[cc];
                    let _ = writeln!(
                        sb,
                        "z{:#x} [label=\"{}\"];",
                        cp.id,
                        self.consumer_access(cc)
                    );
                    let _ = writeln!(
                        sb,
                        "z{:#x} -> z{:#x};",
                        cp.id, self.providers[cp.provider].id
                    );
                    let _ = writeln!(sb, "z{:#x} -> z{:#x};", gp.id, cp.id);
                }
                for &p in &gp.providers {
                    let pp = &self.providers[p];
                    let _ = writeln!(
                        sb,
                        "z{:#x} [shape=hexagon,label=\"{}\\n{}\\nerr#0\\nsector={}\\nstripe={}\"];",
                        pp.id,
                        pp.name,
                        self.provider_access(ProviderRef(p)),
                        pp.media.sectorsize,
                        pp.media.stripesize
                    );
                    let _ = writeln!(sb, "z{:#x} -> z{:#x};", pp.id, gp.id);
                }
            }
        }
        sb.push_str("}\n");
        sb
    }

    /// The names of class `class`'s geoms, newest first, a blank between
    /// (`kern.disks`, for the DISK class: `g_kern_disks`).
    #[must_use]
    pub fn geom_names(&self, class: &str) -> String {
        let mut sb = String::new();
        for c in self.classes.iter().filter(|c| c.name == class) {
            for &g in &c.geoms {
                if !sb.is_empty() {
                    sb.push(' ');
                }
                sb.push_str(&self.geoms[g].name);
            }
        }
        sb
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// A disk with one partition, its LABEL and DEV geoms, and a file
    /// system mounted read-write from the label.
    fn sample() -> Mesh {
        let mut m = Mesh::new();
        let disk = m.add_class(0xffff_ffff_8100_0000, "DISK");
        let part = m.add_class(0xffff_ffff_8100_0100, "PART");
        let label = m.add_class(0xffff_ffff_8100_0200, "LABEL");
        let dev = m.add_class(0xffff_ffff_8100_0300, "DEV");
        let vfs = m.add_class(0xffff_ffff_8100_0400, "VFS");
        let media = Media {
            mediasize: 1 << 20,
            sectorsize: 512,
            stripesize: 0,
            stripeoffset: 0,
        };
        let dg = m.add_geom(disk, 0x10, "vtbd0", Kind::Disk);
        let dp = m.add_provider(dg, 0x11, "vtbd0", media);
        m.set_provider_config(
            dp,
            vec![Field::new("fwheads", 16), Field::new("ident", "A&B")],
            String::from(" hd 16 sc 63"),
        );
        let pg = m.add_geom(part, 0x20, "vtbd0", Kind::Part);
        m.set_geom_config(pg, vec![Field::new("scheme", "GPT")]);
        m.attach(pg, 0x21, dp);
        let devd = m.add_geom(dev, 0x30, "vtbd0", Kind::Dev);
        m.attach(devd, 0x31, dp);
        let pp = m.add_provider(
            pg,
            0x22,
            "vtbd0p1",
            Media {
                mediasize: 1 << 19,
                stripeoffset: 20480,
                ..media
            },
        );
        m.set_provider_config(pp, vec![Field::new("index", 1)], String::from(" i 1"));
        let lg = m.add_geom(label, 0x40, "vtbd0p1", Kind::Label);
        m.attach(lg, 0x41, pp);
        let lp = m.add_provider(
            lg,
            0x42,
            "gpt/root",
            Media {
                mediasize: 1 << 19,
                ..media
            },
        );
        m.set_provider_config(lp, vec![], String::from(" i 0 o 0"));
        let devp = m.add_geom(dev, 0x50, "vtbd0p1", Kind::Dev);
        m.attach(devp, 0x51, pp);
        let devl = m.add_geom(dev, 0x60, "gpt/root", Kind::Dev);
        m.attach(devl, 0x61, lp);
        let vg = m.add_geom(vfs, 0x70, "ufs.gpt/root", Kind::Vfs);
        let vc = m.attach(vg, 0x71, lp);
        m.open(vc, Access::new(1, 1, 1));
        m
    }

    #[test]
    fn access_passes_down_as_each_class_passes_it() {
        let m = sample();
        let p = |n| m.provider_access(m.provider(n).unwrap());
        assert_eq!(p("gpt/root"), Access::new(1, 1, 1));
        // The label's consumer: one more exclusive count.
        assert_eq!(p("vtbd0p1"), Access::new(1, 1, 2));
        // The table's: an exclusive count for the writer too.
        assert_eq!(p("vtbd0"), Access::new(1, 1, 3));
        assert_eq!(alloc::format!("{}", Access::new(1, 1, 3)), "r1w1e3");
    }

    #[test]
    fn xml_as_geom_dump_writes_it() {
        let x = sample().confxml();
        // Newest class first.
        assert!(x.starts_with(
            "<mesh>\n  <class id=\"0xffffffff81000400\">\n    <name>VFS</name>\n    <geom id=\"0x70\">\n      <class ref=\"0xffffffff81000400\"/>\n      <name>ufs.gpt/root</name>\n      <rank>4</rank>\n\t<consumer id=\"0x71\">\n\t  <geom ref=\"0x70\"/>\n\t  <provider ref=\"0x42\"/>\n\t  <mode>r1w1e1</mode>\n\t</consumer>\n    </geom>\n  </class>\n"
        ));
        assert!(x.contains(
            "    <geom id=\"0x20\">\n      <class ref=\"0xffffffff81000100\"/>\n      <name>vtbd0</name>\n      <rank>2</rank>\n      <config>\n\t<scheme>GPT</scheme>\n      </config>\n\t<consumer id=\"0x21\">\n\t  <geom ref=\"0x20\"/>\n\t  <provider ref=\"0x11\"/>\n\t  <mode>r1w1e3</mode>\n\t  <config>\n\t  </config>\n\t</consumer>\n\t<provider id=\"0x22\">\n\t  <geom ref=\"0x20\"/>\n\t  <mode>r1w1e2</mode>\n\t  <name>vtbd0p1</name>\n\t  <mediasize>524288</mediasize>\n\t  <sectorsize>512</sectorsize>\n\t  <stripesize>0</stripesize>\n\t  <stripeoffset>20480</stripeoffset>\n\t  <config>\n\t    <index>1</index>\n\t  </config>\n\t</provider>\n    </geom>\n"
        ));
        // Escaped values; DEV geoms, newest first, with no config.
        assert!(x.contains("\t    <ident>A&#x26;B</ident>\n"));
        assert!(x.contains(
            "    <name>DEV</name>\n    <geom id=\"0x60\">\n      <class ref=\"0xffffffff81000300\"/>\n      <name>gpt/root</name>\n      <rank>4</rank>\n\t<consumer id=\"0x61\">\n"
        ));
        assert!(x.ends_with("  </class>\n</mesh>\n"));
    }

    #[test]
    fn txt_as_geom_dump_writes_it() {
        assert_eq!(
            sample().conftxt(),
            "0 DISK vtbd0 1048576 512 hd 16 sc 63\n\
             1 PART vtbd0p1 524288 512 i 1\n\
             2 LABEL gpt/root 524288 512 i 0 o 0\n"
        );
    }

    #[test]
    fn dot_as_geom_dump_writes_it() {
        let d = sample().confdot();
        assert!(d.starts_with(
            "digraph geom {\nz0x70 [shape=box,label=\"VFS\\nufs.gpt/root\\nr#4\"];\nz0x71 [label=\"r1w1e1\"];\nz0x71 -> z0x42;\nz0x70 -> z0x71;\n"
        ));
        assert!(d.contains(
            "z0x11 [shape=hexagon,label=\"vtbd0\\nr1w1e3\\nerr#0\\nsector=512\\nstripe=0\"];\nz0x11 -> z0x10;\n"
        ));
        assert!(d.ends_with("}\n"));
    }

    #[test]
    fn disks_newest_first() {
        let mut m = Mesh::new();
        let disk = m.add_class(1, "DISK");
        m.add_geom(disk, 2, "nda0", Kind::Disk);
        m.add_geom(disk, 3, "nda1", Kind::Disk);
        assert_eq!(m.geom_names("DISK"), "nda1 nda0");
        assert_eq!(m.geom_names("MD"), "");
    }
}
