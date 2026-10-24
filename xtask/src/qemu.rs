// SPDX-License-Identifier: BSD-2-Clause
//! Running images under QEMU (both architectures) or bhyve (amd64, on a
//! FreeBSD host), interactively or captured for tests.

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

const BHYVE_CODE: &str = "/usr/local/share/uefi-firmware/BHYVE_UEFI_CODE.fd";
const BHYVE_VARS: &str = "/usr/local/share/uefi-firmware/BHYVE_UEFI_VARS.fd";

/// Which hypervisor runs the machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Vmm {
    Qemu,
    /// bhyve, through `doas`; amd64 guests on FreeBSD hosts only.
    Bhyve,
}

impl Vmm {
    pub(crate) fn parse(s: &str) -> Result<Self> {
        match s {
            "qemu" => Ok(Self::Qemu),
            "bhyve" => Ok(Self::Bhyve),
            _ => Err(format!("unknown VMM `{s}` (qemu, bhyve)").into()),
        }
    }

    /// Whether this host can run the VMM for `arch`.
    pub(crate) fn available(self, arch: Arch) -> bool {
        match self {
            Self::Qemu => true,
            Self::Bhyve => cfg!(target_os = "freebsd") && arch == Arch::X86_64,
        }
    }
}

/// One virtual machine to start.
#[derive(Debug)]
pub(crate) struct Vm {
    pub(crate) vmm: Vmm,
    pub(crate) arch: Arch,
    pub(crate) disk: PathBuf,
    /// The boot disk is an NVMe controller's namespace, not virtio-blk.
    pub(crate) nvme: bool,
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
    /// Let a guest reset restart the machine instead of ending QEMU.
    pub(crate) allow_reset: bool,
    /// A second raw disk (bhyve slot 4, or a second virtio-blk on QEMU).
    pub(crate) extra_disk: Option<PathBuf>,
    /// bhyve: pin vCPU *i* to host CPU [`PIN_STRIDE`]` * i`, one per
    /// physical core (docs/PERFORMANCE.md §3).
    pub(crate) pin: bool,
    /// QEMU x86 machine type instead of `q35` (for example `pc`).
    pub(crate) machine: Option<String>,
    /// Extra devices: bhyve `-s` slot specifications (`5,nvme,ram=64`) or
    /// QEMU `-device`/`-drive` argument pairs, passed through as given.
    pub(crate) devices: Vec<String>,
    /// The USB and input set ([`USB_SLOT`] on bhyve, [`Vm::usb_args`] on
    /// QEMU).
    pub(crate) usb: bool,
}

/// bhyve's xHCI slot: its only USB device is a tablet.
pub(crate) const USB_SLOT: &str = "30,xhci,tablet";

/// The size of each USB scratch disk.
const USB_DISK_SIZE: u64 = 16 << 20;

/// Host CPUs per physical core on the build server (P-cores have SMT).
pub(crate) const PIN_STRIDE: u32 = 2;

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
    let usb = args.take_flag("--usb");
    let qmp = args.take_value("--qmp")?;
    let vmm = Vmm::parse(&args.take_value("--vmm")?.unwrap_or_else(|| "qemu".into()))?;
    if !vmm.available(arch) {
        return Err(format!("{vmm:?} cannot run {arch} guests on this host").into());
    }
    // A fixed kernel address makes debugger sessions repeatable.
    let default_cmdline = if gdb { "kaslr=0" } else { "" };
    let cmdline = args
        .take_value("--cmdline")?
        .unwrap_or_else(|| default_cmdline.into());
    args.finish()?;

    let disk = image::build(arch, opts, &cmdline)?;
    let vm = Vm {
        smp,
        mem,
        gdb,
        display,
        el2,
        usb,
        ..Vm::new(vmm, arch, disk)
    };

    let mut cmd = vm.command()?;
    if vmm == Vmm::Qemu {
        cmd.arg("-serial").arg("mon:stdio");
        if let Some(sock) = &qmp {
            let _ = fs::remove_file(sock);
            cmd.arg("-qmp")
                .arg(format!("unix:{sock},server=on,wait=off"));
        }
    } else if qmp.is_some() {
        return Err("--qmp needs QEMU".into());
    }
    if gdb {
        let kernel = build::kernel_path(arch, opts);
        let gdb = match arch {
            Arch::X86_64 => "gdb",
            Arch::Aarch64 => "gdb-multiarch (or gdb with aarch64 support)",
        };
        eprintln!("xtask: QEMU is waiting for a debugger on localhost:1234; the kernel is unslid");
        eprintln!(
            "xtask:   {gdb} {} -ex 'target remote localhost:1234'",
            kernel.display()
        );
    }
    match vmm {
        Vmm::Qemu => eprintln!("xtask: starting QEMU ({arch}); Ctrl-A X quits"),
        Vmm::Bhyve => eprintln!("xtask: starting bhyve ({arch}); the guest powering off ends it"),
    }
    let result = util::run(&mut cmd);
    vm.cleanup();
    // bhyve exits 1 when the guest powers off, which is success here.
    match (vmm, result) {
        (Vmm::Bhyve, Err(e)) if e.to_string().contains("exit status: 1") => Ok(()),
        (_, r) => r,
    }
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
    /// A machine with the defaults: 4 CPUs, 1 GiB, no test devices.
    pub(crate) fn new(vmm: Vmm, arch: Arch, disk: PathBuf) -> Self {
        Self {
            vmm,
            arch,
            disk,
            nvme: false,
            smp: 4,
            mem: "1G".into(),
            gdb: false,
            display: false,
            el2: false,
            test_devices: false,
            allow_reset: false,
            extra_disk: None,
            pin: false,
            machine: None,
            devices: Vec::new(),
            usb: false,
        }
    }

    /// The hypervisor command line. For QEMU, without a serial backend.
    pub(crate) fn command(&self) -> Result<Command> {
        match self.vmm {
            Vmm::Qemu => self.qemu_command(),
            Vmm::Bhyve => self.bhyve_command(),
        }
    }

    /// The bhyve instance name: unique per xtask process.
    fn bhyve_name(&self) -> String {
        format!("abyss-{}", std::process::id())
    }

    fn bhyve_command(&self) -> Result<Command> {
        if !self.vmm.available(self.arch) {
            return Err("bhyve runs amd64 guests on FreeBSD hosts only".into());
        }
        let vars = self.disk.with_extension("bhyve-vars.fd");
        fs::copy(BHYVE_VARS, &vars)
            .map_err(|e| format!("{BHYVE_VARS}: {e} (pkg install edk2-bhyve)"))?;
        // A stale instance with our name would make bhyve refuse to start.
        self.cleanup();
        let status = Command::new("doas").args(["kldload", "-n", "vmm"]).status();
        if !status.is_ok_and(|s| s.success()) {
            return Err("cannot load vmm.ko (doas kldload vmm)".into());
        }
        let mut c = Command::new("doas");
        c.arg("bhyve");
        c.arg("-c").arg(self.smp.to_string());
        c.arg("-m").arg(&self.mem);
        // Yield on HLT, tolerate unimplemented MSRs, destroy on power-off,
        // and keep the RTC in UTC (what FreeBSD assumes without
        // /etc/wall_cmos_clock).
        c.args(["-H", "-w", "-D", "-u"]);
        c.args(["-s", "0,hostbridge", "-s", "31,lpc", "-l", "com1,stdio"]);
        c.arg("-l")
            .arg(format!("bootrom,{BHYVE_CODE},{}", vars.display()));
        let model = if self.nvme { "nvme" } else { "virtio-blk" };
        c.arg("-s")
            .arg(format!("2,{model},{}", self.disk.display()));
        c.args(["-s", "3,virtio-rnd"]);
        if let Some(d) = &self.extra_disk {
            c.arg("-s").arg(format!("4,virtio-blk,{}", d.display()));
        }
        for d in &self.devices {
            c.arg("-s").arg(d);
        }
        if self.usb {
            c.arg("-s").arg(USB_SLOT);
        }
        if self.pin {
            for v in 0..self.smp {
                c.arg("-p").arg(format!("{v}:{}", v * PIN_STRIDE));
            }
        }
        // A framebuffer so the guest gets a GOP; VNC on a per-process port.
        let port = 5900 + std::process::id() % 1000;
        c.arg("-s")
            .arg(format!("29,fbuf,tcp=127.0.0.1:{port},w=1024,h=768"));
        c.arg(self.bhyve_name());
        Ok(c)
    }

    /// Releases hypervisor state a run may leave behind.
    pub(crate) fn cleanup(&self) {
        if self.vmm == Vmm::Bhyve {
            let _ = Command::new("doas")
                .args(["bhyvectl", "--destroy"])
                .arg(format!("--vm={}", self.bhyve_name()))
                .stderr(Stdio::null())
                .status();
        }
    }

    fn qemu_command(&self) -> Result<Command> {
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
            Arch::X86_64 => c.args(["-machine", self.machine.as_deref().unwrap_or("q35")]),
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
        if self.arch == Arch::X86_64 {
            // The default VGA, in the slot it would take (before the
            // disks), with an id that input devices can be bound to.
            c.args(["-vga", "none", "-device", "VGA,id=video0"]);
        }
        c.arg("-drive").arg(format!(
            "if=none,id=disk0,format=raw,file={}",
            self.disk.display()
        ));
        if self.nvme {
            c.args(["-device", "nvme,serial=abyssboot,drive=disk0,bootindex=0"]);
        } else {
            c.args(["-device", "virtio-blk-pci,drive=disk0,bootindex=0"]);
        }
        if let Some(d) = &self.extra_disk {
            c.arg("-drive")
                .arg(format!("if=none,id=disk1,format=raw,file={}", d.display()));
            c.args(["-device", "virtio-blk-pci,drive=disk1"]);
        }
        // Gives firmware an EFI_RNG_PROTOCOL, the loader's KASLR source.
        // It sits behind a PCIe root port so a PCI-PCI bridge, and
        // interrupts through one, are always under test.
        // (Conventional PCI machines such as `pc` have no root ports.)
        if self.machine.is_none() {
            c.args(["-device", "pcie-root-port,id=rp0,chassis=1,slot=1"]);
            c.args(["-device", "virtio-rng-pci,bus=rp0"]);
        } else {
            c.args(["-device", "virtio-rng-pci"]);
        }
        if self.arch == Arch::Aarch64 {
            // `virt` has no display; ramfb gives edk2 a linear-framebuffer
            // GOP (virtio-gpu's GOP is blit-only).
            c.args(["-device", "ramfb,id=video0"]);
        }
        c.args(&self.devices);
        if self.usb {
            c.args(self.usb_args()?);
        }
        c.args(["-nic", "none"]);
        if !self.allow_reset {
            c.arg("-no-reboot");
        }
        if !self.display {
            c.args(["-display", "none"]);
        }
        if self.gdb {
            c.args(["-s", "-S"]);
        } else if std::env::var_os("ABYSS_QEMU_GDB").is_some() {
            // A stub to attach to a hung test run (not waiting at start).
            c.arg("-s");
        }
        Ok(c)
    }

    /// The USB and input devices: an xHCI with a keyboard, a tablet, a hub
    /// with a mouse behind it, a Bulk-Only disk and a UAS disk (blank
    /// scratch images beside the boot disk), and virtio keyboard and
    /// tablet.
    ///
    /// The virtio devices are bound to the display (`video0`), so QEMU
    /// sends them only input addressed to it (`xtask qmp --device
    /// video0`), and everything else to the USB ones.
    ///
    /// Ports are fixed so names and locations stay stable. Each of the
    /// xHCI's eight ports takes either speed: the keyboard and tablet run
    /// at high speed, the hub (QEMU's is USB 1.1) and its mouse at full
    /// speed, and both disks at SuperSpeed.
    fn usb_args(&self) -> Result<Vec<String>> {
        let scratch = |what: &str| -> Result<PathBuf> {
            let p = self.disk.with_extension(format!("usb-{what}.img"));
            fs::File::create(&p)?.set_len(USB_DISK_SIZE)?;
            Ok(p)
        };
        let bbb = scratch("bbb")?;
        let uas = scratch("uas")?;
        let bbb = format!("if=none,id=usbbbb,format=raw,file={}", bbb.display());
        let uas = format!("if=none,id=usbuas,format=raw,file={}", uas.display());
        let args = [
            "-device",
            "qemu-xhci,id=xhci,p2=8,p3=8",
            "-device",
            "usb-kbd,bus=xhci.0,port=1",
            "-device",
            "usb-tablet,bus=xhci.0,port=2",
            "-device",
            "usb-hub,bus=xhci.0,port=3",
            "-device",
            "usb-mouse,bus=xhci.0,port=3.1",
            "-drive",
            &bbb,
            "-device",
            "usb-storage,bus=xhci.0,port=4,drive=usbbbb,serial=abyssbbb",
            "-device",
            "usb-uas,id=uas0,bus=xhci.0,port=5",
            "-drive",
            &uas,
            "-device",
            "scsi-hd,bus=uas0.0,scsi-id=0,lun=0,drive=usbuas,serial=abyssuas",
            "-device",
            "virtio-keyboard-pci,id=vkbd,display=video0",
            "-device",
            "virtio-tablet-pci,id=vtablet,display=video0",
        ];
        Ok(args.iter().map(|&a| a.to_owned()).collect())
    }

    /// Runs headless with the serial console captured to `log`, until the
    /// image reports a ktest verdict, exits, or `timeout` passes.
    pub(crate) fn run_captured(
        &self,
        timeout: Duration,
        log: &Path,
        echo: bool,
    ) -> Result<Captured> {
        self.run_until(timeout, log, echo, &ktest_verdict)
    }

    /// As [`Self::run_captured`], with `verdict` judging each console line.
    pub(crate) fn run_until(
        &self,
        timeout: Duration,
        log: &Path,
        echo: bool,
        verdict_of: &dyn Fn(&str) -> Option<Outcome>,
    ) -> Result<Captured> {
        let mut cmd = self.command()?;
        // A QMP socket, for tests that press the power button.
        let qmp = (self.vmm == Vmm::Qemu && self.test_devices).then(|| {
            let sock = log.with_extension("qmp");
            let _ = fs::remove_file(&sock);
            sock
        });
        if self.vmm == Vmm::Qemu {
            cmd.args(["-serial", "stdio", "-monitor", "none"]);
        }
        if let Some(sock) = &qmp {
            cmd.arg("-qmp")
                .arg(format!("unix:{},server=on,wait=off", sock.display()));
        }
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
                    verdict = verdict_of(&line);
                    if let Some(sock) = qmp
                        .as_ref()
                        .filter(|_| line.contains(ktest::POWER_BUTTON_MARKER))
                        && let Err(e) = press_power_button(sock)
                    {
                        println!("  (could not press the power button: {e})");
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => verdict = Some(Outcome::TimedOut),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    verdict = Some(Outcome::Failed("QEMU exited without a verdict".into()));
                }
            }
        }
        // A hang: record where every CPU is, for symbolizing.
        if matches!(verdict, Some(Outcome::TimedOut))
            && let Some(sock) = &qmp
        {
            match qmp_command(sock, "info registers -a") {
                Ok(regs) => {
                    writeln!(logfile, "xtask: timed out; CPU registers:")?;
                    writeln!(logfile, "{regs}")?;
                }
                Err(e) => writeln!(logfile, "xtask: timed out; no registers ({e})")?,
            }
        }
        // Give the image a moment to power off by itself, then make sure;
        // what it prints meanwhile (how it powered off) is logged too.
        let grace = Instant::now() + Duration::from_secs(5);
        while Instant::now() < grace && child.try_wait()?.is_none() {
            if let Ok(line) = rx.recv_timeout(Duration::from_millis(50)) {
                let line = strip_ansi(&line);
                writeln!(logfile, "{line}")?;
                if echo {
                    println!("  | {line}");
                }
            }
        }
        // And whatever was still in flight when it exited.
        while let Ok(line) = rx.recv_timeout(Duration::from_millis(100)) {
            writeln!(logfile, "{}", strip_ansi(&line))?;
        }
        let exit_code = match child.try_wait()? {
            Some(status) => status.code(),
            None => {
                // Under doas, killing the child would leave bhyve running.
                self.cleanup();
                let _ = child.kill();
                let _ = child.wait();
                None
            }
        };
        self.cleanup();
        let _ = reader.join();
        Ok(Captured {
            outcome: verdict.unwrap_or(Outcome::TimedOut),
            exit_code,
        })
    }
}

/// Runs a human monitor command through the QMP socket; returns its
/// output.
fn qmp_command(sock: &Path, command: &str) -> Result<String> {
    use std::io::Read;
    use std::os::unix::net::UnixStream;
    let mut s = UnixStream::connect(sock)?;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut buf = vec![0u8; 1 << 20];
    let _ = s.read(&mut buf)?;
    writeln!(s, r#"{{"execute":"qmp_capabilities"}}"#)?;
    let _ = s.read(&mut buf)?;
    writeln!(
        s,
        r#"{{"execute":"human-monitor-command","arguments":{{"command-line":"{command}"}}}}"#
    )?;
    let mut out = Vec::new();
    // The reply can span reads; it ends with a newline after the JSON.
    loop {
        let n = s.read(&mut buf)?;
        out.extend_from_slice(&buf[..n]);
        if n == 0 || out.ends_with(b"\n") {
            break;
        }
    }
    let text = String::from_utf8_lossy(&out);
    Ok(text.replace("\\r\\n", "\n").replace("\\n", "\n"))
}

/// Presses the VM's power button through its QMP socket.
fn press_power_button(sock: &Path) -> Result<()> {
    use std::io::Read;
    use std::os::unix::net::UnixStream;
    let mut s = UnixStream::connect(sock)?;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut buf = [0u8; 4096];
    // The greeting, then each command's reply.
    let _ = s.read(&mut buf)?;
    for cmd in [
        r#"{"execute":"qmp_capabilities"}"#,
        r#"{"execute":"system_powerdown"}"#,
    ] {
        writeln!(s, "{cmd}")?;
        let _ = s.read(&mut buf)?;
    }
    Ok(())
}

/// The ktest pass and fail markers.
fn ktest_verdict(line: &str) -> Option<Outcome> {
    if line.contains(ktest::PASS_MARKER) {
        Some(Outcome::Passed)
    } else if line.contains(ktest::FAIL_MARKER) {
        Some(Outcome::Failed("image reported failure".into()))
    } else {
        None
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
