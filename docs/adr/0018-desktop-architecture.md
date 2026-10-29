# ADR-0018: The desktop is the Swift desktop, run as FreeBSD userland

- Status: Accepted
- Date: 2026-10-29

## Context

ROADMAP M-D asked for an original Wayland desktop. A forward design for one ("Sable": a
Vulkan-only tiling compositor, Conduit IPC, a KDL configuration tree, a Rust toolkit, terminal,
file manager, and installer) was drafted in DESKTOP.md and six companion documents on 2026-10-27.
None of it was built.

A desktop already exists. The AbyssBSD monorepo's `desktop/` (`../AbyssBSD/desktop`) is a complete
Swift 6 desktop on Wayland: the `undertow` compositor (Swift over wlroots), the Aqua toolkit and
shell (Jaguar-style menu bar, Dock, Finder), portals, a D-Bus bridge for GTK applications, System
Preferences with a privileged helper, the Islands/Shoals/Ebb window model, a theme system with two
themes, the application layer (Terminal, TextEdit, Grab, Activity Monitor, Disk Utility, Firefox
ESR), the login window, lock and power, application jails, opt-in agents, and an Anaconda-style
installer. Its PLAN.md has every phase but 4 (first metal) and 17 (delivery) complete. A harness
(`abyss/tests/run.sh --vm --live`) proves each piece in a FreeBSD guest with no human in the loop.

It runs on FreeBSD (a fork of `main`) through FreeBSD's ordinary interfaces: wlroots, libseat,
libinput, Mesa and libdrm from ports; kqueue, `pdfork`, Capsicum, jails with descriptors, devd,
sysctl, OSS, and `rc` from base. AbyssBSD's contract is to run unmodified FreeBSD 64-bit userland
(ADR-0004). A desktop that already runs on FreeBSD is therefore one this kernel has to run anyway.
Writing a second one would split the work across two products.

## Decision

1. **M-D is the Swift desktop.** AbyssBSD ships the desktop from `../AbyssBSD/desktop` and designs
   no desktop of its own. DESKTOP.md is replaced by the plan for running it here. TOOLKIT.md,
   FILEMANAGER.md, TERMINAL.md, CLIPBOARD.md, STYLE.md, and `docs/desktop-themes/` are deleted.
   They remain in git history at commit `8ff58ee`.
2. **The desktop is userland, consumed as packages.** It is built in the monorepo by its own
   toolchain (Swift 6.3.3, poudriere with its ports overlay) and installed on AbyssBSD as FreeBSD
   packages. No desktop source enters this workspace. ADR-0002 and ADR-0003 are unaffected.
3. **This repository's job is the kernel beneath it.** Each interface the desktop uses is FreeBSD
   ABI, implemented as FreeBSD 15.1 behaves (DESKTOP.md §3 lists them). The desktop's own test
   harness becomes a conformance suite here, run the way kyua is: per test, against the same
   test's result on FreeBSD 15.1 in the same machine, with every difference listed.
4. **Changes to the desktop are made in the monorepo.** Where running here needs the desktop to
   change (a UFS install layout before M-Z, a build for FreeBSD 15.x, an aarch64 package, a harness
   lane without ssh), the request goes to the monorepo's plan (DESKTOP.md §8). AbyssBSD does not
   carry patches to it.
5. **The desktop's kernel-side needs follow FreeBSD.** Where the desktop's fork plans a kernel
   change (`allow.rtprio` for jails, a devctl notify for the fixed-feature power button), AbyssBSD
   implements it once it exists in the fork, as the fork defines it, and records it in
   compat-deviations.md as an addition to FreeBSD 15.1.
6. **Graphics stays as GRAPHICS.md proposes:** the DRM user interface FreeBSD's ports see,
   implemented natively, with no LinuxKPI. The desktop reaches the GPU only through wlroots, libdrm,
   and Mesa, so it needs nothing beyond that interface. Its metal target is RDNA2, which settles
   GRAPHICS.md §13 question 1 in favor of RDNA2 as the first native amd64 GPU.

## Consequences

- M-D's first step needs no graphics. The harness's `--live` lanes run `undertow` headless with
  wlroots' pixman renderer, so they can run once P6 exits (DESKTOP.md §5, M-D.1). That makes the
  desktop a broad test of the P6 ABI well before M-G.
- M-G's exits are stated in terms of the desktop: `undertow` on `gop-kms` (M-G.1), with virtio-gpu
  acceleration (M-G.3), and on native RDNA2 (M-G.6).
- New kernel work that the original plan did not schedule: nullfs, the MAC framework with
  `mac_priority`, and `sound(4)` with OSS, `snd_dummy`, and the `sndstat` nvlist ioctls
  (DESKTOP.md §3.2). ZFS (M-Z) is needed for the installer's default
  layout, boot environments, and Disk Utility's ZFS panes.
- VISION goal 11 applies to the kernel's interfaces. The desktop is already built on BSD
  interfaces (kqueue, devd, Capsicum, jails, `pdfork`, seatd, `rc`), and its foreign stacks
  (wlroots, libinput, GTK, D-Bus for GTK applications) are FreeBSD ports, run here as FreeBSD runs
  them.
- The window model is the desktop's: overlapping windows with Islands, Shoals, and Ebb, not
  tiling. The look is Aqua by default, with the desktop's theme system.
- The desktop's metal target (an i7-12700KF with a Navi 22 RDNA2 card) is the build server, the
  Z690-A in HARDWARE.md §7. It is tested from USB, and from a spare NVMe once that is free. The
  X870E and B650E have RDNA2 iGPUs.
- The desktop's base is FreeBSD `main` (16-CURRENT); AbyssBSD's ABI target is 15.x (ADR-0004).
  Decided 2026-10-29: the desktop's packages for AbyssBSD are built for FreeBSD 15.1. Moving to
  16-CURRENT comes after the desktop runs stably here, as its own decision (DESKTOP.md §2).
- The `--full` lanes boot nested bhyve, and AbyssBSD is not a bhyve host (VISION non-goals). Their
  scenarios (medium, install, installed desktop) run as QEMU boots driven from `xtask` instead.
