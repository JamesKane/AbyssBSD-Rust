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

pub(crate) const fn step(send: &'static str, expect: &'static str, what: &'static str) -> Step {
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
pub(crate) const PROMPT: &str = "@sh> ";

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
    // Not "^Z[1] …": a console message may come between the echo and the
    // shell's line.
    step("\x1a", "[1] + Suspended", "^Z stops it"),
    step("jobs\n", "\n[1] + Suspended", "jobs lists it"),
    job_then_settle("fg\n", "sleep 60", "fg resumes it"),
    step("\x03", "\n", "^C ends it"),
    step("jobs; echo done\n", "\ndone\r\n", "no jobs left"),
];

/// What follows [`SHELL`] on a UFS2 root holding all of `base.txz`
/// (ROADMAP P5.8): the root is read-only, as FreeBSD mounts it first, and
/// `mount -u` makes it read-write with soft updates; `mtree` finds every
/// entry of the base with its owner, mode, flags, size, and digest; and
/// `tar` and `cp -R` copy a tree that `mtree` then finds the same. `find`
/// and `mtree` are static builds (`/usr/tests/static`) until base's own,
/// dynamically linked, can run (P6).
pub(crate) const UFS_ROOT: &[Step] = &[
    step(
        "PATH=/rescue:/usr/tests/static\n",
        "\n",
        "add the static tools",
    ),
    step(
        "mount\n",
        "/dev/gpt/rootfs0 on / (ufs, local, read-only)",
        "the root is read-only",
    ),
    // What fsck asks after repairing the root (`chkdoreload`): the
    // metadata read again under the running system.
    step(
        "mount -u -o ro,reload / && echo reload-$?\n",
        "\nreload-0\r\n",
        "mount -u -o reload reads the read-only root again",
    ),
    step(
        "mount -u -o rw / && echo rw-$?\n",
        "\nrw-0\r\n",
        "mount -u makes it read-write",
    ),
    step(
        "mount\n",
        "/dev/gpt/rootfs0 on / (ufs, local, soft-updates)",
        "with soft updates",
    ),
    step(
        "ls -lo /usr/bin/su\n",
        "-r-sr-xr-x  1 root wheel schg",
        "ls shows owners, modes, and flags",
    ),
    step(
        "find /etc/defaults -name rc.conf\n",
        "\n/etc/defaults/rc.conf\r\n",
        "find",
    ),
    step(
        "mtree -e -f /usr/tests/static/base.mtree -p / && echo spec-$?\n",
        "\nspec-0\r\n",
        "mtree: owners, modes, flags, and links",
    ),
    step(
        "mtree -e -f /usr/tests/static/base-data.mtree -p / && echo data-$?\n",
        "\ndata-0\r\n",
        "mtree: sizes and digests",
    ),
    step(
        "mtree -c -k type,size,mode,link,sha256digest -p /usr/share/misc \
         > /var/tmp/misc.mtree && echo made-$?\n",
        "\nmade-0\r\n",
        "mtree -c",
    ),
    step(
        "tar -cf /var/tmp/misc.tar -C /usr/share misc && mkdir /var/tmp/x && \
         tar -xpf /var/tmp/misc.tar -C /var/tmp/x && \
         mtree -f /var/tmp/misc.mtree -p /var/tmp/x/misc && echo tar-$?\n",
        "\ntar-0\r\n",
        "tar copies a tree",
    ),
    step(
        "cp -Rp /usr/share/misc /var/tmp/cp && \
         mtree -f /var/tmp/misc.mtree -p /var/tmp/cp && echo cp-$?\n",
        "\ncp-0\r\n",
        "cp -R copies a tree",
    ),
    step(
        "rm -r /var/tmp/x /var/tmp/misc.tar && echo rm-$?\n",
        "\nrm-0\r\n",
        "rm -r",
    ),
];

/// Base's own, dynamically linked, programs (ROADMAP P6.1): they run, the
/// run-time linker runs as a program itself, `ldd` lists what a program
/// links, and rtld's `LD_*` variables work. It writes nothing, so it runs
/// on FreeBSD's read-only single-user root too.
pub(crate) const DYNAMIC: &[Step] = &[
    step(
        "PATH=/usr/bin:/bin:/usr/sbin:/sbin\n",
        "\n",
        "base's own programs",
    ),
    step("ls -lo /bin/ls\n", "-r-xr-xr-x  1 root wheel -", "ls"),
    step("ps -o comm= -p $$\n", "\nsh\r\n", "ps"),
    step(
        "tar -cf - -C /etc/defaults rc.conf | tar -tvf -\n",
        " rc.conf\r\n",
        "tar",
    ),
    step(
        "find /etc/defaults -name rc.conf\n",
        "\n/etc/defaults/rc.conf\r\n",
        "find",
    ),
    step(
        "ldd /usr/bin/tar\n",
        "libarchive.so.7 => /usr/lib/libarchive.so.7",
        "ldd",
    ),
    step(
        "/libexec/ld-elf.so.1 /bin/echo direct\n",
        "\ndirect\r\n",
        "rtld run as a program",
    ),
    step("LD_BIND_NOW=1 /bin/echo now\n", "\nnow\r\n", "LD_BIND_NOW"),
    // rtld finds a preloaded name in LD_LIBRARY_PATH or the hints (which
    // a fresh root has not until rc runs ldconfig), not the default path.
    step(
        "LD_TRACE_LOADED_OBJECTS=1 LD_LIBRARY_PATH=/lib LD_PRELOAD=libm.so.5 /bin/echo\n",
        "\t/lib/libm.so.5 (0x",
        "LD_PRELOAD and LD_LIBRARY_PATH",
    ),
    step(
        "kbdcontrol -r fast < /dev/ttyv0 && echo kbd-$?\n",
        "\nkbd-0\r\n",
        "kbdcontrol",
    ),
    step(
        "vidcontrol -h 500 < /dev/ttyv0 && echo vid-$?\n",
        "\nvid-0\r\n",
        "vidcontrol",
    ),
    // `head` opens its files through Casper's `system.fileargs`, a
    // service process at the other end of a UNIX-domain socket pair.
    step(
        "head -n 1 /etc/defaults/rc.conf\n",
        "\n#!/bin/sh\r\n",
        "head, through libcasper",
    ),
    // `logger` to a `syslogd` started by hand, over `/var/run/log`
    // (`syslogd` writes only to files that exist, as `newsyslog` leaves
    // them).
    step(
        "touch /var/log/messages && syslogd -ss && logger -t abysslog hello-syslog && sleep 1 && \
         grep -c 'abysslog.*hello-syslog' /var/log/messages; \
         kill $(cat /var/run/syslog.pid)\n",
        "\n1\r\n",
        "logger to syslogd",
    ),
    // `script` runs a shell on a pseudo-terminal of its own, relaying
    // between it and this terminal, which it puts in raw mode meanwhile.
    step(
        "script -q /dev/null sh -c 'echo in-$(tty | cut -c1-9)'\n",
        "\nin-/dev/pts/\r\n",
        "script",
    ),
    step(
        "stty -a | grep -c ' icanon'\n",
        "\n1\r\n",
        "script restores the terminal",
    ),
    // `jail -c` runs a shell with its own hostname and root: a directory
    // holding only `/rescue`'s binary, as `sh` and `hostname`.
    step(
        "mkdir -p /tmp/j/bin && cp /rescue/sh /tmp/j/bin/sh && ln -f /tmp/j/bin/sh \
         /tmp/j/bin/hostname && jail -c path=/tmp/j host.hostname=inside.jail \
         command=/bin/sh -c 'echo in-$(/bin/hostname) /*'\n",
        "\nin-inside.jail /bin\r\n",
        "a jail",
    ),
    step(
        // A first `jls` takes the kernel's first-use notes for the
        // `kern.features` it asks about out of the way.
        "jls > /dev/null; hostname | grep -c inside; jls | wc -l\n",
        "\n0\r\n       1\r\n",
        "the jail is gone, and the host's name is its own",
    ),
    // `uuidgen(2)`: version-1 UUIDs, the RFC 4122 variant, a batch
    // consecutive in time.
    step(
        "uuidgen -n 3 | grep -c \
         '^[0-9a-f]\\{8\\}-[0-9a-f]\\{4\\}-1[0-9a-f]\\{3\\}-[89ab][0-9a-f]\\{3\\}-[0-9a-f]\\{12\\}$'\n",
        "\n3\r\n",
        "uuidgen",
    ),
    // One memory domain, first-touch, as on a FreeBSD machine with one;
    // `-n` sets a policy for the command it runs.
    step(
        "cpuset -g | grep domain\n",
        "\npid -1 domain policy: first-touch mask: 0\r\n",
        "cpuset -g",
    ),
    step(
        "cpuset -n rr:0 cpuset -g | grep domain\n",
        "\npid -1 domain policy: round-robin mask: 0\r\n",
        "cpuset -n",
    ),
];

/// What system-information tools read (fastfetch's fields, ROADMAP P9):
/// the loader's SMBIOS variables, `hw.model`, a disk's GEOM attributes,
/// and devstat through FreeBSD's own `iostat`, `gstat`, and `systat`
/// (none of which may complain about devstat). It shows each tool's
/// output for the log, then checks what holds on any machine: the first
/// NVMe, virtio, or ATA disk `kern.disks` names has a rotation rate and an identifier, and
/// `gstat` lists it. It writes nothing, so it runs on FreeBSD's
/// read-only single-user root too.
pub(crate) const SYSINFO: &[Step] = &[
    step(
        "kenv | grep smbios; sysctl hw.model hw.acpi.battery kern.devstat.numdevs; \
         geom disk list; iostat -x; gstat -b -I 100ms; echo show-$((1 + 1))\n",
        "\nshow-2\r\n",
        "the tools' output",
    ),
    step(
        "kenv | grep -c '^smbios\\.version='\n",
        "\n1\r\n",
        "the loader's SMBIOS variables",
    ),
    step(
        "kenv hint.smbios.0.mem | cut -c1-2\n",
        "\n0x\r\n",
        "hint.smbios.0.mem",
    ),
    step("sysctl -n hw.model | grep -c .\n", "\n1\r\n", "hw.model"),
    step(
        "d=$(sysctl -n kern.disks | tr ' ' '\\n' | grep -E '^(nda|vtbd|ada)' | sort | head -1); \
         geom disk list $d | grep -c -e 'rotationrate: ' -e 'ident: [^( ]'\n",
        "\n2\r\n",
        "geom disk list: a rotation rate and an identifier",
    ),
    step(
        "iostat -x -c 2 -w 1 > /dev/null && echo iostat-$((6 * 7))\n",
        "\niostat-42\r\n",
        "iostat",
    ),
    step(
        "gstat -b -I 100ms -f \"^$d\\$\" | grep -c \" $d\\$\"\n",
        "\n1\r\n",
        "gstat lists the disk",
    ),
    step(
        "script -q /dev/null timeout 2 systat -vmstat 1 | grep -c devstat\n",
        "\n0\r\n",
        "systat -vmstat finds devstat",
    ),
    step(
        "stty sane; echo sane-$((2 + 2))\n",
        "\nsane-4\r\n",
        "the terminal",
    ),
];

/// The user tests again, dynamically linked (ROADMAP P6.1's exit), from
/// a UFS2 root: each is named on the console as it starts, and each that
/// fails is named again, with what it printed. They run
/// as the kernel tests run them, in a session of their own with no
/// terminal (`daemon -f`): under the boot shell's job control, with the
/// shell as pid 1 in their session, process groups are never orphaned
/// and the terminal is theirs to take.
pub(crate) const DYNAMIC_TESTS: &[Step] = &[step(
    "rm -f /var/tmp/dyn.*; daemon -f sh -c 'f=0; for t in /usr/tests/abyss-dynamic/*; do \
     echo \"dyntest: ${t##*/}\" > /dev/console; \
     $t > /var/tmp/dyn.out 2>&1 || { echo \"FAIL ${t##*/}\"; cat /var/tmp/dyn.out; f=1; } \
     >> /var/tmp/dyn.res; done; echo dyntests-$f >> /var/tmp/dyn.res; touch /var/tmp/dyn.done'; \
     while [ ! -e /var/tmp/dyn.done ]; do sleep 1; done; cat /var/tmp/dyn.res\n",
    "\ndyntests-0\r\n",
    "the user tests, dynamically linked",
)];

/// FreeBSD's `pciconf` through `/dev/pci` (docs/DRIVER-MODEL.md §11.5),
/// on the shell VM's QEMU devices, whose IDs QEMU fixes: the boot NVMe
/// controller (`1b36:0010`) and the PCIe root port (`1b36:000c`) with the
/// virtio-rng behind it. Each line is what FreeBSD 15.1's `pciconf` prints
/// for the device: the selector, IDs, and names from the device tree
/// (`-l`), `pci_vendors` (`-v`), BARs (`-b`), capabilities (`-c`), bridge
/// windows (`-B`), configuration reads and writes (`-r`, `-w`),
/// attachment (`-a`), and a BAR mapped (`-D`). The slots differ by
/// architecture (x86's VGA takes slot 1), so each has its own script.
pub(crate) fn pciconf(arch: Arch) -> &'static [Step] {
    match arch {
        Arch::X86_64 => PCICONF_X86_64,
        Arch::Aarch64 => PCICONF_AARCH64,
    }
}

/// The steps both architectures share, given the root port's name.
macro_rules! pciconf_steps {
    ($nvme_line:literal, $port_line:literal, $rng_sel:literal, $rng_line:literal, $hostb_line:literal) => {
        &[
            step(
                "ls -l /dev/pci\n",
                "crw-r--r--  1 root wheel ",
                "/dev/pci is root:wheel 0644",
            ),
            step("pciconf -l nvme0\n", $nvme_line, "pciconf -l: a function by name"),
            // Everything, for the log: FreeBSD's whole listing.
            step(
                "pciconf -lvbcB\n",
                "\n    class      = mass storage\r\n",
                "pciconf -lvbcB: every function",
            ),
            step(
                "pciconf -lv nvme0\n",
                "\n    vendor     = 'Red Hat, Inc.'\r\n    device     = 'QEMU NVM Express Controller'\r\n    class      = mass storage\r\n    subclass   = NVM\r\n",
                "pciconf -v: names from pci_vendors",
            ),
            step(
                "pciconf -lb nvme0\n",
                "\n    bar   [10] = type Memory, range 64, base 0x",
                "pciconf -b: BARs",
            ),
            step(
                "pciconf -lc nvme0 | grep -c -e 'cap 10\\[[0-9a-f]*\\] = PCI-Express 2 ' \
                 -e 'cap 11\\[[0-9a-f]*\\] = MSI-X supports'\n",
                "\n2\r\n",
                "pciconf -c: capabilities",
            ),
            step("pciconf -l pcib1\n", $port_line, "the root port"),
            step("pciconf -l hostb0\n", $hostb_line, "the host bridge function (hostb)"),
            step(
                "pciconf -lB pcib1\n",
                "\n    bus range  = 1-1\r\n",
                "pciconf -B: the bus behind the root port",
            ),
            step($rng_sel, $rng_line, "a function by selector, behind the bridge"),
            step("pciconf -r nvme0 0\n", "\n00101b36 \r\n", "pciconf -r: a dword"),
            step("pciconf -r -h nvme0 2\n", "\n0010 \r\n", "pciconf -r -h: a word"),
            // The cache line size is writable on every QEMU device and
            // means nothing to one emulated; it is put back.
            step(
                "o=$(pciconf -r -b nvme0 0xc); pciconf -w -b nvme0 0xc 0x10 && \
                 n=$(pciconf -r -b nvme0 0xc); pciconf -w -b nvme0 0xc 0x$o; echo cls-$n\n",
                "\ncls-10\r\n",
                "pciconf -w: a byte",
            ),
            step("pciconf -a nvme0\n", "\nnvme0: attached\r\n", "pciconf -a"),
            // NVMe's VS register (offset 8): version 1.4.
            step(
                "pciconf -D nvme0 0x10 2 1 | od -An -tx4\n",
                " 00010400",
                "pciconf -D: the BAR mapped (PCIOCBARMMAP)",
            ),
            // Anyone may list; only root opens it for writing.
            step(
                "su -m nobody -c 'pciconf -l nvme0 | cut -c1-6; pciconf -r nvme0 0' 2>&1\n",
                "\nnvme0@\r\npciconf: /dev/pci: Permission denied\r\n",
                "pciconf as nobody",
            ),
        ]
    };
}

const PCICONF_X86_64: &[Step] = pciconf_steps!(
    "\nnvme0@pci0:0:2:0:\tclass=0x010802 rev=0x02 hdr=0x00 vendor=0x1b36 device=0x0010 subvendor=0x1af4 subdevice=0x1100\r\n",
    "\npcib1@pci0:0:3:0:\tclass=0x060400 rev=0x00 hdr=0x01 vendor=0x1b36 device=0x000c subvendor=0x1b36 subdevice=0x0000\r\n",
    "pciconf -l pci0:1:0:0\n",
    "\nvirtio_pci0@pci0:1:0:0:\tclass=0x00ff00 rev=0x01 hdr=0x00 vendor=0x1af4 device=0x1044 subvendor=0x1af4 subdevice=0x1100\r\n",
    "\nhostb0@pci0:0:0:0:\tclass=0x060000 rev=0x00 hdr=0x00 vendor=0x8086 device=0x29c0 subvendor=0x1af4 subdevice=0x1100\r\n"
);

const PCICONF_AARCH64: &[Step] = pciconf_steps!(
    "\nnvme0@pci0:0:1:0:\tclass=0x010802 rev=0x02 hdr=0x00 vendor=0x1b36 device=0x0010 subvendor=0x1af4 subdevice=0x1100\r\n",
    "\npcib1@pci0:0:2:0:\tclass=0x060400 rev=0x00 hdr=0x01 vendor=0x1b36 device=0x000c subvendor=0x1b36 subdevice=0x0000\r\n",
    "pciconf -l pci0:1:0:0\n",
    "\nvirtio_pci0@pci0:1:0:0:\tclass=0x00ff00 rev=0x01 hdr=0x00 vendor=0x1af4 device=0x1044 subvendor=0x1af4 subdevice=0x1100\r\n",
    "\nhostb0@pci0:0:0:0:\tclass=0x060000 rev=0x00 hdr=0x00 vendor=0x1b36 device=0x0008 subvendor=0x1af4 subdevice=0x1100\r\n"
);

/// What FreeBSD's tests need of the UFS2 root when `ABYSS_ATF` runs them
/// alone ([`crate::image::atf_only`]): it read-write, and base's
/// programs first in the path.
pub(crate) const ATF_SETUP: &[Step] = &[
    step(
        "mount -u -o rw / && echo rw-$?\n",
        "\nrw-0\r\n",
        "mount -u makes it read-write",
    ),
    step(
        "PATH=/usr/bin:/bin:/usr/sbin:/sbin:/usr/tests/static\n",
        "\n",
        "base's own programs",
    ),
];

/// FreeBSD's own tests the root holds (`tests.txz`'s libthr), run case by
/// case by `run-atf`; [`atf_verdict`] compares the results with FreeBSD's.
pub(crate) const FREEBSD_TESTS: &[Step] = &[step(
    "/usr/tests/static/run-atf\n",
    "\natf-done-42\r\n",
    "FreeBSD's libthr tests",
)];

// Where FreeBSD 15.1's own results are (`kyua-baseline`), and the cases
// whose results may differ here yet, with why.
use crate::kyua::{BASELINE as KYUA_BASELINE, KNOWN_FAILURES as KYUA_KNOWN};

/// The `atf:` lines in `console` against FreeBSD's results: every case
/// FreeBSD has in [`crate::image::FREEBSD_TESTS`], that this architecture's programs list
/// (`atf-case:` lines), must run and end as it does there (its result's
/// first word), but those `KYUA_KNOWN` lists. Returns how many matched.
pub(crate) fn atf_verdict(console: &str) -> Result<usize> {
    let root = crate::util::root();
    let baseline = fs::read_to_string(root.join(KYUA_BASELINE))?;
    let freebsd: std::collections::HashMap<&str, &str> = baseline
        .lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            Some((f.next()?, f.next()?))
        })
        .collect();
    let known = fs::read_to_string(root.join(KYUA_KNOWN))?;
    let known: Vec<&str> = known
        .lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .collect();
    let mut matched = 0;
    let mut wrong = Vec::new();
    let mut ran = std::collections::HashSet::new();
    for line in console
        .lines()
        .filter_map(|l| l.trim_end().strip_prefix("atf: "))
    {
        let Some((case, result)) = line.split_once(' ') else {
            continue;
        };
        ran.insert(case);
        let status = result.split(':').next().unwrap_or(result).trim();
        // A case that expected to hang and was killed for it: kyua
        // reports that as `expected_failure`.
        let status = if status == "expected_timeout" {
            "expected_failure"
        } else {
            status
        };
        match freebsd.get(case) {
            Some(&want) if want == status => matched += 1,
            _ if known.contains(&case) => {}
            Some(want) => wrong.push(format!("{case}: {result} (FreeBSD: {want})")),
            None => wrong.push(format!("{case}: {result} (FreeBSD has no such case)")),
        }
    }
    // The cases this architecture's programs have (`atf-case:` lines):
    // one FreeBSD has that a program here lacks is not missing (arm64's
    // builds leave out amd64-only cases).
    let listed: std::collections::HashSet<&str> = console
        .lines()
        .filter_map(|l| l.trim_end().strip_prefix("atf-case: "))
        .collect();
    let mut missing: Vec<&str> = freebsd
        .keys()
        .copied()
        .filter(|c| listed.contains(c))
        .filter(|c| !ran.contains(c) && !known.contains(c))
        .collect();
    missing.sort_unstable();
    wrong.extend(missing.iter().map(|c| format!("{c}: did not run")));
    if !wrong.is_empty() {
        return Err(format!("FreeBSD's tests ended otherwise:\n  {}", wrong.join("\n  ")).into());
    }
    if matched == 0 {
        return Err("FreeBSD's tests: no case ran".into());
    }
    Ok(matched)
}

/// What the kernel prints when it panics.
const PANIC: &str = "\npanic: ";

/// How much unmatched console output a [`Session`] keeps, for errors.
const TAIL: usize = 300;

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
            vm.serial(&mut cmd, "stdio");
            cmd.args(["-monitor", "none"]);
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

    /// Drops searched output that no match of a pattern `longest` bytes
    /// long can start in, keeping [`TAIL`] bytes for error messages.
    /// Without this, a console that prints much before the next match
    /// (a test's kernel messages) is searched again in full per read.
    fn forget_searched(&mut self, longest: usize) {
        let keep = TAIL.max(longest.saturating_sub(1));
        let excess = self.pending.len().saturating_sub(keep);
        self.pending.drain(..excess);
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
            self.forget_searched(needle.len());
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(bytes) => {
                    self.log.write_all(&bytes)?;
                    self.pending.extend_from_slice(&bytes);
                }
                Err(e) => {
                    let tail = &self.pending[self.pending.len().saturating_sub(TAIL)..];
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

    /// Waits up to `timeout` for any of `patterns`; consumes the output
    /// through the earliest match and returns which pattern it was (the
    /// first listed, if several match there).
    pub(crate) fn expect_any(&mut self, patterns: &[&str], timeout: Duration) -> Result<usize> {
        let deadline = Instant::now() + timeout;
        loop {
            let found = patterns
                .iter()
                .enumerate()
                .filter_map(|(i, p)| {
                    let n = p.as_bytes();
                    self.pending
                        .windows(n.len())
                        .position(|w| w == n)
                        .map(|at| (at, i, n.len()))
                })
                .min_by_key(|&(at, i, _)| (at, i));
            if let Some((at, i, len)) = found {
                self.pending.drain(..at + len);
                return Ok(i);
            }
            self.forget_searched(patterns.iter().map(|p| p.len()).max().unwrap_or(0));
            let left = deadline.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left) {
                Ok(bytes) => {
                    self.log.write_all(&bytes)?;
                    self.pending.extend_from_slice(&bytes);
                }
                Err(e) => {
                    let tail = &self.pending[self.pending.len().saturating_sub(TAIL)..];
                    let why = match e {
                        mpsc::RecvTimeoutError::Timeout => {
                            format!("nothing new for {} s", timeout.as_secs())
                        }
                        mpsc::RecvTimeoutError::Disconnected => "the VM exited".to_owned(),
                    };
                    return Err(format!(
                        "{why} waiting for {patterns:?}; last output: {:?}",
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
            // A panic ends the step at once, rather than at its timeout.
            self.expect_any(&[s.expect, PANIC], timeout)
                .and_then(|which| match which {
                    0 => Ok(()),
                    _ => {
                        let tail = self.pending.len().min(TAIL);
                        Err(format!(
                            "the kernel panicked: {:?}",
                            String::from_utf8_lossy(&self.pending[..tail])
                        )
                        .into())
                    }
                })
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

    /// Raises an NMI on every vCPU and logs what the guest prints for
    /// the next `wait`: a hung kernel's backtraces.
    pub(crate) fn nmi(&mut self, wait: Duration) -> Result<bool> {
        if !self.vm.nmi() {
            return Ok(false);
        }
        let deadline = Instant::now() + wait;
        while Instant::now() < deadline {
            if let Ok(bytes) = self.rx.recv_timeout(Duration::from_millis(100)) {
                self.log.write_all(&bytes)?;
            }
        }
        Ok(true)
    }

    /// Stops the VM at once, as pulling the plug would (what is written
    /// stays on its disks; the rest is lost).
    pub(crate) fn kill(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        while let Ok(bytes) = self.rx.recv_timeout(Duration::from_millis(100)) {
            let _ = self.log.write_all(&bytes);
        }
        self.vm.cleanup();
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

/// `cargo xtask freebsd-shell`: runs [`SHELL`], [`DYNAMIC`], and [`SYSINFO`] against
/// FreeBSD 15.1's `/rescue/sh` in bhyve, proving the scripts and this
/// harness.
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
        session.script(DYNAMIC, Duration::from_secs(30))?;
        session.script(SYSINFO, Duration::from_secs(120))?;
        session.send("exit\n")
    })();
    let exited = session.finish(Duration::from_secs(180))?;
    result.map_err(|e| format!("{e} (console: {})", log.display()))?;
    if !exited {
        return Err(format!("FreeBSD did not power off; see {}", log.display()).into());
    }
    println!(
        "freebsd-shell: {} steps passed against FreeBSD; console in {}",
        SHELL.len() + DYNAMIC.len() + SYSINFO.len(),
        log.display()
    );
    Ok(())
}
