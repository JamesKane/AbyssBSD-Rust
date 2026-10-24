# FreeBSD's view of the USB and input devices

How FreeBSD 15.1-RELEASE names and describes the USB and input devices of the track's reference
machines (ROADMAP U.0), for the USB, input, and vt differential tests.

| Directory | Configuration |
|---|---|
| `qemu/` | QEMU `q35` (TCG, on the build server) with the boot tier's `--usb` set: `qemu-xhci` (9 USB 2 and 8 USB 3 ports) with `usb-storage` (Bulk-Only, port 4), `usb-uas` with two `scsi-hd` LUNs (port 5), a second `usb-uas` on the USB 2-only port 9, `usb-kbd` (port 1), `usb-tablet` (port 2), and `usb-hub` (port 3) with `usb-mouse` behind it (port 3.1); `virtio-keyboard-pci` and `virtio-tablet-pci`. The release VM image on virtio-blk, the script disk on a second; 2 vCPUs, 2 GiB |
| `bhyve/` | bhyve on the build server with `-s 30,xhci,tablet`, plus the framebuffer (29) and LPC (31); 4 vCPUs, 2 GiB |

`cargo xtask --remote freebsd-record --usb` captures both (the guest runs a script from a second
disk and prints the output on its serial console). Each file is one command's output:

| File | Command |
|---|---|
| `dmesg.txt` | `dmesg` |
| `devinfo-rv.txt` | `devinfo -rv` |
| `usbconfig.txt`, `usbconfig-device-desc.txt`, `usbconfig-config-desc.txt` | `usbconfig`, `usbconfig dump_device_desc`, `usbconfig dump_curr_config_desc` |
| `dev.txt` | `ls -l /dev`, the keyboard, mouse, disk, and USB nodes |
| `dev-input-usb.txt` | `ls -lR /dev/input /dev/usb` |
| `sysctl.txt` | `sysctl kern.evdev hw.usb`, without the `.debug` knobs |
| `kbdcontrol-i.txt` | `kbdcontrol -i < /dev/ttyv0` |
| `devctl.txt` | `/dev/devctl` read with `devd` stopped, while each device's port (root hubs aside) is powered off and on with `usbconfig power_off`/`power_on` |

Things to know when comparing against these:

- FreeBSD 15 drives HID devices through the `hid` stack, not `ukbd`/`ums`: `usbhid*` on the hub,
  `hidbus*` on it, then `hkbd*` (keyboard, `kbd2`, under `kbdmux`) and `hms*` (mice and tablets).
  Each also gets an evdev node (`/dev/input/event*`).
- FreeBSD has no UAS driver. The UAS devices stay bare `ugen`s, and `devctl` reports them as
  unmatched (`? at ... intprotocol=0x62`). It has no virtio-input driver either: the two virtio
  input functions attach as `virtio_pci3` and `virtio_pci4` with no child.
- `usbconfig` names devices from its own vendor table, so QEMU's `0x0627:0x0001` devices all show
  as "QEMU Tablet Adomax Technology"; `dmesg` and `devinfo` show the descriptors' strings.
- QEMU's xHCI has 17 root ports here (`uhub0: 17 ports`): with `p2=9,p3=8`, it lists the USB 2 and
  USB 3 ports separately. bhyve's has 8.
- QEMU makes up serial numbers for its HID devices and the UAS device from a per-run number and
  the device's path (`68284-0000:00:05.0-1`). Mask the number before comparing.
- The `devctl` capture starts after boot: it has the detach and attach events that power cycling
  produces, not the ones from the first enumeration.
