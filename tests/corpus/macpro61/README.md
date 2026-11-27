# Mac Pro (Late 2013, `MacPro6,1`) ground truth

How FreeBSD 16-CURRENT and macOS see the Mac Pro (HARDWARE.md §9, docs/MACPRO.md MP0), for the
ACPI corpus tests of MP2 and the device-tree comparison of the track's exit.

The FreeBSD-fork project (`../AbyssBSD`) captured it on 2026-11-27 from its FreeBSD stick and from
macOS 12.7.6, read-only. Its own index is [CAPTURE.md](CAPTURE.md). Withheld throughout: serial
numbers (USB device serials included), the system UUID, Ethernet addresses (Apple's OUI kept,
`00:3e:e1:xx:xx:01`/`:02`), Wi-Fi networks, the hostname, and secret-looking NVRAM values.

The machine: Xeon E5-2697 v2, 64 GB, two FirePro D700. The kernel is the fork's (`orangepi-6-plus`,
built 2026-11-26, `__FreeBSD_version` 1600034; see `uname.txt`), not stock 16-CURRENT.

| Path | Boot |
|---|---|
| `freebsd/osi1-quiet/` | Normal boot, `hw.acpi.apple_darwin_osi` unset (default 1). Also `ichwd.txt` and `usb-portmap.txt` |
| `freebsd/osi1/` | Verbose, `hw.acpi.apple_darwin_osi=1`, `kern.msgbufsize=1M` (complete dmesg) |
| `freebsd/osi0/` | Verbose, `hw.acpi.apple_darwin_osi=0`, `kern.msgbufsize=1M` |
| `freebsd/acpidump.dsl` | `acpidump -dt`, the same in all three boots apart from the temporary file name |
| `macos/macos.txt` | `sw_vers`, `bless --getBoot`, `nvram -p`, `diskutil list` |
| `display.txt` | The boot display and the GOP modes |

Each FreeBSD directory has `pciconf -lvc`, `devinfo -rv`, `dmesg.boot`, `kenv`, `uname`,
`loader.conf`, `usbconfig`, `efivar -l`, `efibootmgr -v` and `apic.txt`.

What it settled (HARDWARE.md §9 has the rows):
- `bge0`/`bge1`: device `14e4:1682`, CHIP ID 0x57766000 (ASIC rev 0x57766), PHY OUI 0x001be9
  model 0x24 rev 0. The bootcode answers the reset handshake.
- With `apple_darwin_osi=0`, all three Thunderbolt 2 controllers disappear (21 PCI functions).
  The DSDT keys 296 places on `OSDW()`, which is true when `\_SB.PCI0._INI` set `OSYS` to 10000
  for `_OSI("Darwin")`.
- x2APIC mode is on.
- `ichwd` finds the Patsburg watchdog "disabled in BIOS or hardware" and does not attach.
- The four rear USB 3 ports are on a Fresco Logic FL1100; one did not respond. Patsburg's EHCI
  carries only the internal Bluetooth.
- The boot display is on Thunderbolt; GOP offers one mode, 2560×1440×32.

The raw ACPI tables are in [`../acpi/macpro61/`](../acpi/macpro61/). Not here yet: `bless --info`,
and which Thunderbolt port carries the display.
