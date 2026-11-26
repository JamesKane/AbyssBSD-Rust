# Graphics performance against FreeBSD

The Radeon (and any later accelerated GPU) is measured against stock FreeBSD 15.1 with `drm-kmod` **on
the same machine** (ADR-0014; `docs/GRAPHICS.md` section 12 and 16.15). The two kernels run the same
binaries from the same disk.

- `drmbench.c`: the render node's kernel interface, by hand as libdrm_amdgpu would call it (buffer
  create/map/close, submit-and-wait round trip, a batch of submissions, the processor's cost of a
  submission, a wait on a finished job, the cost of an ioctl itself). One binary for both kernels:
  `cc -O2 -o drmbench drmbench.c; ./drmbench /dev/dri/renderD129 9` (the node number differs between the
  kernels). Every value is nanoseconds per operation, the median of 9 rounds; larger is slower.
- `glmark2` (the `glmark2-es2-gbm` binary of the FreeBSD package, the same on both kernels), off screen on
  the render node: `glmark2-es2-gbm --off-screen --winsys-options=drm-device=/dev/dri/renderD129`, three
  full runs per side. A run takes about five minutes.
- `compare-glmark2.py`: per test median of each side, the ratios and their geometric mean.
- `results/`: the raw output: `fbsd-*` (FreeBSD), `abyss-start-*` (AbyssBSD before the work of 16.15),
  `abyss-*` (after).

## FreeBSD side

Stock FreeBSD 15.1 is installed on the X870E's second disk (`docs/HARDWARE.md` section 5; boot it with
`efibootmgr -n -b 0002`). Packages: `drm-612-kmod`, the firmware packages for the chip
(`gpu-firmware-amd-kmod-gc-10-3-6`, `-dcn-3-1-5`, `-psp-13-0-5`, `-sdma-5-2-6`, `-vcn-3-1-2`), `mesa-dri`,
`libdrm`, `libglvnd` (without it glmark2 reports "Error loading EGL library"), `glmark2`. Then
`kldload amdgpu`; the node is `/dev/dri/renderD128`.
