# Live media, graphical boot loader (`loader-efi`), and installer

- Status: **Draft**. A forward design for installing and booting AbyssBSD: the graphical UEFI boot
  loader (`boot/loader-efi`), boot environments, the live medium, the installer, and the login
  path from the loader's splash to a Scoria session
- Date: 2026-10-28; revised 2026-10-29 for ADR-0018; revised 2026-11-23 for
  [ADR-0026](adr/0026-desktop-scoria.md): the monorepo desktop's installer, login window, and
  medium recipe are replaced by `scoria-install`, `scoria-greetd`, and `cargo xtask live-image`,
  all in this workspace
- Inputs: [ADR-0006](adr/0006-uefi-acpi-only-own-loader-and-acpi.md),
  [ADR-0007](adr/0007-license-and-clean-implementation.md),
  [ADR-0026](adr/0026-desktop-scoria.md), [DESKTOP.md](DESKTOP.md) (§2.4, §4, §7, §8.2, M-D.5),
  [GRAPHICS.md](GRAPHICS.md) (§7.5, §7.6), [BOOTSPLASH.md](BOOTSPLASH.md), FreeBSD's
  `bsdinstall` (its ZFS and encrypted layouts) and `bectl(8)` as the behavioral reference, and the
  monorepo desktop's installer (`de/install`, `de/installrun`, `abyss/mk/live-image.sh` in
  `../AbyssBSD`) for the lessons it learned

The short version: **boot silently from the power button to Scoria's greeter with one mode set,
show a boot-environment and crash-log menu only when asked, and install from a live Scoria session
with a graphical installer whose privileged half trusts nothing the interface sends it.**
FreeBSD's Forth/Lua `loader.efi` is not used (ADR-0006). `boot/loader-efi` grows a splash screen,
a boot menu, boot environments read from ZFS, and FreeBSD's `loader.conf`. The installer
(`scoria-install`, unprivileged, and `scoria-installd`, root) and the login path (`scoria-greetd`,
root, and `scoria-greet`, unprivileged) are Scoria components in `desk/`. What they write is
FreeBSD's own layout and configuration, so an installed system is one FreeBSD tools understand.

---

## 1. Goals and non-goals

### 1.1 Patterns adopted

| Peer system | Pattern | Verdict | Why |
|---|---|---|---|
| **macOS / Fedora: BGRT, early kernel mode setting, compositor handover** | Firmware sets the panel's native mode; the loader keeps it; the kernel's display driver inherits the running scanout (`FirmwareState::LeftRunning`) | **Adopt**, without Plymouth or an initramfs | No resolution jumps or black frames between firmware and the greeter |
| **`systemd-boot` / Limine** | Silent boot by default; a held key shows the menu | **Adopt** | The menu appears when asked, when `BootNext` says so, or after an unclean reset |
| **FreeBSD `bsdinstall` (auto ZFS), `bectl(8)`** | `zroot/ROOT/<be>` with `bootfs`, separate `/home` and `/var/*`; GELI with `-b -g` for encryption | **Adopt** | FreeBSD's tools then manage what the installer made, unchanged |
| **FreeBSD's `loader.conf`** | `key="value"` tunables into the kernel environment; `autoboot_delay`, `boot_single`, `boot_verbose` | **Adopt** | FreeBSD is the reference for its names and meaning |
| **greetd** | A small privileged daemon that runs PAM and starts sessions, and an unprivileged greeter that only collects input | **Adopt** | The greeter, a Wayland client with fonts and images, never runs as root; no setuid helper is needed to log in |
| **Anaconda, the monorepo installer** | An unprivileged interface builds a plan; a root service validates and carries it out | **Adopt** | The interface is the large, untrusted part; the part that writes disks is small and checks everything again |

### 1.2 Principles

| # | Principle | Consequence |
|---|---|---|
| B1 | **One mode set, no flicker** | `loader-efi` keeps the firmware's native mode and hands it to the kernel and the compositor without a text-mode reset (§3.5) |
| B2 | **Silent unless asked** | No countdown. The menu appears on a held key, after a crash, or when `autoboot_delay` says so (§3.3) |
| B3 | **FreeBSD's configuration, read as FreeBSD reads it** | `/boot/loader.conf`, `loader.conf.local`, and `loader.conf.d/*.conf` give tunables and loader variables with FreeBSD's names. `*_load` lines for modules built into the kernel are accepted (ADR-0005) |
| B4 | **Crash evidence on screen** | On machines with no serial port, the saved `LogRing` is readable from the menu (§3.3) |
| B5 | **Plan, then act** | The installer changes nothing until the plan is complete and confirmed; `scoria-installd` validates the plan itself, and can print the commands it would run instead of running them (§5.1) |
| B6 | **No surprise erasure** | Data on a disk is named before it is erased, and erasing needs a hold-to-confirm, not a dialog (§5.3, DESKTOP.md §8.2 rule 7) |

### 1.3 Non-goals

- Legacy BIOS, CSM, or MBR boot (ADR-0006).
- Forth or Lua menus, `loader.rc`, or loading kernel modules (the kernel is static).
- Installing beside another system on the same disk. The installer uses whole disks; resizing
  another system's partitions is out of scope.
- Network installs. Sets and packages come from the medium.
- An update tool. Updating into a new boot environment is §10 question 1.

---

## 2. Who builds what

Everything is in this workspace.

| Piece | Where | Kind | Purpose |
|---|---|---|---|
| `boot/bootproto` | `boot/` | `no_std`, pure | `BootInfo` v3: `SplashState` and `BootFlags` (§3.5), and the GELI key buffer (§3.4) |
| `boot/loader-efi` | `boot/` | UEFI | Splash, boot menu, boot environments, GELI unlock, `loader.conf`, `PREVLOG.TXT` viewer |
| `lib/loaderconf` | `lib/` | `no_std`, pure, fuzzed | FreeBSD `loader.conf` syntax: variables, `*_load` lines, includes, and the `loader.conf.d` order |
| `lib/qoi` | `lib/` | `no_std`, pure, fuzzed | QOI decoder for the splash image |
| `lib/zfs-boot` | `lib/` | `no_std`, pure, fuzzed | Read-only ZFS: vdev labels, uberblocks, the MOS, DSL under `zroot/ROOT/*`, and ZPL files (`lz4`, `zstd`, uncompressed) |
| `lib/install-plan` | `lib/` | `no_std`, pure, host-tested | The `InstallPlan` type, its validation, and the step list it expands to (§5.1) |
| `cargo xtask live-image` | `xtask/` | host | The live medium, for QEMU, USB sticks, and release (§4) |
| `scoria-install` | `desk/install/` | Rust, `scoria-ui` | The installer's interface: builds and confirms a plan |
| `scoria-installd` | `desk/installd/` | Rust, root | Validates the plan, runs it, reports progress |
| `scoria-greetd` | `desk/greetd/` | Rust, root, from `rc(8)` | PAM, sessions, automatic login, the owner's password and disk key (§6, §7) |
| `scoria-greet` | `desk/greet/` | Rust, `scoria-ui` | The greeter's interface, in a restricted `scoriad` session |

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
   [BOOTSPLASH.md](BOOTSPLASH.md) carries it on through the kernel's boot.

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
  `vfs.root.mountfrom=zfs:zroot/ROOT/<be>`. This is FreeBSD's way: its `loader.efi` reads kernels
  from the pool, so `bectl` creates an environment with no step on the ESP, and an update that
  does not boot falls back to the previous environment. Kernels are not copied to the ESP
  (decided 2026-10-29).
- **Crash log.** On a machine with no serial port (the X870E, HARDWARE.md §5), after a panic or a
  watchdog reset, `loader-efi` saves the `LogRing` to `\EFI\ABYSS\PREVLOG.TXT` (as it does now) and
  offers it in the menu.

### 3.4 Encrypted disks and one password

Encryption is FreeBSD's: the pool sits on GELI providers, laid out as `bsdinstall`'s encrypted ZFS
install does (`geli init -b -g`: boot-time attach, readable by the loader). Decided 2026-10-29: one
password unlocks the disk and the session.

- **One prompt.** `loader-efi` finds GELI providers flagged for boot, asks for the passphrase once
  over the splash, decrypts them (AES-XTS, the key derived with PKCS #5 v2 as GELI records it), and
  reads the pool through them. It hands the keys to the kernel in the boot key buffer, as FreeBSD's
  loader does (`MODINFOMD_KEYBUF`), so the kernel attaches the providers without asking again.
  The passphrase itself is wiped once the keys are derived and is never passed on.
- **One password.** The installer uses the owner's account password as the GELI passphrase. When
  the loader unlocked the root disk at this boot, it says so in the kernel environment
  (`abyss.boot.unlocked_root=1`, an AbyssBSD addition recorded in compat-deviations.md), and
  `scoria-greetd` logs the owner in without asking, once per boot (§6.3). Unlocking the disk was
  the authentication. The lock screen, a logout, and every other account still ask.
- **Staying one password.** Changing the owner's password through Scoria re-keys the disk in the
  same step (§7).
- **Kernel side.** `geom_eli` (GELI providers, the keys from the boot key buffer) and the crypto it
  uses, on `lib/crypto`. Not yet scheduled; it is needed before encrypted installs (I.4).

### 3.5 Handover (`bootproto` v3 → `sys/vt` → `gop-kms` → `scoriad`)

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
3. **Display driver.** `gop-kms` and native display drivers attach with `FirmwareState::LeftRunning`
   and adopt the scanout buffer without blanking (GRAPHICS.md §7.5).
4. **The greeter.** `scoria-greetd` starts the greeter's `scoriad` through seatd. The mode it finds
   is the one the firmware set, so its first frame replaces the splash with no mode set, and it
   reads the splash's background color (BOOTSPLASH.md §5.7) so that frame matches. On an
   automatic login the greeter is skipped and the user's own `scoriad` takes the display the same
   way.

---

## 4. Live media

`cargo xtask live-image --arch {x86_64,aarch64}` builds the medium, for QEMU, for USB sticks on the
reference machines, and for release: one recipe, so test and shipping media cannot drift. It uses
`makefs` and `mkimg` on the build server and contains:

- FreeBSD 15.1's `base.txz`, with this kernel as `/boot/kernel/kernel`;
- `loader-efi` on the ESP (`\EFI\BOOT\BOOTX64.EFI`, `BOOTAA64.EFI`);
- the Scoria package and its dependencies, installed into the image, plus the same packages as an
  offline repository (`/usr/local/share/scoria/media/pkg`) for the installer to install from;
- `base.txz` itself, for the installer to extract;
- a UFS root, read-only, with tmpfs over `/tmp`, `/var`, and the live user's home.

The live session. With `LIVE_MEDIA` set, `scoria-greetd` logs in the `live` user (no password,
not in `wheel`) without a greeter, and the session starts `scoria-install` once, which the user may
close; it stays in the launcher. `scoria-installd` runs only on live media (`rc.conf` there enables
it).

**Persistence** (decided 2026-10-29). On its first boot from a stick with spare space beyond the
image, the medium creates a writable UFS partition in that space, labelled `abyss-persist`, and
mounts it on every later boot. It keeps the live session's settings (`~/.config/scoria/`) and saved
files across boots, and corpus captures. A stick with no spare space boots as before, with nothing
kept.

On new hardware, the medium is also how a board's ground truth is captured (`acpidump -dt`,
`devinfo -rv`, `pciconf -lv`, EDIDs) into `tests/corpus/<board>/` (P8.0), onto `abyss-persist`.

---

## 5. The installer

### 5.1 Two processes and a plan

`scoria-install` is an ordinary Scoria application: a `scoria-ui` window in the live session, with
no privilege. It reads what it shows (disks, their contents, keyboard layouts, time zones) through
`scoria-installd`, builds an **`InstallPlan`**, and sends it for confirmation and execution.

`scoria-installd` is root. It listens on `/var/run/scoria-installd.sock` (`SOCK_SEQPACKET`, the
framing and CBOR codec of DESKTOP.md §7) and accepts only the live session's user, checked with
`LOCAL_PEERCRED`. It answers queries, and for a plan:

1. **Validates it again** with `lib/install-plan`: the target disks exist and match the
   identities (serial number, size) the interface saw, none is excluded by §5.3, sizes fit, the
   account name is legal, and every option is one this release supports. A plan that fails is
   refused with the reason; nothing has been touched.
2. **Expands it** into a fixed list of steps (§5.4, §5.5), each a FreeBSD command with its
   arguments, and with `--dry-run` prints that list instead of running it. The tests compare the
   list with stored expectations.
3. **Runs the steps**, streaming progress events, and stops at the first failure with the step,
   its command, and its output. A failure before the first write leaves every disk unchanged;
   after it, the report says what was written.

```rust
pub struct InstallPlan {
    pub target: TargetDisks,       // one disk, or two or more for a ZFS mirror
    pub layout: Layout,            // Ufs | Zfs | ZfsEncrypted
    pub swap: Bytes,
    pub owner: Account,            // name, full name, password (never logged)
    pub host_name: HostName,
    pub keyboard: KeymapName,
    pub time_zone: ZoneName,
    pub boot_entry: BootEntry,     // add a UEFI entry, and the removable-media path
}
```

The password crosses the socket once, is held in memory that is zeroed after use, and never
appears in a log, a dry-run listing, or a progress event.

### 5.2 Screens

Short, keyboard-complete, and in one window (DESKTOP.md §8): **Disk** (each disk with its model,
size, serial, and contents), **Layout** (UFS, ZFS, encrypted ZFS; ZFS options appear once M-Z
exists), **Owner** (name and password, with the note that the password also unlocks the disk when
encrypted), **Summary** (everything the plan will do, in words, with the disks to erase named
again), then **Install** with hold-to-confirm, a progress view, and **Restart**. Keyboard layout
and time zone default from the live session and are editable on the summary.

### 5.3 Disks with data

The installer names what is on each disk and refuses what it must not touch. `scoria-installd`
refuses a disk that is mounted, holds the running root (the medium itself), or holds a ZFS pool
that is not imported. It names the partitions on a disk (`ntfs`, `efi`, `linux-data`, `apfs`, …)
and the file systems it can recognize, and erases them only after the hold-to-confirm on the
summary. These rules come from the monorepo installer's `safety.rs`, reimplemented in
`lib/install-plan`.

That warning is only as good as what the kernel reports, so `geom disk list`, `gpart show`, and
`zpool import` must describe a disk here exactly as they do on FreeBSD 15.1, which the GEOM work
checks differentially.

### 5.4 Layouts

All three are GPT with an ESP (FAT32, 260 MiB, as `bsdinstall`), then:

| Layout | Partitions | Root |
|---|---|---|
| **UFS** (before M-Z) | `freebsd-swap`, `freebsd-ufs` | `newfs -U -j`; `/` holds everything |
| **ZFS** (M-Z) | `freebsd-swap`, `freebsd-zfs` | `bsdinstall`'s layout: `zroot/ROOT/default` as `bootfs`, `zroot/home`, `zroot/tmp`, `zroot/usr/{ports,src}`, `zroot/var/{audit,crash,log,mail,tmp}` |
| **Encrypted ZFS** (M-Z, `geom_eli`) | `freebsd-swap` (encrypted with a one-time key, `.eli` in `fstab`), `freebsd-zfs` on GELI | As ZFS, the pool on `geli init -b -g -s 4096` with the owner's password (§3.4) |

### 5.5 What it writes

1. The partitions and file systems of §5.4.
2. `base.txz` extracted to the new root, and this kernel as `/boot/kernel/kernel`.
3. The Scoria package and its dependencies from the medium's repository (`pkg -r <root> install`).
4. The owner with `pw -R`: in `wheel` (for `su`) and `video` (for seatd), with the password.
5. `rc.conf`: host name, `keymap`, `seatd_enable`, `scoria_greetd_enable`, `devd`, `dumpdev`,
   `zfs_enable` where ZFS; `/etc/localtime`; `fstab`.
6. `loader.conf`: `autoboot_delay="0"`, `boot_mute="YES"` (BOOTSPLASH.md), `zfs_load` and
   `geom_eli_load` where used (accepted as built in, §3.1), and the splash.
7. `/usr/local/etc/scoria/greet.ucl`: the owner, for automatic login after an unlock (§6.3).
8. `loader-efi` on the ESP, at `\EFI\ABYSS\LOADER.EFI` and `\EFI\BOOT\BOOT<arch>.EFI`, and a UEFI
   boot entry for it with `efibootmgr`.

| Step | Interfaces | Where here |
|---|---|---|
| Find disks | `geom disk list`, `kern.disks`, `DIOCGMEDIASIZE`, `DIOCGIDENT` | GEOM work in progress (`lib/geom`, `geom_kern`); P3.7 |
| Partition | `gpart create/add/destroy` (GEOM's `gctl` requests), `/dev/gpt/*` labels | GPT and MBR done (P9, `/dev/geom.ctl`); BSD labels read only |
| ESP | `newfs_msdos`, `mount_msdosfs` | P5.6 |
| Root | `newfs -U -j` (UFS); `zpool create`, `zfs create`, `bootfs` (ZFS); `geli init`, `geli attach` | P5.5, P5.7; M-Z; `geom_eli` |
| Extract, configure | `tar` (libarchive), `pkg -r`, `pw -R`, `pwd_mkdb`, file flags and ACLs | P5, P6.7 |
| Boot entry | `efibootmgr` (`/dev/efi`, UEFI variable services) | In: runtime services (P3.8) and `/dev/efi` (`kernel/src/efidev.rs`); the multi-user boot case sets and deletes `BootNext` |

---

## 6. Login: `scoria-greetd` and `scoria-greet`

### 6.1 Processes

| Process | Privilege | Does |
|---|---|---|
| `scoria-greetd` | root, from `rc(8)` (`scoria_greetd_enable`) | Starts the greeter, runs PAM, opens and closes sessions, automatic login, the owner's password change (§7). No graphics and no parsing beyond its socket's messages and `greet.ucl` |
| Greeter `scoriad` | `_scoriagreet` (an unprivileged system user), devices from seatd | A restricted session: one output layout, no IPC bus, no `exec` bindings, no clients but `scoria-greet` |
| `scoria-greet` | `_scoriagreet` | The interface: user list or name field, password, keyboard layout, power actions. It holds no secrets beyond the password it is typing |

The greeter shares `scoria-lock`'s look and the floors of DESKTOP.md §8.2: it never takes a mode
set (§3.5), its targets reach the edges, and its power actions use hold-to-confirm.

### 6.2 A login

1. `scoria-greetd` starts the greeter `scoriad` and `scoria-greet` as `_scoriagreet`, connected to
   it by a socket pair.
2. `scoria-greet` sends a user name and a password. `scoria-greetd` runs PAM as root
   (`/usr/local/etc/pam.d/scoria-greet`: `auth` and `account` from `system`, and `session` with
   `pam_xdg`), so the greeter needs no setuid helper.
3. On success it stops the greeter's processes (the seat returns to seatd), then forks the
   session: `setusercontext(LOGIN_SETALL)`, the user's `XDG_RUNTIME_DIR` from `pam_xdg`, and
   `scoria-sup`, which starts the user's `scoriad`. `scoria-greetd` holds the PAM session open for
   the session's life, which `pam_xdg` needs (DESKTOP.md §3).
4. When `scoria-sup` exits, `scoria-greetd` closes the PAM session and starts the greeter again.

### 6.3 Automatic login

- **After an unlock.** Once per boot, if the kernel environment has `abyss.boot.unlocked_root=1`
  and `greet.ucl` names an owner, `scoria-greetd` opens the owner's session without the greeter,
  using a separate PAM policy (`scoria-greet-autologin`: `account` and `session` only). A later
  logout returns to the greeter, which then asks.
- **Live media.** With `LIVE_MEDIA`, the `live` user is logged in the same way on every start.
- Nothing else logs in automatically. There is no configuration for automatic login without an
  unlocked disk.

---

## 7. The owner's password and the disk

On an encrypted install the owner's password is also the GELI passphrase (§3.4). To keep them one:

- `scoriactl account password` (and the quick-settings entry that runs it) asks for the old and new
  passwords and sends them to `scoria-greetd`, which accepts the request only from the owner's own
  session (`LOCAL_PEERCRED`). It verifies the old password with PAM, changes it with PAM's
  `chauthtok`, and runs `geli setkey` on each boot provider with the old and new passphrases. If
  re-keying fails, the account password is changed back and the error is shown.
- `passwd(1)` still works, as on FreeBSD, but changes only the account. The next boot then needs
  the old passphrase at the loader, and the automatic login is skipped because the unlock no
  longer matches the account (§10 question 3).

---

## 8. Phasing

| Step | Work | Needs | Exit |
|---|---|---|---|
| **I.1** | `lib/loaderconf`, `lib/qoi`, `bootproto` v3; splash, silence, and the boot menu in `loader-efi`; the quiet console in `fb-console` and `sys/vt` | — | QEMU boots silently through the splash on both arches; a held `Space` opens the menu; FreeBSD 15.1's `loader.conf` from a P6 image is honored |
| **I.2** | `lib/zfs-boot`, fuzzed against `zpool`-made images from the build server; GELI in `loader-efi` and the boot key buffer | M-Z's on-disk work, or images made on FreeBSD | `loader-efi` lists and boots `zroot/ROOT/*` from images FreeBSD wrote, including a GELI-encrypted one after one passphrase prompt |
| **I.3** | `cargo xtask live-image`; `scoria-greetd` and `scoria-greet`; the live session with automatic login | DESKTOP.md M-D.2 (a session on `gop-kms`) | The medium boots this kernel to a live Scoria session in QEMU, and an image with a test account boots to the greeter and logs in |
| **I.4** | `lib/install-plan`, `scoria-install`, `scoria-installd`: UFS, then ZFS, then encrypted ZFS with the one password; §7's password change | I.3, §5.5's rows, DESKTOP.md M-D.3 (`scoria-ui`), M-Z for ZFS, `geom_eli` for encryption | DESKTOP.md M-D.5's exit: each layout installs and boots to the greeter as QEMU boots driven by `xtask`, dry-run step lists match their expectations, and an install to the X870E's second NVMe boots to Scoria |

---

## 9. Decisions

Decided 2026-10-29:

1. **Kernels are read from ZFS**, from each boot environment, as FreeBSD's loader reads them
   (§3.3). Nothing is staged on the ESP.
2. **One password** unlocks the disk at boot and logs the owner in (§3.4).
3. **The live medium keeps a persistence partition** in a stick's spare space (§4).

Decided 2026-11-23 (ADR-0026):

4. **The installer is Scoria's**: `scoria-install` and `scoria-installd` in `desk/`, an
   unprivileged interface and a root service that validates the plan itself (§5).
5. **Login goes through a greeter**: `scoria-greetd` (root, PAM) and `scoria-greet` (unprivileged,
   in a restricted `scoriad`) (§6).
6. **The medium is built here**, by `cargo xtask live-image`, for testing and release alike (§4).

## 10. Open questions

1. **Updates.** The monorepo's `abyss update` is gone with it. Updating into a new boot
   environment (`bectl create`, then upgrade inside it, then activate) needs a tool: `pkg` and
   `freebsd-update` wrapped by a small Scoria command, or a design of its own with M-U.
2. **Several users at once.** The greeter serves one session at a time. Fast user switching needs
   a `vt` and a seat per session and `scoria-greetd` tracking them; later.
3. **`passwd(1)` on an encrypted install.** Should a PAM module (in `desk/`) re-key the disk on any
   password change, so `passwd` keeps the one password too, or is the Scoria path enough?
4. **Packages on the medium.** An offline repository duplicates what is installed in the image.
   Copying the live image's installed packages to the target is smaller but ties the target to the
   medium's exact state.
5. **Localization.** Which languages and keyboard layouts the greeter and installer offer in the
   first release.
