# X870E ground truth

How the reference OSes see the AMD64 reference machine (Ryzen 9 7900X, Gigabyte X870E AORUS ELITE
WIFI7, HARDWARE.md §5), for ROADMAP P8.0 and the device-tree comparisons of P8.2.

| Directory | Source |
|---|---|
| `linux/` | Fedora 44, kernel `7.2.11-200.fc44.x86_64`. Captured 2026-11-09 with `tools/capture/linux-ground-truth.sh x870e`, which lists the command behind each file |
| `freebsd/` | Not yet: FreeBSD 15.1 (`acpidump`, `devinfo -rv`, `pciconf -lv`, ...) |

The ACPI tables, from Linux's `/sys/firmware/acpi/tables`, are in
[`../acpi/x870e/`](../acpi/x870e/): 30 tables (the DSDT and 15 SSDTs among them), every checksum
valid, no tables loaded later (`dynamic/` was empty). `VFCT` is left out: it carries AMD's video
BIOS image, which isn't ours to redistribute.

At capture time the machine had one NVMe, the Samsung 980 PRO 2 TB (`S6B0NG0R739549M`) that
Fedora runs from. The second NVMe that AbyssBSD is to use (HARDWARE.md §5) was not installed yet.

Linux reports one ACPI warning: the SystemIO range `0xB00`-`0xB08` (the SMBus controller)
conflicts with the AML operation region `\GSA1.SMBI`.

The DSDT's module-level code reads firmware setup variables (around `0x8FE0D000`) and defines
devices and handlers by them: `\_GPE._L16`, for one, exists only for some values. The AMD
`DISCUSB4` SSDT defines its own `\_GPE._L16` (chaining to `\_GPE.OL16` if present), so loading
all the tables with made-up variable values reports `AlreadyExists` for it (and for a `_DSD`).
Linux loaded all 16 AML tables without an error, so on this machine the DSDT leaves `_L16` out.
`lib/aml`'s `x870e_devices_match_linux` seeds the variables with the one setting that reproduces
Linux's namespace (`\CPVS` bytes 0-3: 1, 0, 2, 1), and the other registers the AML reads, as
Linux's results imply. `_STA` that AMD's CPM library (SSDT10) decides (`APAD`, and the `GPP7` and
`GPP8` bridges) isn't compared: it reads a firmware data structure under a lock the test host
doesn't model.

## `linux/amdgpu/` (2026-11-24)

How Fedora 45 (kernel 7.3.4-300.fc45) sees the Radeon 610M (`1002:164e`, at `7a:00.0` on this boot; the
PCI bus numbers in the older captures above differ), captured with a read-only script for the amd-rdna2
specification: `lspci` dumps of its four functions, `/proc/iomem`, the sysfs IP discovery tree and the raw
debugfs `amdgpu_discovery` table, the firmware versions Linux reports, the gca configuration, and the
SHA-256 sums of the firmware files. The video BIOS (`vbios.bin`, and the `VFCT` ACPI table that carries it) is
**not** in the repository (AMD's image is not ours to redistribute); a copy is kept outside it, in
`../AbyssBSD-Rust-notes/amdgpu-capture-2026-11-24/`, for the specification's author.

## `linux/amdgpu-igpu-primary/` (2026-11-24)

The same capture as `linux/amdgpu/`, with the monitor on the motherboard's HDMI port and the firmware setup's
*Initial Display Output* set to the integrated graphics (and *Integrated Graphics* forced on), so the Radeon 610M
is the boot VGA device (`boot_vga` 1; the RTX 4070 has none). It adds `amdgpu_dm_dtn_log` (the display core's
state with the firmware's mode still on screen: 1920x1080 on one HUBP) and the display capability files.
The video BIOS is the same as in the first capture and is not in the repository.

## `abyssbsd/` (2026-11-24)

What AbyssBSD's `drmn1` (the Radeon 610M, boot VGA through the firmware setting above) read from the hardware,
`dmesg` lines with the node name stripped: `raphael-drmn1.txt` holds the attach line, the IP discovery table read from
VRAM through the index registers (30 blocks, identical to `linux/amdgpu/ip_discovery.txt`), and the 224 display
registers of the amd-rdna2 specification's section 08 section 2.6 **as the firmware's GOP left them**, before any
driver programmed the display (the one display pipe active: 1920x1080 in a 2200x1125 total, the surface at VRAM
`0xf400000000`).
