# Orange Pi 6 Plus ground truth

The state of the reference OSes on the Orange Pi 6 Plus (CIX P1 / Sky1, HARDWARE.md §4), captured
before the board is reimaged for AbyssBSD (ROADMAP P8.0, MULTIUSER.md §15). After the reimage these
files are the only record of how FreeBSD and Linux saw the hardware.

| Directory | Source |
|---|---|
| `freebsd/` | FreeBSD 16.0-CURRENT, the sibling project's `GENERIC-HMP-IOMMU` kernel (`orangepi-6-plus-f12c9f4ee6e3`, built 2026-10-11), from the board's NVMe. Captured 2026-11-09 |
| `linux/` | Ubuntu 26.04.1, kernel `7.0.0-41-cix`, from the same NVMe. Captured 2026-11-09 |

The board has two NVMe drives: a Crucial P3 500 GB (`CT500P3SSD8`, Micron controller), which holds
the ESP, Ubuntu, FreeBSD, and swap, and a Samsung 990 PRO 1 TB with one partition (`taisce`) that
is not ours. The binary ACPI tables are in [`../acpi/opi6p/`](../acpi/opi6p/).

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

This build's `acpidump` has no `-b`, so the binary tables come from Linux. Do not read them through
`/dev/mem` on FreeBSD: that panicked the board.

## `linux/`

Read-only: `sudo` over ssh, sysfs and procfs reads, and the listing tools. No `/dev/mem` and no
register access.

| File | Command |
|---|---|
| `uname.txt` | `uname -a`, `/etc/os-release`, `/proc/cmdline` |
| `dmesg.txt` | `dmesg` (includes ACPICA's `AE_NOT_FOUND` for `^I2C0.UXC0`-`UXC3`) |
| `lspci.txt` | `lspci -vvvnn; lspci -tvnn` |
| `lsusb.txt` | `lsusb -v; lsusb -tv` |
| `acpi-devices.txt` | each `/sys/bus/acpi/devices/*`: `path`, `hid`, `uid`, `status`, `adr`, and its physical nodes |
| `interrupts.txt`, `iomem.txt`, `cpuinfo.txt` | `/proc/interrupts`, `/proc/iomem`, `/proc/cpuinfo` |
| `cpu-sysfs.txt` | `cpufreq`, `acpi_cppc`, `cpu_capacity`, `cpuidle` states, and topology for every CPU |
| `lsmod.txt` | `lsmod` |
| `dmidecode.txt` | `dmidecode` (from sysfs) |
| `net.txt` | `ethtool -i` for every interface; `ip -d addr` |
| `storage.txt` | `lsblk` (`nvme-cli` is not installed) |
| `efi.txt` | `efibootmgr -v` |
