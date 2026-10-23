# FreeBSD reference recordings

How FreeBSD 15.1-RELEASE names and describes the devices of the P3 reference configurations, for
the differential tests in `lib/devtree` (and later the device manager's).

| Directory | Configuration |
|---|---|
| `bhyve/` | bhyve on the build server: the release VM image on virtio-blk (slot 2), virtio-rnd (3), a second virtio-blk (4), NVMe (`nvme,ram=64`, 5), an AHCI disk (`ahci-hd`, 6), framebuffer (29), LPC (31); 4 vCPUs, 2 GiB |
| `usb/` | The USB and input devices, in QEMU and in bhyve (ROADMAP U.0); see [usb/README.md](usb/README.md) |

Each file is one command's output: `devinfo -rv`, `devinfo -u`, `pciconf -lv`, and `sysctl dev`.
`cargo xtask --remote freebsd-record` captures them (the guest runs a script from a second disk
and prints the output on its serial console).
