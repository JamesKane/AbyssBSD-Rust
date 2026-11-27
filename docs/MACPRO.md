# Mac Pro (Late 2013) bring-up plan

Status: draft, 2026-11-27. The plan for the roadmap's "Track: Mac Pro (Tier 2)". The machine's
profile is [HARDWARE.md §9](HARDWARE.md#9-board-profile-amd64-mac-pro-late-2013-macpro61); its
scope and firmware floor are [ADR-0023](adr/0023-hardware-scope-consumer-decade.md).

## 1. Decisions (owner, 2026-11-27)

| Question | Decision |
|---|---|
| Disk AbyssBSD may write | **An external USB 3 disk** (ESP + root), identified by serial number as the Orange Pi's Samsung is. The internal Apple SSD holds macOS 12.7.6 and is never written: `ahci` may attach to it, `deploy` refuses it by serial number |
| Sharing the machine | **QEMU and host tests first.** The FreeBSD-fork project (`../AbyssBSD`) is running thermal experiments on the machine now, booted from its own FreeBSD stick. This track goes to the machine only when that work is done or the owner hands it over. The full ground-truth capture (§5, MP0) is asked of that project, since it already has FreeBSD running there |
| Where the fan table lives | **`platform/apple-mac` states it as typed properties of the SMC's `APP0001` node; `asmc` holds all the behavior** ([ADR-0027](adr/0027-platform-osi-and-device-properties.md), which also settles the `_OSI` hook). A Mac with no such properties runs the SMC's own curve |
| Roadmap placement | **Its own track**, parallel to P10/P11. Per ADR-0023 a Tier 2 machine is checked for correctness, booted at milestone exits, and gates nothing. It has no performance gate |

## 2. What is already measured

Since 2026-10-02 the FreeBSD-fork project has run FreeBSD 16-CURRENT (its fork,
`__FreeBSD_version` 1600034) and macOS 12.7.6 on this unit. Its results are the best ground truth
we have. They are restated here so this document does not depend on another project's notes, which
change hourly. "Fork" below means `../AbyssBSD/src`; "drm-kmod" means `../AbyssBSD/kmod/drm`.
Hashes checked against both trees on 2026-11-27; neither is pushed.

**Configuration (SMBIOS, `pciconf`, `dmesg`, 2026-11-26):**
- `MacPro6,1`, board `Mac-F60DEB81FF30ACF6` (`j90`), boot ROM `481.0.0.0.0` (2023-10-07).
- CPU: Xeon E5-2697 v2, 12 cores and 24 threads. Memory: 64 GB.
- GPUs: two FirePro D700 (Tahiti XT, `1002:6798`). `pci0:2:0:0` has subsystem `106b:0128`
  (compute: no outputs connected) and `pci0:6:0:0` has `106b:0127` (display: the 2560×1440 monitor
  is on it). Each has its own VBIOS (`113-C3861XA`/`XB`).
- Network: `bge0`, `bge1`.
- SMC: `APP0001`, SMC revision 2.20f18. It has **MMIO** at `0xfef00000` (`LDKN` = 2) as well as
  ports `0x300-0x31f`, on IRQ 6. **One fan** ("Main"): firmware minimum 790 rpm, real maximum
  2600 rpm.
- No ACPI thermal zone (`_TZ`). TjMax 99 °C. `IA32_MISC_ENABLE` bit 3 is set; `0x1B2` = 0.
- Boot: firmware boot order is macOS. FreeBSD boots from the stick when the owner holds Option.

**Thermal behavior.** This is the finding that shapes the plan. The CPU and both GPUs share one
heatsink cooled by one fan:
- From a **cold boot** the SMC holds the fan at 790 rpm until the GPU dies pass about 58 °C. It then
  slews like an integrator, by minutes, not seconds. Undriven D700s (at their VBIOS boot clocks)
  idle at 59–64 °C. Under macOS they idle at 51–55 °C, about 15 W each.
- macOS writes **no** fan keys. After macOS has run, the SMC keeps the fan at its maximum (2600)
  even at idle. That "macOS mode" is SMC state: it survives a warm reboot into another OS, and a
  power-off clears it.
- A written `F0Mn` (fan minimum) is a floor in automatic mode, and it survives a warm reboot.
  `F0Tg` (target) is overwritten by the SMC in automatic mode. `F0Mx` is not a cap.
- Under a combined CPU and GPU load (about 296 W), **the SMC throttles the CPU by itself**,
  whatever the OS: it asserts PROCHOT and the CPU drops to 1.2 GHz. It reports this through its own
  keys (`MSLT` 0..100, `MSTc`, `MSLG`), and it latches until both dies cool to about 58 °C. The GPUs
  are **not** throttled by the SMC: macOS's own graphics power management drops them to 300 MHz
  when `MSLT` rises. FreeBSD does nothing, and the GPUs ran on to 87 °C.
- A hard freeze the fork saw in October fits heat soak. One hard reset at `kldload amdgpu` has
  not reproduced.

**What the fork built on these measurements** (BSD-2-Clause, the AbyssBSD project's own):
- `asmc` fan floor policy for `MacPro6,1`: fork `8fd5e03530ad`, with the curve fitted to the macOS
  baseline in `50a46e98369f` (`sys/dev/asmc/asmc_fan.c`). Every 2 s it reads the SMC's own
  temperature keys (GPU `TG0D`/`TG1D`, CPU `TCXc`/`TC0P`; no OS sensor is involved) and raises
  `F0Mn` along a linear curve, GPU 45→65 °C and CPU 55→80 °C, from 790 up to 2600 rpm. It rises at
  once and falls at most 50 rpm a sample. It restores 790 at shutdown, detach and suspend. It is
  gated on `F0Mn` having SMC type `fpe2`, because upstream `asmc` sets `sc_is_t2` for **any**
  MMIO SMC, the Mac Pro's included, and T2 handling differs (one mode key per fan instead of
  `FS! `). From a cold boot under 5 min of 24-thread CPU load it held GPU 64→68.5 °C and CPU 76 °C,
  with the fan at about 2600, as macOS does.
- `spa5` sensor decoding (per-GPU rail power `PG0R`/`PG1R`, system power `PSTR`): `dc9ade6fabec`.
- The platform-limit hook: fork `eaeaeea29cca` (`asmc_plimit.c`) and drm-kmod `e7da0f18f5`. It
  polls `MSLT` every 2 s and fires an event handler (`platform_power_limit`). It turns off only after 15 zero samples (30 s),
  because `MSLT` flickers to 0 for about 50 s at onset and each flip cost the GPUs a state switch.
  The GPU driver then forces its thermal (lowest) state.
- drm-kmod (AMD's MIT-licensed code): `b85c6d4327` keeps a device alive when UVD 3.1 firmware
  validation fails (the display GPU used to die with "Fatal error during GPU init").
  `3aadfdfe05` runs SI DPM levels at their VBIOS voltages. Upstream patched the leakage table
  before the extended power table filled it, so 650 MHz ran at 1062 mV instead of about 893. It
  also skips the display-clock voltage floor, which held every level at 900 mV. Before the fix
  the GPUs drew 115–127 W each where macOS drew 78–86 W at the same work.

The clean-room specification for all of this is `~/Projects/OS/chinese-wall/specs/apple-macpro6-1-thermal/`
(sections 01–07). Its §04 list of "what neither reference OS does" is a requirement list for us.

## 3. Principles for this machine

1. **Thermal comes first.** No sustained load on the Mac Pro (stress, kyua, a build, fsx) until
   `asmc` runs the fan floor policy. Until then, boots are short and interactive. The SMC's own
   PROCHOT protects the CPU; nothing protects undriven GPUs except the fan.
2. **"Nothing special."** No `loader.conf` knob makes this machine work. The model's fan table and
   limit keys are device properties from `platform/apple-mac` (ADR-0027), so `asmc` and `sys/acpi`
   name no model.
3. **FreeBSD 16-CURRENT is the behavioral reference** for `_OSI("Darwin")` (ADR-0023 §4) and for
   `asmc`. The fork's additions are the reference for the fan policy. macOS is observation only:
   what it did is a measured baseline, never a source.
4. **Licences.** FreeBSD and the fork's `asmc_fan.c`/`asmc_plimit.c` are BSD-2-Clause (headers
   read). drm-kmod's AMD files (`si_dpm.c`, `legacy_dpm.c`, headers read) are MIT and usable under the MIT rule in CLAUDE.md. Apple's
   `X86PlatformPlugin` plist and the two VBIOS ROMs the fork dumped are **not** committed to the
   corpus without the owner's say-so.
5. **The SMC is never jammed.** Every SMC access has a bounded total time, a jammed SMC is retired,
   and a failed, stuck or 0 °C temperature read counts as hot (spec §04 items 1–5).

## 4. Gaps in AbyssBSD today

| Area | State on `main` (0e8973c) | Needed |
|---|---|---|
| CPU model | QEMU boot cases use `-cpu host` or `-cpu max` (`xtask/src/qemu.rs`); every machine so far is 2021 or later. Not audited for newer-CPU assumptions | An Ivy Bridge boot case: AVX but no AVX2, BMI, INVPCID, SMAP, XSAVEC/XSAVES or RDSEED. `arch/x86_64/src/fpu.rs` uses `xsave64`, which Ivy Bridge has |
| EFI 1.10 | ADR-0023 promises unit tests of the loader's protocol lookups against a 1.10-sized system table and runtime table; a search found none (to confirm at MP1) | The tests, and a fallback at each newer service |
| `_OSI("Darwin")` | `lib/aml` answers 0 (`lib/aml/src/tests/corpus.rs:151`) | `platform/apple-mac` with a pre-DSDT `_OSI` hook; ADR-0020's interface gains the SMBIOS maker and the hook (ADR-0023 §4) |
| ACPI corpus | No table older than ACPI 5.0 | `tests/corpus/macpro61/`: older FADT/MADT revisions tested, not assumed |
| Watchdog | `ichwd` stops at ICH10 (`drivers/ichwd/src/lib.rs:129`) | Patsburg LPC `0x1d40` and `0x1d41`, TCO version 2, as FreeBSD's `ichwd.c:165` |
| SMC | None | `asmc` (MMIO and port back ends, keys, sensors, fans), the fan floor policy, the platform-limit state |
| CPU temperature | None (`coretemp` absent) | `coretemp` (`IA32_THERM_STATUS`, `0x1A2`), for observation and `sysctl dev.cpu.N.temperature`. The fan policy reads the SMC's keys, not this |
| Ethernet | None for Broadcom | `bge` for the BCM57762 (the 57765 family); FreeBSD's `if_bge.c` is about 6,900 lines |
| USB | `xhci` only, no EHCI | The front-panel controller identified (MP0). If the four USB 3 ports are on an xHCI, nothing new is needed |
| CPU frequency | `hwpstate_amd` and CPPC only | EIST through ACPI `_PSS`/`_PCT` (FreeBSD `est`, `acpi_perf`). Not needed for correctness: the firmware's state runs |
| Graphics | GOP console; native `amd-kms` is RDNA2 only | Southern Islands under M-G, later (§6) |
| Boot selection | `BootNext` (Orange Pi, X870E) | Apple's firmware ignores most of `BootOrder`/`BootNext`; to be checked (MP0, MP5) |

## 5. Milestones

Each milestone runs only the tests it touches (targeted runs). The full gates run at the track exit.

### MP0: Ground truth (no AbyssBSD code)

- HARDWARE.md §9: replace "Expected" rows with the measured facts in §2, dated and attributed.
- Ask the fork project, on its next FreeBSD boot, for the read-only capture HARDWARE.md §9 lists.
  The thermal capture of 2026-11-26 has only SMBIOS, `asmc`, MSRs and the display devices, so
  what is outstanding is: `acpidump -dt` (every table, the DSDT `APP0001` device and the VFCT),
  `pciconf -lvc`, `devinfo -rv` and a verbose `dmesg`, each **twice**, with
  `hw.acpi.apple_darwin_osi=1` (the default) and `=0`, for MP2; `kenv`; `usbconfig` with the
  controller each front port is on; `efivar -l`; `efibootmgr -v`; the GOP modes on each output;
  whether `kldload ichwd` attaches (MP5 relies on it); and `uname -a` and the kernel's commit, since
  the capture comes from the fork's kernel, not stock. From macOS: `bless --info` and `nvram -p`
  (the `efi-boot-device` variables). It goes to
  `tests/corpus/macpro61/` in the layout of `tests/corpus/x870e/` (`freebsd/`, later `abyssbsd/`).
- Record in the corpus README which outputs GOP lights, and whether the firmware leaves the APIC in
  xAPIC or x2APIC mode.

### MP1: The CPU and firmware floor, in QEMU

- A boot case on `-cpu IvyBridge` (under KVM on the laptop, TCG elsewhere), x86_64 only. Fix
  whatever assumes a newer CPU, gated on CPUID as FreeBSD gates it.
- The EFI 1.10 unit tests ADR-0023 names (loader protocol lookups against a 1.10-sized system
  table; runtime services through a 1.10-sized runtime table, never reaching `QueryVariableInfo`
  or the capsule entries).
- The loader on a FAT ESP at `\EFI\BOOT\BOOTX64.EFI` only, with no `Boot####` of its own, since
  the Startup Manager lists only that path.

### MP2: ACPI for Apple firmware, against the corpus

- `platform/apple-mac`: matches the SMBIOS maker `Apple Inc.` or `Apple Computer, Inc.`, as
  `acpi_apple_darwin_osi` does. It supplies the `_OSI` change (answer `Darwin`, drop the Windows
  strings) before the DSDT loads. The tunable is `hw.acpi.apple_darwin_osi`, default on.
- The interface change of ADR-0027: `FirmwareIdentity` gains the SMBIOS identity, the `osi` hook
  runs before the DSDT loads, and the generic device-property applier lands with its first user.
- `lib/acpi-tables` and `lib/aml` against `macpro61`: the old FADT and MADT revisions, `OSYS` =
  10000 after `_INI`, and the namespace FreeBSD 16-CURRENT shows (`devinfo`) for both `_OSI`
  answers.

### MP3: Drivers, host-tested

- `ichwd`: the Patsburg IDs. This is small; land it first.
- `coretemp`: the per-core and package digital thermal sensors.
- `asmc`, from FreeBSD's `sys/dev/asmc` (port and MMIO back ends) with the fork's
  `asmc_fan.c` and `asmc_plimit.c` as the policy reference. Trap: do not carry over upstream's
  "MMIO means T2" (`sc_is_t2`); the Mac Pro has an MMIO SMC and no T2. It goes beyond the fork where spec §04
  says neither OS goes: writes clamped to the fan's minimum and maximum; fans returned to
  automatic with the firmware minimum at shutdown, reboot **and panic** (a warm reboot otherwise
  carries a raised floor into the next OS); bounded transactions; a failed sensor counted as hot.
  The model table (fwmin 790, top 2600, the sensor keys and curve points, the `MSLT`/`MSTc`/`MSLG`
  keys) is `APP0001`'s device properties from `platform/apple-mac` (ADR-0027); `asmc` reads them
  through the `_DSD` property accessor and runs the SMC's own curve when there are none. The
  platform-limit state is exported (`dev.asmc.0.plimit.*`) and published as a kernel event, with
  the fork's off hysteresis (15 samples), for a GPU driver to consume later.
  Host tests drive a fake SMC built from the fork's key dumps: macOS mode, cold boot, latch and
  release.
- `bge` for the BCM57762, from FreeBSD's `if_bge.c`, scoped to the 57765 family (its ASIC revision
  checks and the 57762's PHY). Host-tested against a register model, as `rge` was. It needs
  no other Broadcom chips.

### MP4: First boot on the machine

When the owner hands the machine over:
- The USB 3 disk (ESP `abyssefi`, root `abyssroot`), written from the build server, its serial
  number recorded in HARDWARE.md §9. Booted through the Startup Manager (Option held).
- GOP console on the display GPU's output, USB keyboard, the loader's persistent log on the ESP.
- **Thermal gate**, run from a **power-off**, never a warm reboot from macOS: macOS leaves the
  SMC in a mode that holds the fan at 2600, which would hide a broken policy (and a
  `bless --nextonly` boot is exactly such a reboot). `asmc` attaches with the MMIO back end, the
  fan policy follows the curve, and it restores 790 at shutdown. Compare against the fork's
  cold-boot CPU-only run (5 min of 24 threads: GPU 64→68.5 °C, CPU 76 °C, fan about 2600). There is
  no GPU load to compare: AbyssBSD drives no GPU in this track. Nothing sustained runs before this
  passes.
- `devinfo` against the capture's FreeBSD tree.

### MP5: Network and an unattended loop

- `bge0` up, DHCP, `sshd`.
- `cargo xtask deploy --machine macpro`, checked by the USB disk's serial number.
- Boot selection. The question is how to boot AbyssBSD once, unattended, and return to macOS by
  default. Candidates, in order: `BootNext` (if the capture shows Apple honors it); from macOS,
  `bless --mount <ESP> --setBoot --nextonly`; Apple's `efi-boot-next` variable written from
  AbyssBSD through runtime services. The owner chooses whether the default boot ever changes
  away from macOS.
- The watchdog (`ichwd`) proven to reset a hung kernel, since there is no serial console.

### Exit

- The `x86_64-multiuser` scenario from the framebuffer and a USB keyboard alone.
- fsx and then `fsck_ffs` on the USB disk.
- The device tree against FreeBSD's `devinfo` from the capture.
- A 30-minute full CPU load from a power-off, with the fan policy: no freeze, temperatures
  plateau, the fan follows the curve. (The SMC's PROCHOT latch tripped only under combined CPU and
  GPU load, about 296 W, which this track cannot produce; the latch and its release are covered by
  the fake-SMC host tests.)
- Correctness only; no performance gate (ADR-0023).

## 6. Later work (not in the track exit)

- **Southern Islands graphics** (M-G, after RDNA2). `lib/amdgpu`'s AtomBIOS interpreter
  (`atom.rs`) is the part likely to carry over. SI has no IP discovery, PSP or DMCUB, its display
  is DCE 6, and its power management is the GPU SMC (`si_dpm`). Requirements already known from
  the fork:
  - a UVD 3.1 firmware failure leaves the device up without UVD;
  - the leakage table is patched after the extended power table is parsed;
  - the display-clock voltage floor is not applied;
  - each GPU uses its own VBIOS image from VFCT (spec §07 §3);
  - when the SMC's platform limit is active, both GPUs drop to their lowest state (macOS's
    behavior, 300 MHz).

  Until then the GPUs idle undriven at boot clocks, and the fan policy covers them.
- **EIST** through `_PSS`, and `_CST` idle (P11 has `_CST`).
- **HD Audio** on the C602J, and the HDMI audio of the display GPU.
- **Thunderbolt 2** (three controllers, powered only under `_OSI("Darwin")`): enumeration, then hot
  plug.
- Out of scope: Wi-Fi (BCM4360), Bluetooth.

## 7. Keeping in step with the fork project

The fork's work is ongoing (the GPU light-load comparison is not done, and the platform-limit
hook's hysteresis was still being tuned). Before MP3's `asmc` work and again before MP4:
- read `git -C ../AbyssBSD/src log -- sys/dev/asmc` and `git -C ../AbyssBSD/kmod/drm log` since
  the hashes in §2 (`eaeaeea29cca` and `e7da0f18f5` are the newest);
- update §2 and HARDWARE.md §9 with what changed, by hash;
- take its logs from `../abyss-swift-vm` (`macpro-thermal.log`, `fbsd-keys-*.txt`,
  `macos-keys-*.txt`) as the fake SMC's data.

## 8. Open questions

1. Which controller the front USB 3 ports are on, and whether anything we need is on the PCH's EHCI.
2. Whether Apple's firmware honors `BootNext` (MP5).
3. Which output GOP lights at which mode, and whether a cold and a warm boot differ.
4. Whether the firmware leaves the APIC in x2APIC mode.
5. Whether `MSLG` reaches 15 only when the GPUs respond (it peaked at 2 under FreeBSD and reached
   15 under macOS). This matters for how a GPU driver maps it to levels, not for this track.
