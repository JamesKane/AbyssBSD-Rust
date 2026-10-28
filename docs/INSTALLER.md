# Live USB Installer & Graphical Bootloader (`sable-install` & `loader-efi`)

- Status: **Draft**. A cross-repository forward design for the AbyssBSD Live USB media, modern
  graphical UEFI bootloader (`boot/loader-efi`), and Sable desktop installer (`sable-install`),
  spanning `AbyssBSD-Rust` (P7–P9, M-G, M-Z) and `sable-de` (M-D.6).
- Date: 2026-10-28
- Scope: replacing FreeBSD's legacy `loader.efi` with a zero-flicker graphical UEFI bootloader and
  Boot Environment selector; assembling the hybrid Live USB image; probing hardware to populate
  Human Interface Guidelines (HIG) defaults without querying the user; the minimal two-step Express
  installation flow; the Custom Storage & OpenZFS Pool Architect; and the privileged `sable-sysd`
  execution pipeline.
- Inputs: [ADR-0006](adr/0006-uefi-acpi-only-own-loader-and-acpi.md),
  [ADR-0007](adr/0007-license-and-clean-implementation.md),
  [ADR-0018](adr/0018-desktop-architecture.md), [DESKTOP.md](DESKTOP.md) (§2.3, §6.8, §7, §8.6),
  [GRAPHICS.md](GRAPHICS.md) (§7.5, §7.6), [STYLE.md](STYLE.md), [TOOLKIT.md](TOOLKIT.md),
  [ASSISTANT.md](ASSISTANT.md), [WINE.md](WINE.md)

The short version: **boot the USB stick straight into a live, hardware-accelerated Sable desktop
with zero mode resets, infer display scaling, refresh rate, touchpad gestures, power policy, and
disk geometry from hardware instead of interrogating the user, and install AbyssBSD into an
OpenZFS Boot Environment (or UFS2 SU+J root) in two steps and under thirty seconds.** FreeBSD's
Forth/Lua `loader.efi` is replaced by `boot/loader-efi`—a Rust UEFI boot manager configured in KDL
(`\EFI\ABYSS\BOOT.KDL`) that renders a high-DPI splash screen (`BGRT` or `\EFI\ABYSS\SPLASH.QOI`)
matched to the user's Sable rice, stays completely silent on normal boots (`timeout = 0`), reveals
an interactive OpenZFS Boot Environment and crash-log selector when `Space` or `Esc` is held (or
automatically after a kernel panic), and hands the live GOP framebuffer to the kernel (`fb-console`
→ `gop-kms` → `sable-comp`) without a single black frame.

---

## 1. Goals, Non-Goals, and Peer Review

### 1.1 Peer Systems Review: Patterns to Adopt and Avoid

| Peer System | Architecture & UX Pattern | Verdict | Rationale |
|---|---|---|---|
| **macOS / Fedora BGRT + Early KMS + Wayland Handover** | Firmware sets native panel mode via UEFI GOP; bootloader preserves scanout; kernel KMS inherits the running pipe (`FirmwareState::LeftRunning`); Wayland compositor cross-fades directly from the splash framebuffer | **Adopt** (without Plymouth or `initramfs`) | Eliminates resolution jumps and black-screen flashes. Because we control `loader-efi`, `fb-console`, `sys/vt`, `gop-kms`, and `sable-comp`, we achieve a seamless splash-to-desktop transition with zero extra daemons |
| **`systemd-boot` / Limine** | Instant silent boot (`timeout = 0`) by default; holding a modifier key reveals the boot menu; declarative configuration file on the ESP | **Adopt** | Staring at a 10-second ASCII countdown on every boot wastes time. The boot menu appears only when asked (`Space` / `Esc`), when `BootNext` is set, or automatically after an unclean reset (`LogRing.recovered > 0`) |
| **FreeBSD `bsdinstall` (Auto-ZFS)** | Opinionated `zroot` pool creation with `bectl(8)`-compatible Boot Environment dataset layout (`zroot/ROOT/default`, separated `/home` and `/var/*`) | **Adopt** (storage layout) / **Avoid** (`dialog` TUI) | FreeBSD's ZFS Boot Environment layout is the foundation for atomic OS updates, [ASSISTANT.md](ASSISTANT.md) task jails, and [WINE.md](WINE.md) CoW prefixes. We keep the exact dataset hierarchy and replace the text wizard with a native GPU-rendered application |
| **Pop!_OS (`distinst`) & Elementary OS Installer** | Opinionated two-step "Clean Install" vs. "Custom Partitioning" split; single passphrase shared between user login and disk encryption | **Adopt** | Eliminates decision fatigue for 95% of installations while keeping a first-class custom path one click away |
| **NixOS Declarative Installer** | Every installer choice materializes as a human-readable configuration file that can be inspected, versioned, or replayed headlessly | **Adopt** | Every control in `sable-install` edits a typed KDL v2 document (`install.kdl`) validated by `lib/install-plan`. Users can view or edit `install.kdl` live (`Ctrl+E`) or run `sable-install --apply install.kdl` headlessly |
| **Calamares / Anaconda / Ubuntu Subiquity** | Multi-hundred-megabyte Python/Qt/Webview stacks; 8–12 mandatory wizard pages asking for keyboard layout, locale, timezone, and display scaling before showing a desktop | **Avoid** | Violates [STYLE.md](STYLE.md) S2 (*"Native or nothing—no web apps"*). Most questions asked by traditional installers can be answered accurately by probing EDID, SMBIOS, ACPI, USB HID, and NVMe identify structures |

### 1.2 Principles

| # | Principle | Consequence |
|---|---|---|
| I1 | **Probe hardware, don't interrogate the user** | Display scaling, refresh rate, VRR, laptop vs. desktop power/panel profile, touchpad gestures, GPU Vulkan ICD, ZFS `ashift`, TRIM, and swap size are derived automatically from hardware descriptors (§5). |
| I2 | **Live desktop first, not a trapped wizard** | The USB boots straight into a complete, unthrottled Sable desktop session with `sable-install` open as an ordinary window. The user can tile `sable-term`, `sable-files`, or `abyssdoc` beside it at any time (§4). |
| I3 | **Two steps for Express, full power for Custom** | Express mode collects only identity, visual rice, and target drive. Custom mode exposes multi-disk ZFS vdev topologies, dual-boot alongside mode, dataset tuning, and live `install.kdl` editing (§6). |
| I4 | **Protected by construction against data loss** | Multi-disk RAID/ZFS data arrays and the booted Live USB are locked out of Express selection; target disks are double-checked by both device node and hardware serial number (`DIOCGIDENT`); foreign OS ESPs and UEFI `BootXXXX` entries are never overwritten (§7.1). |
| I5 | **Zero-flicker boot from power button to desktop** | `loader-efi` sets or keeps native GOP resolution once, draws the splash screen, and hands `SplashState` to the kernel and `sable-comp` with zero text-mode resets (§3). |

### 1.3 Non-Goals

- **Legacy BIOS / CSM boot or MBR partitioning.** AbyssBSD is strictly UEFI 2.7+ and GPT on all
  architectures ([ADR-0006](adr/0006-uefi-acpi-only-own-loader-and-acpi.md)).
- **Forth or Lua scripting in the bootloader.** Boot configuration is declarative KDL v2
  (`\EFI\ABYSS\BOOT.KDL`), parsed by `lib/kdl`.
- **Network-mandatory installation.** The Live USB carries complete offline `.txz` distribution sets
  and a local `pkg` repository for GPU/Vulkan/XKB dependencies so air-gapped installs succeed out of
  the box.

---

## 2. Cross-Repository Architecture

Because the boot pipeline, live root mount, and on-disk format engines live in `AbyssBSD-Rust` while
the Wayland compositor, hardware-to-HIG session configuration, `sable-sysd`, and native GUI toolkit
live in `sable-de`, the design is partitioned cleanly across both workspaces:

```
 ┌────────────────────────────────────────────────────────────────────────────┐
 │                        AbyssBSD-Rust Workspace                             │
 │                                                                            │
 │  boot/loader-efi ──► boot/bootproto (v3) ──► drivers/fb-console & sys/vt   │
 │   • \EFI\ABYSS\BOOT.KDL (lib/kdl)             • SplashState.quiet hold     │
 │   • Splash renderer (BGRT / lib/qoi)          • Esc/F1/panic → text log    │
 │   • UFS2 (lib/ufs) & ZFS BE (lib/zfs-boot)    • gop-kms LeftRunning        │
 │   • Interactive BE + Crash Log picker                                      │
 │                                                                            │
 │  xtask/src/live.rs (`cargo xtask live-image`)                              │
 │   • Builds hybrid GPT `abyss-live-<arch>.img` (ESP + read-only UFS2 +      │
 │     /usr/dist/{base,kernel,desktop,wine,agent}.txz + offline pkg cache)    │
 └─────────────────────────────────────┬──────────────────────────────────────┘
                                       │ FreeBSD 15.1 / AbyssBSD ABI seam
                                       ▼
 ┌────────────────────────────────────────────────────────────────────────────┐
 │                           sable-de Workspace                               │
 │                                                                            │
 │  sable-comp (M-D.2)                                                        │
 │   • Adopts KMS framebuffer & cross-fades boot splash → Live Desktop        │
 │                                                                            │
 │  lib/install-plan (#![no_std], #![forbid(unsafe_code)], fuzzed)            │
 │   • HardwareProfile → HIG Config generator (scale, VRR, panel, touchpad)   │
 │   • Drive safety classifier, 1 MiB GPT calculator, ZFS/UFS2 layout engine  │
 │   • Declarative `install.kdl` parser, validator, and formatter             │
 │                                                                            │
 │  desk/install (`sable-install`, #[app]) ◄──Conduit──► desk/sysd (`install`)│
 │   • 2-step Express UI & Custom Architect               • Hardware prober   │
 │   • Live 1-frame Rice preview                          • GPT/ZFS/UFS2 init │
 │   • Inline `install.kdl` split editor                  • cap_enter unpack  │
 └────────────────────────────────────────────────────────────────────────────┘
```

### 2.1 Crates and Deliverables

| Crate / Module | Workspace | Kind | `unsafe` | Purpose |
|---|---|---|---|---|
| `boot/bootproto` | `AbyssBSD-Rust` | `no_std`, pure | Forbidden | `BootInfo` v3: adds `SplashState` and `BootFlags` (`SPLASH_QUIET`, `LIVE_MEDIA`, `SAFE_MODE`, `SINGLE_USER`) |
| `lib/qoi` | `AbyssBSD-Rust` | `no_std`, pure, fuzzed | Forbidden | Lossless QOI image decoder/encoder (~300 lines) for bootloader and kernel splash graphics |
| `lib/zfs-boot` | `AbyssBSD-Rust` | `no_std`, pure, fuzzed | Forbidden | Read-only OpenZFS vdev label, uberblock, MOS, DSL Boot Environment (`zroot/ROOT/*`), and ZPL file reader (`lz4`/`zstd`/raw) for `loader-efi` |
| `boot/loader-efi` | `AbyssBSD-Rust` | UEFI (`pe32+`) | Minimal EFI FFI | Graphical UEFI bootloader, `BOOT.KDL` reader, splash screen, Boot Environment selector, and `PREVLOG.TXT` crash viewer |
| `xtask/src/live.rs` | `AbyssBSD-Rust` | Host (`std`) | Forbidden | `cargo xtask live-image`: builds reproducible Live USB images for `x86_64`, `aarch64`, and `riscv64` |
| `lib/install-plan` | `sable-de` & `AbyssBSD-Rust` | `no_std` + `alloc`, fuzzed | Forbidden | Hardware-to-HIG rules, drive eligibility & RAID lockouts, GPT/ZFS/UFS2 partition math, and `install.kdl` codec |
| `desk/sysd` (`install`) | `sable-de` | Privileged provider (`std`) | Ioctls (`// SAFETY:`) | Hardware discovery, serial-verified disk partitioning, `zpool`/`zfs`/`newfs` execution, `cap_enter` `libarchive` extraction, and UEFI NVRAM registration |
| `desk/install` (`sable-install`) | `sable-de` | GUI App (`#[app]`) | Forbidden | Native graphical installer (Express whole-drive mode, Custom ZFS/partition architect, live rice switcher, KDL view) |

**Zero New Foreign C Libraries.** `sable-sysd`'s `install` provider links only `libarchive` and
`libcrypto` (plus `libcrypt` for password hashing) from FreeBSD `base.txz`—already approved in
[ADR-0018](adr/0018-desktop-architecture.md) and `desk/third_party/inventory.kdl`.

---

## 3. Modern Graphical Bootloader (`boot/loader-efi`)

FreeBSD's `loader.efi` is not used ([ADR-0006](adr/0006-uefi-acpi-only-own-loader-and-acpi.md)).
`boot/loader-efi` handles both installed systems and Live USB media across `amd64`, `aarch64`, and
`riscv64`.

### 3.1 Declarative Configuration (`\EFI\ABYSS\BOOT.KDL`)

`loader-efi` embeds `lib/kdl` (which is already `#![no_std]` + `alloc` with zero dependencies) and
reads `\EFI\ABYSS\BOOT.KDL` from the ESP:

```kdl
// \EFI\ABYSS\BOOT.KDL — written by sable-install and updated by sable-sysd
timeout (ms)0                       // 0 = silent instant boot; hold Space/Esc for menu
default "active-be"                 // Boots zpool bootfs (zroot/ROOT/default) or UFS2 /

splash {
    bgrt #true                      // Preserve OEM ACPI BGRT logo if valid, else draw SPLASH.QOI
    background (color)"#11111b"     // Synced with active rice @base token
    accent (color)"#89b4fa"         // Synced with active rice @accent token
    emblem "/EFI/ABYSS/SPLASH.QOI"
}

entry "active-be" title="AbyssBSD" {
    root "auto"                     // Searches zroot active BE -> UFS2 -> ESP
    kernel "/boot/kernel/kernel"
    flags quiet=#true kaslr=#true
}

entry "safe-mode" title="AbyssBSD (Safe Mode — Default Config & Software GPU)" {
    root "auto"
    kernel "/boot/kernel/kernel"
    cmdline "sable.safe=1"
    flags quiet=#false kaslr=#true
}
```

Whenever the user changes their desktop rice in `sable-settings`, `sable-sysd` optionally updates
`splash.background` and `splash.accent` in `\EFI\ABYSS\BOOT.KDL` so the boot splash and boot menu
match the user's desktop theme from the first second of boot.

### 3.2 Splash Screen & Silent Instant Boot

1. **Native GOP Mode Selection:**
   - `loader-efi` inspects the active `EFI_GRAPHICS_OUTPUT_PROTOCOL` mode. If firmware initialized
     the display to the monitor's native EDID resolution (`≥ 1024×768` linear 32-bit `Rgbx`/`Bgrx`),
     `loader-efi` **never calls `SetMode`**, preventing monitor resync blackouts.
   - Only if firmware left the display in a legacy 640×480 / 800×600 text-compatible mode does
     `loader-efi` query available GOP modes and switch once to the highest native resolution.
2. **Silence on `SimpleTextOutput`:**
   - Calling UEFI `ConOut->OutputString` (`writeln!(con, ...)`) forces UEFI firmware to stamp 8×16
     bitmap text over the GOP framebuffer. When `flags.quiet` is `#true`, `loader-efi` suppresses
     all `ConOut` calls unless boot fails or the user enters verbose mode (`V`). Loader logs still
     go to the hardware UART (`SPCR` / `DBG2`) and the persistent RAM `LogRing`.
3. **Splash Rendering (`boot/loader-efi/src/splash.rs`):**
   - Renders into an offscreen heap buffer and copies to the write-combining GOP framebuffer in one
     frame.
   - Paints `splash.background` (`@base`), centers the AbyssBSD vector-rasterized emblem from
     `\EFI\ABYSS\SPLASH.QOI` (or preserves the OEM ACPI `BGRT` bitmap when `bgrt #true` is set on a
     matching black background), and draws a subtle 4 px pill progress bar in `splash.accent`
     (`@accent`) at `75%` vertical height.

### 3.3 On-Demand Boot Menu, ZFS Boot Environments & Crash Recovery

By default (`timeout (ms)0`), `loader-efi` checks `SimpleTextInputEx` (and USB HID boot keyboards)
once before loading the kernel:

- **Normal Boot (No Key Held, Previous Boot Clean):** Loads the kernel immediately with zero menu
  delay (`< 150 ms` in loader).
- **Interactive Menu Triggered (`Space`, `Esc`, or `Shift` held, or `LogRing.recovered > 0`):**
  Renders a crisp, anti-aliased graphical selector card below the splash emblem:

```
┌──────────────────────────────────────────────────────────────────────────────┐
│                                  ╭──────╮                                    │
│                                 ╱   ◈    ╲                                   │
│                                ╰──────────╯                                  │
│                                  AbyssBSD                                    │
│                                                                              │
│      ⚠ Previous boot ended with a kernel panic (saved to PREVLOG.TXT)        │
│      ┌────────────────────────────────────────────────────────────────┐      │
│      │ ▸ AbyssBSD (zroot/ROOT/default)                         Active │      │
│      │   AbyssBSD (zroot/ROOT/pre-upgrade-2026-10-28)    Boot Env     │      │
│      │   AbyssBSD (zroot/ROOT/default@fresh-install)     Day-0 Snap   │      │
│      │   AbyssBSD — Safe Mode (Default Config & Lavapipe GPU)         │      │
│      │   AbyssBSD — Single-User Shell (boot -s)                       │      │
│      │   View Previous Boot Crash Log (\EFI\ABYSS\PREVLOG.TXT)        │      │
│      │   Reboot into UEFI Firmware Setup                              │      │
│      └────────────────────────────────────────────────────────────────┘      │
│       ↑/↓ Select   Enter Boot   V Verbose Text Boot   K Toggle KASLR         │
└──────────────────────────────────────────────────────────────────────────────┘
```

- **OpenZFS Boot Environment Discovery (`lib/zfs-boot`):**
  - `loader-efi` scans GPT partitions of type `FREEBSD_ZFS` (`516E7CBA-6ECF-11D6-8FF8-00022D09712B`).
  - `lib/zfs-boot` reads the 4× 256 KiB vdev labels, selects the highest-txg valid uberblock, walks
    the Meta Object Set (MOS) and DSL directory tree under `zroot/ROOT/*`, and lists every Boot
    Environment (plus `@fresh-install` and `@sable-agent-*` snapshots) directly in the boot menu.
  - Selecting an alternate Boot Environment loads `/boot/kernel/kernel` from that dataset's ZPL and
    passes `vfs.root.mountfrom=zfs:zroot/ROOT/<be>` on the kernel command line.
- **Built-in Crash Log Viewer:**
  - On machines without a rear serial port (such as the Ryzen 9 7900X / X870E,
    [HARDWARE.md §5](HARDWARE.md#5-board-profile-amd64-ryzen-9-7900x--x870e-aorus-elite-wifi7)),
    if a kernel panic or watchdog reset occurred, `loader-efi` saves the persistent RAM `LogRing` to
    `\EFI\ABYSS\PREVLOG.TXT` and highlights **"View Previous Boot Crash Log"** in the menu so the
    developer can read the symbolized backtrace directly on screen before booting.

### 3.4 Zero-Flicker Handover (`bootproto` v3 → `sys/vt` → `gop-kms` → `sable-comp`)

1. **`bootproto::BootInfo` v3 (`SplashState` & `BootFlags`):**
   ```rust
   bitflags! {
       #[repr(transparent)]
       pub struct BootFlags: u32 {
           /// Keep the GOP splash on screen; buffer early console text in memory.
           const SPLASH_QUIET = 1 << 0;
           /// Booted from read-only Live USB installer media (`abyss-live`).
           const LIVE_MEDIA   = 1 << 1;
           /// Start Sable in Safe Mode (`Defaults` + `System` config only, lavapipe).
           const SAFE_MODE    = 1 << 2;
           /// Boot into single-user `/bin/sh` (`RB_SINGLE`).
           const SINGLE_USER  = 1 << 3;
       }
   }

   #[repr(C)]
   #[derive(Clone, Copy, Debug, PartialEq, Eq)]
   pub struct SplashState {
       pub flags: BootFlags,
       pub bg_rgb: u32,
       pub accent_rgb: u32,
       pub progress_rect: [u16; 4], // [x, y, width, height] in framebuffer pixels
   }
   ```
2. **Kernel `drivers/fb-console` & `sys/vt` Quiet Splash Mode:**
   - When `BootFlags::SPLASH_QUIET` is set, `fb-console` and `sys/vt` record all console output into
     `ttyv0`'s cell buffer, `/dev/klog`, and `LogRing` without drawing characters over the GOP
     framebuffer.
   - Major kernel init milestones (ACPI, PCIe, Storage, USB, VFS root mount, `rc` start) advance the
     fill width of `SplashState.progress_rect`.
   - **Automatic Unmask on Fault or Keypress:** A kernel panic, an `init(8)` failure, or pressing
     `Esc` or `F1` on any keyboard immediately drops `SPLASH_QUIET` and blits `ttyv0`'s buffered
     text log to the screen.
3. **KMS & Compositor Cross-Fade ([GRAPHICS.md §7.5](GRAPHICS.md#75-firmware-handover)):**
   - `gop-kms` (and native display drivers attaching with `FirmwareState::LeftRunning`) adopts the
     active GOP scanout buffer without blanking the CRTC.
   - When `sable-comp` starts, it imports the initial KMS primary plane buffer and performs a
     `180 ms` `ease-out` shader cross-fade from the boot splash into the Live Desktop or
     `sable-greet`.

---

## 4. Live USB Media Architecture (`cargo xtask live-image`)

### 4.1 Hybrid GPT Layout (`abyss-live-<arch>.img`)

`cargo xtask live-image --arch {x86_64,aarch64}` builds a single hybrid GPT disk image that can be
written directly to a USB flash drive with `dd(1)` or booted as a USB mass-storage / VirtIO drive in
QEMU and `bhyve`:

```
 ┌──────────────────┬───────────────────────────────────────────────┬──────────────────────┐
 │ p1: efiboot0     │ p2: abyss-live (Read-Only UFS2, ~2.4 GiB)     │ p3: abyss-scratch    │
 │ 128 MiB FAT32    │ • Extracted base.txz + desktop.txz (live OS)  │ (Optional, auto-made │
 │ • BOOTX64.EFI /  │ • /usr/dist/MANIFEST (SHA-256 digests)        │  in remaining USB    │
 │   BOOTAA64.EFI   │ • /usr/dist/{base,kernel,desktop,wine,        │  space on first boot)│
 │ • BOOT.KDL       │              agent}.txz                       │ • Saved install.kdl  │
 │ • SPLASH.QOI     │ • /usr/dist/pkg/* (offline Mesa, Vulkan,      │ • Board ACPI/devinfo │
 │ • LIVE.KDL       │   XKB, and reference board firmware packages) │   corpus captures    │
 └──────────────────┴───────────────────────────────────────────────┴──────────────────────┘
```

### 4.2 Live Desktop Boot Flow

1. `loader-efi` detects `\EFI\ABYSS\LIVE.KDL` on the ESP, sets `BootFlags::LIVE_MEDIA |
   BootFlags::SPLASH_QUIET`, and mounts `/dev/gpt/abyss-live` read-only at `/`.
2. `/etc/rc.d/abyss_live` mounts `tmpfs` instances over `/etc` (seeded from `/rescue/etc` /
   `/usr/share/examples/etc`), `/var`, `/tmp`, and `/home/live`.
3. `sable-sysd` runs the hardware prober (§5) to generate `/etc/sable/system.kdl` and
   `/home/live/.config/sable/sable.kdl`, then auto-logs in user `live` (UID `1000`, groups `wheel`,
   `operator`, `video`) without a greeter password prompt.
4. `sabled` starts `sable-comp`, `sable-panel`, and **`sable-install`** on Workspace 1.
5. **Try or Install Without Mode Switching:** Because the live environment *is* a complete Sable
   desktop session, the user can minimize or tile `sable-install` (`Super+H`/`Super+V`), launch
   `sable-term`, inspect disks in `sable-files`, read the manuals (`F1`), or test monitors and
   controllers before committing to installation.
6. **Built-In Board Corpus Capture (`P7.0` helper):** On new hardware, running `sablectl
   capture-corpus` from the Live USB dumps `acpidump -dt`, `devinfo -rv`, `pciconf -lv`, and EDID
   blobs directly onto `p3` (`abyss-scratch`) for inclusion in `tests/corpus/<board>/`.

---

## 5. Hardware Probing for HIG (Zero-Question Defaults)

Traditional installers ask users to configure settings that the machine can determine itself. At
live-session boot (and before writing the target system), `sable-sysd`'s `install` provider collects
a `HardwareProfile` and passes it to pure `lib/install-plan::derive_hig_defaults`, which configures
both the live session and the installed OS:

| HIG / System Domain | Hardware Source Probed | Derived Default Written to KDL / System | Question Eliminated |
|---|---|---|---|
| **Monitor Resolution, Scale & VRR** | DRM/KMS connectors + `lib/edid` (physical `mm_width × mm_height`, native preferred timing, DisplayID Adaptive-Sync range, HDR static metadata) | • Computes physical PPI and connector type (`eDP` vs. `DP`/`HDMI`): sets `scale 1.0` (`< 120 PPI`), `scale 1.25` (`120–160 PPI`), `scale 1.5` (`160–210 PPI`), or `scale 2.0` (`> 210 PPI`).<br>• Picks highest native refresh rate (e.g., `240 Hz`) and sets `vrr "auto"` when supported.<br>• Writes `output "<name>"` blocks into `sable.kdl`. | *"Choose display resolution and HiDPI scaling factor"* |
| **Chassis Form Factor & Panel Layout** | SMBIOS Type 3 (`ChassisType`), ACPI battery (`_BIF`/`_BST`), ACPI lid (`PNP0C0D`), `/dev/backlight/*` | • **Desktop / Workstation** (X870E, B650E, Z690-A, Orange Pi 6 Plus): omits `sable-battery` and `sable-backlight` from `sable-panel`; disables idle suspend; sets performance CPPC policy; leaves disk encryption optional.<br>• **Laptop / Portable** (Q8B): adds `sable-battery` and `sable-backlight` to `sable-panel`; enables idle dim + suspend and lid-close lock; **enables disk encryption checkbox by default**. | *"Select desktop vs. laptop profile and panel widgets"* |
| **Touchpad, Mouse & Keymap** | `sys/evdev` (`INPUT_PROP_BUTTONPAD`, pointer report rate), USB HID `bCountryCode`, UEFI `PlatformLang` | • Multi-touch buttonpad present: enables `tap-to-click #true`, `natural-scroll #true`, and 3-finger workspace swipe gestures (`lib/gesture`).<br>• High-rate mouse (`≥ 1000 Hz`): enables `rtprio` raw pointer thread.<br>• Pre-selects XKB keymap (`us`, `gb`, `de`, `jp`, …) from HID `bCountryCode` / UEFI `PlatformLang`, exposed as a compact `[⌨ US ▾]` badge in the installer header bar. | Dedicated *"Choose your keyboard layout"* and *"Configure mouse/touchpad"* pages |
| **GPU & Vulkan ICD** | `/dev/pci` (`pciconf`) & ACPI `_HID` (`1002:*` AMD RDNA, `10de:*` NVIDIA, `CIXH5000` Immortalis-G720, `QCOM0636` Adreno, `1af4:1050` VirtIO-GPU) | • Selects matching Mesa Vulkan ICD (`radv`, `panvk`, `turnip`, `venus`, or `lavapipe` fallback) and stages required GPU firmware from `/usr/dist/pkg/`. | *"Select graphics driver / Vulkan implementation"* |
| **Disk Sector Size, TRIM & Swap** | `DIOCGIDENT`, `DIOCGSECTORSIZE`, `DIOCGSTRIPESIZE`, NVMe `IDENTIFY`, physical RAM (`hw.physmem`) | • Sets ZFS `ashift=12` (4Kn / NVMe) or `ashift=13` (8 KiB NAND pages).<br>• Enables `autotrim=on` (ZFS) or `trim` (UFS2) when NVMe `Dataset Management` / SATA `TRIM` is supported.<br>• Sizes swap + crash dump partition to `min(RAM / 2, 16 GiB)` on disks `≥ 64 GiB`. | *"Configure sector alignment, TRIM, and swap partition size"* |
| **Network & Timezone** | `netlink` / `ifconfig`, DHCP/SLAAC, UEFI `GetTime` | • Wired NICs (`rge0`, `igc0`, `em0`, `vtnet0`) auto-negotiate DHCP + SLAAC silently in the background. Wi-Fi popover appears only when clicked.<br>• Sets hardware clock to UTC and exposes timezone as a compact `[🕒 UTC ▾]` header-bar popover. | Mandatory *"Configure Network"* and *"Select Region / City"* wizard screens |

---

## 6. Installer User Experience (`sable-install`)

Built on [TOOLKIT.md](TOOLKIT.md) (`desk/app` + `lib/scene`) and obeying [STYLE.md](STYLE.md),
`sable-install` fits its entire Express workflow into **one window with two sections**, requiring no
wizard "Next / Back / Next" paging:

```
┌──────────────────────────────────────────────────────────────────────────────┐
│  AbyssBSD Installer                 [⌨ US ▾] [🕒 UTC-5 ▾] [</> install.kdl] │
├──────────────────────────────────────────────────────────────────────────────┤
│  1. Identity & Visual Style                                                  │
│  ┌───────────────────────────────────┐ ┌───────────────────────────────────┐ │
│  │ Username:  [ jkane              ] │ │ Desktop Theme (Rice):             │ │
│  │ Password:  [ ••••••••••••       ] │ │ ┌─────────┐ ┌─────────┐ ┌───────┐ │ │
│  │ Hostname:  [ abyss-x870e        ] │ │ │● Abyssal│ │  Hadal  │ │ Swiss │ │ │
│  │                                   │ │ └─────────┘ └─────────┘ └───────┘ │ │
│  │ Optional Sets:                    │ │ Clicking any card re-skins the    │ │
│  │ [✓] Sable Desktop (desktop.txz)   │ │ live desktop immediately (1 frame)│ │
│  │ [ ] Windows Gaming (wine.txz)     │ └───────────────────────────────────┘ │
│  │ [ ] Local AI Assistant (agent.txz)│                                       │
│  └───────────────────────────────────┘                                       │
│                                                                              │
│  2. Target Drive                             (●) Express  ( ) Custom / ZFS   │
│  ┌───────────────────────────────────┐ ┌───────────────────────────────────┐ │
│  │ ● WD Black SN850X 1.0 TB          │ │ ○ Samsung 980 PRO 2.0 TB          │ │
│  │   nda1 · NVMe Gen4 · Empty        │ │   nda0 · Contains Fedora Linux 44 │ │
│  │   Layout: OpenZFS (zroot + BEs)   │ │   (Select in Custom to install    │ │
│  │   [✓] Encrypt with my password    │ │    alongside or replace)          │ │
│  └───────────────────────────────────┘ └───────────────────────────────────┘ │
│  🔒 3 data drives protected (2× WD 8 TB Linux RAID, 1× HGST 6 TB)            │
│                                                                              │
│         [ Erase WD Black SN850X (1.0 TB) and Install AbyssBSD ]   (.danger)  │
└──────────────────────────────────────────────────────────────────────────────┘
```

### 6.1 Section 1: Opinionated Identity, Live Rice Picker & Optional Sets

1. **One User Account & One Passphrase:**
   - Collects **Username** (validated `[a-z_][a-z0-9_-]*`) and **Password**.
   - Creates the primary user in groups `wheel`, `operator`, and `video`.
   - Direct `root` password login is disabled by default (`pw mod user root -w no`), eliminating a
     second password prompt. Root administration uses the user's credential through `sable-sysd`'s
     PAM `auth` provider or `doas(1)` / `su(1)` via `wheel`.
   - When disk encryption is enabled, the same passphrase wraps the OpenZFS `zroot` encryption root
     (or `geli` key), and `loader-efi` can forward the unlocked key to `sable-greet` so the user is
     not prompted twice at boot.
2. **Deterministic Default Hostname:**
   - Pre-filled from SMBIOS board/product strings (`abyss-x870e`, `abyss-taichi`, `abyss-opi6p`,
     `abyss-z690`, `abyss-q35`), editable inline.
3. **Live Rice Carousel ([STYLE.md §7](STYLE.md#7-the-semantic-theme-contract)):**
   - Shows cards for the built-in rices (`abyssal`, `hadal`, `esports-zero`, `swiss-grid`,
     `fluorite`, etc.).
   - Clicking any rice stages `include "rices/<name>"` into `sabled`'s `Runtime` layer immediately,
     re-skinning the entire Live Desktop, panel, and installer window in one frame (`≤ 4 ms`) and
     recording that rice for the installed user's `~/.config/sable/sable.kdl` and `\EFI\ABYSS\BOOT.KDL`.
4. **Optional Distribution Sets:**
   - **`Sable Desktop (desktop.txz)`** — enabled by default; unchecking installs a minimal CLI/server
     base (`base.txz` + `kernel.txz`).
   - **`Windows Gaming & Wine (wine.txz)`** — installs `sable-wine` and pre-creates the three-layer
     OpenZFS prefix tree (`zroot/home/<user>/wine/{base,redist,games}`) specified in
     [WINE.md §3.1](WINE.md#31-three-layer-openzfs-copy-on-write-prefix-tree).
   - **`Agentic Assistant (agent.txz)`** — strictly unchecked by default per
     [ASSISTANT.md §1.2](ASSISTANT.md#12-principles) A1 (`0 bytes / 0 cycles` unless opted in).

### 6.2 Section 2A: Express Whole-Drive Mode (Default)

Express mode lets the user grab an entire drive and partition it with one click while preventing
accidental destruction of data arrays or foreign OS installs:

1. **Drive Eligibility & Protection Tiers (`lib/install-plan`):**
   - **Tier 1 — Empty or Dedicated Target Drives (Pre-selected):** Blank disks or disks carrying an
     `abyss-media` label are sorted first and pre-selected.
   - **Tier 2 — Single-Disk Foreign OS Drives:** Disks holding an existing Windows, Linux, or
     FreeBSD installation display an explicit warning badge (`Contains Fedora Linux 44`) and require
     an explicit click to select.
   - **Tier 3 — Protected Multi-Disk RAID / Pool Members (Locked in Express Mode):** Any disk carrying
     Linux `mdadm` superblocks, multi-vdev ZFS pool labels, or Windows Storage Spaces metadata (such
     as the 2× 8 TB WD RAID-0 and 6 TB HGST disks on the X870E reference machine,
     [HARDWARE.md §5](HARDWARE.md#5-board-profile-amd64-ryzen-9-7900x--x870e-aorus-elite-wifi7)) is
     hidden behind the **🔒 Protected Data Drives** banner and cannot be wiped in Express mode.
   - **Tier 4 — Booted Live USB Media (Excluded):** The disk holding `abyss-live` is never shown as a
     target.
2. **Opinionated Whole-Drive Layout:**
   - **OpenZFS Default (`M-Z` / FreeBSD 15.1 surface, RAM `≥ 4 GiB`):**

     | GPT # | Label | Type GUID | Size | Details |
     |---|---|---|---|---|
     | `p1` | `efiboot0` | `EFI_SYSTEM` | `512 MiB` | FAT32 ESP mounted at `/boot/efi` (`\EFI\ABYSS\LOADER.EFI`, `BOOT.KDL`, `SPLASH.QOI`, `log\`) |
     | `p2` | `swap0` | `FREEBSD_SWAP` | `min(RAM/2, 16 GiB)` | Encrypted swap (`swap0.eli`) + kernel crash dump device (`dumpon`) |
     | `p3` | `zfs0` | `FREEBSD_ZFS` | Remaining | Pool `zroot` (`ashift=12`, `compression=zstd`, `atime=off`, `autotrim=on`, optional AES-256-GCM encryption) |

   - **Canonical `zroot` Dataset Tree (compatible with `bectl(8)`, `sable-agent`, and `sable-wine`):**
     ```text
     zroot                                  (canmount=off, mountpoint=none)
     ├── zroot/ROOT                         (canmount=off, mountpoint=none)
     │   └── zroot/ROOT/default             (canmount=noauto, mountpoint=/, bootfs)
     ├── zroot/home                         (mountpoint=/home)
     │   └── zroot/home/<user>              (mountpoint=/home/<user>)
     │       └── zroot/home/<user>/wine     (created if wine.txz is checked; WINE.md §3.1)
     ├── zroot/tmp                          (mountpoint=/tmp, exec=on, setuid=off)
     ├── zroot/var                          (canmount=off, mountpoint=/var)
     │   ├── zroot/var/log                  (mountpoint=/var/log, exec=off, setuid=off)
     │   ├── zroot/var/tmp                  (mountpoint=/var/tmp, setuid=off)
     │   └── zroot/var/crash                (mountpoint=/var/crash, exec=off, setuid=off)
     └── zroot/usr                          (canmount=off, mountpoint=/usr)
         └── zroot/usr/local                (mountpoint=/usr/local)
     ```
   - **UFS2 SU+J Layout (Pre-`M-Z` kernel phases or systems with `< 4 GiB` RAM):**
     `p3` is formatted as `freebsd-ufs` (`rootfs0`) with Soft Updates + Journaling (`SU+J`),
     POSIX.1e ACLs (`acls`), and `noatime`.
3. **Self-Describing Destructive Button ([STYLE.md §5.1](STYLE.md#51-undo-over-confirmation-dialogs)):**
   The install button names the exact disk model and capacity:
   **`Erase WD Black SN850X (1.0 TB) and Install AbyssBSD`** in `.danger()` style.

### 6.3 Section 2B: Custom Mode for Advanced Users

Switching the segmented control to **`Custom / ZFS`** expands Section 2 into the **Storage & Pool
Architect** without leaving the window:

1. **Install Alongside / Dual-Boot Mode:**
   - Carve unallocated space on an existing disk or select specific partitions across drives.
   - **Non-Destructive UEFI Coexistence:** Creates a dedicated `efiboot0` ESP (or writes only under
     `\EFI\ABYSS\` on a shared ESP) and adds an isolated `AbyssBSD` UEFI `BootXXXX` variable,
     leaving Windows, Fedora, or FreeBSD boot entries untouched ([HARDWARE.md §5](HARDWARE.md#5-board-profile-amd64-ryzen-9-7900x--x870e-aorus-elite-wifi7)).
2. **Multi-Disk OpenZFS Topology Builder:**
   - Select multiple drives to build `stripe`, `mirror` (RAID-1), `raidz1`, `raidz2`, or `raidz3`
     pools, and assign optional dedicated `log` (SLOG), `cache` (L2ARC), or `special` metadata
     vdevs.
   - Displays live usable capacity, fault-tolerance rating, automatic `ashift` (`max(ashift)` of
     member disks), and dataset property overrides (`recordsize`, `compression`, `quota`).
3. **Clean Reinstall into New Boot Environment (Preserve `/home`):**
   - When an existing `zroot` pool is detected on a disk, Custom mode offers **"Install into New
     Boot Environment (`zroot/ROOT/abyss-YYYYMMDD`)"**, keeping `zroot/home` and all user data
     intact while provisioning a clean root dataset and activating it via `zpool set bootfs`.
4. **Custom GPT Partition Table Editor:**
   - Create, resize, delete, and label arbitrary `efi`, `freebsd-zfs`, `freebsd-ufs`, and
     `freebsd-swap` partitions with automatic 1 MiB (2048-sector) alignment.
5. **Live `install.kdl` Split Editor (`</> install.kdl`, `Ctrl+E`):**
   - Toggles a synchronized KDL v2 text editor (`lib/textedit`) beside the visual storage map. Any
     edit in the GUI updates the KDL text, and any edit in the KDL text updates the visual storage
     map immediately via `lib/install-plan`.

---

## 7. Declarative Installation Plan (`lib/install-plan`)

### 7.1 `install.kdl` Schema

Every installation is represented by a single KDL v2 document that can be saved to `abyss-scratch`,
checked into git, or passed to `sable-install --apply /path/to/install.kdl`:

```kdl
// Declarative AbyssBSD Installation Plan (validated by lib/install-plan)
version 1

system {
    hostname "abyss-x870e"
    timezone "America/Chicago"
    keymap "us"
}

user "jkane" {
    full-name "J. Kane"
    uid 1001
    groups "wheel" "operator" "video"
    shell "/bin/sh"
    password-hash "$6$rounds=65536$..."
}

desktop {
    enable #true
    rice "abyssal"
    sets "base.txz" "kernel.txz" "desktop.txz"
    chassis "desktop"
    output "DP-1" {
        mode 2560 1440 refresh=(hz)240
        scale 1.0
        vrr "auto"
    }
}

storage {
    mode "whole-disk" // "whole-disk" | "custom-gpt" | "zfs-topology" | "new-be"
    target path="/dev/nda1" serial="23424R801942" model="WD_BLACK SN850X 1000GB"
    filesystem "zfs" {
        pool "zroot"
        vdev "stripe" "/dev/nda1p3"
        ashift 12
        compression "zstd"
        autotrim #true
        encryption #true
    }
    swap size=(gib)16 encrypt=#true
}

bootloader {
    esp-size (mib)512
    timeout (ms)0
    splash "umbral" bgrt=#true background=(color)"#11111b" accent=(color)"#89b4fa"
    register-uefi-nvram #true
}
```

### 7.2 Safety Invariants in `lib/install-plan`

`lib/install-plan` is `#![no_std]` + `alloc` and `#![forbid(unsafe_code)]`. It enforces four hard
invariants before `sable-sysd` opens any disk for writing:

1. **Path + Hardware Serial Number Binding:** Every target device in `storage` must specify both its
   kernel device name (`path="/dev/nda1"`) and its hardware serial number (`serial="..."` from
   `DIOCGIDENT`). `sable-sysd` re-queries `DIOCGIDENT` on the opened file descriptor immediately
   before partitioning and aborts with `InstallError::SerialMismatch` if they differ.
2. **Live Media Immunity:** Any disk containing the GPT partition GUID of the mounted `abyss-live`
   filesystem is rejected with `InstallError::TargetIsLiveMedia`.
3. **Protected Array Lock:** A disk flagged with `RaidMember` or `ForeignZpoolMember` in the
   `HardwareProfile` is rejected unless `storage` explicitly includes `allow-raid-member-wipe #true`
   (which only the Custom Architect can set after an explicit per-drive unlock).
4. **1 MiB LBA Alignment & Non-Overlap:** All partition start and end LBAs are aligned to 1 MiB
   boundaries (`2048` 512-byte sectors) and verified by `lib/gpt::validate`.

---

## 8. Privileged Execution Pipeline (`sable-sysd` `install` Provider)

Following [DESKTOP.md §6.8](DESKTOP.md#68-system-providers-sable-sysd), the GUI (`sable-install`)
runs unprivileged as user `live` inside Capsicum capability mode (`cap_enter(2)`). It holds only its
Wayland socket and a Conduit channel to `sable-sysd`'s `install` provider:

```rust
#[conduit::interface(name = "sable.sys.Install", version = 1)]
pub trait Install {
    /// Returns the probed hardware profile, eligible disks, and derived HIG defaults.
    async fn probe(&self) -> Result<HardwareProfile, InstallError>;
    /// Validates and executes a declarative KDL installation plan.
    async fn execute(&self, plan_kdl: String, passphrase: Option<SecretString>) -> Result<(), InstallError>;
    /// Streams phase progress, throughput, and log lines during installation.
    #[event]
    fn progress(&self) -> InstallProgress;
}
```

### 8.1 Six-Stage Atomic Execution (`15–35 s` on NVMe)

While `execute` runs, `sable-install` replaces the drive cards with a smooth progress bar, live MB/s
extraction telemetry, and a collapsible real-time operation log (Meanwhile, the user can continue
using the Live Desktop):

1. **Stage 1 — Preflight & Digest Check (`5%`):**
   Re-verifies `DIOCGIDENT` serial numbers on every target disk fd, verifies that no target
   partition is mounted, and checks the SHA-256 digests of `/usr/dist/*.txz` against
   `/usr/dist/MANIFEST`.
2. **Stage 2 — Partitioning & Pool / Filesystem Creation (`15%`):**
   Clears stale ZFS/RAID labels on target partitions, writes the protective MBR and primary/backup
   GPT tables via `lib/gpt`, formats `efiboot0` as FAT32 via `lib/fat`, and initializes either the
   OpenZFS `zroot` pool + dataset hierarchy (`zpool create`, `zfs create`) or UFS2 (`SU+J`, `acls`).
3. **Stage 3 — Sandboxed Distribution Set Extraction (`50%`):**
   For each selected `.txz` set (`base.txz`, `kernel.txz`, `desktop.txz`, and optionally `wine.txz`
   or `agent.txz`), spawns a `pdfork(2)` + `cap_enter(2)` worker linked against base `libarchive`.
   The worker receives only a read-only fd to the `.txz` archive and a `UF_RESOLVE_BENEATH`
   directory fd to `/mnt/install`, preserving `mtree` ownership, modes, `schg` flags, and ACLs.
4. **Stage 4 — Offline Hardware Package Installation (`15%`):**
   Installs the hardware-matched packages (`mesa-libs`, `mesa-dri`, `vulkan-loader`,
   `xkeyboard-config`, HarfBuzz, libpng, libjpeg-turbo, and board-specific firmware) from the USB's
   `/usr/dist/pkg/` repository into `/mnt/install` with zero network traffic required.
5. **Stage 5 — System & HIG Configuration Generation (`10%`):**
   - Writes `/mnt/install/etc/rc.conf` (`hostname`, `zfs_enable="YES"`, `sable_sysd_enable="YES"`,
     `ifconfig_DEFAULT="DHCP inet6 accept_rtadv"`), `/etc/fstab` (using `/dev/gpt/*` labels), and
     `/boot/loader.conf` compatibility hints.
   - Creates the user account (`pw`), locks direct `root` password login, and writes
     `/etc/sable/system.kdl` and `/home/<user>/.config/sable/sable.kdl` pre-populated with the
     chosen rice and hardware-probed `output`, `input`, and `panel` settings.
6. **Stage 6 — Bootloader Installation & `@fresh-install` Snapshot (`5%`):**
   - Installs `loader-efi` to `/boot/efi/EFI/ABYSS/LOADER.EFI` and the removable fallback path
     `/boot/efi/EFI/BOOT/BOOT{X64,AA64}.EFI`, writes `/boot/efi/EFI/ABYSS/BOOT.KDL` (with the
     selected rice's `@base` and `@accent` splash colors) and `SPLASH.QOI`, and registers the
     `AbyssBSD` UEFI boot entry (`efivar` / `SetVariable`).
   - Takes a read-only **`zroot/ROOT/default@fresh-install`** snapshot and unmounts `/mnt/install`
     cleanly (`zpool export zroot`).

At `100%`, `sable-install` displays **`Installation complete in 22.4 s — Reboot Now`** alongside
**`Keep Exploring Live Desktop`**.

---

## 9. Phasing and Verification

| Step | Workspace | Deliverables | Exit Criteria |
|---|---|---|---|
| **I.1** | `AbyssBSD-Rust` & `sable-de` | `lib/qoi` (fuzzed); `bootproto` v3 (`SplashState`, `BootFlags`); `loader-efi` `BOOT.KDL` parser, GOP splash renderer, and interactive boot menu; `fb-console`/`sys/vt` quiet splash hold; `lib/install-plan` (HIG heuristics, drive classifier, GPT/ZFS/UFS2 layout engine, `install.kdl` codec + fuzzer) | `cargo xtask check` and host unit/fuzz tests pass in both repos; QEMU boots silently with graphical splash from `loader-efi` to kernel, and holding `Space` opens the graphical boot menu |
| **I.2** | `AbyssBSD-Rust` | `lib/zfs-boot` read-only ZFS MOS/DSL/ZPL reader (fuzzed against `zpool` images); `xtask/src/live.rs` (`cargo xtask live-image`) building `abyss-live-{x86_64,aarch64}.img` with read-only UFS2 root + `tmpfs` overlay + `/usr/dist/*.txz` | `loader-efi` enumerates and boots `zroot/ROOT/*` Boot Environments in QEMU; `abyss-live` image boots to live multi-user session on `amd64` and `aarch64` |
| **I.3** | `sable-de` | `desk/sysd` `install` provider (`sable-sysd-install`): hardware prober, serial-locked GPT/ZFS/UFS2 formatter, `cap_enter` `libarchive` `.txz` extractor, HIG `sable.kdl` writer, `@fresh-install` snapshotter | Automated VM test boots `abyss-live.img` in `bhyve`/`QEMU` with a blank target NVMe (`vtbd1`), applies `install.kdl` headlessly, reboots from `vtbd1` into the installed OS, and passes `fsck_ffs -f -n` / `zpool status` |
| **I.4** | `sable-de` | `desk/install` (`sable-install` Express + Custom Architect GUI), 1-frame live rice preview, inline `install.kdl` editor, and `sable-comp` splash cross-fade | End-to-end graphical installation in QEMU (`virtio-gpu` / `lavapipe`) and on physical reference hardware (X870E second NVMe & Orange Pi 6 Plus USB → NVMe) with zero display mode resets |

---

## 10. Open Questions

To be resolved before starting Milestone I.1 / I.2:

1. **ZFS Boot Environment Kernel Loading (`lib/zfs-boot` vs. ESP Kernel Staging):**
   - **Option A (`lib/zfs-boot`, §3.3):** `loader-efi` reads `/boot/kernel/kernel` and enumerates
     `zroot/ROOT/*` Boot Environments directly from the OpenZFS pool on disk (`lz4` / `zstd` /
     uncompressed ZPL), requiring zero ESP synchronization when `bectl` or `sable-agent` creates a
     snapshot.
   - **Option B (ESP Kernel Staging):** `bectl` / `sable-sysd` copies active kernels to
     `\EFI\ABYSS\KERNELS\` and updates `\EFI\ABYSS\BOOT.KDL`, avoiding ZFS MOS/ZPL parsing inside
     `loader-efi`.
2. **Encryption Passphrase Single-Sign-On (`loader-efi` → `sable-greet`):**
   - When whole-disk encryption is enabled using the user's account password (§6.1), decide whether
     `loader-efi` passes a one-time in-memory token so `sable-greet` auto-logs into the desktop on
     cold boot without asking for the same password a second time, or whether `sable-greet` always
     prompts independently.
3. **Default Live USB Persistence (`p3: abyss-scratch`):**
   - When the Live USB image is flashed to a larger thumb drive (`≥ 8 GiB`), decide whether the
     remaining space is automatically formatted on first boot as a writable `abyss-scratch`
     partition (for saving `install.kdl` profiles and `tests/corpus/<board>/` hardware captures), or
     left unallocated unless requested.

