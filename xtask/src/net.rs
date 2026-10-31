// SPDX-License-Identifier: BSD-2-Clause
//! The networked boot (docs/NETWORKING.md §15 and §16.4, ROADMAP P7.0):
//! the installed root with what `bsdinstall` writes for an interface
//! configured by DHCP and SLAAC with `sshd` ticked, and the script the
//! `<arch>-net` boot cases and `cargo xtask --remote freebsd-net` both
//! run.
//!
//! The machine is on its VMM's test network: QEMU's user-mode network,
//! or for bhyve the build server's test bridge (tools/netlab). Once
//! logged in on the serial console, the script waits for `dhclient`'s
//! address and the SLAAC one, checks both default routes, pings the
//! router over IPv4 and IPv6, resolves a name, fetches a file over HTTPS,
//! and installs a package with `pkg`. It then installs a key the harness
//! made into `/root/.ssh`, and the harness logs in with `ssh` (through a
//! forwarded port in QEMU, at the guest's address on the bridge) and
//! copies a file each way with `scp`. Running it against FreeBSD 15.1
//! keeps it honest.

use std::fs;
use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use crate::arch::Arch;
use crate::cli::Args;
use crate::expect::{PROMPT, Session, Step, step};
use crate::multiuser::ROOT_PROMPT;
use crate::qemu::{Net, Tap, Vm, Vmm};
use crate::util::Result;

/// What `bsdinstall` adds to [`crate::multiuser::RC_CONF`] for `vtnet0`
/// configured by DHCP and SLAAC (its `netconfig_ipv4` and
/// `netconfig_ipv6`) and `sshd` ticked (`services`). Base's `sshd`
/// refuses root; the flag lets the harness in with its key without
/// changing `/etc/ssh`.
const RC_CONF_NET: &str = "ifconfig_vtnet0=\"DHCP\"\n\
                           ifconfig_vtnet0_ipv6=\"inet6 accept_rtadv\"\n\
                           sshd_enable=\"YES\"\n\
                           sshd_flags=\"-o PermitRootLogin=prohibit-password\"\n";

/// [`crate::multiuser::installed_files`] with the network's `rc.conf`.
pub(crate) fn installed_files(full: &Path) -> Result<crate::multiuser::InstalledFiles> {
    let mut files = crate::multiuser::installed_files(full)?;
    for (path, _, data) in &mut files {
        if *path == "etc/rc.conf" {
            data.extend_from_slice(RC_CONF_NET.as_bytes());
        }
    }
    Ok(files)
}

/// The guest's ssh port.
const SSH: u16 = 22;

/// What a test network gives a guest, as the script checks it.
pub(crate) struct Lab {
    /// The guest's DHCP address, or the start of the pool's.
    v4: &'static str,
    /// The IPv4 router (and the default route's gateway).
    router4: &'static str,
    /// The start of the advertised IPv6 prefix.
    v6: &'static str,
    /// The router's address in that prefix.
    router6: &'static str,
    /// The start of the IPv6 default route's gateway (link-local).
    gateway6: &'static str,
    /// How the host reaches the guest's ssh.
    ssh: SshAt,
}

/// Where the host connects for ssh.
#[derive(Clone, Copy)]
enum SshAt {
    /// A port on the host's loopback forwarded to the guest's.
    Forwarded(u16),
    /// The guest's own address, on the test bridge.
    Direct,
}

impl Lab {
    /// QEMU's user-mode network ([`Net::User`]), the guest's ssh
    /// forwarded from `port`.
    pub(crate) const fn qemu(port: u16) -> Self {
        Self {
            v4: "10.0.2.15",
            router4: "10.0.2.2",
            v6: "fec0:",
            router6: "fec0::2",
            gateway6: "fe80::2",
            ssh: SshAt::Forwarded(port),
        }
    }

    /// The build server's test bridge ([`Net::Bridge`]).
    pub(crate) const BRIDGE: Self = Self {
        v4: "10.77.0.",
        router4: "10.77.0.1",
        v6: "fd77:77:77:",
        router6: "fd77:77:77::1",
        gateway6: "fe80::",
        ssh: SshAt::Direct,
    };

    /// What the script checks once logged in on the serial console, after
    /// setting [`PROMPT`]. `dhclient` runs when the link comes up, and
    /// `rtsol` once at boot, so the addresses are waited for. The first
    /// step prints the guest's address for [`SshAt::Direct`].
    fn steps(&self) -> Vec<Step> {
        // The steps live as long as the run; xtask exits after it.
        let s = |t: String| -> &'static str { Box::leak(t.into_boxed_str()) };
        let Self {
            v4,
            router4,
            v6,
            router6,
            gateway6,
            ..
        } = *self;
        vec![
            step(
                s(format!(
                    "for i in $(jot 90); do ifconfig vtnet0 inet | grep -q 'inet {v4}' && break; \
                     sleep 1; done; ifconfig vtnet0 inet | grep -o 'inet {v4}[0-9.]*' | sed 's/^/got-/'\n"
                )),
                s(format!("\ngot-inet {v4}")),
                "dhclient's address",
            ),
            step(
                "route -n get default | grep gateway\n",
                s(format!("gateway: {router4}\r\n")),
                "the DHCP default route",
            ),
            step(
                s(format!(
                    "ping -c 1 {router4} | grep -o '1 packets received'\n"
                )),
                "\n1 packets received\r\n",
                "ping the router",
            ),
            step(
                s(format!(
                    "for i in $(jot 90); do ifconfig vtnet0 inet6 | grep -q 'inet6 {v6}' && break; \
                     sleep 1; done; ifconfig vtnet0 inet6 | grep -o 'inet6 {v6}' | head -1\n"
                )),
                s(format!("\ninet6 {v6}\r\n")),
                "a SLAAC address",
            ),
            step(
                s(format!(
                    "route -n get -inet6 default | grep -o 'gateway: {gateway6}'\n"
                )),
                s(format!("\ngateway: {gateway6}\r\n")),
                "the router-advertised default route",
            ),
            step(
                s(format!(
                    "ping -6 -c 1 {router6} | grep -o '1 packets received'\n"
                )),
                "\n1 packets received\r\n",
                "ping the router over IPv6",
            ),
            step(
                "host -t A pkg.freebsd.org | grep -c 'has address'\n",
                "\n1\r\n",
                "DNS",
            ),
            step(
                "fetch -qo /tmp/MANIFEST https://download.freebsd.org/releases/amd64/15.1-RELEASE/MANIFEST \
                 && grep -c '^base.txz' /tmp/MANIFEST\n",
                "\n1\r\n",
                "fetch over HTTPS",
            ),
            step(
                "env ASSUME_ALWAYS_YES=yes pkg bootstrap -f > /dev/null && pkg update -q \
                 && pkg install -qy tree > /dev/null && tree -d -L 1 /boot | head -1\n",
                "\n/boot\r\n",
                "pkg installs a package",
            ),
            step(
                "pgrep -qx sshd && echo sshd-$((6*7))\n",
                "\nsshd-42\r\n",
                "sshd runs",
            ),
        ]
    }
}

/// Waits for `login:` on the serial console (failing at once on any of
/// `give_up` before it), logs in as root, and sets [`PROMPT`].
pub(crate) fn login(session: &mut Session, boot_timeout: Duration, give_up: &[&str]) -> Result<()> {
    let mut patterns = vec!["login: "];
    patterns.extend_from_slice(give_up);
    let hit = session
        .expect_any(&patterns, boot_timeout)
        .map_err(|e| format!("no login prompt on the serial console: {e}"))?;
    if hit > 0 {
        return Err(format!("{:?} before a login prompt", patterns[hit]).into());
    }
    session.send("root\n")?;
    session
        .expect(ROOT_PROMPT, Duration::from_secs(120))
        .map_err(|e| format!("serial login: {e}"))?;
    session.send("PS1='@''sh> '\n")?;
    session.expect(PROMPT, Duration::from_secs(30))?;
    Ok(())
}

/// Runs `steps` as [`Session::script`] does, except that a step whose
/// command ends (the prompt is back) without printing what it should
/// fails then, not at the timeout: a system without the network fails in
/// seconds.
pub(crate) fn run(session: &mut Session, steps: &[Step], timeout: Duration) -> Result<()> {
    for (i, s) in steps.iter().enumerate() {
        session.send(s.send)?;
        match session.expect_any(&[s.expect, "\npanic: ", PROMPT], timeout) {
            Ok(0) => session.expect(PROMPT, timeout),
            Ok(1) => Err("the kernel panicked".into()),
            Ok(_) => Err("the command ended without it".into()),
            Err(e) => Err(e),
        }
        .map_err(|e| format!("step {} ({}): {e}", i + 1, s.what))?;
    }
    Ok(())
}

/// A free TCP port on the host's loopback, for a forward.
fn free_port() -> Result<u16> {
    Ok(TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

/// The guest's address the first step printed, from the console log.
fn guest_address(log: &Path, lab: &Lab) -> Result<String> {
    let console = fs::read_to_string(log)?;
    let mark = format!("got-inet {}", lab.v4);
    let at = console
        .rfind(&mark)
        .ok_or("the guest's address is not in the console log")?;
    let addr: String = console[at + "got-inet ".len()..]
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    Ok(addr)
}

/// Runs the script on a booting machine on network `lab`, whose console
/// is logged to `log`; `dir` holds the key and the files copied. Waits for
/// `login:` on the serial console, logs in, checks, logs in with `ssh`
/// and copies with `scp` from the host, and shuts down. Returns whether
/// the machine powered off by itself. Any of `give_up` before the prompt
/// fails the run at once.
pub(crate) fn net_script(
    mut session: Session,
    lab: &Lab,
    log: &Path,
    dir: &Path,
    boot_timeout: Duration,
    give_up: &[&str],
) -> (Result<()>, Result<bool>) {
    let result = (|| -> Result<()> {
        let key = dir.join("id_ed25519");
        let _ = fs::remove_file(&key);
        let _ = fs::remove_file(key.with_extension("pub"));
        crate::util::run(
            Command::new("ssh-keygen")
                .args(["-q", "-t", "ed25519", "-N", "", "-C", "abyss-net", "-f"])
                .arg(&key),
        )?;
        let public = fs::read_to_string(key.with_extension("pub"))?;
        login(&mut session, boot_timeout, give_up)?;
        // `pkg` and the waits for addresses take minutes under emulation.
        run(&mut session, &lab.steps(), Duration::from_secs(900))?;
        session.send(&format!(
            "mkdir -p -m 700 /root/.ssh && echo '{}' > /root/.ssh/authorized_keys && echo key-$((6*7))\n",
            public.trim()
        ))?;
        session.expect("\nkey-42\r\n", Duration::from_secs(30))?;
        session.expect(PROMPT, Duration::from_secs(30))?;
        let (host, port) = match lab.ssh {
            SshAt::Forwarded(port) => ("127.0.0.1".to_owned(), port),
            SshAt::Direct => (guest_address(log, lab)?, SSH),
        };
        ssh_and_scp(&key, &host, port, dir)?;
        session.send("shutdown -p now\n")
    })();
    let exited = session.finish(Duration::from_secs(300));
    (result, exited)
}

/// `ssh` and `scp` options: a fresh guest each run, so no host keys kept,
/// and no prompts.
const SSH_OPTIONS: &[&str] = &[
    "-o",
    "StrictHostKeyChecking=no",
    "-o",
    "UserKnownHostsFile=/dev/null",
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=60",
    "-o",
    "LogLevel=ERROR",
];

/// From the host: `ssh` runs a command, and `scp` copies a file there
/// and back unchanged.
fn ssh_and_scp(key: &Path, host: &str, port: u16, dir: &Path) -> Result<()> {
    let opts = |c: &mut Command| {
        c.arg("-i").arg(key).args(SSH_OPTIONS);
    };
    let mut ssh = Command::new("ssh");
    opts(&mut ssh);
    let out = ssh
        .arg("-p")
        .arg(port.to_string())
        .arg(format!("root@{host}"))
        .arg("uname -s")
        .output()?;
    if String::from_utf8_lossy(&out.stdout).trim() != "FreeBSD" {
        return Err(format!(
            "ssh login: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    // A megabyte that differs from run to run.
    let sent = dir.join("scp-out");
    let back = dir.join("scp-back");
    let _ = fs::remove_file(&back);
    let mut x = u64::from(std::process::id()) | 1;
    let data: Vec<u8> = (0..1 << 20)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x.to_le_bytes()[0]
        })
        .collect();
    fs::write(&sent, &data)?;
    let there = format!("root@{host}:/tmp/scp");
    let (sent_s, back_s) = (sent.display().to_string(), back.display().to_string());
    for (from, to) in [(&sent_s, &there), (&there, &back_s)] {
        let mut scp = Command::new("scp");
        opts(&mut scp);
        crate::util::run(scp.arg("-P").arg(port.to_string()).arg(from).arg(to))
            .map_err(|e| format!("scp {from} {to}: {e}"))?;
    }
    if fs::read(&back)? != data {
        return Err("scp: the file came back changed".into());
    }
    Ok(())
}

/// A machine on its VMM's test network ([`Net`]): QEMU's user-mode
/// network with the guest's ssh forwarded, or a tap on the build server's
/// test bridge for bhyve. Returns the machine and how the script reaches
/// it.
pub(crate) fn networked(vm: Vm) -> Result<(Vm, Lab)> {
    let (net, lab) = match vm.vmm {
        Vmm::Qemu => {
            let port = free_port()?;
            (
                Net::User {
                    forwards: vec![(port, SSH)],
                },
                Lab::qemu(port),
            )
        }
        Vmm::Bhyve => (Net::Bridge(Tap::new()?), Lab::BRIDGE),
    };
    Ok((
        Vm {
            net: Some(net),
            ..vm
        },
        lab,
    ))
}

/// `cargo xtask --remote freebsd-net [--arch A] [--vmm qemu|bhyve]`:
/// boots FreeBSD 15.1's release image, configured as the installer would
/// leave it with a DHCP and SLAAC interface and `sshd`, on each VMM's test
/// network (bhyve: the test bridge; QEMU: user-mode networking), and runs
/// the network script against it.
pub(crate) fn cmd_freebsd_net(mut args: Args) -> Result<()> {
    let arches = Arch::from_args(&mut args)?;
    let only = args
        .take_value("--vmm")?
        .map(|v| Vmm::parse(&v))
        .transpose()?;
    args.finish()?;
    if !cfg!(target_os = "freebsd") {
        return Err("freebsd-net needs the build server (mdconfig, the test bridge)".into());
    }
    for arch in arches {
        let (image, work) = crate::perf::freebsd_net_image(arch)?;
        for vmm in [Vmm::Bhyve, Vmm::Qemu] {
            if only.is_some_and(|o| o != vmm) || !vmm.available(arch) {
                continue;
            }
            let name = format!("freebsd-net ({arch}, {vmm:?})");
            let dir = work.join(format!("net-{arch}-{vmm:?}").to_lowercase());
            fs::create_dir_all(&dir)?;
            let disk = dir.join("disk.img");
            crate::multiuser::copy_sparse(&image, &disk)?;
            let log = dir.join("console.log");
            let (vm, lab) = networked(Vm {
                smp: 4,
                mem: "2G".into(),
                ..Vm::new(vmm, arch, disk)
            })?;
            let session = Session::start(vm, &log)?;
            // FreeBSD under QEMU's emulation boots slowly.
            let (result, exited) =
                net_script(session, &lab, &log, &dir, Duration::from_secs(1800), &[]);
            result.map_err(|e| format!("{name}: {e} (console: {})", log.display()))?;
            if !exited? {
                return Err(format!("{name}: did not power off; see {}", log.display()).into());
            }
            println!("{name}: passed; console in {}", log.display());
        }
    }
    Ok(())
}
