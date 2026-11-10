# Orange Pi 6 Plus ground truth

The state of the reference OSes on the Orange Pi 6 Plus (CIX P1 / Sky1, HARDWARE.md §4), captured
before the board is reimaged for AbyssBSD (ROADMAP P8.0, MULTIUSER.md §15). After the reimage these
files are the only record of how FreeBSD and Linux saw the hardware.

| Directory | Source |
|---|---|
| `freebsd/` | FreeBSD 16.0-CURRENT, the sibling project's `GENERIC-HMP-IOMMU` kernel (`orangepi-6-plus-f12c9f4ee6e3`, built 2026-10-11), from the board's NVMe. Captured 2026-11-09 |
| `linux/` | Not yet: Ubuntu (CIX kernel) from the same NVMe, including `/sys/firmware/acpi/tables` |

## `freebsd/`

Each file is the output of the commands below, run as root over ssh and streamed back, so nothing
was written to the board's disk.

| File | Command |
|---|---|
| `uname.txt` | `uname -a; freebsd-version -ku` |
| `dmesg.boot.txt` | `cat /var/run/dmesg.boot` |
| `acpidump-dt.txt` | `acpidump -dt`: every table decoded, then the DSDT and both SSDTs disassembled |
| `devinfo-rv.txt`, `devinfo-u.txt` | `devinfo -rv`, `devinfo -u` |
| `pciconf-lvbc.txt` | `pciconf -lvbc` |
| `usbconfig.txt` | `usbconfig; usbconfig dump_device_desc` |
| `sysctl-a.txt` | `sysctl -a` |
| `kenv.txt` | `kenv` |
| `vmstat-i.txt` | `vmstat -i` |
| `ifconfig-a.txt` | `ifconfig -a` |
| `storage.txt` | `nvmecontrol devlist; nvmecontrol identify nvme0; camcontrol devlist; geom disk list; gpart show` |
| `efi.txt` | `efibootmgr -v; efivar -l` |

The binary ACPI tables (`<signature>.dat`, for `tests/corpus/acpi/opi6p/`) are not here: this
build's `acpidump` has no `-b`. They come from Linux's `/sys/firmware/acpi/tables`. Do not read
them through `/dev/mem` on FreeBSD: that panicked the board.
