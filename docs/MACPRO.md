# Mac Pro (Late 2013) bring-up plan

Status: draft, 2026-11-27. The plan for the roadmap's "Track: Mac Pro (Tier 2)". The machine's
profile is [HARDWARE.md §9](HARDWARE.md#9-board-profile-amd64-mac-pro-late-2013-macpro61); its
scope and firmware floor are [ADR-0023](adr/0023-hardware-scope-consumer-decade.md).

## 1. Decisions (owner, 2026-11-27)

| Question | Decision |
|---|---|
| Disk AbyssBSD may write | **A dedicated USB stick** booted through the Startup Manager, as the FreeBSD-fork project's live stick is (replanned 2026-11-27, MP4): `disk-board.img` written to it by the owner, identified by serial number. The internal Apple SSD holds macOS 12.7.6 and is never written: `ahci` may attach to it, `deploy` refuses it by serial number |
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
- The platform-limit hook: fork `eaeaeea29cca` (`asmc_plimit.c`) and drm-kmod `6730343fad`. It
  polls `MSLT` every 2 s and fires an event handler (`platform_power_limit`). It turns off only after 15 zero samples (30 s),
  because `MSLT` flickers to 0 for about 50 s at onset and each flip cost the GPUs a state switch.
  The GPU driver then forces its thermal (lowest) state.
  Later fixes in both trees:
  - fork `421c666bda5a`: the "on" event is re-sent on every sample while on;
  - drm-kmod `e90c912900`: a limit already on when SI DPM starts is applied.

  Both cover the same case: a GPU driver that loads while the SMC holds the limit latched, which
  it does across a warm reboot, would otherwise run unlimited until the release. Our `asmc`
  therefore publishes the limit as state a late listener can read, not only as edges.
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

**Done 2026-11-27.**
- The fork project's capture is in `tests/corpus/macpro61/` (README there; HARDWARE.md §9 has the
  measured rows).
- The raw ACPI tables, read from FreeBSD's `/dev/mem` without `VFCT`, are in
  `tests/corpus/acpi/macpro61/`.
- Still missing: `bless --info`, and which Thunderbolt port carries the display.

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

**Done 2026-11-27.** What was built:
- The boot case `x86_64-ivybridge` runs QEMU's `IvyBridge-v2` with `vendor=GenuineIntel`, because
  KVM on an AMD host otherwise keeps the host's vendor. It passes: the kernel sees "Intel family
  0x6 model 0x3a", and XSAVE covers x87, SSE and AVX (XCR0 `0x7`, 832-byte areas).
- `boot/efi` defines each table's EFI 1.10 size (`BootServices::EFI_1_10_SIZE` = 0x170,
  `RuntimeServices::EFI_1_10_SIZE` = 0x70) and `TableHeader::provides`.
- Compile-time checks: every boot and runtime service the efi crate calls, and every runtime
  service the kernel calls (`kernel/src/firmware.rs`), lies inside those sizes.
- Host tests run the safe layer's lookups against a revision-1.10 system table whose UEFI 2.0
  slots are poisoned.
- The loader prints the revision as FreeBSD does (`%d.%02d`: 2.70, and 1.10 rather than 1.1).
- The boot cases already boot from `\EFI\BOOT\BOOTX64.EFI` with fresh variables and no `Boot####`
  of the loader's own. The loader's one NV variable (`AbyssLogRing`, 8 bytes) is written only when
  it makes a fresh ring, not every boot, which matters for Apple's small NVRAM.

On the planned checks:

- A boot case on `-cpu IvyBridge` (under KVM on the laptop, TCG elsewhere), x86_64 only. Fix
  whatever assumes a newer CPU, gated on CPUID as FreeBSD gates it.
- The EFI 1.10 unit tests ADR-0023 names (loader protocol lookups against a 1.10-sized system
  table; runtime services through a 1.10-sized runtime table, never reaching `QueryVariableInfo`
  or the capsule entries).
- The loader on a FAT ESP at `\EFI\BOOT\BOOTX64.EFI` only, with no `Boot####` of its own, since
  the Startup Manager lists only that path.

### MP2: ACPI for Apple firmware, against the corpus

**Done 2026-11-27.** The corpus tests run Apple's real tables:
- `lib/aml`:
  - all nine AML tables load without warnings and `_INI` runs without errors under both answers;
    integers are 32 bits (DSDT revision 1);
  - `OSYS` is 10000 and `OSDW()` 1 under Darwin, and 2012 and 0 under FreeBSD's default strings;
  - with an empty PCI space, a non-Darwin `_INI` cuts all three Thunderbolt controllers (each
    bridge's `RMCR` runs `RMC1`: link disabled, 2 s, power off) and Darwin cuts none. That is
    FreeBSD's `apple_darwin_osi=0` boot, which saw no Thunderbolt functions;
  - the SMC is at `\_SB.PCI0.LPCB.SMC` with FreeBSD's `asmc0` resources.
- `lib/acpi-tables`, against FreeBSD's `acpidump -t` decode:
  - MADT revision 2: 24 APICs, IO APICs 8 and 9, the SCI override active high and level;
  - FADT revision 4: SCI 9, PM registers at 0x400, a 24-bit timer, reset 0xcf9/6;
  - HPET and MCFG.

  The FADT showed a real bug: `minor_version()` read the reserved byte of a revision 4 FADT as
  minor 0. It is now `None` before revision 5.
- `lib/acpi-corpus` lists the machine, so `sys/acpi`'s and `kernel/table`'s whole-machine checks
  run it:
  - every device is in its machine's driver table;
  - nothing with a driver waits forever;
  - the firmware-shared ranges include `PLMT`, the SMC power-limit bytes the CPU methods read at
    ports 0x310-0x319.
- `platform/apple-mac`, end to end on the real tables: the Mac Pro's identity and the tunable through
  `osi_interfaces` give `OSYS` 10000, or 2012 with the tunable at 0. Every `MACPRO6_1` property names
  a device the namespace has.

The interface work:
- `lib/acpi-platform` (ADR-0027 §1–2):
  - `FirmwareIdentity` carries the SMBIOS system maker, product and family.
  - `PlatformSource::osi` returns `OsiChanges`.
  - `osi_interfaces` applies every platform's changes to FreeBSD's defaults, drops before additions.
    A tunable at 0 vetoes a change, and a vendor string added against a drop is reported.
- `lib/aml`: `OSI_FEATURE_GROUPS`, so dropping the vendor strings keeps "Extended Address Space
  Descriptor", as ACPICA's `ACPI_DISABLE_ALL_VENDOR_STRINGS` does. The interpreter already took
  its `_OSI` list as configuration.
- `sys/acpi`: the AML service settles `_OSI` before the DSDT loads and logs what a platform
  changed. `Ops::tunable` reads `hw.acpi.apple_darwin_osi` from the kernel environment.
- `platform/apple-mac`, in `PLATFORMS_X86_64`:
  - it matches the makers `Apple Inc.` and `Apple Computer, Inc.`, and answers Darwin and no
    Windows string unless the tunable is 0;
  - tests run a DSDT in Apple's pattern (Darwin, then Windows strings, each setting `OSYS`): 10000
    on an Apple machine, 2009 with the tunable at 0 or another maker.
- The kernel fills the environment (`kenv::init`) before any device attaches, as FreeBSD's static
  environment exists from early boot. It used to run after `devices::init`, so `acpi` saw no
  SMBIOS maker.
- `x86_64-ivybridge` now boots with Apple's SMBIOS system strings (`MacPro6,1`) and must log
  `acpi: apple-mac: _OSI answers Extended Address Space Descriptor, Darwin`. It passes.

Left for MP3, with its first user: the device-property applier (ADR-0027 §3), for `asmc`.

Left for the capture: the corpus tests below against `tests/corpus/macpro61/`.

The planned checks:

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
  "MMIO means T2" (`sc_is_t2`); the Mac Pro has an MMIO SMC and no T2. It goes beyond the fork
  where spec §04 says neither OS goes:
  - fan writes range-checked, never wrapped;
  - bounded transactions, and a jammed SMC retired;
  - a failed sensor counted as hot.

  The fan gets its firmware minimum back at shutdown, reboot and detach, since a warm reboot
  otherwise carries a raised floor into the next OS. After a panic the floor stays raised, as in
  the fork: a fan stuck high fails safe, and a power-off clears it.
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

**MP3 status, 2026-11-27.** `ichwd` (Patsburg), `coretemp`, the ADR-0027 property applier,
`asmc` and a first `bge` are done.

- **`drivers/bge`**, the BCM57765 family only (`BGE_FLAG_57765_PLUS`), from a line-cited digest of
  `if_bge.c` and `brgphy.c`:
  - identification by the `GEN15_PRODID_ASICREV` chip ID;
  - `bge_reset` with the bootcode handshake, the configuration header and MSI capability saved and
    restored around the core reset;
  - `bge_chipinit`, `bge_blockinit`, and the station address (the bootcode's "HK" copy in SRAM,
    else the EEPROM);
  - MII and the BCM57765 PHY, which `brgphy` only resets;
  - MSI with tagged status and one-shot, re-enabled after every reset;
  - the producer, return and send rings, all below 4 GiB, with frames copied through 2 KiB slots;
  - RX checksums; no TX checksum offload (FreeBSD's default), TSO, jumbo frames or VLAN tag
    stripping yet;
  - the PHY polled every second, where FreeBSD polls only while the link is down.

  14 host tests run against a model chip with the Mac Pro's identity.
- **Mac Pro facts** from the fork's capture (2026-11-27, FreeBSD 16 verbose boot): `bge0`/`bge1` are
  device `14e4:1682` with CHIP ID **0x57766000** (ASIC rev 0x57766, chip rev 0x577660). So the 57766
  branch applies: the PADRNG fix, a tolerated EEPROM configuration failure, and none of the 57765
  A0 waits. The PHY is OUI 0x001be9, model 0x24, rev 0 (`brgphy`'s reset-only case). There was no
  handshake timeout.

- **`ichwd`:** LPC `0x1d40`/`0x1d41`.
- **`coretemp`** (`kernel/src/coretemp.rs`):
  - FreeBSD's sysctls, its TjMax rules, and its critical-temperature `devd` event;
  - ktests for the decoding and the TjMax choice;
  - KVM shows no sensor, so the driver declines in QEMU.
- **Property applier:**
  - `lib/acpi-platform` has `DeviceProperty` and `PlatformSource::properties`;
  - `sys/acpi::properties::property` returns the device's `_DSD` value first, then a platform's
    value for its `_HID`;
  - a property for an absent device is logged once per device.
- **`platform/apple-mac`:** `MacPro6,1`'s `APP0001` properties are
  `apple,fan-floor-{fan,firmware-minimum,top,gpu-keys,gpu-range,cpu-keys,cpu-range}` and
  `apple,power-limit-keys`.
- **`drivers/asmc`:**
  - memory window first, else ports;
  - every transaction bounded at 200 ms;
  - set-aside rule:
    - only an SMC that does not answer at all (a command never acknowledged on the ports, the
      memory window never ready) three times in a row is set aside;
    - it is then tried again every 30 s;
    - a key the SMC lacks never counts, since on the ports it looks like a timeout too;
  - FreeBSD's fan and `temp` sysctls; speeds by each key's own type (`fpe2` or `flt `);
  - the fan floor and the platform limit as pure state machines (`policy.rs`), run by a 2 s
    worker;
  - `platform_limit()` and `on_platform_limit()`, which reports the current state at
    registration.
- **Host tests** (19), on a fake SMC on both windows whose keys have the types, lengths and values
  of the fork's cold-boot dump (`fbsd-keys-cold.txt`):
  - the encodings;
  - missing keys, which do not set the SMC aside, and the back-off and retry;
  - the driver's own sampling (`machine.rs`), with `platform/apple-mac`'s `MACPRO6_1` properties:
    - cold boot: the GPUs at 62 °C put the floor at 2328 rpm;
    - a stale 1600 rpm floor from a warm reboot goes back to 790;
    - a missing GPU sensor sends the floor to 2600;
    - turning the policy off restores 790, and no sample writes after that;
    - `MSLT` 0 → 100 → 0 turns the limit on, re-sends it each sample, and turns it off once;
  - the floor curve and its fall.
- **QEMU check:** `x86_64-ivybridge` adds QEMU's `isa-applesmc`. `asmc0` attaches through the
  ports and reads `SMC revision: 1.13f3`. The Mac Pro properties reach it and are declined there
  ("no such fan", "no level key"), since QEMU's SMC has neither.
- **Fan floor's fall:** with the fork's original fall rule, a raised floor never returned to the
  firmware minimum. It stopped up to 100 rpm above it (890 rpm) and was rewritten there every
  sample. Ours falls the rest of the way once demand is back at the minimum. The fork took the
  same rule in `9a912af88f81` (not yet run on the machine).
- **Never run on hardware or in a kernel:**
  - `coretemp`'s attach and sysctls (KVM shows no sensor);
  - the memory window inside the kernel;
  - the floor writes and the limit;
  - the `asmc` sysctl handlers;
  - all of `bge` beyond the host model (no emulator has this chip).

  All of these wait for MP4.

### MP4: First boot, from a USB stick

Replanned 2026-11-27 to follow the FreeBSD-fork project's way onto this machine. It boots a
developer live stick through the Startup Manager, runs everything from the stick, and is driven
over ssh as root by key. Only the stick is ever written: never the internal SSD, macOS, or the
firmware's boot settings.

- **The stick.** `ABYSS_ROOT_FREE=16g ABYSS_REMOTE_DIR=abyss/src-macpro cargo xtask --remote image
  --arch x86_64 --release --root board --authorized-key "$(cat ~/.ssh/id_ed25519.pub)"` builds
  `disk-board.img`:
  - a GPT disk with a FAT32 ESP (`efiboot0`), the loader at `\EFI\BOOT\BOOTX64.EFI` and the
    kernel at `EFI/ABYSS/KERNEL`;
  - a UFS2 root (`rootfs0`) with all of FreeBSD 15.1's `base.txz`;
  - DHCP on every interface, and sshd letting root in by key only.

  It is the analogue of the fork's `live-image.sh --ssh-key --stay`. **The owner writes it to a
  dedicated USB stick with `dd`**, which is the fork's step.
- **Sized for packages.** `pkg` installs into the root (MP6), so the root is made with room to spare:
  - `ABYSS_ROOT_FREE` is `makefs -b`, the free space left in the UFS2 root; by default only 25%;
  - MP6's set, measured from the FreeBSD 15.1 package catalogue on the build server, is about 3 GB
    installed (LLVM 19 alone is 1.77 GiB, which Mesa's software renderers need), plus `pkg`'s cache
    of what it downloaded;
  - **16 GiB free** holds that several times over, for development tools and logs, in an image of
    about 18 GB, so the stick is **32 GB or larger**. The graphics dev loop's 6 GiB held Sway, foot,
    fastfetch, glmark2 and Mesa.

  The image is written whole with `dd`, not sparsely: a sparse write would leave the stick's old
  bytes wherever the image has zeros, and some of those blocks may be file-system metadata. The root does not grow to fill a larger stick on its own; `growfs` on a mounted root is
  not something AbyssBSD has been tested with. The stick's serial number goes in
  HARDWARE.md §9 at its first boot (`usbconfig`, `camcontrol inquiry -S`).
- **Booting it.** Power on holding Option, then pick "EFI Boot". macOS stays the default, so a
  reboot without Option goes back to macOS. Rules:
  - only one live stick is plugged in at a time: ours and the fork's both carry FreeBSD-style root
    labels;
  - the stick goes in a rear USB 3 port, on the FL1100 (`xhci`). One port did not respond in the
    capture; HARDWARE.md §9 records which once known.
- **What runs.** The kernel mounts root from the stick, through `xhci`, `umass` and `da` on the
  FL1100. There is no GPU driver, which is the fork's thermal-pass medium by construction. `asmc`
  runs the fan floor from attach, and `coretemp`, `bge` and `ichwd` attach if they can. The GOP
  console is on the Thunderbolt-connected display (2560×1440, the only mode).
- **Writes outside the stick.** One 8-byte NVRAM variable, `AbyssLogRing`, written when the loader
  makes a fresh log ring (MP1). Nothing else.
- **If the network does not come up** (`bge` is host-tested only), the console and the loader's
  persistent log tell why. The previous boot's kernel log is saved to the stick's ESP as
  `EFI/ABYSS/PREVLOG.TXT`, readable by plugging the stick into the dev box. There is no serial
  port and no watchdog (§8, question 7).
- **Thermal gate**, unchanged: from a **power-off**, never a warm reboot from macOS. macOS leaves
  the SMC holding the fan at 2600, which would hide a broken policy. `asmc` attaches with the MMIO
  back end, the floor follows the curve, and 790 comes back at shutdown. Compare against the fork's
  cold-boot CPU-only run (5 min of 24 threads: GPU 64→68.5 °C, CPU 76 °C, fan about 2600). Nothing
  sustained runs before this passes.
- `devinfo` against the capture's FreeBSD tree (`tests/corpus/macpro61/freebsd/osi1-quiet/`).

### MP5: Network and the loop

- `bge0` up, DHCP, and `ssh root@<address>` with the stick's key. The fork's FreeBSD got .12 from
  this MAC address, so the same lease is likely.
- **Iterating, as the fork does.** Build, copy over ssh, boot again with Option held.
  - A kernel change goes onto the running stick: `/boot/kernel/kernel` on its root (the old one
    kept as `kernel.prev`), checked by SHA-256.
  - A change to the root's files goes the same way; a new `disk-board.img` is written only when
    the layout changes.
  - `cargo xtask deploy --machine macpro` does it with `deploy`'s checks: AbyssBSD is running, `/`
    is `gpt/rootfs0`, and that disk's serial number is the stick's. It writes no `BootNext`: the
    owner's Option press is the boot selection, as on the fork's stick.
- **Unattended boots are later work, and the owner's call:**
  - from macOS, `bless --mount <the stick's ESP> --setBoot --nextonly`, which needs sudo;
  - from AbyssBSD, Apple's `efi-boot-next` variable through runtime services.

  Neither changes the default away from macOS.
- **No watchdog.** The Patsburg watchdog is disabled by the firmware, so a hung kernel needs the
  power button, and the saved log tells why (§8, question 7).

### MP6: Sway and vkmark from the stick

The track's aim beyond its exit. The first step is software rendering: the Mac Pro's D700s have
no driver until Southern Islands comes under M-G (§6).
- `pkg install sway foot fastfetch seatd vkmark glmark2 grim mesa-dri mesa-libs dejavu`, from
  FreeBSD 15.1's packages (`IGNORE_OSVERSION=yes`, as `cargo xtask graphics --install` does). It
  needs the network (MP5) and the stick's free space.
- Sway on `gop-kms`: the boot framebuffer, which is the GOP's one mode, 2560×1440 on the
  Thunderbolt display. A USB keyboard and mouse go on the FL1100.
- glmark2 on llvmpipe, then **vkmark on lavapipe**: `mesa-dri` is built with `swrast_vk`. That is
  M-G.1's acceptance on a physical amd64 machine, as `tests/graphics/` takes it in QEMU, with
  vkmark added. Its screenshot goes with the run.
- Watch: the one-off PAM failure after the first large `pkg install` in the graphics dev loop.
  It was not reproduced afterwards.

Accelerated Sway and vkmark (radeonsi, RADV) need the native Southern Islands driver (§6). Whether
Mesa's RADV takes GCN 1 (GFX6) as FreeBSD packages build it is to check before that work starts.

### Exit

- The `x86_64-multiuser` scenario from the framebuffer and a USB keyboard alone.
- fsx and then `fsck_ffs` on the stick.
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
  the hashes in §2 (newest on 2026-11-27: `421c666bda5a` and `e90c912900`);
- update §2 and HARDWARE.md §9 with what changed, by hash;
- take its logs from `../abyss-swift-vm` (`macpro-thermal.log`, `fbsd-keys-*.txt`,
  `macos-keys-*.txt`) as the fake SMC's data.

## 8. Open questions

7. **No watchdog.** `ichwd` finds the TCO watchdog "disabled in BIOS or hardware". Whether that is
   the TCO `NO_REBOOT` strap or a disabled TCO block is not known. Recovering from a hang means
   holding the power button. Option: a software watchdog (a per-CPU NMI or an APIC timer that logs
   to the ring and resets through ACPI), which FreeBSD does not have either.

1. Which controller the front USB 3 ports are on, and whether anything we need is on the PCH's EHCI.
2. Whether Apple's firmware honors `BootNext` (MP5).
3. Which output GOP lights at which mode, and whether a cold and a warm boot differ.
4. Whether the firmware leaves the APIC in x2APIC mode.
5. Whether `MSLG` reaches 15 only when the GPUs respond (it peaked at 2 under FreeBSD and reached
   15 under macOS). This matters for how a GPU driver maps it to levels, not for this track.
