// SPDX-License-Identifier: BSD-2-Clause
//! `cargo xtask deploy --machine x870e|opi6p [--kernel FILE] [--host H] [--reboot] [--dry-run]`:
//! puts a kernel on a reference machine's AbyssBSD disk and has the next boot take it.
//!
//! The machine must be running AbyssBSD from its designated disk. Before anything is written:
//!
//! - `kern.version` says AbyssBSD;
//! - `/` is mounted from `gpt/abyssroot`, and `gpt/abyssroot` and `gpt/abyssefi` are partitions
//!   of one disk (`kern.geom.conftxt`);
//! - that disk's serial number (`camcontrol identify`) is the machine's designated one
//!   (docs/HARDWARE.md: the X870E's media NVMe, the Orange Pi's Samsung 990 PRO);
//! - the machine's AbyssBSD boot entry points at that disk's `abyssefi` partition (its
//!   `efibootmgr -v` device path names the partition's GPT UUID).
//!
//! Then the kernel is copied beside the running one, its SHA-256 checked, the old kernel kept as
//! `/boot/kernel/kernel.prev`, and the new one moved into place; BootNext is set to the AbyssBSD
//! entry (the firmware's default boots another system on both machines). With `--reboot` the
//! machine restarts and the command waits for it to come back with a new boot time and the same
//! kernel hash. `--dry-run` runs the checks only.
//!
//! The kernel is `--kernel FILE`, else a release build for the machine's architecture.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::arch::Arch;
use crate::build::{self, Options};
use crate::cli::Args;
use crate::util::{self, Result};

/// A reference machine AbyssBSD may be deployed to.
#[derive(Clone, Copy, Debug)]
struct Machine {
    name: &'static str,
    arch: Arch,
    /// The last known address (DHCP: it moves; `--host` or `ABYSS_<NAME>_HOST` overrides it).
    host: &'static str,
    /// The serial number of the one disk AbyssBSD may write.
    serial: &'static str,
    /// The firmware boot entry of AbyssBSD on that disk.
    boot_entry: &'static str,
}

const MACHINES: &[Machine] = &[
    Machine {
        name: "x870e",
        arch: Arch::X86_64,
        host: "192.168.0.23",
        // WD PC SN740 256 GB, the media NVMe (HARDWARE.md §5).
        serial: "24413C804394",
        boot_entry: "0003",
    },
    Machine {
        name: "opi6p",
        arch: Arch::Aarch64,
        host: "192.168.0.19",
        // Samsung 990 PRO 1 TB (HARDWARE.md §4); the Crucial boot NVMe is never written.
        serial: "S7LANU0Y715456L",
        boot_entry: "0007",
    },
];

const ROOT_LABEL: &str = "gpt/abyssroot";
const ESP_LABEL: &str = "gpt/abyssefi";

pub(crate) fn cmd(mut args: Args) -> Result<()> {
    let name = args
        .take_value("--machine")?
        .ok_or("--machine x870e|opi6p")?;
    let m = *MACHINES
        .iter()
        .find(|m| m.name == name)
        .ok_or_else(|| format!("unknown machine `{name}` (x870e, opi6p)"))?;
    let kernel = args.take_value("--kernel")?.map(PathBuf::from);
    let env_host = std::env::var(format!("ABYSS_{}_HOST", m.name.to_uppercase())).ok();
    let host = args
        .take_value("--host")?
        .or(env_host)
        .unwrap_or_else(|| m.host.to_owned());
    let known_hosts = args.take_value("--known-hosts")?.map_or_else(
        || {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                .join(".ssh/known_hosts.abyss")
        },
        PathBuf::from,
    );
    let reboot = args.take_flag("--reboot");
    let dry = args.take_flag("--dry-run");
    args.finish()?;

    let t = Target { host, known_hosts };
    println!("deploy: checking {} at {}", m.name, t.host);
    let facts = preflight(&t, &m)?;
    println!(
        "deploy: AbyssBSD on {} (serial {}), root {}, ESP {} (Boot{} points at it)",
        facts.disk, m.serial, facts.root_part, facts.esp_part, m.boot_entry
    );
    if dry {
        println!("deploy: dry run, nothing written");
        return Ok(());
    }

    let kernel = match kernel {
        Some(k) => k,
        None => {
            let opts = Options {
                release: true,
                ktest: false,
            };
            build::all(m.arch, opts)?.kernel
        }
    };
    let hash = sha256_local(&kernel)?;
    println!("deploy: kernel {} ({hash})", kernel.display());
    util::run(
        t.scp(&kernel, "/boot/kernel/kernel.new")
            .stdout(Stdio::null()),
    )?;
    let installed = t.out(&format!(
        "set -e; test \"$(sha256 -q /boot/kernel/kernel.new)\" = {hash}; \
         cp -p /boot/kernel/kernel /boot/kernel/kernel.prev; \
         mv /boot/kernel/kernel.new /boot/kernel/kernel; sync; sha256 -q /boot/kernel/kernel"
    ))?;
    if installed.trim() != hash {
        return Err(format!(
            "the installed kernel hashes to {}, not {hash}",
            installed.trim()
        )
        .into());
    }
    t.out(&format!("efibootmgr -n -b {} > /dev/null", m.boot_entry))?;
    println!(
        "deploy: installed (the previous kernel is /boot/kernel/kernel.prev); BootNext = Boot{}",
        m.boot_entry
    );
    if !reboot {
        return Ok(());
    }
    let before = t.out("sysctl -n kern.boottime")?;
    // The connection may drop before the command returns.
    let _ = t
        .ssh("shutdown -r now")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    println!("deploy: rebooting");
    let start = Instant::now();
    loop {
        std::thread::sleep(Duration::from_secs(10));
        if let Ok(now) = t.out("sysctl -n kern.boottime")
            && now != before
        {
            break;
        }
        if start.elapsed() > Duration::from_secs(600) {
            return Err("the machine did not come back within 10 minutes".into());
        }
    }
    let running = t.out("sysctl -n kern.version | head -1; sha256 -q /boot/kernel/kernel")?;
    println!(
        "deploy: back after {} s: {}",
        start.elapsed().as_secs(),
        running.trim().replace('\n', ", ")
    );
    if !running.contains("AbyssBSD") {
        return Err("the machine came back, but not in AbyssBSD".into());
    }
    Ok(())
}

/// How to reach the machine: root, by key, never a password prompt.
struct Target {
    host: String,
    known_hosts: PathBuf,
}

impl Target {
    fn options(&self, c: &mut Command) {
        c.args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10"])
            .args(["-o", "StrictHostKeyChecking=accept-new"])
            .arg("-o")
            .arg(format!("UserKnownHostsFile={}", self.known_hosts.display()));
    }

    fn ssh(&self, script: &str) -> Command {
        let mut c = Command::new("ssh");
        self.options(&mut c);
        c.arg(format!("root@{}", self.host)).arg(script);
        c.stdin(Stdio::null());
        c
    }

    fn out(&self, script: &str) -> Result<String> {
        util::output(&mut self.ssh(script))
    }

    fn scp(&self, local: &Path, remote: &str) -> Command {
        let mut c = Command::new("scp");
        self.options(&mut c);
        c.arg("-q")
            .arg(local)
            .arg(format!("root@{}:{remote}", self.host));
        c
    }
}

/// What the checks found.
#[derive(Debug, PartialEq)]
struct Facts {
    disk: String,
    root_part: String,
    esp_part: String,
}

fn preflight(t: &Target, m: &Machine) -> Result<Facts> {
    let version = t.out("sysctl -n kern.version")?;
    if !version.contains("AbyssBSD") {
        return Err(format!(
            "{} is not running AbyssBSD (`{}`): boot it first",
            m.name,
            version.lines().next().unwrap_or("")
        )
        .into());
    }
    let root_dev = t.out("df / | tail -1 | cut -d' ' -f1")?;
    if root_dev.trim() != format!("/dev/{ROOT_LABEL}") {
        return Err(format!(
            "/ is mounted from {}, not /dev/{ROOT_LABEL}",
            root_dev.trim()
        )
        .into());
    }
    let conftxt = t.out("sysctl -n kern.geom.conftxt")?;
    let (disk, root_part) = labelled(&conftxt, ROOT_LABEL)
        .ok_or_else(|| format!("no partition labelled {ROOT_LABEL}"))?;
    let (esp_disk, esp_part) = labelled(&conftxt, ESP_LABEL)
        .ok_or_else(|| format!("no partition labelled {ESP_LABEL}"))?;
    if esp_disk != disk {
        return Err(format!("{ROOT_LABEL} is on {disk} but {ESP_LABEL} on {esp_disk}").into());
    }
    let ident = t.out(&format!("camcontrol identify {disk}"))?;
    let got = serial(&ident).ok_or_else(|| format!("no serial number for {disk}"))?;
    if got != m.serial {
        return Err(format!(
            "{disk} has serial {got}, not {} ({}'s AbyssBSD disk): nothing written",
            m.serial, m.name
        )
        .into());
    }
    let list = t.out(&format!("gpart list {disk}"))?;
    let uuid = rawuuid(&list, &esp_part).ok_or_else(|| format!("no GPT UUID for {esp_part}"))?;
    let entries = t.out("efibootmgr -v")?;
    let entry = boot_entry(&entries, m.boot_entry)
        .ok_or_else(|| format!("no Boot{} entry", m.boot_entry))?;
    if !entry.to_lowercase().contains(&uuid.to_lowercase()) {
        return Err(format!(
            "Boot{} does not point at {esp_part} ({uuid}): `{entry}`",
            m.boot_entry
        )
        .into());
    }
    Ok(Facts {
        disk,
        root_part,
        esp_part,
    })
}

/// The disk and partition carrying GEOM label `label`, from `kern.geom.conftxt` (a depth-0
/// `DISK`, its depth-1 `PART`s, each with its depth-2 `LABEL`s).
fn labelled(conftxt: &str, label: &str) -> Option<(String, String)> {
    let (mut disk, mut part) = (None, None);
    for line in conftxt.lines() {
        let w: Vec<&str> = line.split_whitespace().collect();
        match w.as_slice() {
            ["0", "DISK", name, ..] => {
                disk = Some(*name);
                part = None;
            }
            ["1", "PART", name, ..] => part = Some(*name),
            ["2", "LABEL", name, ..] if *name == label => {
                return Some((disk?.to_owned(), part?.to_owned()));
            }
            _ => {}
        }
    }
    None
}

/// The serial number in `camcontrol identify`'s output.
fn serial(ident: &str) -> Option<String> {
    ident.lines().find_map(|l| {
        let l = l.trim();
        let rest = l
            .strip_prefix("Serial Number:")
            .or_else(|| l.strip_prefix("serial number"))?;
        let s = rest.trim();
        (!s.is_empty()).then(|| s.to_owned())
    })
}

/// Partition `part`'s `rawuuid` in `gpart list`'s output.
fn rawuuid(list: &str, part: &str) -> Option<String> {
    let mut current = None;
    for l in list.lines() {
        let l = l.trim();
        if let Some(name) = l.split_once("Name: ").map(|(_, n)| n.trim()) {
            current = Some(name.to_owned());
        } else if let Some(u) = l.strip_prefix("rawuuid: ")
            && current.as_deref() == Some(part)
        {
            return Some(u.trim().to_owned());
        }
    }
    None
}

/// `efibootmgr -v`'s line for entry `num` (`0003`).
fn boot_entry(out: &str, num: &str) -> Option<String> {
    let key = format!("Boot{num}");
    out.lines()
        .map(|l| l.trim_start_matches(['+', ' ']))
        .find(|l| l.starts_with(&key))
        .map(str::to_owned)
}

fn sha256_local(p: &Path) -> Result<String> {
    let out = util::output(Command::new("sha256sum").arg(p))?;
    out.split_whitespace()
        .next()
        .map(str::to_owned)
        .ok_or_else(|| format!("no hash for {}", p.display()).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    // From the Orange Pi 6 Plus under AbyssBSD (2026-11-25), cut down.
    const CONFTXT: &str = "0 DISK nda1 1000204886016 512 hd 0 sc 0
1 PART nda1p2 273804165120 512 i 2 o 537919488 ty freebsd-ufs xs GPT xt 516e7cb6
2 LABEL gptid/07f64321-dd0e-11f1-a3fd-c0742bf94234 273804165120 512 i 0 o 0
2 LABEL gpt/abyssroot 273804165120 512 i 0 o 0
1 PART nda1p1 536870912 512 i 1 o 1048576 ty efi xs GPT xt c12a7328
2 LABEL gpt/abyssefi 536870912 512 i 0 o 0
0 DISK nda0 500107862016 512 hd 0 sc 0
1 PART nda0p3 8589934592 512 i 3 o 0 ty freebsd-swap xs GPT xt 516e7cb5
2 LABEL gpt/swap0 8589934592 512 i 0 o 0
";

    #[test]
    fn labels_find_their_disk_and_partition() {
        assert_eq!(
            labelled(CONFTXT, "gpt/abyssroot"),
            Some(("nda1".into(), "nda1p2".into()))
        );
        assert_eq!(
            labelled(CONFTXT, "gpt/abyssefi"),
            Some(("nda1".into(), "nda1p1".into()))
        );
        assert_eq!(
            labelled(CONFTXT, "gpt/swap0"),
            Some(("nda0".into(), "nda0p3".into()))
        );
        assert_eq!(labelled(CONFTXT, "gpt/nothing"), None);
    }

    #[test]
    fn serial_numbers_are_read() {
        let ident =
            "pass1: <Samsung SSD 990 PRO 1TB> NVMe\nSerial Number:               S7LANU0Y715456L\n";
        assert_eq!(serial(ident).as_deref(), Some("S7LANU0Y715456L"));
        let ata = "protocol              ACS-2\nserial number         BTDA3330088F2403GN\n";
        assert_eq!(serial(ata).as_deref(), Some("BTDA3330088F2403GN"));
        assert_eq!(serial("Serial Number:   \n"), None);
    }

    #[test]
    fn partition_uuids_are_read() {
        let list = "Geom name: nda1
Providers:
1. Name: nda1p1
   rawuuid: 07f3125d-dd0e-11f1-a3fd-c0742bf94234
   label: abyssefi
2. Name: nda1p2
   rawuuid: 07f64321-dd0e-11f1-a3fd-c0742bf94234
";
        assert_eq!(
            rawuuid(list, "nda1p1").as_deref(),
            Some("07f3125d-dd0e-11f1-a3fd-c0742bf94234")
        );
        assert_eq!(
            rawuuid(list, "nda1p2").as_deref(),
            Some("07f64321-dd0e-11f1-a3fd-c0742bf94234")
        );
        assert_eq!(rawuuid(list, "nda1p9"), None);
    }

    #[test]
    fn boot_entries_are_found_with_or_without_the_bootnext_mark() {
        let out = "BootCurrent: 0007
 Boot0005* FreeBSD HD(1,GPT,aaaa,0x800,0x82000)/File(\\EFI\\FREEBSD\\LOADER.EFI)
+Boot0007* AbyssBSD (Samsung) HD(1,GPT,07f3125d-dd0e-11f1-a3fd-c0742bf94234,0x800,0x100000)/File(\\EFI\\BOOT\\BOOTAA64.EFI)
";
        let e = boot_entry(out, "0007").unwrap();
        assert!(e.contains("07f3125d-dd0e-11f1-a3fd-c0742bf94234"));
        assert!(boot_entry(out, "0005").unwrap().starts_with("Boot0005"));
        assert_eq!(boot_entry(out, "0003"), None);
    }
}
