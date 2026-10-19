# Deviations from FreeBSD

FreeBSD's names and behavior are the compatibility contract (DRIVER-MODEL.md §4.3). This file lists
where AbyssBSD differs, and why. A difference not listed here is a bug.

## Driver names for hardware FreeBSD has no driver for

| Name | Device | Why |
|---|---|---|
| `sbsa_gwdt` | Arm SBSA generic watchdog (GTDT) | FreeBSD 15.1 has no driver for it. The name follows the earlier FreeBSD bring-up branch for the Orange Pi 6 Plus, where it was first written. |

## Device tree

| What | FreeBSD | AbyssBSD | Why |
|---|---|---|---|
| arm64 `virtio_pci` on QEMU `virt` | FreeBSD would assign the unassigned legacy I/O BAR | The modern interface | Same driver and description FreeBSD 16 picks for transitional devices; no I/O BARs are assigned on arm64. |
| ACPI devices whose `_STA` says absent | Added, disabled | Not added (except `PNP0C0F` and `PNP0B00`) | `devinfo -v` shows neither; the recorded trees match. |
