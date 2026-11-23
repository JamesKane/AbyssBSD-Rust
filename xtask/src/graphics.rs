// SPDX-License-Identifier: BSD-2-Clause
//! The graphics acceptance run (docs/GRAPHICS.md §14, `tests/graphics`):
//! boots a prepared installed root in QEMU, starts stock Sway from the
//! FreeBSD 15.1 packages on the card, and takes the capture with `grim`.
//!
//! The root is a development image (`--root board`, with room for
//! packages and the harness's key); `--install` adds the packages the
//! first time. Without it the run is on a snapshot, so the image stays
//! as it was. The checks are what the capture's log must say: Sway's
//! renderer and a successful commit, and glmark2's renderer.

use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::arch::Arch;
use crate::cli::Args;
use crate::qemu::{Net, Vm, Vmm};
use crate::util::{self, Result};

const PACKAGES: &str = "sway foot fastfetch glmark2 grim seatd mesa-dri mesa-libs dejavu";

/// The guest's environment for Wayland clients, as `run-sway.sh` leaves it.
const WAYLAND_ENV: &str = "XDG_RUNTIME_DIR=/var/run/xdg WAYLAND_DISPLAY=wayland-1";

/// How to reach the guest: key only, never a password prompt.
struct Guest {
    port: u16,
    key: PathBuf,
}

impl Guest {
    fn options(&self, cmd: &mut Command, port_flag: &str) {
        cmd.args(["-i"])
            .arg(&self.key)
            .args([port_flag, &self.port.to_string()])
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "PasswordAuthentication=no",
                "-o",
                "IdentitiesOnly=yes",
                "-o",
                "StrictHostKeyChecking=no",
                "-o",
                "UserKnownHostsFile=/dev/null",
                "-o",
                "LogLevel=ERROR",
                "-o",
                "ConnectTimeout=10",
            ]);
    }

    fn ssh(&self, script: &str) -> Command {
        let mut c = Command::new("ssh");
        self.options(&mut c, "-p");
        c.arg("root@127.0.0.1").arg(script);
        c
    }

    /// The script's output, or `None` if it failed.
    fn out(&self, script: &str) -> Option<String> {
        let o = self.ssh(script).stderr(Stdio::null()).output().ok()?;
        o.status
            .success()
            .then(|| String::from_utf8_lossy(&o.stdout).into_owned())
    }

    fn scp_to(&self, files: &[&Path]) -> Result<()> {
        let mut c = Command::new("scp");
        self.options(&mut c, "-P");
        c.args(files).arg("root@127.0.0.1:/root/");
        util::run(&mut c)
    }

    fn scp_from(&self, remote: &str, local: &Path) -> Result<()> {
        let mut c = Command::new("scp");
        self.options(&mut c, "-P");
        c.arg(format!("root@127.0.0.1:{remote}")).arg(local);
        util::run(&mut c)
    }
}

/// Kills the machine, which is this run's own child, when dropped.
struct Machine(Child);

impl Drop for Machine {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_for(what: &str, secs: u64, mut ok: impl FnMut() -> bool) -> Result<()> {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(secs) {
        if ok() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    Err(format!("timed out after {secs} s waiting for {what}").into())
}

pub(crate) fn cmd(mut args: Args) -> Result<()> {
    let arch = Arch::one_from_args(&mut args)?;
    let disk = args.take_value("--disk")?.ok_or("--disk IMAGE")?;
    let key = args.take_value("--key")?.ok_or("--key PRIVATE_KEY")?;
    let out = PathBuf::from(
        args.take_value("--out")?
            .unwrap_or_else(|| "build/graphics".into()),
    );
    let install = args.take_flag("--install");
    // Under emulation (arm64 on an x86 host) everything takes far longer.
    let slow: u64 = if cfg!(target_arch = "x86_64") && arch == Arch::Aarch64 {
        6
    } else {
        1
    };
    args.finish()?;
    fs::create_dir_all(&out)?;
    let root = util::root();
    let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let qmp = std::env::temp_dir().join(format!("abyss-gfx-{}.qmp", std::process::id()));
    let vm = Vm {
        net: Some(Net::User {
            forwards: vec![(port, 22)],
        }),
        smp: 4,
        mem: "4G".into(),
        qmp: Some(qmp.clone()),
        ..Vm::new(Vmm::Qemu, arch, PathBuf::from(&disk))
    };
    let mut cmd = vm.command()?;
    vm.serial(
        &mut cmd,
        &format!("file:{}", out.join("serial.log").display()),
    );
    if !install {
        cmd.arg("-snapshot");
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::null());
    eprintln!("xtask: starting QEMU ({arch}); ssh on localhost:{port}");
    let machine = Machine(cmd.spawn()?);
    let guest = Guest {
        port,
        key: PathBuf::from(&key),
    };
    wait_for("ssh", 300 * slow, || guest.out("true").is_some())?;
    guest.scp_to(&[
        &root.join("tests/graphics/sway.conf"),
        &root.join("tests/graphics/run-sway.sh"),
    ])?;
    if install {
        eprintln!("xtask: installing packages");
        let s = format!("env ASSUME_ALWAYS_YES=yes IGNORE_OSVERSION=yes pkg install -y {PACKAGES}");
        util::run(&mut guest.ssh(&s))?;
    }
    guest
        .out("chmod +x /root/run-sway.sh; (nohup /root/run-sway.sh >/dev/null 2>&1 &)")
        .ok_or("could not start run-sway.sh")?;
    wait_for("the compositor and its clients", 240 * slow, || {
        guest
            .out("pgrep glmark2-es2-wayland >/dev/null && pgrep foot >/dev/null && echo up")
            .is_some_and(|o| o.contains("up"))
    })?;
    // Let a few frames draw, then capture as the acceptance does.
    std::thread::sleep(Duration::from_secs(15 * slow));
    let shot = out.join(format!("accept-{arch}.png"));
    guest
        .out(&format!("{WAYLAND_ENV} grim /root/accept.png"))
        .ok_or("grim failed")?;
    guest.scp_from("/root/accept.png", &shot)?;
    let log = guest
        .out("cat /var/log/sway.log")
        .ok_or("no /var/log/sway.log")?;
    fs::write(out.join("sway.log"), &log)?;
    let gl = guest
        .out(&format!(
            "{WAYLAND_ENV} glmark2-es2-wayland -b jellyfish:duration=2 2>&1 | grep -E 'GL_RENDERER|FPS'"
        ))
        .unwrap_or_default();
    fs::write(out.join("glmark2.txt"), &gl)?;

    let png = fs::read(&shot)?;
    let checks = [
        (
            "the capture is a PNG of a drawn screen",
            png.starts_with(b"\x89PNG") && png.len() > 40_000,
        ),
        (
            "Sway used the pixman renderer",
            log.contains("Creating pixman renderer"),
        ),
        (
            "Sway's modeset commit succeeded",
            log.contains("Commit of 1 outputs succeeded"),
        ),
        ("glmark2 drew on llvmpipe", gl.contains("llvmpipe")),
    ];
    let mut failed = false;
    for (what, ok) in checks {
        println!("{}: {what}", if ok { "ok" } else { "FAIL" });
        failed |= !ok;
    }
    if let Some(l) = gl.lines().find(|l| l.contains("GL_RENDERER")) {
        println!("{}", l.trim());
    }
    println!("capture: {}", shot.display());
    let _ = guest.out("poweroff");
    drop(machine);
    let _ = fs::remove_file(&qmp);
    if failed {
        return Err("the graphics acceptance run failed".into());
    }
    println!("graphics: {arch}: PASS");
    Ok(())
}
