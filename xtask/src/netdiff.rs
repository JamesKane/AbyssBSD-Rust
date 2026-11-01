// SPDX-License-Identifier: BSD-2-Clause
//! Differential captures (docs/NETWORKING.md §16.3, ROADMAP P7.0): a
//! scenario from `tests/net/scenarios/` runs on a guest on the build
//! server's test bridge, with the server as the peer, while `tcpdump`
//! records the guest's tap. The capture, decoded by `tcpdump` and
//! normalized ([`normalize`]), is compared line by line with FreeBSD
//! 15.1's, recorded in `tests/net/freebsd/`.
//!
//! A scenario file holds a first comment line saying what it exchanges,
//! then `key: value` lines: `filter` (the capture's `pcap` filter),
//! `guest` (a command run on the guest's console), and optionally `peer`
//! (a command started on the server first, in the background), `raw` (a
//! command run first on the scripted peer, [`RAW_PEER`]), `ports`
//! (the fixed ports, kept when ephemeral ones are blanked), and `vary`
//! (what else differs between FreeBSD's runs: `peer-window`, the window a
//! peer advertises, which races with its application's reads).
//!
//! Normalization leaves what a peer can rely on and removes what differs
//! from run to run on FreeBSD too: addresses and MACs become roles
//! (`GUEST`, `PEER`, ...), and IP IDs, checksums, initial sequence
//! numbers, timestamp values, ICMP identifiers, IPv6 flow labels, DHCP
//! transaction IDs, and ephemeral ports become `_`. FreeBSD against itself is its first test.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use crate::arch::Arch;
use crate::cli::Args;
use crate::expect::{PROMPT, Session};
use crate::qemu::{Net, Tap, Vm, Vmm};
use crate::util::{self, Result};

/// The scripted peer's address: a second FreeBSD 15.1 machine on the test
/// bridge, with Python and scapy, that plays a TCP end the way a script in
/// `tests/net/peers/` says (dropping a segment, reporting a smaller MTU),
/// its own kernel dropping what it has no socket for
/// (`net.inet.tcp.blackhole=2`). Outside the bridge's DHCP range.
const RAW_PEER: &str = "10.77.0.2";

/// Bumped whenever [`peer_disk`]'s kept image changes.
const PEER_PREP_VERSION: &str = "1";

/// One scenario, parsed.
#[derive(Debug, PartialEq)]
struct Scenario {
    name: String,
    what: String,
    filter: String,
    guest: String,
    peer: Option<String>,
    raw: Option<String>,
    ports: Vec<u16>,
    /// `vary: peer-window`.
    peer_window_varies: bool,
}

fn parse(name: &str, text: &str) -> Result<Scenario> {
    let mut s = Scenario {
        name: name.to_owned(),
        what: String::new(),
        filter: String::new(),
        guest: String::new(),
        peer: None,
        raw: None,
        ports: Vec::new(),
        peer_window_varies: false,
    };
    for line in text.lines() {
        if let Some(c) = line.strip_prefix('#') {
            if !s.what.is_empty() {
                s.what.push_str("\n# ");
            }
            s.what.push_str(c.trim());
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().to_owned();
        match key.trim() {
            "filter" => s.filter = value,
            "guest" => s.guest = value,
            "peer" => s.peer = Some(value),
            "raw" => s.raw = Some(value),
            "ports" => {
                s.ports = value
                    .split_whitespace()
                    .map(str::parse)
                    .collect::<std::result::Result<_, _>>()
                    .map_err(|e| format!("{name}: ports: {e}"))?;
            }
            "vary" => {
                for v in value.split_whitespace() {
                    match v {
                        "peer-window" => s.peer_window_varies = true,
                        v => return Err(format!("{name}: vary: unknown `{v}`").into()),
                    }
                }
            }
            k => return Err(format!("{name}: unknown key `{k}`").into()),
        }
    }
    if s.filter.is_empty() || s.guest.is_empty() {
        return Err(format!("{name}: needs `filter` and `guest`").into());
    }
    Ok(s)
}

/// The scenarios, by name, or those named (`a,b`).
fn scenarios(only: Option<&str>) -> Result<Vec<Scenario>> {
    let dir = util::root().join("tests/net/scenarios");
    let mut out = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let path = entry?.path();
        let Some(name) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".scn"))
        else {
            continue;
        };
        if only.is_some_and(|o| !o.split(',').any(|o| o == name)) {
            continue;
        }
        out.push(parse(name, &fs::read_to_string(&path)?)?);
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    if out.is_empty() {
        return Err(format!("no scenario {only:?} in {}", dir.display()).into());
    }
    Ok(out)
}

/// What each address in a capture stands for, longest first so that an
/// address that is a prefix of another (`10.77.0.1` of `10.77.0.15`) is
/// replaced after it.
#[derive(Debug, Default)]
struct Roles(Vec<(String, &'static str)>);

impl Roles {
    fn add(&mut self, text: &str, role: &'static str) {
        if !text.is_empty() {
            self.0.push((text.to_owned(), role));
            self.0.sort_by_key(|(t, _)| core::cmp::Reverse(t.len()));
        }
    }
}

/// `line` with each whole `text` (not part of a longer address: no letter
/// or digit just before or after it) replaced by `role`.
fn replace_whole(line: &str, text: &str, role: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    let mut before = None;
    while let Some(at) = rest.find(text) {
        let prev = rest[..at].chars().next_back().or(before);
        let next = rest[at + text.len()..].chars().next();
        out.push_str(&rest[..at]);
        if prev.is_some_and(|c| c.is_ascii_alphanumeric())
            || next.is_some_and(|c| c.is_ascii_alphanumeric())
        {
            out.push_str(text);
        } else {
            out.push_str(role);
        }
        before = text.chars().next_back();
        rest = &rest[at + text.len()..];
    }
    out.push_str(rest);
    out
}

/// Replaces the token after each `key` (digits, or hex after `0x`) with
/// `_`, unless `keep` accepts it.
fn blank_after(line: &str, key: &str, keep: impl Fn(&str) -> bool) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find(key) {
        let (head, tail) = rest.split_at(at + key.len());
        out.push_str(head);
        let hex = tail.starts_with("0x");
        let body = if hex { &tail[2..] } else { tail };
        let len = body
            .find(|c: char| !(c.is_ascii_hexdigit() && (hex || c.is_ascii_digit())))
            .unwrap_or(body.len());
        let token = &tail[..len + if hex { 2 } else { 0 }];
        if len == 0 || keep(token) {
            out.push_str(token);
        } else {
            out.push('_');
        }
        rest = &tail[token.len()..];
    }
    out.push_str(rest);
    out
}

/// Replaces the bytes of a neighbor solicitation's nonce option (RFC 7527,
/// which DAD sends), shown by `tcpdump` as an unknown option's hex dump,
/// with `_`.
fn blank_nonce(line: &str) -> String {
    const KEY: &str = "unknown option (14), length ";
    let Some(at) = line.find(KEY) else {
        return line.to_owned();
    };
    let Some(dump) = line[at..].find("0x0000:").map(|d| at + d + "0x0000:".len()) else {
        return line.to_owned();
    };
    let mut end = dump;
    let tail = &line[dump..];
    let mut pos = 0;
    for word in tail.split(' ') {
        let next = pos + word.len() + 1;
        if !word.is_empty() && !word.chars().all(|c| c.is_ascii_hexdigit()) {
            break;
        }
        if !word.is_empty() {
            end = dump + pos + word.len();
        }
        pos = next;
    }
    format!("{} _{}", &line[..dump], &line[end..])
}

/// Replaces `ROLE.<port>` with `ROLE.eph` for ports not in `fixed`.
fn blank_ports(line: &str, fixed: &[u16]) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(at) = rest.find(|c: char| c.is_ascii_uppercase()) {
        let (head, tail) = rest.split_at(at);
        out.push_str(head);
        let role_len = tail
            .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '-'))
            .unwrap_or(tail.len());
        let (role, after) = tail.split_at(role_len);
        out.push_str(role);
        rest = after;
        if let Some(port) = after.strip_prefix('.') {
            let n = port
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(port.len());
            if n > 0 && port[..n].parse().is_ok_and(|p: u16| !fixed.contains(&p)) {
                out.push_str(".eph");
                rest = &port[n..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// `tcpdump -n -e -t -v` output, normalized: continuation lines joined,
/// roles for addresses, and the per-run values blanked.
fn normalize(decoded: &str, roles: &Roles, fixed_ports: &[u16], peer_window: bool) -> String {
    let mut packets: Vec<String> = Vec::new();
    for line in decoded.lines() {
        if line.starts_with(char::is_whitespace) {
            if let Some(last) = packets.last_mut() {
                last.push(' ');
                last.push_str(line.trim());
            }
        } else if !line.trim().is_empty() {
            packets.push(line.trim().to_owned());
        }
    }
    let mut out = String::new();
    for p in packets {
        let mut l = p;
        for (text, role) in &roles.0 {
            l = replace_whole(&l, text, role);
        }
        l = blank_after(&l, "id ", |_| false);
        l = blank_after(&l, "cksum ", |_| false);
        l = blank_after(&l, "flowlabel ", |_| false);
        l = blank_after(&l, "TS val ", |_| false);
        // A DHCP transaction.
        l = blank_after(&l, "xid ", |_| false);
        // An IPv6 fragment's identification.
        l = blank_after(&l, "frag (", |_| false);
        l = blank_nonce(&l);
        l = blank_after(&l, " ecr ", |t| t == "0");
        // A SYN's sequence number, and a SYN-ACK's acknowledgement, are
        // absolute (`tcpdump` makes the rest relative to them).
        if l.contains("Flags [S") {
            l = blank_after(&l, " seq ", |_| false);
            l = blank_after(&l, " ack ", |_| false);
        }
        // So is a reset's acknowledgement of a SYN `tcpdump` saw no answer to.
        if l.contains("Flags [R") {
            l = blank_after(&l, " ack ", |_| false);
        }
        if peer_window && (l.contains(") PEER.") || l.contains(") PEER6.")) {
            l = blank_after(&l, " win ", |_| false);
        }
        l = blank_ports(&l, fixed_ports);
        out.push_str(&l);
        out.push('\n');
    }
    out
}

/// The first line where `a` and `b` differ, as a report; `None` if equal.
fn first_difference(a: &str, b: &str) -> Option<String> {
    let (mut x, mut y) = (a.lines(), b.lines());
    for n in 1.. {
        match (x.next(), y.next()) {
            (None, None) => return None,
            (l, r) if l == r => {}
            (l, r) => {
                return Some(format!(
                    "line {n}:\n  freebsd: {}\n  this:    {}",
                    l.unwrap_or("(end)"),
                    r.unwrap_or("(end)")
                ));
            }
        }
    }
    None
}

/// `tcpdump` on `tap`, writing `pcap`, until [`stop_capture`].
fn start_capture(tap: &str, filter: &str, pcap: &Path) -> Result<Child> {
    let mut child = Command::new("doas")
        .args(["tcpdump", "-i", tap, "-n", "-U", "-w"])
        .arg(pcap)
        .arg(filter)
        .stderr(Stdio::piped())
        .spawn()?;
    // It says when it is listening; nothing is missed after that.
    let stderr = child.stderr.take().ok_or("tcpdump: no stderr")?;
    let mut lines = BufReader::new(stderr).lines();
    loop {
        match lines.next() {
            Some(Ok(l)) if l.contains("listening on") => break,
            Some(Ok(_)) => {}
            _ => return Err("tcpdump ended before listening".into()),
        }
    }
    // Keep draining its stderr so it never blocks on a full pipe.
    std::thread::spawn(move || for _ in lines {});
    Ok(child)
}

/// Stops a capture (`doas` runs `tcpdump` in its place, as root).
fn stop_capture(mut child: Child) -> Result<()> {
    crate::perf::doas(&["kill", "-INT", &child.id().to_string()])?;
    child.wait()?;
    Ok(())
}

/// Runs `command` on the guest's console and waits for it to finish.
fn on_guest(session: &mut Session, command: &str, timeout: Duration) -> Result<()> {
    session.send(&format!("{command}; echo nd-done-$((6*7))\n"))?;
    session.expect("\nnd-done-42\r\n", timeout)?;
    session.expect(PROMPT, timeout)
}

/// The guest's MAC, IPv4, link-local, and SLAAC addresses, once it has them.
/// The console drops long input lines, so each command stays short, and the
/// addresses come back as `nd-<keyword> <address>` lines.
fn guest_identity(session: &mut Session, log: &Path) -> Result<[String; 4]> {
    let wait = Duration::from_secs(150);
    on_guest(
        session,
        "for i in $(jot 120); do ifconfig vtnet0 > /dev/null || break; ifconfig vtnet0 | grep -q 'inet 10.77.0.' && break; sleep 1; done",
        wait,
    )?;
    on_guest(
        session,
        "for i in $(jot 120); do ifconfig vtnet0 > /dev/null || break; ifconfig vtnet0 | grep -q 'fd77:.* autoconf' && break; sleep 1; done",
        wait,
    )?;
    on_guest(
        session,
        "for i in $(jot 30); do ifconfig vtnet0 > /dev/null || break; ifconfig vtnet0 | grep -q tentative || break; sleep 1; done",
        wait,
    )?;
    on_guest(
        session,
        "ifconfig vtnet0 | awk '/ether|inet/ {print \"nd-\" $1, $2}'",
        Duration::from_secs(30),
    )?;
    let lines = reported(log)?;
    let find = |key: &str, prefix: &str| {
        lines
            .iter()
            .find(|(k, a)| k == key && a.starts_with(prefix))
            .map(|(_, a)| a.clone())
            .ok_or_else(|| match key {
                "nd-ether" => "the guest has no vtnet0".to_owned(),
                _ => format!("the guest has no {prefix}... address on vtnet0"),
            })
    };
    // An IPv6 address may be missing (a guest without IPv6): its
    // scenarios then differ, and the others still compare.
    Ok([
        find("nd-ether", "")?,
        find("nd-inet", "10.77.0.")?,
        find("nd-inet6", "fe80:").unwrap_or_default(),
        find("nd-inet6", "fd77:").unwrap_or_default(),
    ])
}

/// The `nd-<keyword> <value>` lines a console printed, newest first, so a
/// lookup finds this run's.
fn reported(log: &Path) -> Result<Vec<(String, String)>> {
    let console = fs::read_to_string(log)?;
    let mut lines = Vec::new();
    for l in console.lines().rev() {
        let mut w = l.split_whitespace();
        match (w.next(), w.next()) {
            (Some(k), Some(a)) if k.starts_with("nd-") && k != "nd-done-42" => {
                lines.push((k.to_owned(), a.split('%').next().unwrap_or(a).to_owned()));
            }
            _ => {}
        }
    }
    Ok(lines)
}

/// The scripted peer's disk for this run: FreeBSD's networked image with
/// kyua's packages (Python and scapy), kept beside the traces as
/// `peer.img`, copied with `tests/net/peers/` in `/root/peers/`.
fn peer_disk(dir: &Path) -> Result<PathBuf> {
    let (image, _) = crate::perf::freebsd_net_image(Arch::X86_64)?;
    let packages = crate::kyua::packages()?;
    let base = dir.with_file_name("peer.img");
    let stamp = base.with_extension("img.prepared");
    let made = fs::metadata(&image)?
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let want = format!("{PEER_PREP_VERSION} {made}");
    let mnt = dir.join("mnt");
    if !base.exists() || fs::read_to_string(&stamp).ok().as_deref() != Some(want.as_str()) {
        eprintln!("xtask: netdiff: preparing {}", base.display());
        let tmp = base.with_extension("img.tmp");
        crate::multiuser::copy_sparse(&image, &tmp)?;
        crate::perf::mounted_root(&tmp, &mnt, |root| {
            crate::kyua::install_packages(root, &packages)
        })?;
        fs::rename(&tmp, &base)?;
        fs::write(&stamp, want)?;
    }
    let disk = dir.join("peer.img");
    crate::multiuser::copy_sparse(&base, &disk)?;
    let mut files = Vec::new();
    for e in fs::read_dir(util::root().join("tests/net/peers"))? {
        let path = e?.path();
        if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && name.ends_with(".py")
        {
            files.push((format!("root/peers/{name}"), 0o644, fs::read(&path)?));
        }
    }
    crate::perf::mounted_root(&disk, &mnt, |root| {
        crate::perf::doas(&["mkdir", "-p", &root.join("root/peers").to_string_lossy()])?;
        let files: Vec<(&str, u32, Vec<u8>)> = files
            .iter()
            .map(|(p, m, d)| (p.as_str(), *m, d.clone()))
            .collect();
        crate::perf::install_files(root, &files)
    })?;
    Ok(disk)
}

/// Readies the logged-in scripted peer: its address, its kernel quiet,
/// and no receive offload in the way of what scapy sees. Returns its MAC.
fn raw_peer_up(session: &mut Session, log: &Path) -> Result<String> {
    let t = Duration::from_secs(30);
    on_guest(session, "sysctl -q net.inet.tcp.blackhole=2", t)?;
    on_guest(
        session,
        &format!("ifconfig vtnet0 inet {RAW_PEER}/32 alias -lro -tso"),
        t,
    )?;
    on_guest(
        session,
        "ifconfig vtnet0 | awk '/ether/ {print \"nd-\" $1, $2}'",
        t,
    )?;
    reported(log)?
        .into_iter()
        .find(|(k, _)| k == "nd-ether")
        .map(|(_, a)| a)
        .ok_or_else(|| "the scripted peer has no vtnet0".into())
}

/// Starts `command`, a script from `tests/net/peers/`, on the scripted
/// peer, and waits until it listens.
fn start_script(peer: &mut Session, command: &str) -> Result<()> {
    peer.send(&format!("{command}; echo nd-raw-$?-$((6*7))\n"))?;
    peer.expect("peer: ready", Duration::from_secs(60))
}

/// Waits for the scripted peer's script to end with its exchange; it says
/// why on the peer's console when it gives up.
fn end_script(peer: &mut Session) -> Result<()> {
    let ends = ["\nnd-raw-0-42\r\n", "\nnd-raw-1-42\r\n"];
    let hit = peer.expect_any(&ends, Duration::from_secs(60))?;
    peer.expect(PROMPT, Duration::from_secs(30))?;
    match hit {
        0 => Ok(()),
        _ => Err("it gave up (see peer-console.log)".into()),
    }
}

/// The test bridge's MAC and link-local address, on this host.
fn bridge_identity() -> Result<(String, String)> {
    let out = util::output(Command::new("ifconfig").arg(Tap::BRIDGE))?;
    let mut mac = String::new();
    let mut ll = String::new();
    for l in out.lines() {
        let mut w = l.split_whitespace();
        match w.next() {
            Some("ether") => mac = w.next().unwrap_or("").to_owned(),
            Some("inet6") => {
                let a = w.next().unwrap_or("");
                if a.starts_with("fe80") {
                    ll = a.split('%').next().unwrap_or("").to_owned();
                }
            }
            _ => {}
        }
    }
    Ok((mac, ll))
}

/// Where traces go on the server, outside the mirrored tree.
fn traces_dir(os: &str) -> PathBuf {
    crate::perf::home().join("abyss/net").join(os)
}

/// `cargo xtask --remote netdiff [--freebsd] [--scenario NAME,...]`: boots
/// AbyssBSD's networked root (or with `--freebsd`, FreeBSD 15.1's) on the
/// test bridge, runs each scenario under capture, and compares the
/// normalized traces with `tests/net/freebsd/`. Traces are written to
/// `~/abyss/net/<os>/` on the server; FreeBSD's are what `tests/net/freebsd/`
/// records.
pub(crate) fn cmd_netdiff(mut args: Args) -> Result<()> {
    let freebsd = args.take_flag("--freebsd");
    let only = args.take_value("--scenario")?;
    args.finish()?;
    if !cfg!(target_os = "freebsd") {
        return Err("netdiff needs the build server (bhyve, the test bridge)".into());
    }
    let scenarios = scenarios(only.as_deref())?;
    let os = if freebsd { "freebsd" } else { "abyss" };
    let out_dir = traces_dir(os);
    fs::create_dir_all(&out_dir)?;
    let disk = if freebsd {
        let (image, _) = crate::perf::freebsd_net_image(Arch::X86_64)?;
        let disk = out_dir.join("disk.img");
        crate::multiuser::copy_sparse(&image, &disk)?;
        disk
    } else {
        let opts = crate::build::Options {
            release: true,
            ktest: false,
        };
        let root =
            crate::image::build_net_root(Arch::X86_64, opts, "boot_multicons=YES boot_serial=YES")?;
        let disk = out_dir.join("disk.img");
        fs::copy(&root, &disk)?;
        disk
    };
    // The scripted peer boots beside the guest when a scenario needs it.
    let mut raw = if scenarios.iter().any(|s| s.raw.is_some()) {
        let log = out_dir.join("peer-console.log");
        let vm = Vm {
            net: Some(Net::Bridge(Tap::new()?)),
            instance: "-peer",
            ..Vm::new(Vmm::Bhyve, Arch::X86_64, peer_disk(&out_dir)?)
        };
        Some((Session::start(vm, &log)?, log))
    } else {
        None
    };
    let log = out_dir.join("console.log");
    let tap = Tap::new()?;
    let tap_name = tap.name().to_owned();
    let vm = Vm {
        smp: 4,
        mem: "2G".into(),
        nvme: !freebsd,
        net: Some(Net::Bridge(tap)),
        ..Vm::new(Vmm::Bhyve, Arch::X86_64, disk)
    };
    let mut session = Session::start(vm, &log)?;
    let result = (|| -> Result<Vec<(String, Option<String>)>> {
        crate::net::login(&mut session, Duration::from_secs(600), &["panic:"])?;
        let [mac, v4, ll, ula] = guest_identity(&mut session, &log)?;
        let (peer_mac, peer_ll) = bridge_identity()?;
        let mut roles = Roles::default();
        roles.add(&mac, "GUEST-MAC");
        roles.add(&v4, "GUEST");
        roles.add(&ll, "GUEST-LL");
        roles.add(&ula, "GUEST6");
        roles.add(&peer_mac, "PEER-MAC");
        roles.add(&peer_ll, "PEER-LL");
        roles.add("10.77.0.1", "PEER");
        roles.add("fd77:77:77::1", "PEER6");
        if let Some((peer, log)) = raw.as_mut() {
            crate::net::login(peer, Duration::from_secs(600), &["panic:"])?;
            roles.add(&raw_peer_up(peer, log)?, "RAW-MAC");
            roles.add(RAW_PEER, "RAW");
        }
        let mut verdicts = Vec::new();
        for s in &scenarios {
            let pcap = out_dir.join(format!("{}.pcap", s.name));
            let capture = start_capture(&tap_name, &s.filter, &pcap)?;
            let mut peer = match &s.peer {
                Some(cmd) => {
                    let child = Command::new("sh").args(["-c", cmd]).spawn()?;
                    std::thread::sleep(Duration::from_millis(500));
                    Some(child)
                }
                None => None,
            };
            let script = match (&s.raw, raw.as_mut()) {
                (Some(cmd), Some((peer, _))) => Some((cmd, peer)),
                _ => None,
            };
            let (ran, scripted) = match script {
                Some((cmd, peer)) => match start_script(peer, cmd) {
                    Ok(()) => {
                        let ran = on_guest(&mut session, &s.guest, Duration::from_secs(60));
                        (ran, end_script(peer))
                    }
                    Err(e) => (Ok(()), Err(e)),
                },
                None => (
                    on_guest(&mut session, &s.guest, Duration::from_secs(60)),
                    Ok(()),
                ),
            };
            // Late packets: a closing connection's last ACK, an ICMP error.
            std::thread::sleep(Duration::from_secs(2));
            if let Some(p) = peer.as_mut() {
                let _ = p.kill();
                let _ = p.wait();
            }
            stop_capture(capture)?;
            ran.map_err(|e| format!("{}: the guest's command: {e}", s.name))?;
            let decoded = util::output(
                Command::new("tcpdump")
                    .args(["-n", "-e", "-t", "-v", "-r"])
                    .arg(&pcap),
            )?;
            let trace = format!(
                "# {}\n{}",
                s.what,
                normalize(&decoded, &roles, &s.ports, s.peer_window_varies)
            );
            fs::write(out_dir.join(format!("{}.txt", s.name)), &trace)?;
            let baseline = util::root().join(format!("tests/net/freebsd/{}.txt", s.name));
            let verdict = match (scripted, fs::read_to_string(&baseline)) {
                (Err(e), _) => Some(format!("the scripted peer: {e}")),
                (Ok(()), Err(_)) => Some("no FreeBSD trace recorded".to_owned()),
                (Ok(()), Ok(want)) => first_difference(&want, &trace),
            };
            verdicts.push((s.name.clone(), verdict));
        }
        Ok(verdicts)
    })();
    if let Some((mut peer, _)) = raw {
        let _ = peer.send("shutdown -p now\n");
        let _ = peer.finish(Duration::from_secs(120));
    }
    let _ = session.send("shutdown -p now\n");
    let _ = session.finish(Duration::from_secs(120));
    let verdicts = result?;
    let mut failed = 0;
    for (name, verdict) in &verdicts {
        match verdict {
            None => println!("netdiff {name}: same as FreeBSD"),
            Some(why) => {
                failed += 1;
                println!("netdiff {name}: DIFFERS: {why}");
            }
        }
    }
    println!("netdiff: traces in {}", out_dir.display());
    if failed > 0 {
        return Err(format!("{failed} of {} scenarios differ", verdicts.len()).into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenario_files_parse() {
        let s = parse(
            "t",
            "# What it does.\nfilter: tcp port 7001\nports: 7001 7002\npeer: nc -l x\nguest: nc y\n",
        )
        .unwrap();
        assert_eq!(s.what, "What it does.");
        assert_eq!(s.ports, [7001, 7002]);
        assert_eq!(s.peer.as_deref(), Some("nc -l x"));
        assert_eq!(s.raw, None);
        let s = parse(
            "t",
            "filter: x\nraw: python3.12 /root/peers/a.py\nguest: y\n",
        )
        .unwrap();
        assert_eq!(s.raw.as_deref(), Some("python3.12 /root/peers/a.py"));
        assert!(parse("t", "guest: x\n").is_err());
        assert!(parse("t", "filter: x\nguest: y\nbogus: z\n").is_err());
        assert!(
            parse("t", "filter: x\nguest: y\nvary: peer-window\n")
                .unwrap()
                .peer_window_varies
        );
        assert!(parse("t", "filter: x\nguest: y\nvary: ttl\n").is_err());
    }

    #[test]
    fn every_scenario_in_the_tree_parses() {
        assert!(!scenarios(None).unwrap().is_empty());
    }

    fn roles() -> Roles {
        let mut r = Roles::default();
        r.add("58:9c:fc:00:00:01", "GUEST-MAC");
        r.add("10.77.0.15", "GUEST");
        r.add("10.77.0.1", "PEER");
        r
    }

    #[test]
    fn per_run_values_are_blanked() {
        let decoded = "58:9c:fc:00:00:01 > 02:00:00:00:00:02, ethertype IPv4 (0x0800), length 74: \
                       (tos 0x0, ttl 64, id 4242, offset 0, flags [DF], proto TCP (6), length 60)\n    \
                       10.77.0.15.51234 > 10.77.0.1.7001: Flags [S], cksum 0x1a2b (correct), \
                       seq 2851262457, win 65535, options [mss 1460,nop,wscale 6,sackOK,TS val 99 ecr 0], length 0\n";
        let n = normalize(decoded, &roles(), &[7001], false);
        assert_eq!(
            n,
            "GUEST-MAC > 02:00:00:00:00:02, ethertype IPv4 (0x0800), length 74: \
             (tos 0x0, ttl 64, id _, offset 0, flags [DF], proto TCP (6), length 60) \
             GUEST.eph > PEER.7001: Flags [S], cksum _ (correct), seq _, win 65535, \
             options [mss 1460,nop,wscale 6,sackOK,TS val _ ecr 0], length 0\n"
        );
    }

    #[test]
    fn relative_numbers_after_the_handshake_stay() {
        let decoded = "x: 10.77.0.15.51234 > 10.77.0.1.7001: Flags [P.], seq 1:101, ack 1, win 1027, \
                       options [nop,nop,TS val 5 ecr 7], length 100\n";
        let n = normalize(decoded, &roles(), &[7001], false);
        assert!(n.contains("seq 1:101, ack 1,"), "{n}");
        assert!(n.contains("TS val _ ecr _"), "{n}");
    }

    #[test]
    fn a_prefix_address_is_replaced_after_the_longer_one() {
        let n = normalize(
            "10.77.0.15 > 10.77.0.1: ICMP echo request, id 77, seq 0\n",
            &roles(),
            &[],
            false,
        );
        assert_eq!(n, "GUEST > PEER: ICMP echo request, id _, seq 0\n");
    }

    #[test]
    fn an_address_is_replaced_only_whole() {
        let mut r = roles();
        r.add("10.77.0.2", "RAW");
        let n = normalize(
            "BR (28), length 4: 10.77.0.255 10.77.0.2.7006 > 10.77.0.15.5\n",
            &r,
            &[7006],
            false,
        );
        assert_eq!(n, "BR (28), length 4: 10.77.0.255 RAW.7006 > GUEST.eph\n");
    }

    #[test]
    fn a_dad_nonce_and_a_fragment_id_are_blanked() {
        let n = normalize(
            ":: > ff02::1:ff00:99: ICMP6, neighbor solicitation, length 32, who has fd77:77:77::99 \
             unknown option (14), length 8 (1): 0x0000:  381e cbce b9db\n",
            &roles(),
            &[],
            false,
        );
        assert!(n.ends_with("length 8 (1): 0x0000: _\n"), "{n}");
        let n = normalize("a > b: frag (0xdc96b494:1232|1232)\n", &roles(), &[], false);
        assert_eq!(n, "a > b: frag (_:1232|1232)\n");
    }

    #[test]
    fn a_varying_peer_window_is_blanked_on_the_peers_segments_only() {
        let decoded = "a: (tos 0x0, length 52) 10.77.0.1.7003 > 10.77.0.15.5: Flags [.], ack 9, win 12, length 0\n\
                       b: (tos 0x0, length 52) 10.77.0.15.5 > 10.77.0.1.7003: Flags [.], ack 1, win 257, length 0\n";
        let n = normalize(decoded, &roles(), &[7003], true);
        assert!(n.contains("ack 9, win _,"), "{n}");
        assert!(n.contains("ack 1, win 257,"), "{n}");
    }

    #[test]
    fn differences_are_reported_by_line() {
        assert_eq!(first_difference("a\nb\n", "a\nb\n"), None);
        let d = first_difference("a\nb\n", "a\nc\n").unwrap();
        assert!(d.starts_with("line 2:"), "{d}");
        assert!(first_difference("a\n", "a\nb\n").is_some());
    }
}
