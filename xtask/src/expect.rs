// SPDX-License-Identifier: BSD-2-Clause
//! Scripted serial-console sessions: send a line, expect a pattern.
//!
//! The boot-to-shell test (ROADMAP P4.7) drives `/rescue/sh` this way. The
//! same script runs against FreeBSD (`cargo xtask --remote freebsd-shell`),
//! which keeps it honest: every step it expects is what FreeBSD 15.1 does.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::arch::Arch;
use crate::cli::Args;
use crate::qemu::{Vm, Vmm};
use crate::util::Result;

/// One step: send `send`, wait for `expect`, then (unless a job is left
/// running in the foreground) for the prompt. Waiting for the prompt keeps
/// input from arriving before the shell reads it, which the terminal would
/// echo twice.
pub(crate) struct Step {
    pub(crate) send: &'static str,
    pub(crate) expect: &'static str,
    pub(crate) prompt: bool,
    /// What the step checks, for the failure message.
    pub(crate) what: &'static str,
    /// How long to let the system settle before the next step.
    pub(crate) settle: Duration,
}

const fn step(send: &'static str, expect: &'static str, what: &'static str) -> Step {
    Step {
        send,
        expect,
        prompt: true,
        what,
        settle: Duration::ZERO,
    }
}

/// A step that leaves a job in the foreground.
const fn job(send: &'static str, expect: &'static str, what: &'static str) -> Step {
    Step {
        send,
        expect,
        prompt: false,
        what,
        settle: Duration::ZERO,
    }
}

/// A job step after which the shell still has work to do (`fg` prints the
/// job before giving it the terminal): the next keystroke waits a moment,
/// or it would reach the shell instead of the job. Emulated systems are
/// slow enough to lose that race.
const fn job_then_settle(send: &'static str, expect: &'static str, what: &'static str) -> Step {
    Step {
        send,
        expect,
        prompt: false,
        what,
        settle: Duration::from_secs(1),
    }
}

/// The prompt the script sets. Its command line is quoted so that the
/// echoed command never matches it.
const PROMPT: &str = "@sh> ";

/// The boot-to-shell script (ROADMAP P4 exit). It starts at the first `#`
/// prompt of `/rescue/sh`, with `/rescue` holding the commands.
pub(crate) const SHELL: &[Step] = &[
    step("PS1='@''sh> ' PATH=/rescue\n", "\n", "set the prompt"),
    step("echo $((6 * 7))\n", "\n42\r\n", "arithmetic"),
    step("echo pipe | cat | cat\n", "\npipe\r\n", "a pipeline"),
    step("ls /rescue\n", "sysctl", "ls"),
    step(
        "sysctl kern.ostype\n",
        "\nkern.ostype: FreeBSD\r\n",
        "sysctl",
    ),
    step("ps -o pid,comm\n", "PID COMMAND", "ps"),
    // The terminal echoes input when it arrives, before the shell has made
    // the job the foreground; the job says when it is running, and only
    // then is ^Z sent (sent earlier, it would reach the shell).
    job(
        "sh -c 'echo running; exec sleep 60'\n",
        "\nrunning\r\n",
        "start a job",
    ),
    step("\x1a", "^Z[1] + Suspended", "^Z stops it"),
    step("jobs\n", "\n[1] + Suspended", "jobs lists it"),
    job_then_settle("fg\n", "sleep 60", "fg resumes it"),
    step("\x03", "\n", "^C ends it"),
    step("jobs; echo done\n", "\ndone\r\n", "no jobs left"),
];

/// A VM whose serial console is a pipe pair.
pub(crate) struct Session {
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<Vec<u8>>,
    /// Console output not yet consumed by a match.
    pending: Vec<u8>,
    log: fs::File,
    vm: Vm,
}

impl Session {
    /// Starts `vm` with its console captured; the whole console goes to
    /// `log`.
    pub(crate) fn start(vm: Vm, log: &Path) -> Result<Self> {
        let mut cmd = vm.command()?;
        if vm.vmm == Vmm::Qemu {
            cmd.args(["-serial", "stdio", "-monitor", "none"]);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("cannot start the VM: {e}"))?;
        let stdin = child.stdin.take().ok_or("VM stdin not captured")?;
        let mut stdout = child.stdout.take().ok_or("VM stdout not captured")?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = stdout.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            rx,
            pending: Vec::new(),
            log: fs::File::create(log)?,
            vm,
        })
    }

    pub(crate) fn send(&mut self, text: &str) -> Result<()> {
        self.stdin.write_all(text.as_bytes())?;
        self.stdin.flush()?;
        Ok(())
    }

    /// Waits up to `timeout` for `pattern`; consumes the output through
    /// it. On a timeout, the error shows the output that did arrive.
    pub(crate) fn expect(&mut self, pattern: &str, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        let needle = pattern.as_bytes();
        loop {
            if let Some(at) = self.pending.windows(needle.len()).position(|w| w == needle) {
                self.pending.drain(..at + needle.len());
                return Ok(());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(bytes) => {
                    self.log.write_all(&bytes)?;
                    self.pending.extend_from_slice(&bytes);
                }
                Err(e) => {
                    let tail = &self.pending[self.pending.len().saturating_sub(300)..];
                    let why = match e {
                        mpsc::RecvTimeoutError::Timeout => "timed out",
                        mpsc::RecvTimeoutError::Disconnected => "the VM exited",
                    };
                    return Err(format!(
                        "{why} waiting for {pattern:?}; last output: {:?}",
                        String::from_utf8_lossy(tail)
                    )
                    .into());
                }
            }
        }
    }

    /// Runs `steps`, each within `timeout`.
    pub(crate) fn script(&mut self, steps: &[Step], timeout: Duration) -> Result<()> {
        for (i, s) in steps.iter().enumerate() {
            self.send(s.send)?;
            self.expect(s.expect, timeout)
                .and_then(|()| {
                    if s.prompt {
                        self.expect(PROMPT, timeout)
                    } else {
                        Ok(())
                    }
                })
                .map_err(|e| format!("step {} ({}): {e}", i + 1, s.what))?;
            std::thread::sleep(s.settle);
        }
        Ok(())
    }

    /// Waits up to `timeout` for the VM to exit by itself (the rest of its
    /// output is logged), then stops it.
    pub(crate) fn finish(mut self, timeout: Duration) -> Result<bool> {
        let deadline = Instant::now() + timeout;
        let mut exited = false;
        while Instant::now() < deadline {
            if self.child.try_wait()?.is_some() {
                exited = true;
                break;
            }
            if let Ok(bytes) = self.rx.recv_timeout(Duration::from_millis(50)) {
                self.log.write_all(&bytes)?;
            }
        }
        while let Ok(bytes) = self.rx.recv_timeout(Duration::from_millis(100)) {
            self.log.write_all(&bytes)?;
        }
        if !exited {
            // Under doas, killing the child would leave bhyve running.
            self.vm.cleanup();
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
        self.vm.cleanup();
        Ok(exited)
    }
}

/// `cargo xtask freebsd-shell`: runs [`SHELL`] against FreeBSD 15.1's
/// `/rescue/sh` in bhyve, proving the script and this harness.
pub(crate) fn cmd_freebsd_shell(args: Args) -> Result<()> {
    args.finish()?;
    if !cfg!(target_os = "freebsd") {
        return Err("freebsd-shell needs bhyve: run it on the build server".into());
    }
    let (image, work) = crate::perf::freebsd_single_user_image()?;
    let work: PathBuf = work.join("shell");
    fs::create_dir_all(&work)?;
    let vm = Vm {
        smp: 4,
        mem: "2G".into(),
        ..Vm::new(Vmm::Bhyve, Arch::X86_64, image)
    };
    let log = work.join("console.log");
    let mut session = Session::start(vm, &log)?;
    // Single-user init asks for a shell; leaving it goes on to multi-user,
    // where rc.local, with no benchmark disk, powers off.
    let result = (|| -> Result<()> {
        session.expect("RETURN for /bin/sh:", Duration::from_secs(180))?;
        session.send("/rescue/sh\n")?;
        session.expect("# ", Duration::from_secs(30))?;
        session.script(SHELL, Duration::from_secs(30))?;
        session.send("exit\n")
    })();
    let exited = session.finish(Duration::from_secs(180))?;
    result.map_err(|e| format!("{e} (console: {})", log.display()))?;
    if !exited {
        return Err(format!("FreeBSD did not power off; see {}", log.display()).into());
    }
    println!(
        "freebsd-shell: {} steps passed against FreeBSD; console in {}",
        SHELL.len(),
        log.display()
    );
    Ok(())
}
