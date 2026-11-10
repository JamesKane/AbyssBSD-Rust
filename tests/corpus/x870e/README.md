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
