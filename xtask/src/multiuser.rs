// SPDX-License-Identifier: BSD-2-Clause
//! The multi-user boot (docs/MULTIUSER.md §14, ROADMAP P6.0 and P6.9):
//! the root as FreeBSD's installer leaves it, and the login script that
//! the `<arch>-multiuser` boot cases and `cargo xtask --remote
//! freebsd-multiuser` both run.
//!
//! The root is all of `base.txz` with what `bsdinstall` adds when no
//! service is ticked and crash dumps are left on (its `services` and
//! `hostname` scripts): an `rc.conf` with a host name, `/etc/localtime`
//! for UTC (`tzsetup`), and an `fstab`. Root keeps base's empty password,
//! as FreeBSD's release VM images do.
//!
//! The script logs in as root on `ttyv0` (typed on the USB keyboard over
//! QMP, in QEMU) and on the serial console, checks that `syslogd`,
//! `cron`, and `devd` run, that a pty works, that `logger` reaches
//! `/var/log/messages`, and that cron runs a job, then shuts down with
//! `shutdown -p now`. Running it against FreeBSD 15.1 keeps it honest:
//! every step expects what FreeBSD does.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::arch::Arch;
use crate::cli::Args;
use crate::expect::{PROMPT, Session, Step, step};
use crate::qemu::{Vm, Vmm};
use crate::qmp::Qmp;
use crate::util::Result;

/// `/etc/rc.conf` as `bsdinstall` writes it with no service ticked and
/// crash dumps on.
pub(crate) const RC_CONF: &str = "hostname=\"abyss\"\n\
                                  moused_nondefault_enable=\"NO\"\n\
                                  # Set dumpdev to \"AUTO\" to enable crash dumps, \"NO\" to disable\n\
                                  dumpdev=\"AUTO\"\n";

/// The files the installer adds to `base.txz`'s tree (`full`), as (path
/// under the root, mode, contents). None of them is in the base, so they
/// can be laid over it.
pub(crate) fn installed_files(full: &Path) -> Result<Vec<(&'static str, u32, Vec<u8>)>> {
    let utc = full.join("usr/share/zoneinfo/UTC");
    Ok(vec![
        ("etc/rc.conf", 0o644, RC_CONF.as_bytes().to_vec()),
        (
            "etc/localtime",
            0o444,
            fs::read(&utc).map_err(|e| format!("{}: {e}", utc.display()))?,
        ),
        ("var/db/zoneinfo", 0o644, b"UTC\n".to_vec()),
    ])
}

/// Root's prompt from base's `/root/.shrc` (`\u@\h:\w \$ `).
pub(crate) const ROOT_PROMPT: &str = "root@abyss:~ # ";

/// What the script checks once logged in on the serial console, after
/// setting [`PROMPT`].
const LOGGED_IN: &[Step] = &[
    step("uname -s\n", "\nFreeBSD\r\n", "uname"),
    step(
        "pgrep -qx syslogd && pgrep -qx cron && pgrep -qx devd && echo daemons-$((6*7))\n",
        "\ndaemons-42\r\n",
        "syslogd, cron, and devd run",
    ),
    step(
        "ls /var/run/log /var/run/devd.pipe\n",
        "/var/run/log\r\n",
        "their sockets exist",
    ),
    step("echo | script -q /dev/null tty\n", "/dev/pts/", "a pty"),
    step(
        "procstat -f $$ | grep -q ' text ' && echo procstat-$((6*7))\n",
        "\nprocstat-42\r\n",
        "procstat",
    ),
    step(
        "logger -t abyss multi-$((6*7)) && sleep 2 && grep -c 'abyss.*multi-42' /var/log/messages\n",
        "\n1\r\n",
        "logger reaches syslogd",
    ),
    step(
        "echo '* * * * * touch /tmp/cron-ran' | crontab - && echo crontab-$((6*7))\n",
        "\ncrontab-42\r\n",
        "crontab",
    ),
    step(
        "for i in $(jot 90); do [ -e /tmp/cron-ran ] && break; sleep 1; done; ls /tmp/cron-ran\n",
        "\n/tmp/cron-ran\r\n",
        "cron runs the job",
    ),
    step(
        "crontab -rf && echo uncron-$((6*7))\n",
        "\nuncron-42\r\n",
        "crontab -r",
    ),
];

/// How the console is reached besides the serial port.
pub(crate) struct Screen<'a> {
    pub(crate) qmp: &'a Path,
    /// Where to leave the last screendump, for a failure.
    pub(crate) shot: &'a Path,
}

/// Runs the script on a booting machine: waits for `login:` on the
/// serial console, logs in on `ttyv0` through `screen` if there is one,
/// then on the serial console, runs the checks, and shuts down. Returns
/// whether the machine powered off by itself.
///
/// Any of `give_up` appearing before the prompt fails the run at once.
pub(crate) fn login_script(
    mut session: Session,
    screen: Option<&Screen<'_>>,
    boot_timeout: Duration,
    give_up: &[&str],
) -> (Result<()>, Result<bool>) {
    let result = (|| -> Result<()> {
        let mut patterns = vec!["login: "];
        patterns.extend_from_slice(give_up);
        let hit = session
            .expect_any(&patterns, boot_timeout)
            .map_err(|e| format!("no login prompt on the serial console: {e}"))?;
        if hit > 0 {
            return Err(format!("{:?} before a login prompt", patterns[hit]).into());
        }
        if let Some(screen) = screen {
            vt_login(&mut session, screen)?;
        }
        session.send("root\n")?;
        session
            .expect(ROOT_PROMPT, Duration::from_secs(120))
            .map_err(|e| format!("serial login: {e}"))?;
        session.send("PS1='@''sh> '\n")?;
        session.expect(PROMPT, Duration::from_secs(30))?;
        session.script(LOGGED_IN, Duration::from_secs(150))?;
        session.send("shutdown -p now\n")
    })();
    let exited = session.finish(Duration::from_secs(300));
    (result, exited)
}

/// Logs in as root on `ttyv0` with the USB keyboard, and proves the shell
/// runs there: it writes its terminal's name to `/dev/console`, which the
/// serial port shows (both are consoles). Typing is repeated until that
/// line arrives, since the keyboard may attach after the prompt and getty
/// may flush what came before its own.
fn vt_login(session: &mut Session, screen: &Screen<'_>) -> Result<()> {
    let mut q = Qmp::connect(screen.qmp)?;
    let deadline = Instant::now() + Duration::from_secs(180);
    let found = loop {
        // getty on ttyv0 may still be flushing its input (`setttymode`)
        // when the serial prompt shows, so the login is typed each time;
        // in a shell it is just a command that is not found.
        q.type_text("root\n", None)?;
        std::thread::sleep(Duration::from_secs(5));
        q.type_text("echo vt-$((6*7)) $(tty) > /dev/console\n", None)?;
        if session
            .expect("vt-42 /dev/ttyv0", Duration::from_secs(10))
            .is_ok()
        {
            break true;
        }
        if Instant::now() > deadline {
            break false;
        }
    };
    let _ = q.screendump(screen.shot);
    if !found {
        return Err(format!(
            "no shell on ttyv0 after typing a login; the screen is in {}",
            screen.shot.display()
        )
        .into());
    }
    q.type_text("exit\n", None)?;
    Ok(())
}

/// The first-use lines in a console log (`first use: ...`), less those
/// `tests/multiuser/absent.txt` lists: interfaces FreeBSD lacks too.
pub(crate) fn unexpected_first_uses(console: &str) -> Result<Vec<String>> {
    let listed = fs::read_to_string(crate::util::root().join("tests/multiuser/absent.txt"))?;
    let absent: Vec<&str> = listed
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .collect();
    let mut out: Vec<String> = console
        .lines()
        .filter_map(|l| l.split("first use: ").nth(1))
        .map(|l| l.trim_end().to_owned())
        .filter(|l| {
            // An entry names the interface, without the program.
            let what = l.rsplit_once(" by ").map_or(l.as_str(), |(w, _)| w);
            !absent.contains(&what)
        })
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

/// `cargo xtask --remote freebsd-multiuser [--arch A] [--vmm qemu|bhyve]`:
/// boots FreeBSD 15.1's release image, configured as the installer would
/// leave it (with both consoles, `boot_multicons`), and runs the login
/// script against it. On amd64 the default is both bhyve (serial only)
/// and QEMU (`ttyv0` too); arm64 runs in QEMU.
pub(crate) fn cmd_freebsd_multiuser(mut args: Args) -> Result<()> {
    let arches = Arch::from_args(&mut args)?;
    let only = args
        .take_value("--vmm")?
        .map(|v| Vmm::parse(&v))
        .transpose()?;
    args.finish()?;
    if !cfg!(target_os = "freebsd") {
        return Err("freebsd-multiuser needs the build server (bhyve, mdconfig)".into());
    }
    for arch in arches {
        let (image, work) = crate::perf::freebsd_multiuser_image(arch)?;
        for vmm in [Vmm::Bhyve, Vmm::Qemu] {
            if only.is_some_and(|o| o != vmm) || !vmm.available(arch) {
                continue;
            }
            let name = format!("freebsd-multiuser ({arch}, {vmm:?})");
            let dir = work.join(format!("multiuser-{arch}-{}", vmm_name(vmm)));
            fs::create_dir_all(&dir)?;
            let disk = dir.join("disk.img");
            copy_sparse(&image, &disk)?;
            let log = dir.join("console.log");
            let sock = dir.join("qmp.sock");
            let shot = dir.join("screen.ppm");
            let qemu = vmm == Vmm::Qemu;
            let vm = Vm {
                smp: 4,
                mem: "2G".into(),
                usb: true,
                qmp: qemu.then(|| sock.clone()),
                ..Vm::new(vmm, arch, disk)
            };
            let session = Session::start(vm, &log)?;
            let screen = qemu.then_some(Screen {
                qmp: &sock,
                shot: &shot,
            });
            // FreeBSD under QEMU's emulation boots slowly.
            let (result, exited) =
                login_script(session, screen.as_ref(), Duration::from_secs(1800), &[]);
            result.map_err(|e| format!("{name}: {e} (console: {})", log.display()))?;
            if !exited? {
                return Err(format!("{name}: did not power off; see {}", log.display()).into());
            }
            println!("{name}: passed; console in {}", log.display());
        }
    }
    Ok(())
}

const fn vmm_name(vmm: Vmm) -> &'static str {
    match vmm {
        Vmm::Qemu => "qemu",
        Vmm::Bhyve => "bhyve",
    }
}

/// Copies a raw image, keeping it sparse (`cp` on FreeBSD does).
pub(crate) fn copy_sparse(from: &Path, to: &PathBuf) -> Result<()> {
    let _ = fs::remove_file(to);
    crate::util::run(std::process::Command::new("cp").arg(from).arg(to))
}
