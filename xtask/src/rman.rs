// SPDX-License-Identifier: BSD-2-Clause
//! FreeBSD's `devinfo -u` and `devinfo -rv` in the shell VMs, against
//! FreeBSD 15.1's own in the same machines (`cargo xtask --remote
//! freebsd-record --shell-vm`, tests/corpus/freebsd/rman): the resource
//! managers `hw.bus.rman` lists (docs/DRIVER-MODEL.md §11.1).
//!
//! Addresses legitimately differ between the two (firmware's
//! assignments, the MSI numbering), and so do the devices each has a
//! driver for, so what is compared is structure:
//!
//! - `devinfo -u`: the rmans, by description, in order.
//! - `devinfo -rv`: for each device both name, the rmans its resources
//!   are in, in order, and how many it has in each but the interrupt
//!   rmans (how many vectors a driver takes is the driver's choice). A
//!   transport whose child FreeBSD has no driver for (QEMU's virtio
//!   input devices) took no interrupts there, so its interrupts are not
//!   compared.
//!
//! [`KNOWN`] lists the devices whose resources differ yet, with why.

use std::fs;

use crate::arch::Arch;
use crate::expect::{Step, step};
use crate::qemu::Vmm;
use crate::util::{self, Result};

/// The step: both listings, each line tagged for [`verdict`].
pub(crate) const STEPS: &[Step] = &[step(
    "devinfo -u | sed 's/^/rman-u: /'; devinfo -rv | sed 's/^/rman-rv: /'; \
     echo rman-$((40 + 2))\n",
    "\nrman-42\r\n",
    "devinfo -u and devinfo -rv",
)];

/// Devices whose resources differ from FreeBSD's, with why: the shell VM
/// (its corpus directory, `None` for all), the device, and the reason.
const KNOWN: &[(Option<&str>, &str, &str)] = &[
    // FreeBSD's atrtc allocates its index and data ports (`IO_RTC`, 2)
    // over the eight _CRS gives; AbyssBSD lists _CRS's.
    (
        None,
        "atrtc0",
        "the RTC's ports: FreeBSD's driver takes two of _CRS's eight",
    ),
    // Each shell VM's virtio-rng is virtio_pci0.
    (
        None,
        "virtio_pci0",
        "the entropy device's interrupt: AbyssBSD's vtrnd takes one, FreeBSD's polls",
    ),
    // QEMU virt's root port is hot-plug capable, and arm64 has no _OSC
    // to keep the slot's control from FreeBSD.
    (
        Some("qemu-aarch64"),
        "pcib1",
        "the root port's interrupt: FreeBSD's pcib takes one for its slot's native hot plug, AbyssBSD's bridges do no hot plug",
    ),
    // bhyve's HPET signals by message.
    (
        Some("bhyve"),
        "hpet0",
        "the HPET's interrupts: FreeBSD's event timers take them; AbyssBSD's clock is the local APIC's",
    ),
    (
        Some("bhyve"),
        "attimer0",
        "IRQ 0: FreeBSD's attimer is an event timer; AbyssBSD's clock is the local APIC's",
    ),
    // bhyve's COM3 and COM4 are on COM1's and COM2's lines.
    (
        Some("bhyve"),
        "uart2",
        "COM3 shares COM1's edge-triggered ISA line: FreeBSD shares it, AbyssBSD shares only level-triggered lines, so the port takes none",
    ),
    (
        Some("bhyve"),
        "uart3",
        "COM4 shares COM2's edge-triggered ISA line: FreeBSD shares it, AbyssBSD shares only level-triggered lines, so the port takes none",
    ),
];

/// The corpus directory of a shell VM.
fn corpus(arch: Arch, vmm: Vmm) -> &'static str {
    match (arch, vmm) {
        (Arch::X86_64, Vmm::Bhyve) => "bhyve",
        (Arch::X86_64, Vmm::Qemu) => "qemu-x86_64",
        (Arch::Aarch64, _) => "qemu-aarch64",
    }
}

/// The rmans' descriptions in a `devinfo -u` listing.
fn rmans(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| !l.starts_with(' '))
        .filter_map(|l| l.strip_suffix(':'))
        .map(str::to_owned)
        .collect()
}

/// A device's resources in `devinfo -rv`: each rman it has some in, with
/// how many.
type Holdings = Vec<(String, usize)>;

/// A device in a `devinfo -rv` listing.
#[derive(Clone, Debug)]
struct Device {
    name: String,
    holdings: Holdings,
    /// Its first child has no driver (`unknown`).
    driverless_child: bool,
    indent: usize,
}

/// Each named device in a `devinfo -rv` listing, with its resources.
fn devices(text: &str) -> Vec<Device> {
    let mut out: Vec<Device> = Vec::new();
    for line in text.lines() {
        let t = line.trim_start();
        if t.is_empty() {
            continue;
        }
        if t.starts_with(|c: char| c.is_ascii_digit()) {
            if let Some(d) = out.last_mut()
                && let Some(last) = d.holdings.last_mut()
            {
                last.1 += 1;
            }
        } else if let Some(rman) = t
            .strip_suffix(':')
            .filter(|r| !r.contains(" <") && !r.contains(" at "))
        {
            if let Some(d) = out.last_mut() {
                d.holdings.push((rman.to_owned(), 0));
            }
        } else {
            let name = t.split([' ', '<']).next().unwrap_or("").to_owned();
            let indent = line.len() - t.len();
            if let Some(parent) = out.last_mut()
                && indent > parent.indent
                && name == "unknown"
            {
                parent.driverless_child = true;
            }
            out.push(Device {
                name,
                holdings: Vec::new(),
                driverless_child: false,
                indent,
            });
        }
    }
    out.retain(|d| d.name != "unknown");
    out
}

/// The tagged lines of `console` after `tag`.
fn tagged(console: &str, tag: &str) -> String {
    let mut out = String::new();
    for line in console.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix(tag) {
            out.push_str(rest);
            out.push('\n');
        }
    }
    out
}

/// Compares the listings in `console` with FreeBSD's for the machine;
/// returns how many devices' resources matched.
pub(crate) fn verdict(arch: Arch, vmm: Vmm, console: &str) -> Result<usize> {
    let machine = corpus(arch, vmm);
    let dir = util::root().join("tests/corpus/freebsd/rman").join(machine);
    let want_u = fs::read_to_string(dir.join("devinfo-u.txt"))?;
    let want_rv = fs::read_to_string(dir.join("devinfo-rv.txt"))?;
    let (got_u, got_rv) = (tagged(console, "rman-u: "), tagged(console, "rman-rv: "));
    let mut wrong = Vec::new();
    let (w, g) = (rmans(&want_u), rmans(&got_u));
    if w != g {
        wrong.push(format!(
            "  devinfo -u's rmans:\n    FreeBSD:  {}\n    AbyssBSD: {}",
            w.join(", "),
            g.join(", ")
        ));
    }
    let irq = |r: &str| r == "Interrupts" || r == "Interrupt request lines";
    let ours = devices(&got_rv);
    let mut matched = 0;
    for d in devices(&want_rv) {
        let Some(mine) = ours.iter().find(|o| o.name == d.name) else {
            continue;
        };
        let (name, mut theirs, mut mine) = (d.name, d.holdings, mine.holdings.clone());
        if d.driverless_child {
            theirs.retain(|h| !irq(&h.0));
            mine.retain(|h| !irq(&h.0));
        }
        let same = theirs.len() == mine.len()
            && theirs
                .iter()
                .zip(&mine)
                .all(|(a, b)| a.0 == b.0 && (irq(&a.0) || a.1 == b.1));
        if same {
            matched += 1;
        } else if let Some((_, _, why)) = KNOWN
            .iter()
            .find(|(m, n, _)| *n == name && m.is_none_or(|m| m == machine))
        {
            println!("rman: {name} differs from FreeBSD, as known: {why}");
        } else {
            let show = |h: &Holdings| {
                h.iter()
                    .map(|(r, n)| format!("{r} ×{n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            wrong.push(format!(
                "  {name}:\n    FreeBSD:  {}\n    AbyssBSD: {}",
                show(&theirs),
                show(&mine)
            ));
        }
    }
    if !wrong.is_empty() {
        return Err(format!(
            "devinfo's resources differ from FreeBSD's:\n{}",
            wrong.join("\n")
        )
        .into());
    }
    Ok(matched)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The recordings parse: the rmans of `devinfo -u`, and the devices
    /// of `devinfo -rv` with what each holds.
    #[test]
    fn recordings_parse() {
        let dir = util::root().join("tests/corpus/freebsd/rman/qemu-x86_64");
        let u = fs::read_to_string(dir.join("devinfo-u.txt")).unwrap();
        let rv = fs::read_to_string(dir.join("devinfo-rv.txt")).unwrap();
        let r = rmans(&u);
        assert_eq!(
            r[..4],
            [
                "Interrupt request lines",
                "DMA request lines",
                "I/O ports",
                "I/O memory addresses"
            ]
        );
        assert_eq!(r.last().map(String::as_str), Some("I/O memory addresses"));
        let d = devices(&rv);
        let get = |n: &str| d.iter().find(|x| x.name == n).unwrap().holdings.clone();
        // QEMU's virtio input devices have no FreeBSD driver.
        let input = d.iter().find(|x| x.name == "virtio_pci1").unwrap();
        assert!(input.driverless_child);
        assert!(
            !d.iter()
                .find(|x| x.name == "nvme0")
                .unwrap()
                .driverless_child
        );
        assert_eq!(
            get("pcib1"),
            [
                ("I/O memory addresses".to_owned(), 3),
                ("PCI domain 0 bus numbers".to_owned(), 1)
            ]
        );
        assert_eq!(get("pci1"), [("pcib1 bus numbers".to_owned(), 1)]);
        assert_eq!(
            get("ahci0"),
            [
                ("Interrupt request lines".to_owned(), 1),
                ("I/O ports".to_owned(), 1),
                ("I/O memory addresses".to_owned(), 1)
            ]
        );
        assert_eq!(get("ahcich3"), [("I/O memory addresses".to_owned(), 1)]);
        assert!(get("hostb0").is_empty());
        // Tagged console lines.
        let console = "# devinfo -u | sed 's/^/rman-u: /'\r\nrman-u: I/O ports:\r\nrman-u:     0x0-0xffff ----\r\n";
        assert_eq!(
            tagged(console, "rman-u: "),
            "I/O ports:\n    0x0-0xffff ----\n"
        );
    }
}
