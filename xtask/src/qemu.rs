// SPDX-License-Identifier: BSD-2-Clause
//! Running images under QEMU, interactively or captured for tests.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::arch::Arch;
use crate::build;
use crate::cli::Args;
use crate::image;
use crate::util::{self, Result};

/// Firmware search paths: Fedora, Debian/Ubuntu, FreeBSD, QEMU's own.
const X86_64_CODE: &[&str] = &[
    "/usr/share/edk2/ovmf/OVMF_CODE.fd",
    "/usr/share/OVMF/OVMF_CODE.fd",
    "/usr/share/OVMF/OVMF_CODE_4M.fd",
    "/usr/local/share/qemu/edk2-x86_64-code.fd",
    "/usr/share/qemu/edk2-x86_64-code.fd",
];
const X86_64_VARS: &[&str] = &[
    "/usr/share/edk2/ovmf/OVMF_VARS.fd",
    "/usr/share/OVMF/OVMF_VARS.fd",
    "/usr/share/OVMF/OVMF_VARS_4M.fd",
    "/usr/local/share/qemu/edk2-i386-vars.fd",
    "/usr/share/qemu/edk2-i386-vars.fd",
];
const AARCH64_CODE: &[&str] = &[
    // The silent build keeps firmware debug chatter out of test logs.
    "/usr/share/edk2/aarch64/QEMU_EFI-silent-pflash.raw",
    "/usr/share/edk2/aarch64/QEMU_EFI-pflash.raw",
    "/usr/share/AAVMF/AAVMF_CODE.fd",
    "/usr/local/share/qemu/edk2-aarch64-code.fd",
    "/usr/share/qemu/edk2-aarch64-code.fd",
];
const AARCH64_VARS: &[&str] = &[
    "/usr/share/edk2/aarch64/vars-template-pflash.raw",
    "/usr/share/AAVMF/AAVMF_VARS.fd",
    "/usr/local/share/qemu/edk2-arm-vars.fd",
    "/usr/share/qemu/edk2-arm-vars.fd",
];

/// One virtual machine to start.
#[derive(Debug)]
pub(crate) struct Vm {
    pub(crate) arch: Arch,
    pub(crate) disk: PathBuf,
    pub(crate) smp: u32,
    pub(crate) mem: String,
    pub(crate) gdb: bool,
    pub(crate) display: bool,
    /// aarch64: firmware runs at EL2 (`virtualization=on`), so the kernel
    /// takes its EL2 entry path.
    pub(crate) el2: bool,
    /// Attach the devices tests report through: `isa-debug-exit` on x86,
    /// semihosting on aarch64.
    pub(crate) test_devices: bool,
}

/// A captured run: the verdict and QEMU's exit status, if it exited by
/// itself.
#[derive(Debug)]
pub(crate) struct Captured {
    pub(crate) outcome: Outcome,
    pub(crate) exit_code: Option<i32>,
}

/// How a captured run ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The image printed the pass marker.
    Passed,
    /// The image printed the fail marker or exited without passing.
    Failed(String),
    /// No verdict before the deadline.
    TimedOut,
}

pub(crate) fn cmd_run(mut args: Args) -> Result<()> {
    let arch = Arch::one_from_args(&mut args)?;
    let opts = build::Options::from_args(&mut args);
    let smp = args
        .take_value("--smp")?
        .map_or(Ok(4), |s| s.parse())
        .map_err(|_| "bad --smp")?;
    let mem = args.take_value("--mem")?.unwrap_or_else(|| "2G".into());
    let gdb = args.take_flag("--gdb");
    let display = args.take_flag("--display");
    let el2 = args.take_flag("--el2");
    // A fixed kernel address makes debugger sessions repeatable.
    let default_cmdline = if gdb { "kaslr=0" } else { "" };
    let cmdline = args
        .take_value("--cmdline")?
        .unwrap_or_else(|| default_cmdline.into());
    args.finish()?;

    let disk = image::build(arch, opts, &cmdline)?;
    let vm = Vm {
        arch,
        disk,
        smp,
        mem,
        gdb,
        display,
        el2,
        test_devices: false,
    };
    let mut cmd = vm.command()?;
    cmd.arg("-serial").arg("mon:stdio");
    if gdb {
        eprintln!("xtask: QEMU is waiting for a debugger on localhost:1234");
    }
    eprintln!("xtask: starting QEMU ({arch}); Ctrl-A X quits");
    util::run(&mut cmd)
}

fn find(env: &str, candidates: &[&str]) -> Result<PathBuf> {
    if let Some(p) = std::env::var_os(env) {
        return Ok(PathBuf::from(p));
    }
    candidates
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
        .ok_or_else(|| format!("no UEFI firmware found; install edk2 or set {env}").into())
}

/// Whether hardware virtualization is usable for `arch` on this host.
fn kvm_available(arch: Arch) -> bool {
    let native = matches!(
        (arch, std::env::consts::ARCH),
        (Arch::X86_64, "x86_64") | (Arch::Aarch64, "aarch64")
    );
    native
        && cfg!(target_os = "linux")
        && fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .is_ok()
}

impl Vm {
    /// The QEMU command line, without a serial backend.
    fn command(&self) -> Result<Command> {
        let (bin, code, vars) = match self.arch {
            Arch::X86_64 => (
                "qemu-system-x86_64",
                find("ABYSS_FW_CODE", X86_64_CODE)?,
                find("ABYSS_FW_VARS", X86_64_VARS)?,
            ),
            Arch::Aarch64 => (
                "qemu-system-aarch64",
                find("ABYSS_FW_CODE", AARCH64_CODE)?,
                find("ABYSS_FW_VARS", AARCH64_VARS)?,
            ),
        };
        // Firmware writes its variable store, so each run gets a fresh copy.
        let vars_copy = self.disk.with_extension("vars.fd");
        fs::copy(&vars, &vars_copy).map_err(|e| format!("{}: {e}", vars.display()))?;

        let mut c = Command::new(bin);
        match self.arch {
            Arch::X86_64 => c.args(["-machine", "q35"]),
            Arch::Aarch64 if self.el2 => {
                c.args(["-machine", "virt,gic-version=3,acpi=on,virtualization=on"])
            }
            Arch::Aarch64 => c.args(["-machine", "virt,gic-version=3,acpi=on"]),
        };
        if self.test_devices {
            match self.arch {
                Arch::X86_64 => c.args(["-device", "isa-debug-exit,iobase=0xf4,iosize=0x04"]),
                Arch::Aarch64 => c.args(["-semihosting-config", "enable=on,target=native"]),
            };
        }
        // EL2 for the guest needs TCG (KVM would need nested virtualization).
        if kvm_available(self.arch) && !self.el2 {
            c.args(["-accel", "kvm", "-cpu", "host"]);
        } else {
            c.args(["-accel", "tcg", "-cpu", "max"]);
        }
        c.arg("-smp").arg(self.smp.to_string());
        c.arg("-m").arg(&self.mem);
        c.arg("-drive").arg(pflash(0, &code, true));
        c.arg("-drive").arg(pflash(1, &vars_copy, false));
        c.arg("-drive").arg(format!(
            "if=none,id=disk0,format=raw,file={}",
            self.disk.display()
        ));
        c.args(["-device", "virtio-blk-pci,drive=disk0,bootindex=0"]);
        // Gives firmware an EFI_RNG_PROTOCOL, the loader's KASLR source.
        c.args(["-device", "virtio-rng-pci"]);
        if self.arch == Arch::Aarch64 {
            // `virt` has no display; ramfb gives edk2 a linear-framebuffer
            // GOP (virtio-gpu's GOP is blit-only).
            c.args(["-device", "ramfb"]);
        }
        c.args(["-nic", "none", "-no-reboot"]);
        if !self.display {
            c.args(["-display", "none"]);
        }
        if self.gdb {
            c.args(["-s", "-S"]);
        }
        Ok(c)
    }

    /// Runs headless with the serial console captured to `log`, until the
    /// image reports a verdict, exits, or `timeout` passes.
    pub(crate) fn run_captured(
        &self,
        timeout: Duration,
        log: &Path,
        echo: bool,
    ) -> Result<Captured> {
        let mut cmd = self.command()?;
        cmd.args(["-serial", "stdio", "-monitor", "none"]);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = cmd.spawn().map_err(|e| format!("cannot start QEMU: {e}"))?;
        let stdout = child.stdout.take().ok_or("QEMU stdout not captured")?;

        let (tx, rx) = mpsc::channel::<String>();
        let reader = std::thread::spawn(move || {
            for line in BufReader::new(stdout).split(b'\n') {
                let Ok(line) = line else { break };
                if tx
                    .send(String::from_utf8_lossy(&line).into_owned())
                    .is_err()
                {
                    break;
                }
            }
        });

        let mut logfile = fs::File::create(log)?;
        let deadline = Instant::now() + timeout;
        let mut verdict = None;
        while verdict.is_none() {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(line) => {
                    let line = strip_ansi(&line);
                    writeln!(logfile, "{line}")?;
                    if echo {
                        println!("  | {line}");
                    }
                    if line.contains(ktest::PASS_MARKER) {
                        verdict = Some(Outcome::Passed);
                    } else if line.contains(ktest::FAIL_MARKER) {
                        verdict = Some(Outcome::Failed("image reported failure".into()));
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => verdict = Some(Outcome::TimedOut),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    verdict = Some(Outcome::Failed("QEMU exited without a verdict".into()));
                }
            }
        }
        // Give the image a moment to power off by itself, then make sure.
        let grace = Instant::now() + Duration::from_secs(5);
        while Instant::now() < grace && child.try_wait()?.is_none() {
            std::thread::sleep(Duration::from_millis(50));
        }
        let exit_code = match child.try_wait()? {
            Some(status) => status.code(),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                None
            }
        };
        let _ = reader.join();
        Ok(Captured {
            outcome: verdict.unwrap_or(Outcome::TimedOut),
            exit_code,
        })
    }
}

fn pflash(unit: u32, path: &Path, readonly: bool) -> String {
    let ro = if readonly { ",readonly=on" } else { "" };
    format!(
        "if=pflash,format=raw,unit={unit}{ro},file={}",
        path.display()
    )
}

/// Removes ANSI escape sequences and carriage returns from firmware output.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => {
                if chars.peek() == Some(&'[') {
                    chars.next();
                    // Parameters end at the first letter.
                    for c in chars.by_ref() {
                        if c.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
            }
            '\r' => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_escapes() {
        assert_eq!(
            strip_ansi("\u{1b}[0m\u{1b}[37;40mBdsDxe: ok\r"),
            "BdsDxe: ok"
        );
        assert_eq!(strip_ansi("plain"), "plain");
    }
}
