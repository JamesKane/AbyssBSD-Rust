# Live media and graphical boot loader (`loader-efi`)

- Status: **Draft**. A forward design for the boot side of installing and booting AbyssBSD: the
  graphical UEFI boot loader (`boot/loader-efi`), boot environments, the live medium as this kernel
  boots it, and the handover to the desktop. The installer itself is the desktop's
  ([ADR-0018](adr/0018-desktop-architecture.md)); §5 lists what it needs from here
- Date: 2026-10-28; revised 2026-10-29 for ADR-0018 (the Rust installer `sable-install` and its
  KDL plan were removed with the Sable desktop)
- Inputs: [ADR-0006](adr/0006-uefi-acpi-only-own-loader-and-acpi.md),
  [ADR-0007](adr/0007-license-and-clean-implementation.md),
  [ADR-0018](adr/0018-desktop-architecture.md), [DESKTOP.md](DESKTOP.md) (§3.2, §8, M-D.5),
  [GRAPHICS.md](GRAPHICS.md) (§7.5, §7.6), the desktop's installer (`desktop/docs/PHASE5.md`,
  `de/install`, `de/installrun`, `abyss/mk/live-image.sh` in `../AbyssBSD`)

The short version: **boot silently from power button to the desktop's login window with one mode
set, show a boot-environment and crash-log menu only when asked, and let the desktop's own medium
and installer put AbyssBSD on a disk.** FreeBSD's Forth/Lua `loader.efi` is not used (ADR-0006).
`boot/loader-efi` grows a splash screen, a boot menu, boot environments read from ZFS, and
FreeBSD's `loader.conf`, so what the desktop's installer writes works unchanged. The desktop's
`Installer` and `abyss-install` do the installing. This repository supplies the kernel, the
loader, and the interfaces they use.

---

## 1. Goals and non-goals

### 1.1 Patterns adopted

| Peer system | Pattern | Verdict | Why |
|---|---|---|---|
| **macOS / Fedora: BGRT, early KMS, compositor handover** | Firmware sets the panel's native mode; the loader keeps it; kernel KMS inherits the running scanout (`FirmwareState::LeftRunning`) | **Adopt**, without Plymouth or an initramfs | No resolution jumps or black frames between firmware and the login window |
| **`systemd-boot` / Limine** | Silent boot by default; a held key shows the menu | **Adopt** | The menu appears when asked, when `BootNext` says so, or after an unclean reset |
| **FreeBSD `bsdinstall` (auto ZFS), `bectl(8)`** | `zroot/ROOT/<be>` with `bootfs`, separate `/home` and `/var/*` | **Adopt** | The desktop's installer already writes this layout, and `abyss update` depends on it |
| **FreeBSD's `loader.conf`** | `key="value"` tunables into the kernel environment; `autoboot_delay`, `boot_single`, `boot_verbose` | **Adopt** | The desktop's installer and live medium write `loader.conf`; FreeBSD is the reference for its names and meaning |

### 1.2 Principles

| # | Principle | Consequence |
|---|---|---|
| B1 | **One mode set, no flicker** | `loader-efi` keeps the firmware's native mode and hands it to the kernel and the compositor without a text-mode reset (§3.4) |
| B2 | **Silent unless asked** | No countdown. The menu appears on a held key, after a crash, or when `autoboot_delay` says so (§3.3) |
| B3 | **FreeBSD's configuration, read as FreeBSD reads it** | `/boot/loader.conf`, `loader.conf.local`, and `loader.conf.d/*.conf` give tunables and loader variables with FreeBSD's names. `*_load` lines for modules built into the kernel are accepted (ADR-0005) |
| B4 | **Crash evidence on screen** | On machines with no serial port, the saved `LogRing` is readable from the menu (§3.3) |

### 1.3 Non-goals

- Legacy BIOS, CSM, or MBR boot (ADR-0006).
- Forth or Lua menus, `loader.rc`, or loading kernel modules (the kernel is static).
- An installer in this repository. The desktop's is the installer (ADR-0018).

---

## 2. Who builds what

| Piece | Where | Kind | Purpose |
|---|---|---|---|
| `boot/bootproto` | here | `no_std`, pure | `BootInfo` v3: `SplashState` and `BootFlags` (§3.4) |
| `boot/loader-efi` | here | UEFI | Splash, boot menu, boot environments, `loader.conf`, `PREVLOG.TXT` viewer |
| `lib/loaderconf` | here | `no_std`, pure, fuzzed | FreeBSD `loader.conf` syntax: variables, `*_load` lines, includes, and the `loader.conf.d` order |
| `lib/qoi` | here | `no_std`, pure, fuzzed | QOI decoder for the splash image |
| `lib/zfs-boot` | here | `no_std`, pure, fuzzed | Read-only ZFS: vdev labels, uberblocks, the MOS, DSL under `zroot/ROOT/*`, and ZPL files (`lz4`, `zstd`, uncompressed) |
| `cargo xtask live-image` | here | host | A test medium: FreeBSD 15.1's `base.txz`, this kernel, `loader-efi`, and the desktop's packages, built with the desktop's medium recipe (§4) |
| Live medium recipe | monorepo (`abyss/mk/live-image.sh`) | shell | The shipping medium: UFS root, the desktop, `Installer` at login |
| `Installer`, `abyss-install` | monorepo (`de/install`, `de/installrun`) | Swift | The installer: an unprivileged GUI sending a plan to a root service over the desktop's IPC |

---

## 3. The boot loader (`boot/loader-efi`)

`boot/loader-efi` boots installed systems and live media on `amd64`, `aarch64`, and later
`riscv64`. Today it reads `\EFI\ABYSS\KERNEL` and `\EFI\ABYSS\CMDLINE`, or `/boot/kernel/kernel` from
a UFS2 root. This section adds to it.

### 3.1 Configuration

`loader-efi` reads FreeBSD's files from the root it boots: `/boot/loader.conf`, then
`/boot/loader.conf.d/*.conf`, then `/boot/loader.conf.local`, as FreeBSD's loader orders them.

- **Tunables** (`kern.*`, `hw.*`, `vfs.root.mountfrom`, …) go into the kernel environment.
- **Loader variables** it acts on: `autoboot_delay` (`-1` waits, `0` silent with the menu on a
  held key, `NO` never shows it), `boot_single`, `boot_verbose`, `boot_serial`, `console`,
  `vfs.root.mountfrom`, `kernel`, `splash_image` (an AbyssBSD addition naming a QOI file).
- **`*_load="YES"`** for a module built into the kernel is accepted silently; for any other module
  it is logged, and the boot continues. Tunables naming LinuxKPI (`compat.linuxkpi.*`) are passed
  through and ignored by the kernel.
- `\EFI\ABYSS\CMDLINE` stays as an override for development.

Anything else `loader.conf` can say is recorded in compat-deviations.md when it first matters.

### 3.2 Splash and silence

1. **Native mode.** If firmware left the display in a native mode (at least 1024×768, 32-bit
   linear), `loader-efi` never calls `SetMode`. Only from a legacy 640×480 or 800×600 mode does it
   switch once, to the highest native mode.
2. **Silence.** `ConOut->OutputString` stamps firmware text over the framebuffer, so on a quiet
   boot `loader-efi` writes nothing to `ConOut` unless the boot fails or `V` is pressed. Its log
   still goes to the UART (`SPCR`/`DBG2`) and the `LogRing`.
3. **Splash.** Drawn offscreen and copied to the framebuffer once: the OEM ACPI `BGRT` image when
   valid, else `splash_image` (default `\EFI\ABYSS\SPLASH.QOI`), and a thin progress bar.

### 3.3 Boot menu, boot environments, crash log

With `autoboot_delay="0"`, `loader-efi` checks the keyboard once (`SimpleTextInputEx`) before
loading the kernel. With no key held and a clean previous boot, the kernel loads at once. `Space`,
`Esc`, or `Shift` held, or `LogRing.recovered > 0`, opens the menu over the splash:

```
┌──────────────────────────────────────────────────────────────────────────────┐
│                                   AbyssBSD                                   │
│                                                                              │
│      ⚠ Previous boot ended with a kernel panic (saved to PREVLOG.TXT)        │
│      ┌────────────────────────────────────────────────────────────────┐      │
│      │ ▸ AbyssBSD (zroot/ROOT/default)                         Active │      │
│      │   AbyssBSD (zroot/ROOT/update-2026-10-28)         Boot Env     │      │
│      │   AbyssBSD: single user (boot -s)                              │      │
│      │   AbyssBSD: verbose                                            │      │
│      │   View previous boot's log (\EFI\ABYSS\PREVLOG.TXT)            │      │
│      │   Reboot into firmware setup                                   │      │
│      └────────────────────────────────────────────────────────────────┘      │
│       ↑/↓ Select   Enter Boot   V Verbose                                    │
└──────────────────────────────────────────────────────────────────────────────┘
```

- **Boot environments** (`lib/zfs-boot`). `loader-efi` scans GPT partitions of type
  `freebsd-zfs`, reads the four vdev labels, picks the newest valid uberblock, walks the MOS and
  the DSL directory under `zroot/ROOT`, and lists each boot environment, the pool's `bootfs` first.
  Choosing one loads its `/boot/kernel/kernel` and `loader.conf` from that dataset and sets
  `vfs.root.mountfrom=zfs:zroot/ROOT/<be>`. This is what the desktop's `abyss update` relies on to
  fall back to the previous environment (`desktop/docs/PRODUCT.md` §6.2).
- **Crash log.** On a machine with no serial port (the X870E, HARDWARE.md §5), after a panic or a
  watchdog reset, `loader-efi` saves the `LogRing` to `\EFI\ABYSS\PREVLOG.TXT` (as it does now) and
  offers it in the menu.

### 3.4 Handover (`bootproto` v3 → `sys/vt` → `gop-kms` → `undertow`)

1. **`BootInfo` v3** carries the splash:
   ```rust
   #[repr(transparent)]
   #[derive(Clone, Copy, Debug, PartialEq, Eq)]
   pub struct BootFlags(u32);

   impl BootFlags {
       /// Keep the splash on screen; buffer console text in memory.
       pub const SPLASH_QUIET: Self = Self(1 << 0);
       /// Booted from read-only live media.
       pub const LIVE_MEDIA: Self = Self(1 << 1);
       /// `boot -s` (`RB_SINGLE`).
       pub const SINGLE_USER: Self = Self(1 << 2);
   }

   #[repr(C)]
   #[derive(Clone, Copy, Debug, PartialEq, Eq)]
   pub struct SplashState {
       pub flags: BootFlags,
       pub progress_rect: [u16; 4], // x, y, width, height in framebuffer pixels
   }
   ```
2. **Quiet console.** With `SPLASH_QUIET`, `fb-console` and `sys/vt` keep console output in
   `ttyv0`'s buffer, `/dev/klog`, and the `LogRing` without drawing it. Boot milestones (ACPI,
   PCIe, storage, USB, root mount, `rc`) advance the progress bar. A panic, an `init` failure, or
   `Esc`/`F1` drops quiet mode and shows the buffered text.
3. **KMS.** `gop-kms` and native display drivers attach with `FirmwareState::LeftRunning` and adopt
   the scanout buffer without blanking (GRAPHICS.md §7.5).
4. **The desktop.** When seatd and `undertow` take the display, the mode they find is the one the
   firmware set, so their first frame (the login window) replaces the splash with no mode set.
   Anything beyond that, such as a fade from the splash, is the desktop's to add.

---

## 4. Live media

The shipping medium is the desktop's: `abyss/mk/live-image.sh` builds it with `makefs` and `mkimg`
from distribution sets and packages, with a UFS root, `seatd`, the login window, and `Installer`
in the session. AbyssBSD's medium is the same recipe with three substitutions:

- this kernel for FreeBSD's `/boot/kernel`;
- `loader-efi` on the ESP (`\EFI\BOOT\BOOTX64.EFI`, `BOOTAA64.EFI`) for FreeBSD's `loader.efi`;
- FreeBSD 15.1's distribution sets, and the desktop's packages built for 15.1 (DESKTOP.md §2).

`cargo xtask live-image --arch {x86_64,aarch64}` makes that medium for QEMU and for USB sticks on
the reference machines, from the recipe in the monorepo, so the two cannot drift. The recipe
accepting a kernel and loader from outside is an ask of the desktop's plan (§6).

On new hardware, the medium is also how a board's ground truth is captured (`acpidump -dt`,
`devinfo -rv`, `pciconf -lv`, EDIDs) into `tests/corpus/<board>/` (P7.0).

---

## 5. What the desktop's installer needs from here

The installer writes a GPT with an ESP, swap, and a ZFS pool, extracts the sets with `tar`,
creates the account with `pw -R`, and writes `loader.conf` and `rc.conf`. Each step is FreeBSD
userland on this kernel:

| Step | Interfaces | Where here |
|---|---|---|
| Find disks | `geom disk list`, `kern.disks`, `DIOCGIDENT` (serial numbers), `DIOCGMEDIASIZE` | GEOM work in progress (`lib/geom`, `geom_kern`); P3.7 |
| Partition | `gpart create/add/destroy` (GEOM's `gctl` requests), `/dev/gpt/*` labels | GEOM work in progress |
| ESP | `newfs_msdos`, `mount_msdosfs` | P5.6 |
| Root | `zpool create`, `zfs create`, `bootfs` (ZFS); `newfs -U -j` (UFS, §6) | M-Z; P5.5, P5.7 |
| Extract, configure | `tar` (libarchive), `pw -R`, `pwd_mkdb`, file flags and ACLs | P5, P6.7 |
| Boot entry | `efibootmgr` (`/dev/efi`, UEFI variable services) | Runtime services are in (P3.8); the `/dev/efi` device for `efivar` is planned (HARDWARE.md §3) and not yet in a phase |
| Loader | `loader-efi` copied to the ESP in place of FreeBSD's `loader.efi` | §6 ask |

**Disks with user data.** On the X870E only the second NVMe may be written (HARDWARE.md §5). The
installer must name its target by serial number as well as by device and refuse a disk whose
serial differs at write time. Whether `abyss-install` already does is to check (§7 question 4).

---

## 6. Asks of the desktop's plan

In addition to DESKTOP.md §8:

1. The medium recipe takes the kernel and the loader as inputs (§4).
2. `abyss-install` installs `loader-efi` when the target runs AbyssBSD's kernel, and offers the UFS
   layout (DESKTOP.md §8 ask 2) until M-Z.
3. The installer identifies target disks by serial number (§5).

---

## 7. Phasing

| Step | Work | Needs | Exit |
|---|---|---|---|
| **I.1** | `lib/loaderconf`, `lib/qoi`, `bootproto` v3; splash, silence, and the boot menu in `loader-efi`; the quiet console in `fb-console` and `sys/vt` | — | QEMU boots silently through the splash on both arches; a held `Space` opens the menu; FreeBSD 15.1's `loader.conf` from a P6 image is honored |
| **I.2** | `lib/zfs-boot`, fuzzed against `zpool`-made images from the build server | M-Z's on-disk work, or images made on FreeBSD | `loader-efi` lists and boots `zroot/ROOT/*` from images FreeBSD wrote |
| **I.3** | `cargo xtask live-image` over the desktop's recipe | DESKTOP.md M-D.2 | The medium boots this kernel to the desktop's login window in QEMU |
| **I.4** | The desktop's installer installs AbyssBSD (UFS, then ZFS) | I.3, §5's rows, M-Z for ZFS | DESKTOP.md M-D.5's exit: the install lanes pass as QEMU boots, and an install to the X870E's second NVMe boots to the desktop |

---

## 8. Open questions

1. **Kernels from ZFS or from the ESP.** `lib/zfs-boot` (§3.3) reads kernels from any boot
   environment with no copying. The alternative stages each environment's kernel on the ESP and
   keeps ZFS out of the loader, at the cost of a copy step whenever a boot environment is made.
2. **One password for disk and login.** With an encrypted pool, should the passphrase entered in
   `loader-efi` also unlock the session, so the login window does not ask again? That needs a
   handoff the desktop's login window accepts.
3. **Live medium persistence.** Whether spare space on a large stick becomes a writable partition
   (for saved settings and corpus captures) on first boot, or stays unallocated unless asked.
4. **Does `abyss-install` check serial numbers today** (§5)? If not, it is a desktop-side change
   before any install to the X870E.
