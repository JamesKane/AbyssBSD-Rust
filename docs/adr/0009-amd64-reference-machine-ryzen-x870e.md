# ADR-0009: Ryzen 9 7900X / X870E AORUS ELITE WIFI7 as the AMD64 physical reference machine

- Status: Accepted
- Date: 2026-10-17

## Context

AMD64 needs a physical Tier 1 machine. The workstation at `192.168.0.30` (Ryzen 9 7900X, Gigabyte
X870E AORUS ELITE WIFI7, Fedora 44) is available, with a second NVMe dedicated to AbyssBSD. It is a
current-generation AMD desktop platform with modern AMI UEFI firmware, a large AML corpus (DSDT +
15 SSDTs), AMD-Vi, NVMe, AHCI, xHCI, and USB4.

## Decision

This machine is the AMD64 Tier 1 physical machine: untouched until FreeBSD runs end to end in QEMU, and gating from P6
([ADR-0010](0010-qemu-first-development.md)). AbyssBSD
is installed only on the second NVMe, with its own ESP and boot entry. Board facts live in
[HARDWARE.md §5](../HARDWARE.md#5-board-profile-amd64-ryzen-9-7900x--x870e-aorus-elite-wifi7).

## Consequences

- **No serial console.** Bring-up relies on the GOP framebuffer, a persistent RAM log saved by the
  loader to the ESP on the next boot, and the FCH watchdog (P3). A PCIe serial card or a polled
  network log are optional aids.
- Firmware leaves the APIC in xAPIC mode, and Zen 4 has no TSC-deadline timer or LA57. The APIC
  driver must support xAPIC, the LAPIC timer must be calibrated, and 4-level paging is used.
- The NIC is an RTL8125, the same family as the Orange Pi 6 Plus's RTL8126, so one `rge`-class
  driver covers both reference machines in P7.
- The machine also holds user data (Fedora's NVMe and a RAID array). The storage stack attaches
  non-designated disks read-only, and `xtask deploy` writes only to the designated media NVMe, which
  it identifies by serial number, never by device name.
- AMD is the AMD64 reference. Intel-specific paths (VT-d, TCO, HWP, hybrid cores) are covered by
  QEMU and by occasional USB boots on the build server's Alder Lake.
