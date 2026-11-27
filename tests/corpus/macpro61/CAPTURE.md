# 2013 Mac Pro (MacPro6,1) hardware corpus, 2026-11-27

Read-only captures for AbyssBSD-Rust. Xeon E5-2697 v2, 2x FirePro D700.
Serial numbers (incl. USB device serials), the system UUID, Ethernet MAC
addresses (OUI kept), Wi-Fi networks, the hostname and
secret-looking NVRAM values are withheld.

| Directory | Boot |
|---|---|
| `freebsd-osi1-quiet/` | normal boot, hw.acpi.apple_darwin_osi unset (default 1); also `ichwd.txt` and `usb-portmap.txt` |
| `freebsd-osi1/` | verbose, osi=1, kern.msgbufsize=1M (complete dmesg) |
| `freebsd-osi0/` | verbose, osi=0, kern.msgbufsize=1M (complete dmesg) |
| `macos/macos.txt` | macOS 12.7.6: sw_vers, bless --getBoot, nvram -p, diskutil list |
| `display.txt` | boot display and GOP modes |

Each FreeBSD directory: acpidump -dt, pciconf -lvc, devinfo -rv, dmesg.boot,
kenv, uname, loader.conf, usbconfig, efivar -l, efibootmgr -v, apic.txt.
Kernel: AbyssBSD src orangepi-6-plus built 2026-11-26 (uname.txt).

Findings:
- osi=0 hides all three Thunderbolt 2 (Falcon Ridge) controllers: 21 PCI
  functions (8086:156d, 8086:156c). The ACPI tables are identical in all
  boots; only the devices present differ.
- x2APIC mode is on (hw.apic.x2apic_mode=1).
- ichwd: Patsburg watchdog found, "disabled in BIOS or hardware", no attach.
- USB 3: all four rear ports on the Fresco Logic FL1100 (uhub1, 8 ports:
  1-4 USB 2, 5-8 SuperSpeed); one physical port did not respond. Patsburg
  EHCI carries only the internal Bluetooth.
- Boot display is on Thunderbolt (not HDMI); GOP offers one mode,
  2560x1440x32.
- The medium's loader.conf is not empty (live-medium console and SI
  tunables): see loader.conf.txt.
- macOS boot-args include -lilubetaall (Lilu is installed);
  csr-active-config is 7f080000 (SIP mostly off).
- Not captured: `bless --info` (it needs sudo with a password) and which
  of the six Thunderbolt ports carries the display.
