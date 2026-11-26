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
- `accept-aarch64-gop-kms.png`: the same on arm64 (`virt` with `ramfb`, 800x600, under emulation, so
  the jellyfish runs at about 7 frames a second).

- `accept-x86_64-amd-display.png`: the same on the X870E, on the Radeon 610M's display (`amd-display`,
  `drivers/amd-kms` through `drivers/drmn`) at 1920x1080, Sway 1.12, Mesa 26.2.2 llvmpipe, the jellyfish at
  about 549 frames a second. fastfetch lists both the GeForce RTX 4070 and the AMD Raphael.
- `kmsflip.c`: a client with raw ioctls and no libdrm that sets the mode and flips between two buffers on a card
  (`kmsflip [node [hold-seconds]]`).

## Running it

```sh
cargo xtask graphics --arch x86_64 --disk disk-board.img --key devvm_key [--install] [--out DIR]
```

Boots the image in QEMU (a snapshot, unless `--install`, which also installs the packages),
starts Sway, takes the capture with `grim`, and checks the logs: the pixman renderer, a successful
modeset commit, and glmark2 on llvmpipe. The capture, `sway.log` and `glmark2.txt` go to `--out`
(default `build/graphics`). ssh is key only and never prompts.

## Making the image (a development guest, QEMU with KVM on the workstation)

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

The two reference machines, and a case in the boot tier (the packages come from the network, so it
needs a prepared image).

## Panthor on the Sky1 (docs/GRAPHICS.md §17)

- `panthor/pt_job.c`: the render node by raw ioctls, no Mesa. It runs a job that adds 5 to a
  word and waits on its syncobj; with `fault`, it also runs a stream at an unmapped address and
  checks that the group reports fatal (`pt_job [rounds [fault]]`). `panthor/panthor_drm.h` is
  Collabora's MIT uAPI header, which libdrm 2.4.134 does not install.
- `panthor/gles_probe.c`: GBM, EGL and GLES 2 on a node. It prints the renderer, then clears and
  draws a triangle and checks the pixels (`gles_probe [node]`).
- `panthor/gl_frames.c`: GL frames with no display, timed every 100 frames, to show a render
  path that slows down (`gl_frames [node [frames [upload]]]`; `upload` adds a texture upload per
  frame).
- `panthor/libdrm-platform/`: **a test harness.** It builds libdrm 2.4.134 with a platform-device
  patch into `/root/harness/libdrm` on the board. Stock 15.1 libdrm describes only PCI devices, so
  stock Mesa cannot find this GPU.
  `build.sh libdrm-2.4.134.tar.xz root@board [known_hosts]`.
- `panthor/run-sway-panthor.sh`: Sway with `WLR_RENDERER=gles2`, komeda's card for the display,
  and panthor's render node, through the harness libdrm. It uses `sway.conf` and logs to
  `/var/log/sway.log`.
- `accept-aarch64-panthor-sway.png`: the capture on the Orange Pi 6 Plus at 1680x1050.
  - Sway 1.12, Mesa 26.2.2 panfrost: `Mali-G720 MC10 (Panfrost)`, OpenGL ES 3.1.
  - The jellyfish holds about 4,100 frames a second over 90 s.
  - Taken through the harness libdrm, so it is not the stock-package acceptance.
