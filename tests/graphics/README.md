# Graphics acceptance (docs/GRAPHICS.md §14)

Stock Wayland and Sway from FreeBSD 15.1 packages, unmodified, on the card under test.

- `sway.conf`: two windows, `foot` running `fastfetch`, and `foot` running glmark2's jellyfish scene
  with its frame rate on screen. glmark2 2023.01 (the 15.1 package) has no `--show-fps`; the scene
  option is `-b jellyfish:show-fps=true`, and `--run-forever` loops it.
- `run-sway.sh`: starts Sway with a private `seatd`, the pixman renderer and software cursors, and
  logs to `/var/log/sway.log`. The log says what drew the capture: wlroots' `Creating pixman
  renderer`, and glmark2's `GL_RENDERER` (`llvmpipe`).
- `accept-x86_64-gop-kms.png`: the capture, taken with `grim` on the machine under test:
  `simpledrm` (`drivers/gop-kms`) in QEMU on x86_64, 1280x800, Sway 1.12, wlroots 0.20.2, Mesa
  26.2.2 llvmpipe. The jellyfish runs at about 400 frames a second.

## Reproducing it (a development guest, QEMU with KVM on the workstation)

```sh
# On the build server's side: an installed root with 6 GiB free, root's key, DHCP and sshd.
ABYSS_ROOT_FREE=6g ABYSS_REMOTE_DIR=abyss/mg-src cargo xtask --remote image --arch x86_64 \
    --release --root board --authorized-key "$(cat key.pub)"
rsync -S abyss-build:abyss/mg-src/build/x86_64/disk-board.img .
cargo xtask run --arch x86_64 --release --disk disk-board.img --forward 2222:22 \
    --qmp /tmp/vm.qmp --mem 4G --smp 4
# In the guest (ssh -p 2222 root@127.0.0.1, key only):
pkg install -y sway foot fastfetch glmark2 grim seatd mesa-dri mesa-libs dejavu
sh run-sway.sh &
XDG_RUNTIME_DIR=/var/run/xdg WAYLAND_DISPLAY=wayland-1 grim /root/accept.png
```

A new kernel goes into a running development guest by copying it over `/boot/kernel/kernel` and
rebooting it; QEMU exits on reboot (`-no-reboot`), so start it again.

## Not yet

arm64 under QEMU, the two reference machines, and an automated case in the boot tier.
