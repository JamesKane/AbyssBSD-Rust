# Hardware Support

## 1. Platform requirements

AbyssBSD supports machines that provide:

| Requirement | AMD64 | ARM64 | RV64 (Day 2) |
|---|---|---|---|
| Firmware | UEFI 2.7+ (64-bit), CSM disabled | UEFI 2.7+, **Arm SystemReady (SR/ES/IR-with-ACPI) or SBSA/BSA-conformant** | UEFI + ACPI (RVI BRS) |
| ACPI | 6.0+: RSDP 2.0/XSDT, MADT, FADT, MCFG, DSDT | 6.3+: MADT (GICC/GICD/GICR/ITS), GTDT, MCFG, SPCR, IORT, PPTT | 6.6+: RHCT, MADT (RINTC/IMSIC/APLIC) |
| CPU | x86-64-v2 minimum (SSE4.2, POPCNT, CMPXCHG16B). xAPIC **and** x2APIC (firmware may leave x2APIC disabled, as on the X870E) | Armv8.1-A minimum (LSE atomics, PAN, VHE) | RV64GC + Zicsr/Zifencei, Sstc preferred |
| Interrupts | x2APIC + IOAPIC, MSI/MSI-X | GICv3 + ITS (GICv2 only as a stretch for RPi4) | AIA (IMSIC + APLIC) |
| Storage boot | GPT disk with an ESP | GPT disk with an ESP | GPT disk with an ESP |

Machines that provide only a device tree (most hobby ARM SBCs without SystemReady firmware) are
out of scope for Day 1.

## 2. Reference platforms

| Tier | AMD64 | ARM64 |
|---|---|---|
| **Tier 1, virtual** (CI gate) | QEMU q35 + OVMF; bhyve UEFI | QEMU virt (gicv3, acpi=on) + edk2 |
| **Tier 1, physical** (untouched until P6, gating from P6) | **Ryzen 9 7900X on Gigabyte X870E AORUS ELITE WIFI7**, see [§5](#5-board-profile-amd64-ryzen-9-7900x--x870e-aorus-elite-wifi7) and [ADR-0009](adr/0009-amd64-reference-machine-ryzen-x870e.md) | **Orange Pi 6 Plus** (CIX P1 / Sky1), see [§4](#4-board-profile-orange-pi-6-plus-cix-p1--sky1) and [ADR-0008](adr/0008-arm64-reference-board-orange-pi-6-plus.md) |
| **Tier 2, best effort** | Other UEFI desktops/laptops | Radxa Orion O6 (same SoC), **Radxa Dragon Q8B** (see below), other SystemReady boards, Raspberry Pi 4/5 with community UEFI+ACPI firmware |

The **Radxa Dragon Q8B** (Qualcomm SC8280XP) runs FreeBSD under ACPI in the sibling project
`../AbyssBSD`: stock GENERIC, root on NVMe, Ethernet, USB, power and thermal, GPU and display. Its
board notes (`../AbyssBSD/docs/boards/radxa-dragon-q8b/`) are our ground truth, and `lessons.md`
there is required reading. Its quirks are driver work, not reasons to exclude it: the kernel runs
at EL1 under Qualcomm's hypervisor, which polices SMMU programming, and ACPI routes clocks and
power through Windows' PEP, so those need native drivers keyed on the SoC. Its GPU, NPU and video
drivers there go through LinuxKPI; here they are rebuilt in Rust ([ROADMAP](ROADMAP.md) open
decision 7).

The build server (Alder Lake i7-12700KF, hybrid P/E cores) is a good
*occasional* amd64 boot target from USB, which exercises the hybrid-core scheduler, but it is not
a CI target.

## 3. Driver matrix by phase

Legend: **P#** = roadmap phase in which the driver is first expected to work.

### Platform / core

| Function | AMD64 | ARM64 | Phase |
|---|---|---|---|
| Early console | 16550 UART (SPCR / COM1), GOP framebuffer text | PL011 / SBSA generic UART (SPCR, else **DBG2**), GOP framebuffer text | P1 |
| Watchdog | AMD FCH watchdog (WDRT table); Intel TCO later | SBSA generic watchdog (GTDT), with the Sky1 `WOR`-refresh quirk | P3 |
| Persistent boot log | Reserved RAM ring that survives warm reset; the loader saves it to `\EFI\ABYSS\log\` on the next boot | same | P1 |
| Broadcast / wake timer | HPET / LAPIC always-running | Platform timer when the generic timer stops in deep idle (Sky1: CIX GPT `CIXH1007`) | P8 (with deep idle) |
| Firmware mailbox | — | PCC (PCCT type 2/3) + SCMI (clocks, power domains, performance) | P3 (PCC for AML) / P8 (SCMI clients) |
| Interrupt controller | x2APIC/xAPIC, IOAPIC | GICv3 distributor/redistributor | P2 |
| MSI/MSI-X | LAPIC message routing | GICv3 ITS | P3 |
| Timers | TSC (invariant) + TSC-deadline, LAPIC timer, HPET (calibration) | Arm generic timer (virtual/physical) | P2 |
| SMP | INIT-SIPI | PSCI (HVC/SMC per FADT) | P2 |
| RTC | CMOS RTC (FADT century), UEFI `GetTime` | PL031 (if described), UEFI `GetTime` | P3 |
| Entropy | RDRAND/RDSEED | RNDR (FEAT_RNG), virtio-rng, SMCCC TRNG | P3 |
| Power | ACPI S5 (AML `_S5`), FADT reset register, power button (SCI/GPE) | PSCI off/reset, GED power button | P3 |
| UEFI runtime | `GetTime`, `GetVariable`/`SetVariable` (for `efivar`), `ResetSystem` | same | P3 |
| IOMMU | VT-d (DMAR), AMD-Vi (IVRS) | SMMUv3 (IORT) | P8 |
| CPU frequency / idle | ACPI `_CST`/`_PSS`/CPPC, MWAIT, HWP | PSCI idle, CPPC | P8 |

### Buses

| Bus | Phase |
|---|---|
| ACPI namespace (AML) | P3 |
| PCIe ECAM, bridges, capabilities, MSI/MSI-X | P3 |
| virtio-pci (modern, VIRTIO 1.x only) | P3 |
| USB xHCI + hubs, attached on **PCI and on ACPI** (`PNP0D10`, as on Sky1) | USB track (after P3, merges into P6) |

### Storage

| Driver | Phase |
|---|---|
| virtio-blk | P3 |
| NVMe | P3 |
| AHCI (SATA) | P3 |
| Memory disk (`md`, preloaded image) | P4 |
| USB mass storage (BBB/UAS) | USB track (P6) |
| virtio-scsi | Later |

### Network

| Driver | Phase |
|---|---|
| Loopback | P6 |
| virtio-net | P7 |
| Intel e1000e (I219/82574) | P7 |
| Intel igc (I225/I226) | P7 |
| **Realtek RTL8125/RTL8126** (`rge` family: the X870E's 2.5 GbE and the Orange Pi 6 Plus's 5 GbE, the only wired NIC on either reference machine) | P7 (polled transmit-only log path optional from P3) |
| Additional (ixgbe/ice, Realtek, Mellanox) | Depending on reference hardware |

### Human interface and display

| Driver | Phase |
|---|---|
| GOP framebuffer console (`vt` compatible) | P1 (text) / P6 (ttyv*) |
| USB HID keyboard/mouse | USB track (P6) |
| virtio-input | USB track (P6) |
| virtio-gpu (2D) | Graphics milestone |
| Native GPU (Intel Xe, AMD RDNA) | Graphics milestone |

## 4. Board profile: Orange Pi 6 Plus (CIX P1 / Sky1)

The ARM64 Tier 1 physical machine. The sibling project `../AbyssBSD` brought FreeBSD 16-CURRENT up
on it. Its board notes (`../AbyssBSD/docs/boards/orangepi-6-plus/README.md`) are our ground truth
for this hardware, and its Q8B `lessons.md` has the general bring-up hazards.

| Item | Fact | What it means for AbyssBSD |
|---|---|---|
| SoC | CIX P1 (CD8180, "Sky1"): 12 Armv9.2 cores, 4× A720 @ 2.6 GHz + 4× A720 @ 2.5 GHz + 4× A520; 32 GB | Three core classes, so the scheduler needs capacity-aware placement (PPTT + `_CPC`). Armv9.2 means PAC, BTI, MTE, and SVE are available |
| Firmware | UEFI, ACPI only (no DT): APIC (MADT), GTDT, IORT, MCFG, PCCT, PPTT, SDEI, DBG2, CSRT, DSDT + 2 SSDTs. **No SPCR** | Early console must fall back to **DBG2** (COM2, `ARMH0011` at `0x40d0000`). No board-specific settings ("nothing special") |
| Exception level | UEFI enters the kernel at **EL2**. VHE capable. No vendor hypervisor | The arm64 entry path must handle EL2: install a minimal EL2 stub and drop to EL1 for Day 1, leaving EL2 available for a future hypervisor |
| FP/SIMD | **SVE** implemented | `FpuState` must save SVE state, `AT_HWCAP`/`AT_HWCAP2` must advertise it correctly, and the SVE vector length must be restored after core power-down |
| Interrupts | GICv3 + ITS (GICv4.1), 512 SPIs. Arm erratum 2941627 applies (Linux works around it) | Errata framework in `arch/aarch64` from P2. Investigate 2941627 before relying on ITS under load |
| Timers | Generic timer at 1 GHz. GTDT has **no memory-mapped timer**. Core timers **stop in `_LPI` power-down states** | Deep idle needs a broadcast event timer: CIX GPT (`CIXH1007`, 25 MHz). Until then, idle uses WFI only |
| Idle / DVFS | `_LPI` (WFI, core off, cluster off) via PSCI. `_CPC` with SystemMemory desired-perf (SCMI fastchannel) and FFixedHW AMU counters | Generic ACPI `_LPI` and CPPC drivers (P8). The firmware leaves `_LPI`'s context-lost flag clear, so use the PSCI state-type bit |
| Clocks / power | Power domains as ACPI power resources (`_PR0`/`_PR3`). Clocks are **AML methods that call SCMI over PCC** | The AML interpreter must support **PCC operation regions** and power resources (P3) |
| AML quirks | DSDT references nonexistent `I2C0.UXC0`–`UXC3`. "Could not update all GPEs" | Unresolved references must be a logged, non-fatal error, never a panic |
| PCIe | ECAM (MCFG), 3 `PNP0A08` root ports | Generic ECAM host. No quirks known |
| Storage | NVMe (Micron 2550), behind SMMUv3. Relaxed ordering off | Works without the SMMU (firmware leaves bypass). SMMUv3 + IORT RMRs come in P8 |
| Network | 2× **RTL8126 5 GbE** on PCIe | The only wired NIC, so P7 needs an `rge`-class driver for the board's exit |
| USB | 10 xHCI hosts described in ACPI as `PNP0D10` (not PCI) | xHCI must attach via ACPI as well as PCI |
| Watchdog | SBSA generic watchdog (GTDT). Writes to the refresh frame **do not refresh** it; refresh through `WOR` | Bring this up early (P3): it turns hangs into resets instead of power cycles |
| Console | 10-pin 3.3 V debug header: UART2 pin 1 TX, pin 3 RX, pin 5 GND. GOP framebuffer 1920×1080 | A USB-TTL adapter is the primary bring-up console. The framebuffer console works from P1 |
| RTC | UEFI `GetTime` works. RX8900 on Cadence I²C | UEFI RTC is enough for Day 1 |
| Not Day 1 | Immortalis-G720 GPU (Panthor/CSF), Linlon display, HDA (ALC269VC), Zhouyi NPU, Mali-V codec, USB-C PD/DP alt mode | GPU and display belong to M-G. HDA to audio later |

**Access and workflow**

- Today the board runs Ubuntu (CIX kernel) and FreeBSD 16-CURRENT from its NVMe. Nothing touches
  it before P6. When the board's gating phase arrives (P6), **it is reimaged for AbyssBSD**. Before that, capture the ground truth (`acpidump` tables, `devinfo`,
  `pciconf`, Linux register dumps) into `tests/corpus/opi6p/`, because the reference OSes will be
  gone afterwards.
- Network: Ubuntu at `192.168.0.27` (user `jkane`, key login; root steps go through a script the
  user runs).
- **Read before experimenting.** Hangs cost power cycles. Every risky step starts from the Linux
  and FreeBSD ground truth (register dumps, ACPI tables), with the serial console attached and the
  watchdog armed once it works.

## 5. Board profile: AMD64 Ryzen 9 7900X / X870E AORUS ELITE WIFI7

The AMD64 Tier 1 physical machine. Surveyed read-only from its Fedora 44 install on 2026-10-17.

| Item | Fact | What it means for AbyssBSD |
|---|---|---|
| CPU | AMD Ryzen 9 7900X (Zen 4): 12 cores / 24 threads, 2 CCDs. AVX-512, FSGSBASE, INVPCID, SMEP, SMAP, UMIP, PKU, RDSEED, SHA, VAES, CPPC. **No LA57.** **No TSC-deadline** | 4-level paging. XSAVE state includes AVX-512. The LAPIC timer runs in one-shot mode, calibrated against HPET/TSC. Two CCDs with separate L3 caches, so topology (CPUID leaf `0x8000001E`/`0x80000026`, PPTT absent) matters for scheduling |
| Memory | 96 GB DDR5 | — |
| Firmware | AMI UEFI F2 (2024-08-14), 64-bit. **Secure Boot disabled, setup mode**. Boot entry `Boot0001` Fedora only | Our unsigned loader boots. AbyssBSD adds its own boot entry and never edits Fedora's |
| ACPI | APIC (MADT), FACP, FACS, DSDT, **15 SSDTs**, HPET, MCFG, IVRS, BGRT, TPM2, VFCT, WDRT, WSMT, FPDT, FIDT. **No SPCR** | A large AML corpus from AMD and AMI firmware. WDRT gives the watchdog. IVRS describes AMD-Vi for P8 |
| Interrupts | Firmware leaves the APIC in **xAPIC** mode (CPUID shows no x2APIC). IOAPIC. MSI/MSI-X | The APIC driver must support xAPIC MMIO as well as x2APIC MSRs |
| Timers | Invariant TSC (`constant_tsc`, `nonstop_tsc`), HPET, LAPIC | TSC is the timecounter. HPET is used for calibration |
| CPU frequency | `amd-pstate-epp` under Linux (ACPI CPPC) | Generic CPPC driver (P8), shared with the Orange Pi 6 Plus |
| Display | Boot VGA (UEFI GOP) is the **NVIDIA RTX 4070** (`10de:2786`). The iGPU (Radeon 610M) is also present | The GOP framebuffer console is the primary bring-up console. Native GPU drivers belong to M-G |
| Storage | NVMe 0: Samsung 980 PRO 2 TB, **Fedora system disk**. **Second NVMe: AbyssBSD media** (to be installed). AMD 600-series AHCI ×2 | AbyssBSD owns only the second NVMe. AHCI is exercised by the P3 driver |
| Data disks | **2× WD 8 TB (Linux RAID 0, `md0`) and 1× HGST 6 TB (`md1` member), user data** | Never written. The storage stack attaches them read-only until P8, and `xtask deploy` refuses any target other than the designated media NVMe |
| Network | **Realtek RTL8125 2.5 GbE** (`10ec:8125`), MediaTek MT7925 Wi-Fi 7 | `rge`-family driver (P7), shared with the RTL8126 on the Orange Pi 6 Plus. Wi-Fi is out of scope for Day 1 |
| USB | AMD 800-series chipset xHCI ×2, CPU xHCI ×3, ASMedia ASM4242 USB4 host router + xHCI | xHCI on PCI. USB4/Thunderbolt tunneling is out of scope for Day 1 |
| Serial | No SPCR. ACPI lists FCH UARTs (`AMDI0020` ×5) and Super I/O ports (`PNP0500` ×4), but no rear-panel COM port is known | **No serial console.** See the console strategy below |
| Other | TPM 2.0, AMD PSP/CCP, HD Audio ×3, I²C (`AMDI0010`), GPIO (`AMDI0030`) | Not Day 1 |

**Console strategy without serial.**

1. **GOP framebuffer console** from P1 (on the RTX 4070's output).
2. **Persistent log ring** in a loader-reserved RAM region that survives a warm reset. On the next
   boot the loader saves the previous boot's log to `\EFI\ABYSS\log\` on the media NVMe's ESP, where
   Linux (or `xtask`) can read it.
3. **AMD FCH watchdog** (from WDRT) in P3, so hangs become warm resets and the log survives.
4. Optional: a polled transmit-only UDP log over the RTL8125 (shared code with the P7 driver), or
   a PCIe 16550 serial card that the loader finds through UEFI PCI I/O and passes to the kernel.
   The serial card is a cheap hardware addition worth considering.
5. USB keyboard input arrives with the USB track (needed for interactive login by P6).

**Access and workflow.**

- Fedora 44 at `192.168.0.30` (user `jkane`, key login; `sudo` requires the user's password, so
  root steps go through a script the user runs).
- Fedora stays as the Linux ground truth (ACPI dumps, `lspci`, register state).
- AbyssBSD is installed onto the **second NVMe**, with its own ESP and its own UEFI boot entry,
  selected with `BootNext` (`efibootmgr -n`) from Fedora so a failed boot falls back to Fedora on
  reset.
