# ADR-0025: Graphics: the Direct Rendering Manager interface as FreeBSD's ports see it, implemented natively, with no LinuxKPI

- Status: Accepted
- Date: 2026-11-22

## Context

M-G needs a user interface for display and graphics hardware, and ROADMAP's open decisions 6
(graphics ABI) and 7 (drivers ported from Linux) are due at its start. [GRAPHICS.md](../GRAPHICS.md)
(draft since 2026-10-27) proposes the design. FreeBSD runs these devices through `drm-kmod`, which
is the Linux driver tree on FreeBSD's LinuxKPI compatibility layer. libdrm, Mesa, wlroots and Xorg
use the Direct Rendering Manager's user interface (the ioctls on `/dev/dri/card<N>` and
`renderD<N>`), which FreeBSD's ports carry unmodified.

M-G.1 has been built to that design in the `worktree-sky1-mg-drivers` branch: `lib/abi/src/drm.rs`
(the structures), `sys/drm` (the core), `lib/kms-check`, `lib/drm-fourcc`, and `drivers/gop-kms`.
This record accepts what it implements.

## Decision

1. **The interface is the Direct Rendering Manager's**, as FreeBSD's ports see it: `/dev/dri/card<N>`
   (modesetting, one master) and later `renderD<128+N>`; ioctls, events read from the file, `mmap`
   at offsets the driver returns, poll and kqueue. Request numbers use FreeBSD's `<sys/ioccom.h>`
   encoding, not Linux's. Structures are the MIT uapi headers' (checked at compile time against
   the sizes a C compiler gives; `abi-check` cannot compare them, as the headers come from libdrm,
   a port). A caller's shorter structure is zero-extended and a longer one cut to the kernel's size
   with its tail ignored, as `drm_ioctl` does; a driver's own requests that need more (panthor's
   zero tail) check it themselves.
2. **No LinuxKPI and no `drm-kmod`.** Drivers are Rust crates against the core's per-object traits
   (`CrtcHw`, `PlaneHw`, `EncoderHw`, `ConnectorHw`), written from specifications and the
   clean-room specifications in `~/Projects/OS/chinese-wall/specs/`, never from GPL driver or core
   source. MIT sources (the uapi headers, libdrm, Mesa, wlroots) may be read and used as
   [ADR-0007](0007-license-and-clean-implementation.md) allows, and their files carry
   `SPDX-License-Identifier: MIT`.
3. **Driver names are Linux's** in `DRM_IOCTL_VERSION` (`simpledrm`, `panthor`, `msm`, `amdgpu`),
   because Mesa selects its driver by them. Any difference from what `drm-kmod` reports goes in
   `compat-deviations.md`.
4. **Buffers are placed by the driver, per buffer** (GRAPHICS.md §4.1): backing (system, contiguous,
   device-local), cache mode, and whether devices see the processor's writes. The same core serves
   chips whose display and graphics engines share the processor's memory and discrete cards with
   their own. Sharing is by dma-buf, a file kind of its own (`DTYPE_DMABUF`, 17, an AbyssBSD
   addition: FreeBSD keeps these inside LinuxKPI) that `PRIME_HANDLE_TO_FD` and
   `PRIME_FD_TO_HANDLE` make and import.
5. **One commit path.** Atomic and the older calls build a new display state and commit it the
   same way; a commit is checked whole first and waits for its frame; flip events are reserved when
   the commit starts.
6. **FreeBSD facts recorded** from its `sys/dev/drm2` (the older driver core still in the tree,
   whose device setup `drm-kmod` keeps): nodes are `root:video` (gid 44), mode 0660; the
   sysctls are `hw.dri.<n>.{name,vm,clients,bufs,vblank}` and `hw.dri.debug`; `SET_MASTER` and
   `DROP_MASTER` are root-only there. The core registers the node as FreeBSD does; it does not yet
   publish the sysctls, and lets any file take the master when there is none (seatd's flow needs
   nothing more). The sysctl names and devd notification strings for `drm-kmod` itself (15.1) are
   still **to record** on the build server; they do not block the core.

## Consequences

- Userland graphics stacks run unmodified: libdrm, Mesa (llvmpipe, then panvk, RADV, Venus),
  wlroots, Sway.
- Every native display or GPU driver is new Rust, and every file system and driver that wants to
  share memory with a device has to go through `Placement` and dma-buf. The clean-room specifications
  make the Sky1 display (`linlondp`, DisplayPort transmitter) and `panthor` possible.
- `DTYPE_DMABUF` shows in `procstat -f` as an unknown type (255) until FreeBSD's tools learn it.
- ROADMAP open decisions 6 and 7 are settled and marked so.
