# Desktop

- Status: **Draft**. The plan for M-D under [ADR-0018](adr/0018-desktop-architecture.md)
  (Accepted)
- Date: 2026-10-29
- Scope: running the AbyssBSD Swift desktop (`../AbyssBSD/desktop`) on this kernel: what it needs
  from the kernel, the order those needs are met in, how it is tested and measured here, and what
  this project asks of the desktop's own plan. The desktop's design is its own documents'
  (`desktop/docs/PLAN.md`, `PRODUCT.md`, `STATUS.md`) and is out of scope here
- Inputs: [ADR-0004](adr/0004-abi-target.md), [ADR-0014](adr/0014-performance-parity-per-component.md),
  [GRAPHICS.md](GRAPHICS.md), [MULTIUSER.md](MULTIUSER.md), [USB.md](USB.md),
  [INSTALLER.md](INSTALLER.md), and a survey of the desktop's sources (2026-10-29)

The short version: **AbyssBSD designs no desktop. It runs the Swift desktop as FreeBSD userland,
from packages, unmodified.** The desktop already runs on FreeBSD through ordinary FreeBSD
interfaces, so every need it has is FreeBSD ABI that this kernel implements anyway. Most of that
ABI is done (P4–P6). The rest is scheduled below. The desktop's own harness becomes a conformance
suite here, compared test by test with FreeBSD 15.1, and its first lanes need no graphics at all.

## 1. The desktop

The desktop lives in the AbyssBSD monorepo (`../AbyssBSD`), beside its FreeBSD fork (`src/`), the
drm-kmod fork, and a ports overlay. It is Swift 6, with C only for shims over system libraries:

| Part | What |
|---|---|
| `undertow` | The compositor: Swift over wlroots 0.20, libseat for the seat, explicit sync (`linux-drm-syncobj`) |
| Aqua | Toolkit and shell: menu bar with a global menu protocol, Dock, Finder, Islands/Shoals/Ebb (workspaces, window sets, an Exposé equivalent), two themes (Jaguar, Trench) |
| Portals, `abyss-dbus` | Capability portals (descriptors, not paths); a D-Bus bridge so GTK applications get the menu bar and the Finder as their file chooser |
| Applications | System Preferences (privileged helper), Terminal, TextEdit, Grab, Activity Monitor, Disk Utility, Firefox ESR from ports |
| Session | Login window, lock, idle, power, `anchor` supervisor, application jails (`abyss-jaild`), opt-in agents in jails |
| Installer | `Installer` (unprivileged GUI) and `abyss-install` (root): GPT, ESP, ZFS root with boot environments, dist-set extraction |
| Harness | `abyss/tests/run.sh`: unit tests, `--live` lanes (headless compositor, virtual input, screenshots), `--full` lanes (nested bhyve: medium, install, installed desktop) |

Status on 2026-10-29 (its PLAN.md): every phase complete but 4 (first metal, mostly done on an
i7-12700KF with an RX 6750 XT) and 17 (delivery: overlay, release pipeline, `abyss update`).

## 2. The contract

- **Binaries, not source.** The desktop is built in the monorepo by its toolchain (`lang/swift6`
  6.3.3, poudriere with the overlay) and installed here as packages. Nothing of it is built in
  this workspace.
- **FreeBSD 15.1 userland.** The desktop's base is FreeBSD `main`; AbyssBSD's ABI target is 15.x
  (ADR-0004). Everything the desktop calls exists in 15.1, including jail descriptors
  (`JAIL_OWN_DESC`, `jail_attach_jd`, `EVFILT_JAILDESC`), `kqueuex`, and `timerfd`, checked
  against the `release/15.1.0` tag. Its packages for AbyssBSD are built in a 15.1 poudriere jail
  (the build server runs 15.1). Decided 2026-10-29; 16-CURRENT follows once the desktop runs
  stably here.
- **Stock world.** The image is FreeBSD 15.1's `base.txz` plus the desktop's packages and their
  dependencies, on this kernel. The fork's `src/` changes are board and LinuxKPI work (the Q8B,
  drm-kmod), none of them desktop-specific, and none needed here.
- **No kernel modules.** The kernel is static (ADR-0005). What the desktop loads (`kld_list="amdgpu
  hms mac_priority"`, `zfs_load`) must already be built in, with `kldload` and `kldstat` answering
  as on a kernel with every module built in (MULTIUSER.md). `loader.conf` tunables that name
  LinuxKPI (`compat.linuxkpi.*`, `hw.amdgpu.si_support`) are accepted and ignored.

## 3. What it needs from the kernel

From the desktop's sources, its port's dependencies (wlroots, libseat, libinput, Mesa, libdrm,
cairo, pixman, GTK, Firefox), and the commands it runs.

### 3.1 Already in place

| Need | Used by | Where here |
|---|---|---|
| rtld, ELF TLS, `dl_iterate_phdr`, libthr and umtx, libc++ | Swift runtime, every program | P6.1–P6.3 |
| kqueue (`EVFILT_READ`, `USER`, `TIMER`, `VNODE`, `PROCDESC`, `JAILDESC`), `kqueuex`, `poll`, `timerfd` | Event loops, libdispatch, config watching (`cpoolwatch`), libwayland's epoll-shim | P6.4 |
| UNIX sockets with `SCM_RIGHTS`, `getpeereid`, `MSG_NOSIGNAL`, `SO_NOSIGPIPE` | Wayland, desktop IPC, portals, seatd | P6.5 |
| `shm_open(SHM_ANON)`, `ftruncate`, `mmap` | Wayland shm buffers | P6.3, P5 |
| ptys (`posix_openpt`, `TIOCSWINSZ`, `TIOCSCTTY`) | Terminal | P6.6 |
| `pdfork` (`PD_CLOEXEC`, `PD_DAEMON`), `pdkill`, `POLLHUP` on process descriptors | `anchor`, `cproc`, `abyss-jaild` | P4.5, P6.4 |
| Capsicum (`cap_enter`, `cap_getmode`) | Sandboxed components | P6.7 |
| Jails with descriptors, `enforce_statfs`, `persist`, `devfs_ruleset`, `ip4`/`ip6` `disable`/`inherit` | Application and agent jails | P6.7 |
| `setusercontext`, login classes, every `RLIMIT_*` (`NPTS`, `KQUEUES`, `UMTXP`, `SWAP`, `SBSIZE`, `VMEM`) | Sessions, jail classes | P6.7 |
| `rtprio(RTP_PRIO_REALTIME)` | Compositor's present thread | P6.2 |
| `kinfo_proc` (`KERN_PROC_PROC`, `KERN_PROC_PATHNAME`), `vm.stats.vm.*`, `kenv` (`smbios.*`) | Activity Monitor, System Profiler | P6.8, P3.8 |
| devd (`/var/run/devd.pipe`), devctl events | `vents`, libinput's libudev-devd | P6 (`devd` runs at the P6 exit) |
| evdev (`/dev/input/event*`, `EVIOCGRAB`, `EVIOCREVOKE`), `hms`, `ukbd` | libinput through seatd | Track U |
| `vt` windows: `VT_ACTIVATE`, `VT_WAITACTIVE`, keymaps, process switching (`VT_SETMODE` with `VT_PROCESS`, `VT_RELDISP`, `KDSETMODE`, `KDSKBMODE`) | Login window, fast user switching, seatd | Track U (ADR-0015) |
| PAM, `crypt`, `arc4random_buf`, OpenSSL | Login, installer, agents' TLS | Base libraries over P6 |
| tmpfs, devfs mounts and rulesets | Jails | P5.2, P6.7 |

Each row is "in place" as far as kyua and the P6 tests show. The desktop's harness (§5) is the
check that it is in place for this program.

### 3.2 Not yet scheduled

| Need | Used by | Proposed home |
|---|---|---|
| **nullfs** (`ro`, `nosuid`) | `abyss-jaild` mounts the session's directories into jails | VFS.md non-goal today; add to P6.10 or a VFS follow-up before M-D.1 |
| **MAC framework with `mac_priority`** (the `realtime` group may use `rtprio`) | Real-time compositor thread for an unprivileged user | MULTIUSER.md answers "no MAC"; add the framework's hooks `mac_priority` uses, before M-D.2 |
| **`sound(4)`**: OSS `/dev/dsp` and `/dev/mixer`, `sndstat` (`SNDSTIOC_*`, nvlist), `hw.snd.default_unit`, `snd_dummy` | Sound pane, volume keys, Firefox (cubeb's OSS backend), harness | No audio plan exists. `snd_dummy` and the sound core before M-D.1's sound lanes; HDA and USB audio before M-D.4 |
| **ACPI sleep and buttons**: `acpiconf -s 3` (S3), `hw.acpi.supported_sleep_state`, `hw.acpi.power_button_state`, lid and button devd notifies, `hw.acpi.battery.*` | Power, lid, Sleep menu item | P9 power management; the sysctls and notifies earlier (P7) |
| **Networking for the desktop**: `getifaddrs` with `AF_LINK`, `PF_ROUTE` sockets, `wlan(4)`, `wpa_supplicant`, `ifconfig wlanN create` | Network pane, Wi-Fi, Firefox | P8; Wi-Fi drivers after P8 |
| **ZFS**: `zpool`, `zfs`, boot environments, `zfs snapshot/rollback` | Installer default layout, Disk Utility, `abyss update` | M-Z |
| **GELI** (`geom_eli`, keys from the loader's boot key buffer) | Encrypted installs: one password for disk and login (INSTALLER.md §3.4) | Not scheduled; before M-D.5's encrypted installs |
| **GEOM tools**: `geom disk list`, `gpart`, `newfs_msdos`, `mdconfig` | Installer, harness | Partly in progress (`geom_kern`); finish before M-D.5 |
| **`allow.rtprio` jail parameter, power-button devctl notify** | Desktop features its fork plans (ADR-0018 rule 5) | When the fork defines them |
| **`vmm`, `nmdm`** | `--full` lanes (nested bhyve) | Not planned (AbyssBSD is not a bhyve host); those lanes become QEMU boots (§5.3) |

### 3.3 Graphics

The desktop never opens `/dev/dri` itself. wlroots (DRM/KMS backend, GBM, EGL, the
`linux-dmabuf` and `linux-drm-syncobj` protocols), libseat (DRM master), and Mesa (radeonsi and
RADV on amdgpu) do, through the interface [GRAPHICS.md](GRAPHICS.md) defines. Specific points to
carry into M-G:

- **libdrm discovery.** The overlay patches libdrm for platform devices and a `dev.drm.N.busid`
  sysctl. GRAPHICS.md §13 question 2 (record FreeBSD's discovery) must include what the patched
  libdrm reads, not only stock libdrm.
- **Syncobj timelines** are required, not optional: `undertow` implements explicit sync.
- **Renderer.** wlroots renders with pixman, GLES2 (EGL), or Vulkan. Headless and `gop-kms` use
  pixman. On virtio-gpu, `undertow` uses **wlroots' Vulkan renderer on Venus** (decided
  2026-10-29), so virgl is not needed. `undertow` creates its renderer with
  `wlr_renderer_autocreate` and lets `WLR_RENDERER` override it (`de/undertow/Backend.swift`), so
  this is a session setting (`WLR_RENDERER=vulkan`), not a compositor change. The Vulkan renderer
  needs Venus to import and export dma-bufs with DRM format modifiers
  (`VK_EXT_image_drm_format_modifier`, `VK_EXT_external_memory_dma_buf`), which M-G.3's exit
  includes. GL clients such as Firefox's WebRender use Mesa's Zink over Venus, or render in
  software.
- **Master and seat.** libseat drives `DRM_IOCTL_SET_MASTER`/`DROP_MASTER` and passes device
  descriptors over `SCM_RIGHTS`; seatd's socket is group `video`.

## 4. Order

The desktop needs graphics in only some of its lanes, so it arrives in steps as the kernel grows:

| Stage | Kernel | What runs |
|---|---|---|
| Headless | P6 exit, plus nullfs and `snd_dummy` | Every `--live` lane: `undertow` with `WLR_BACKENDS=headless WLR_RENDERER=pixman`, clients, virtual pointer and keyboard, screenshots by `grim` |
| First light | M-G.1 (`gop-kms`) | `undertow` on KMS with pixman, seatd, libinput on evdev, `vt` switching, the login window; QEMU and both reference machines' firmware framebuffers |
| Accelerated in QEMU | M-G.2, M-G.3 | dma-buf, syncobj, GPU rendering on virtio-gpu, Firefox with acceleration |
| Metal | M-G.6 (native RDNA2), audio, P9 power | The full desktop on the reference machines, frame timing measured against vblank |
| Installed | INSTALLER.md, M-Z | The desktop's medium and installer put AbyssBSD on a disk |
| aarch64 | M-G.4, M-G.5 | The Orange Pi 6 Plus, once the desktop has aarch64 packages |

## 5. Testing

### 5.1 The harness as a conformance suite

The desktop's `abyss/tests/run.sh` lanes run on AbyssBSD and on FreeBSD 15.1, on the same QEMU
machine, from the same packages and the same test tree. As with kyua, a lane that passes on
FreeBSD must pass here, or be listed in `tests/desktop/known-differences.txt` with its reason and
the milestone that removes it. Lanes that skip on FreeBSD (`live-gpu.sh` and `live-syncobj.sh`
read Linux's `/sys/class/drm` and `/proc`) are not counted until they have a FreeBSD form (§8).

### 5.2 Running it without a network

`run.sh --vm` copies the tree into its guest with rsync over ssh, and AbyssBSD has no networking
until P8. Until then `xtask` builds an image with the packages and the test tree already in it,
and drives the lanes from the serial console, as `xtask multiuser` drives the P6 tests. After P8,
the `--vm` path is used as it is.

### 5.3 The `--full` lanes

These boot nested bhyve inside the guest. Here each becomes a QEMU boot that `xtask` drives
directly: the medium boots, the installer runs against a scratch disk, and the installed disk
boots to the desktop. This matches ADR-0010 and needs no `vmm`.

## 6. Performance

Under ADR-0014 the desktop is a component (`desk`), measured against the same packages on FreeBSD
15.1 on the same machine:

- **Headless** (bhyve, from M-D.1): the desktop's C1–C3 benchmarks, which its PHASE6 already
  defines on the headless backend (frame time, isolation from adversarial clients), plus session
  start time and the `--live` lanes' wall time as diagnostics.
- **Metal** (from M-D.4): C1 against a real vblank on the reference machines, against FreeBSD with
  `drm-kmod` on the same machine. GRAPHICS.md §12's `drm.*` components gate the driver beneath it.

The geometric mean of `desk` is within 2% of FreeBSD, as for every component. The desktop's own
contract (C1–C5) still applies, judged by its plan.

## 7. Hardware

- **amd64.** The desktop's metal target, an i7-12700KF with a Navi 22 card (RDNA2, `1002:73df`),
  is the build server: the Z690-A in HARDWARE.md §7. Desktop runs there boot from USB, outside CI
  and benchmark windows, and never write its internal drives. Installs go to a spare NVMe, which
  is fitted once it is freed, after the CIX board (the Orange Pi 6 Plus) is running. The X870E
  and B650E carry RDNA2 iGPUs (Radeon 610M). One native driver family, RDNA2, covers all three,
  which is why ADR-0018 makes it the first native amd64 GPU. NVIDIA (RTX 4070, RTX 5070 Ti) does
  not gate the desktop.
- **arm64.** The desktop's port is amd64-only today. Swift 6.3.3 runs on arm64 FreeBSD (built on
  the Q8B), and the desktop has run there by hand. The Orange Pi 6 Plus needs the desktop's arm64
  packages (§8) and GRAPHICS.md's Sky1 display and `panthor`.
- **rv64.** Not planned. Swift on FreeBSD riscv64 does not exist.

## 8. Asks of the desktop's plan

Changes to the desktop are made in the monorepo (ADR-0018 rule 4). This plan needs:

1. **A FreeBSD 15.1 package build** of the desktop and its overlay, from a 15.1 poudriere jail
   (§2), kept green alongside its `main` build.
2. **A UFS install layout** in `abyss-install`, used when ZFS is not available (before M-Z). Its
   live medium already has a UFS root.
3. **A FreeBSD form of the GPU lanes** (`live-gpu.sh`, `live-syncobj.sh`) using `sysctl` and
   `procstat` instead of `/sys` and `/proc`.
4. **A harness mode that runs from a prepared image** without ssh or rsync (§5.2), and the
   `--full` scenarios as steps an outer driver can run (§5.3).
5. **arm64 packages**: the port's `ONLY_FOR_ARCHS` widened, and the Q8B's hand-built Swift made
   a package.
6. **The Vulkan renderer on virtio-gpu**: the session sets `WLR_RENDERER=vulkan` when the GPU is
   virtio-gpu (§3.3), and `undertow`'s lanes pass with the Vulkan renderer as they do with GLES2.

## 9. Open questions

1. ~~15.1 or 16~~: decided 2026-10-29. The desktop is built for 15.1 (§2). 16-CURRENT, which the
   desktop and its fork already use, comes after the desktop is stable here.
2. ~~Is the desktop's 12700KF the build server?~~ Yes (decided 2026-10-29). It is tested from USB,
   and from a spare NVMe once that is free (§7).
3. ~~virtio-gpu renderer~~: decided 2026-10-29. wlroots' Vulkan renderer on Venus (§3.3).
4. **Audio.** No audio design exists in this repository. The desktop needs OSS `sound(4)` first and
   HDA soon after. It needs its own document, like USB.md.
5. **What AbyssBSD releases.** The desktop's Phase 17 builds a release from its fork's world. An
   AbyssBSD release is this kernel with FreeBSD's 15.1 world and the desktop's packages. Whether
   the two releases converge (one medium, either kernel) is for the desktop's Phase 17 and
   INSTALLER.md to settle together.

## 10. Phasing within M-D

| Step | Work | Needs | Exit |
|---|---|---|---|
| **M-D.0** | The desktop's 15.1 packages (§8 ask 1); `xtask` image with the packages and test tree (§5.2); FreeBSD 15.1's results for every lane recorded | P6 | The FreeBSD 15.1 baseline is recorded in `tests/desktop/` |
| **M-D.1** | Headless: nullfs, `snd_dummy` and the `sound(4)` core, whatever the lanes find | P6 exit | Every `--live` lane passes as on FreeBSD 15.1, or is listed; `desk` (headless) within 2% in bhyve |
| **M-D.2** | First light: `undertow` on `gop-kms`, seatd, MAC with `mac_priority`, libinput on evdev, the login window | M-G.1, P7 | The desktop is used with a real keyboard and mouse in QEMU and on the X870E and Orange Pi 6 Plus framebuffers (the latter on arm64 packages, §8 ask 5) |
| **M-D.3** | Accelerated in QEMU: dma-buf, syncobj, `undertow` on wlroots' Vulkan renderer over Venus (§8 ask 6); Firefox; network lanes | M-G.2, M-G.3, P8 | `undertow` composites on Venus in QEMU with explicit sync; Firefox renders a page under it (WebRender on Zink, or software, recorded); the network lanes pass |
| **M-D.4** | Metal: native RDNA2, HDA audio, S3 and lid | M-G.6, audio, P9 | `desk` (metal) within 2% of FreeBSD with `drm-kmod` on the same machine (the X870E, and the Z690-A from USB); the desktop's C1 holds |
| **M-D.5** | Installed: the desktop's medium boots this kernel, `abyss-install` installs it (UFS before M-Z, ZFS with boot environments after) | INSTALLER.md I.1–I.3; M-Z for ZFS | The `--full` scenarios pass as QEMU boots (§5.3), and an install to the X870E's second NVMe, and to the Z690-A's spare NVMe once fitted, boots to the desktop |
| **M-D.6** | arm64: the Orange Pi 6 Plus with accelerated graphics | M-G.4, M-G.5 | The headless lanes on arm64 in QEMU, and the desktop on the board with `panthor` |
