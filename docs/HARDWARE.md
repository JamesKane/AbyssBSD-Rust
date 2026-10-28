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

| Tier | AMD64 | ARM64 | RV64 (Day 2, P10) |
|---|---|---|---|
| **Tier 1, virtual** (CI gate) | QEMU q35 + OVMF; bhyve UEFI | QEMU virt (gicv3, acpi=on) + edk2 | QEMU virt (aia=aplic-imsic, acpi=on) + edk2 |
| **Tier 1, physical** (untouched until P6's exit, gating from P7; RV64: untouched until P10's QEMU exit, gating from then) | **Ryzen 9 7900X on Gigabyte X870E AORUS ELITE WIFI7**, see [§5](#5-board-profile-amd64-ryzen-9-7900x--x870e-aorus-elite-wifi7) and [ADR-0009](adr/0009-amd64-reference-machine-ryzen-x870e.md) | **Orange Pi 6 Plus** (CIX P1 / Sky1), see [§4](#4-board-profile-orange-pi-6-plus-cix-p1--sky1) and [ADR-0008](adr/0008-arm64-reference-board-orange-pi-6-plus.md) | **Banana Pi BPI-SM10** (SpacemiT K3), see [§6](#6-board-profile-rv64-banana-pi-bpi-sm10-spacemit-k3) and [ADR-0017](adr/0017-rv64-reference-board-bpi-sm10.md). Tier 1 only once it boots UEFI with ACPI |
| **Tier 2, best effort** | Other UEFI desktops/laptops | Radxa Orion O6 (same SoC), **Radxa Dragon Q8B** (see below), other SystemReady boards, Raspberry Pi 4/5 with community UEFI+ACPI firmware | Other UEFI+ACPI (RVI BRS) boards |

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

## 3. Driver matrix

Legend:

- **Status:**
  - **Supported:** Implemented in the tree and passing CI on the virtual Tier 1 platforms (QEMU `q35`, QEMU `virt`, bhyve).
  - **Experimental:** Implemented in the tree (and unit-tested or exercised in QEMU where emulated), awaiting physical-board bring-up and bare-metal verification in **P7** ([ADR-0010](adr/0010-qemu-first-development.md)).
  - **Planned:** Scheduled for a later phase or covered by a clean-room hardware specification in `~/Projects/OS/chinese-wall/specs/`.
- **Phase:** Roadmap phase in which the driver or subsystem is first expected to work.

### Platform and core

| Function / Subsystem | AMD64 | ARM64 | RV64 (Day 2) | Crate / Spec | Status | Phase |
|---|---|---|---|---|---|---|
| Early console & serial terminals | 16550 UART (`uart`, `ttyu*`/`cuau*`, SPCR / COM1) | PL011 / SBSA generic UART (`uart`, SPCR, else **DBG2**) | 16550 UART (32-bit stride, SPCR) | `drivers/uart-16550`, `drivers/uart-pl011`, `drivers/uart` | **Supported** (AMD64/ARM64); **Planned** (RV64) | P1 / P6 / P10 |
| Framebuffer console (`vt`) | GOP framebuffer text (`fb-console`, P1) and virtual terminals (`ttyv0`–`ttyvb`, Track U) | same | same | `drivers/fb-console`, `sys/vt` | **Supported** (AMD64/ARM64); **Planned** (RV64) | P1 / P6 / P10 |
| Persistent boot log | Reserved RAM ring that survives warm reset; loader saves it to `\EFI\ABYSS\log\` on next boot | same | same | `lib/logring`, `boot/loader-efi` | **Supported** | P1 |
| Interrupt controller | x2APIC / xAPIC, I/O APIC | GICv3 distributor & redistributor | AIA (APLIC MSI mode + S-level IMSIC) | `arch/x86_64`, `arch/aarch64`, `sys/irq` | **Supported** (AMD64/ARM64); **Planned** (RV64) | P2 / P10 |
| MSI / MSI-X | LAPIC message routing | GICv3 ITS | AIA IMSIC | `sys/irq`, `sys/pci`, `arch/aarch64` | **Supported** (AMD64/ARM64); **Planned** (RV64) | P3 / P10 |
| Core timers | TSC (invariant) calibrated against ACPI PM timer (`acpi_timer`), LAPIC timer, 8254 (`attimer`), HPET (`hpet`) | Arm generic timer (virtual/physical) | SBI timer (ACLINT behind firmware, `Sstc`) | `arch/x86_64`, `arch/aarch64`, `drivers/timers` | **Supported** (AMD64/ARM64); **Planned** (RV64) | P2 / P3 / P10 |
| Broadcast / wake timer | HPET / LAPIC always-running | Platform timer when generic timer stops in deep idle (Sky1: CIX GPT `CIXH1007`) | — | `specs/sky1-soc` (`03-gpt-timer.md`) | **Planned** | P9 |
| SMP bring-up | INIT-SIPI | PSCI (HVC/SMC per FADT) | SBI HSM | `arch/x86_64`, `arch/aarch64` | **Supported** (AMD64/ARM64); **Planned** (RV64) | P2 / P10 |
| Real-time clock (RTC) | CMOS RTC (`atrtc`, `PNP0B00`, FADT century), UEFI `GetTime` | UEFI `GetTime`; PL031 / I²C RTC (RX8900) later | UEFI `GetTime`; SpacemiT MMP RTC / RPMI | `drivers/atrtc`, `sys/clock` | **Supported** (`atrtc`, UEFI `GetTime`); **Planned** (SoC/I²C RTCs) | P3 / P10 |
| Watchdog | Intel ICH TCO (`ichwd`); AMD FCH watchdog (`amdsbwd`) | SBSA generic watchdog (`sbsa_gwdt`, GTDT, with Sky1 `WOR`-refresh quirk) | SpacemiT K3/MMP watchdog (`0xD4014000`) | `drivers/ichwd` (**Supported**); `drivers/amdsbwd`, `drivers/sbsa-gwdt` (**Experimental**); `specs/spacemit-k3` (**Planned**) | **Supported** (`ichwd`) / **Experimental** (`amdsbwd`, `sbsa_gwdt`) / **Planned** (RV64) | P3 / P7 / P10 |
| Hardware entropy | `RDRAND` / `RDSEED`, `virtio-random` (`vtrnd`) | `RNDR` (`FEAT_RNG`), `virtio-random` (`vtrnd`); SMCCC TRNG later | `Zkr` (`seed`) / `virtio-random` | `drivers/virtio-random`, `sys/random` | **Supported** | P3 |
| System power & events | ACPI S5 (AML `_S5`), FADT reset register, power button (`acpi_button`, SCI/GPE) | PSCI off/reset, ACPI GED (`acpi_ged`) & power button | SBI SRST, ACPI / RPMI power key | `sys/acpi` | **Supported** (AMD64/ARM64); **Planned** (RV64) | P3 / P10 |
| UEFI runtime services | `GetTime`, `GetVariable`/`SetVariable`, `ResetSystem` | same | same | `boot/efi`, `sys/clock` | **Supported** (`GetTime`, `ResetSystem`); **Planned** (`efivar` userland wiring) | P3 |
| Firmware mailbox & transport | — | PCC (PCCT type 2/3 in AML) + SCMI (clocks, resets, power domains, perf) | RPMI over SBI MPXY | `lib/aml` (PCC region: **Experimental**); `specs/sky1-soc`, `specs/spacemit-k3` (SCMI/RPMI providers: **Planned**) | **Experimental** (AML PCC) / **Planned** (SCMI & RPMI providers) | P3 / P7 / P9 / P10 |
| IOMMU | Intel VT-d (DMAR), AMD-Vi (IVRS) | Arm SMMUv3 (IORT), Arm SMMUv2 (SC8280XP) | RISC-V IOMMU (`0xC0F00000`) | `specs/adreno-a690`, `specs/qcom-sc8280xp` | **Planned** | P9 / Later |
| CPU frequency & idle | ACPI `_CST`/`_PSS`/CPPC (`_CPC`), `MWAIT`, HWP | PSCI `_LPI` idle, ACPI CPPC (`_CPC` with SCMI fastchannel & AMU), Qualcomm EPSS (`QCOM0617`) | ACPI / SBI HSM idle & CPPC | `sys/acpi/src/cpc.rs` (table parsing: **Experimental**); `specs/qcom-sc8280xp` (drivers: **Planned**) | **Planned** | P9 |

### Buses and interconnects

| Bus / Controller | Description / Attachment | Crate / Spec | Status | Phase |
|---|---|---|---|---|
| ACPI namespace (`acpi`) | AML interpreter (`lib/aml`), namespace enumeration, `_STA`/`_CRS`/`_PRT`/`_DEP`, host-PCI bridges (`pcib`), power resources | `sys/acpi`, `lib/aml` | **Supported** (QEMU/bhyve & Q8B DSDT corpus); **Experimental** (physical board AML) | P3 / P7 |
| PCIe (`pci`, `pcib`) | ECAM (MCFG), PCI-PCI bridges, capability parsing, BAR allocation, MSI/MSI-X | `sys/pci`, `sys/acpi` | **Supported** | P3 |
| ISA / LPC bridge (`isab`, `isa`) | PCI-ISA (LPC) bridge and ISA child bus for chipset peripherals (`ichwd`, `amdsbwd`) | `drivers/isa` | **Supported** | P3 |
| VirtIO PCI (`virtio_pci`) | Modern (`VIRTIO 1.x`) and transitional PCI transport, virtqueues, MSI-X / INTx | `sys/virtio` | **Supported** | P3 |
| USB xHCI (`xhci`) & USB bus (`usbus`, `uhub`) | xHCI 1.2 host controller on PCI (class `0c0330`) and ACPI (`PNP0D10`, Sky1), MSI-X, bulk streams, USB 2.0/3.0 root and external hubs | `drivers/xhci`, `sys/usb`, `lib/xhci`, `lib/usb` | **Supported** (PCI in QEMU); **Experimental** (ACPI `PNP0D10` & physical controllers) | Track U (P6) / P7 |
| HID bus (`usbhid`, `hidbus`) | USB HID transport and generic HID report descriptor parser | `sys/hid`, `lib/hid` | **Supported** | Track U (P6) |
| USB4 / Thunderbolt 3 Connection Manager | Software Connection Manager for ASMedia ASM4242 (`174c:2428`, PCI class `0c0340`) on X870E: NHI rings, router enumeration, TMU, CLx, PCIe/USB3/DP tunneling | `specs/usb4-cm` | **Planned** | Later |
| Synopsys DWC3 & USB PHYs (SoC glue) | SpacemiT K3 USB2 PHY, USB3/PCIe combo PHY, and DWC3 host/dual-role integration; Qualcomm SC8280XP DWC3 RX threshold quirk | `specs/spacemit-k3` (`07`, `08`), `specs/qcom-sc8280xp` (`01`) | **Planned** | P7 / P10 / Later |
| SpacemiT K3 PCIe (`k3-pcie`) | DesignWare PCIe Gen3 controller and combo PHY (if not fully initialized by UEFI/ECAM) | `specs/spacemit-k3` (`13-pcie.md`) | **Planned** | P10 |

### SoC infrastructure, low-speed peripherals, and hardware monitoring

Drivers planned from the clean-room specifications in `~/Projects/OS/chinese-wall/specs/` for the Tier 1 and Tier 2 boards:

| Device / Subsystem | Target Platform | Description | Spec / Crate | Status | Phase |
|---|---|---|---|---|---|
| CIX Sky1 SoC infrastructure | Orange Pi 6 Plus / Radxa Orion O6 (Sky1) | CIX mailbox, SCMI clock (`CLKT`) & power-domain providers, S5 reset controllers (`RSTL`/`RSNL`/`DLKL`), subsystem RCSUs, and CIX GPT (`CIXH1007`) | `specs/sky1-soc` | **Planned** | P7 / P9 |
| CIX Sky1 GPIO, I²C, SPI/XSPI & pinctrl | Orange Pi 6 Plus / Radxa Orion O6 (Sky1) | Cadence GPIO r1p02 (`CIXH1002`, `CIXH1003`), Cadence I²C r1p14, Cadence SPI r1p6 & XSPI flash, and pad/pin controllers (`CIXH1001`) | `specs/sky1-peripherals` (`01`–`03`, `05`) | **Planned** | P7 |
| Realtek RTS5453H USB-C PD controller | Orange Pi 6 Plus / Radxa Orion O6 (Sky1) | I²C USB Type-C Power Delivery controller: port status, data/power role, and DisplayPort Alt-Mode HPD/pin-assignment reporting to the combo PHY and DP-TX | `specs/sky1-peripherals` (`04-usb-pd-rts5453.md`) | **Planned** | M-G / Later |
| ITE IT8689E / IT87952E Super I/O & Gigabyte WMI | Ryzen 9 7900X / Gigabyte X870E AORUS ELITE WIFI7 | LPC Super I/O Environment Controller (`0x8689` / `0x8695` / `0x8696`): 16-bit fan tachometers, 6-channel PWM fan control, voltages, PECI/AMD-TSI temperatures, SMBus interlock, and Gigabyte WMI temperature sensor interface | `specs/ite-it87-superio` | **Planned** | Later |
| SpacemiT K3 clocks, resets, pinctrl, GPIO, I²C, SPI, PWM, PDMA, PMIC & thermal | Banana Pi BPI-SM10 (SpacemiT K3) | APBS/MPMU/APBC/APMU/DCIU clocks & resets, pinctrl/GPIO, TWSI I²C, QSPI, PWM, peripheral DMA, P1 PMIC, power domains, and thermal sensors (fallback where not owned by UEFI/ACPI + RPMI) | `specs/spacemit-k3` (`01`, `02`, `04`, `05`, `09`–`11`, `14`, `15`) | **Planned** | P10 / Later |
| Qualcomm SC8280XP core SoC & PEP replacements | Radxa Dragon Q8B (SC8280XP, Tier 2) | QUPv3 GENI UART & I²C, TLMM ACPI GPIO, TrustZone SCM & PAS loader, Command DB, RPMh RSC DRV2 power/clock voting, EPSS CPU DVFS, and TSENS v2 thermal sensors (replacing Windows `PEP0` / `QCOM0617`) | `specs/qcom-sc8280xp` (`01`–`03`) | **Planned** | Tier 2 (P7+) |
| Qualcomm SC8280XP GLINK fabric & PMIC USB-C | Radxa Dragon Q8B (SC8280XP, Tier 2) | TCSR hardware mutex, SMEM v12, IPCC, GLINK native protocol, QRTR, and PMIC GLINK Type-C / DisplayPort Alt-Mode notifications | `specs/qcom-sc8280xp` (`04-smem-ipcc-glink-usbc.md`) | **Planned** | Tier 2 (M-G / Later) |

### Storage

| Driver | Hardware / Interface | Crate / Spec | Status | Phase |
|---|---|---|---|---|
| `vtblk` (`vtbd*`) | VirtIO block device (modern and transitional) | `drivers/virtio-blk` | **Supported** | P3 |
| `nvme` (`nda*`) | PCIe NVM Express 2.0 controllers (admin & per-CPU I/O queues, MSI-X, PRPs) | `drivers/nvme` | **Supported** (QEMU/bhyve); **Experimental** (bare-metal NVMe on X870E & Orange Pi 6 Plus) | P3 / P7 |
| `ahci` / `ahcich` (`ada*`) | SATA AHCI 1.3.1 controllers (NCQ, DMA, MSI-X/MSI/INTx) | `drivers/ahci` | **Supported** (QEMU/bhyve); **Experimental** (AMD 600-series AHCI on X870E) | P3 / P7 |
| `md` (`md*`) | Memory disks (malloc and swap-backed) | `kernel` (`sys/storage`) | **Supported** | P4 / P5 |
| `umass` (`da*`) | USB mass storage over Bulk-Only Transport (BBB) and USB Attached SCSI (UAS) | `drivers/umass`, `lib/scsi` | **Supported** (QEMU); **Experimental** (physical USB media in P7) | Track U (P6) / P7 |
| `sdhci_msm` / `k3-sdhci` | Qualcomm SC8280XP SDHCI-MSM v5 and SpacemiT K3 SD/MMC host controller | `specs/qcom-sc8280xp` (`01`), `specs/spacemit-k3` (`03-sdhci.md`) | **Planned** | P10 / Later |
| `k3-ufs` | SpacemiT K3 UFS host controller (128 GB onboard UFS module) | `specs/spacemit-k3` (`17-ufs.md`) | **Planned** | Later |
| `virtio-scsi` | VirtIO SCSI host controller | — | **Planned** | Later |

### Network

| Driver | Hardware / Interface | Crate / Spec | Status | Phase |
|---|---|---|---|---|
| `lo` | Loopback network interface | `sys/socket` / net stack | **Planned** | P6 / P8 |
| `vtnet` | VirtIO network device (multi-queue, checksum & TSO offloads) | — | **Planned** | P8 |
| `em` (`e1000e`) | Intel I219 / 82574 1 GbE PCIe controllers | — | **Planned** | P8 |
| `igc` | Intel I225 / I226 2.5 GbE PCIe controllers | — | **Planned** | P8 |
| `rge` | **Realtek RTL8125 (2.5 GbE) & RTL8126 (5 GbE)** PCIe controllers (onboard NICs on the X870E and Orange Pi 6 Plus: multi-queue RSS, MSI-X, TSO, EEE, PHY microcode patching) | `specs/realtek-rtl8125-delta` | **Planned** | P8 |
| `dwc_eqos` (`dwmac`) | Synopsys DesignWare EQoS 1 GbE MAC (`0xCAC82000`) with MDIO child bus and Realtek RTL8211F RGMII PHY (onboard NIC on the Banana Pi BPI-SM10) | `specs/spacemit-k3` (`06-dwmac.md`) | **Planned** | P8 / P10 |
| `tc956x` | **Toshiba TC9563** PCIe Gen3 switch + dual Synopsys DesignWare XGMAC2 v3.01a 2.5 GbE MACs, XPCS, SerDes PMA, and QCA8081 PHYs (onboard NICs on the Radxa Dragon Q8B) | `specs/tc956x` | **Planned** | Tier 2 (P8+) |

### Human interface (input)

| Driver | Hardware / Interface | Crate / Spec | Status | Phase |
|---|---|---|---|---|
| `hkbd` | USB HID keyboards (`kbd` under `kbdmux`, LED control, key repeat, `evdev`) | `drivers/hkbd`, `sys/input`, `sys/evdev` | **Supported** | Track U (P6) |
| `hms` | USB HID mice and absolute pointing devices / tablets (`evdev` `/dev/input/event*` and `/dev/sysmouse`) | `drivers/hms`, `sys/evdev` | **Supported** | Track U (P6) |
| `vtinput` | VirtIO input devices: keyboards, mice, and tablets (`virtio 1.2 §5.8`, `kbdmux` and `evdev`) | `drivers/virtio-input`, `sys/evdev` | **Supported** | Track U (P6) |

### Graphics and display (M-G)

See [docs/GRAPHICS.md](GRAPHICS.md) for the DRM/KMS architecture and step order.

| Driver | Hardware / Target | Description | Spec / Crate | Status | Phase |
|---|---|---|---|---|---|
| `gop-kms` | All UEFI platforms (QEMU, bhyve, X870E, Orange Pi 6 Plus, BPI-SM10) | Simple atomic KMS driver over the UEFI GOP boot framebuffer (`/dev/dri/card0`) with seamless handover to native display drivers | [GRAPHICS.md §7.5](GRAPHICS.md#75-firmware-handover) | **Planned** | M-G.1 |
| `virtio-gpu` | QEMU (`x86_64`, `aarch64`, `riscv64`) | VirtIO GPU 2D KMS and 3D (Venus / virglrenderer) with host-visible blob memory, fences, and `dma-buf` | [GRAPHICS.md §9](GRAPHICS.md#9-drivers-and-order) | **Planned** | M-G.2 / M-G.3 |
| `linlondp` + `sky1-dp-tx` | Orange Pi 6 Plus / Radxa Orion O6 (CIX Sky1) | Arm China **Linlon-D6** (Mali-D71 / Komeda family) DPU (`CIXH5010`) + 5× **Trilinear DPTX** DisplayPort 1.4 transmitters (`CIXH502F`), USB-C/DP combo PHY, eDP panel (`CIXH5040`), and backlight (`CIXH5041`) | `specs/linlon-dpu`, `specs/sky1-dp-tx` | **Planned** | M-G.4 |
| `panthor` | Orange Pi 6 Plus / Radxa Orion O6 (CIX Sky1) & RK3588 | Arm **Immortalis-G720** (and Mali-G610) CSF GPU (`CIXH5000`): MCU firmware loader, Command Stream Frontend (CSF) scheduler, GPU MMU (`IoAddressSpace`), tiler heap, and DRM render node for Mesa `panvk` | `specs/panthor` | **Planned** | M-G.5 |
| `amdgpu` / native AMD64 GPU | Ryzen 9 7900X / X870E | AMD Radeon 610M (RDNA2 iGPU) or NVIDIA RTX 4070 (Ada, boot GOP GPU) native DRM/KMS and render driver | [GRAPHICS.md §9](GRAPHICS.md#9-drivers-and-order) | **Planned** | M-G.6 |
| `msm` (MDSS DPU1 + DP) | Radxa Dragon Q8B (Qualcomm SC8280XP, Tier 2) | Dual-MDSS v8.1 / DPU1 display processor (`0x0ae00000`), 4× DP/eDP controllers, `DISP_CC_0`, and QMP V5 5nm USB4/USB3/DP combo PHYs | `specs/qcom-mdss-dpu` | **Planned** | Tier 2 (M-G) |
| `msm` (Adreno 690) | Radxa Dragon Q8B (Qualcomm SC8280XP, Tier 2) | Qualcomm **Adreno 690** 3D/compute GPU (`QCOM0636` / `0x03d00000`), Cortex-M3 GMU (`a660_gmu.bin`), HFI DVFS, GPUCC, per-process SMMUv2 (`0x03da0000`), and TrustZone PAS 13 Zap shader for Mesa Turnip | `specs/adreno-a690` | **Planned** | Tier 2 (M-G) |
| `k3-display` & PowerVR `pvr` | Banana Pi BPI-SM10 (SpacemiT K3) | SpacemiT K3 DPU (DisplayPort 1.2 / MIPI DSI) and Imagination **PowerVR BXM-4-64** Rogue GPU (`rogue_36.56.104.183_v1.fw`) | `specs/spacemit-k3` (`16-display.md`) | **Planned** | Later |

### Video codecs, NPUs, and audio (Post-Day 1)

| Driver | Hardware / Target | Description | Spec / Crate | Status | Phase |
|---|---|---|---|---|---|
| `linlon-vpu` | Orange Pi 6 Plus / Radxa Orion O6 (CIX Sky1) & SpacemiT K3 | Arm China **Linlon v52_v76** (Mali-V lineage, `CIXH3010`) multi-core stateful video decoder/encoder: session MMU, firmware RPC queues, and V4L2 M2M ABI | `specs/linlon-vpu`, `specs/spacemit-k3` (`18-vpu.md`) | **Planned** | Later |
| `zhouyi-npu` | Orange Pi 6 Plus / Radxa Orion O6 (CIX Sky1) | Arm China **Zhouyi X2 (v3)** NPU (`CIXH4000` at `0x14260000`): TSM, command pools, TCB chains, and `/dev/aipu` ABI | `specs/zhouyi-npu` | **Planned** | Later |
| `sky1-audio` | Orange Pi 6 Plus / Radxa Orion O6 (CIX Sky1) | Audio clock/reset unit (`CIXHA018`), IPbloq HD-Audio controller (`CIXH6020`, ALC269VC codec), 5× DP multi-channel I²S controllers, Arm DMA-350, and HiFi5 DSP | `specs/sky1-audio` | **Planned** | M-D / Later |
| `qcom-iris-vpu` | Radxa Dragon Q8B (Qualcomm SC8280XP, Tier 2) | Qualcomm **Iris / Venus VPU 2.0** (`0x0aa00000`): PAS 9 firmware boot, `videocc` Lucid 5LPE PLL & GDSCs, HFI Gen2 queues, and V4L2 M2M stateful H.264/HEVC/VP9 codec | `specs/qcom-iris-vpu` | **Planned** | Tier 2 (Later) |
| `qcom-fastrpc` | Radxa Dragon Q8B (Qualcomm SC8280XP, Tier 2) | Qualcomm **Hexagon CDSP / NSP0** (v68 HVX + HMX): PAS 18 boot, GLINK RPC transport, 14 Apps SMMU context banks, and `/dev/fastrpc-cdsp*` ABI | `specs/qcom-fastrpc` | **Planned** | Tier 2 (Later) |
| `qcom-audioreach` | Radxa Dragon Q8B (Qualcomm SC8280XP, Tier 2) | Qualcomm LPASS AudioReach (GPR, APM/PRM over ADSP GLINK), SoundWire controllers, and WCD9385 codec | `specs/qcom-sc8280xp` (`05-audioreach-soundwire-wcd9385.md`) | **Planned** | Tier 2 (Later) |
| `spacemit-i2s` | Banana Pi BPI-SM10 (SpacemiT K3) | SpacemiT K3 I²S audio controller | `specs/spacemit-k3` (`12-i2s.md`) | **Planned** | Later |

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
| Idle / DVFS | `_LPI` (WFI, core off, cluster off) via PSCI. `_CPC` with SystemMemory desired-perf (SCMI fastchannel) and FFixedHW AMU counters | Generic ACPI `_LPI` and CPPC drivers (P9). The firmware leaves `_LPI`'s context-lost flag clear, so use the PSCI state-type bit |
| Clocks / power | Power domains as ACPI power resources (`_PR0`/`_PR3`). Clocks are **AML methods that call SCMI over PCC** | The AML interpreter must support **PCC operation regions** and power resources (P3) |
| AML quirks | DSDT references nonexistent `I2C0.UXC0`–`UXC3`. "Could not update all GPEs" | Unresolved references must be a logged, non-fatal error, never a panic |
| PCIe | ECAM (MCFG), 3 `PNP0A08` root ports | Generic ECAM host. No quirks known |
| Storage | NVMe (Micron 2550), behind SMMUv3. Relaxed ordering off | Works without the SMMU (firmware leaves bypass). SMMUv3 + IORT RMRs come in P9 |
| Network | 2× **RTL8126 5 GbE** on PCIe | The only wired NIC, so P8 needs an `rge`-class driver for the board's exit |
| USB | 10 xHCI hosts described in ACPI as `PNP0D10` (not PCI) | xHCI must attach via ACPI as well as PCI |
| Watchdog | SBSA generic watchdog (GTDT). Writes to the refresh frame **do not refresh** it; refresh through `WOR` | Bring this up early (P3): it turns hangs into resets instead of power cycles |
| Console | 10-pin 3.3 V debug header: UART2 pin 1 TX, pin 3 RX, pin 5 GND. GOP framebuffer 1920×1080 | A USB-TTL adapter is the primary bring-up console. The framebuffer console works from P1 |
| RTC | UEFI `GetTime` works. RX8900 on Cadence I²C | UEFI RTC is enough for Day 1 |
| Not Day 1 | Immortalis-G720 GPU (Panthor/CSF), Linlon display, HDA (ALC269VC), Zhouyi NPU, Mali-V codec, USB-C PD/DP alt mode | GPU and display belong to M-G. HDA to audio later |

**Access and workflow**

- Today the board runs Ubuntu (CIX kernel) and FreeBSD 16-CURRENT from its NVMe. Nothing touches
  it before P6's exit. When the board's gating phase arrives (P7), **it is reimaged for AbyssBSD**. Before that, capture the ground truth (`acpidump` tables, `devinfo`,
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
| ACPI | APIC (MADT), FACP, FACS, DSDT, **15 SSDTs**, HPET, MCFG, IVRS, BGRT, TPM2, VFCT, WDRT, WSMT, FPDT, FIDT. **No SPCR** | A large AML corpus from AMD and AMI firmware. WDRT gives the watchdog. IVRS describes AMD-Vi for P9 |
| Interrupts | Firmware leaves the APIC in **xAPIC** mode (CPUID shows no x2APIC). IOAPIC. MSI/MSI-X | The APIC driver must support xAPIC MMIO as well as x2APIC MSRs |
| Timers | Invariant TSC (`constant_tsc`, `nonstop_tsc`), HPET, LAPIC | TSC is the timecounter. HPET is used for calibration |
| CPU frequency | `amd-pstate-epp` under Linux (ACPI CPPC) | Generic CPPC driver (P9), shared with the Orange Pi 6 Plus |
| Display | Boot VGA (UEFI GOP) is the **NVIDIA RTX 4070** (`10de:2786`). The iGPU (Radeon 610M) is also present | The GOP framebuffer console is the primary bring-up console. Native GPU drivers belong to M-G |
| Storage | NVMe 0: Samsung 980 PRO 2 TB, **Fedora system disk**. **Second NVMe: AbyssBSD media** (to be installed). AMD 600-series AHCI ×2 | AbyssBSD owns only the second NVMe. AHCI is exercised by the P3 driver |
| Data disks | **2× WD 8 TB (Linux RAID 0, `md0`) and 1× HGST 6 TB (`md1` member), user data** | Never written. The storage stack attaches them read-only until P9, and `xtask deploy` refuses any target other than the designated media NVMe |
| Network | **Realtek RTL8125 2.5 GbE** (`10ec:8125`), MediaTek MT7925 Wi-Fi 7 | `rge`-family driver (P8), shared with the RTL8126 on the Orange Pi 6 Plus. Wi-Fi is out of scope for Day 1 |
| USB | AMD 800-series chipset xHCI ×2, CPU xHCI ×3, ASMedia ASM4242 USB4 host router + xHCI | xHCI on PCI. USB4/Thunderbolt tunneling is out of scope for Day 1 |
| Serial | No SPCR. ACPI lists FCH UARTs (`AMDI0020` ×5) and Super I/O ports (`PNP0500` ×4), but no rear-panel COM port is known | **No serial console.** See the console strategy below |
| Other | TPM 2.0, AMD PSP/CCP, HD Audio ×3, I²C (`AMDI0010`), GPIO (`AMDI0030`) | Not Day 1 |

**Console strategy without serial.**

1. **GOP framebuffer console** from P1 (on the RTX 4070's output).
2. **Persistent log ring** in a loader-reserved RAM region that survives a warm reset. On the next
   boot the loader saves the previous boot's log to `\EFI\ABYSS\log\` on the media NVMe's ESP, where
   Linux (or `xtask`) can read it.
3. **AMD FCH watchdog** (from WDRT) in P3, so hangs become warm resets and the log survives.
4. Optional: a polled transmit-only UDP log over the RTL8125 (shared code with the P8 driver), or
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

## 6. Board profile: RV64 Banana Pi BPI-SM10 (SpacemiT K3)

The RV64 Tier 1 physical machine ([ADR-0017](adr/0017-rv64-reference-board-bpi-sm10.md)), on
order as of 2026-10-27. Facts come from the clean-room specification
(`~/Projects/OS/chinese-wall/specs/spacemit-k3/README.md`), not yet from the board. Nothing here
has been checked on hardware.

| Item | Fact | What it means for AbyssBSD |
|---|---|---|
| Board | SpacemiT **K3-CoM260** module on the "K3-COM260 KIT" carrier (schematic V2.0, March 2026). Module EEPROM `product_name` `k3_com260_ifx` | Record the carrier revision on arrival: V1.0 swapped the console's TX and RX |
| CPU | Harts 0–7: **X100**, RV64 with V (256-bit vectors), H, and AIA (`smaia`/`ssaia`), two four-core clusters. Harts 8–15: **A100** AI cores, 1024-bit vectors, no H | Use harts 0–7 only. Vector state is sized per hart class if the A100s are ever used |
| Memory | 8, 16, or 32 GB LPDDR5 depending on the module, at `0x1_0000_0000` | Take the size from the UEFI memory map, never from a constant |
| Firmware (shipped) | Boot ROM → SPL → OpenSBI (SBI v2.0, RPMI services) → U-Boot with a devicetree, in an 8 MiB SPI NOR. **No UEFI firmware.** A recovery (flashing) pin on the carrier's button header | Not usable as shipped ([ADR-0006](adr/0006-uefi-acpi-only-own-loader-and-acpi.md)). Dump the SPI NOR and save the U-Boot environment before changing anything |
| Firmware (needed) | SpacemiT's K3 edk2 port (`spacemit-com/edk2-platforms`, branch `k3`): UEFI with ACPI (DSDT, MADT, RHCT, MCFG, PPTT, SPCR) | How it reaches the board (SPI NOR, or a payload from SD) is a P10 decision. Its ACPI tables join the corpus as `tests/corpus/acpi/bpi-sm10/` |
| Interrupts | Supervisor APLIC (`0xE0804000`, 512 sources) delivering **only in MSI mode** through the S-level IMSIC (`0xE0400000`) | APLIC MSI mode plus IMSIC, from the AIA specification. FreeBSD's APLIC driver supports only direct mode |
| Timer, IPI, SMP, reset | SBI (timer, IPI, HSM, SRST), ACLINT behind firmware | The standard SBI path, as on QEMU `virt` |
| Firmware services | RPMI over SBI MPXY: some regulators, the RTC, the power key | On ACPI, firmware owns clocks, resets, pads, and power (ADR-0017). An RPMI provider backs `PowerDomain` and friends (DRIVER-MODEL.md §9.3) |
| Console | UART0 (`0xD4017000`), ns16550-compatible with a 32-bit register stride, 115200 8N1, 3.3 V on the carrier's 12-pin button header (pin 3 RX, pin 4 TX) | The existing 16550 driver, with SPCR's access width |
| Storage | 128 GB UFS on the module; SD slot (card detect on GPIO 4); M.2 M-key 2280 (PCIe Gen3 x4) and 2230 (x1) | NVMe in the 2280 slot is the boot disk: no UFS driver needed to start |
| Network | One GbE port: RTL8211F PHY (RGMII, MDIO address 1) on a Synopsys DW EQoS MAC at `0xCAC82000`. RTL8852BE Wi-Fi in the M.2 E-key slot | A DW EQoS driver with an MDIO child bus (DRIVER-MODEL.md §16, P8). Wi-Fi is out of scope |
| USB | VL817 USB 3 hub on USB3 port B (shares a lane with PCIe); USB-C dual-role port with an FUSB301 at I2C0 `0x25` | DWC3 in host mode as xHCI. Whether firmware describes the hub's reset and port-power GPIOs in ACPI is to check |
| PCIe | ECAM in MCFG | The generic ECAM host bridge, if firmware has trained the links |
| IOMMU | A standard RISC-V IOMMU at `0xC0F00000`, left disabled by the vendor devicetree | Later, from the public specification |
| Display, GPU, video | DisplayPort 1.2 (DP1), MIPI DSI header; PowerVR **BXM-4-64** (BVNC 36.56.104.183, firmware `powervr/rogue_36.56.104.183_v1.fw`); Linlon video codec (Sky1's family) | M-G and later ([GRAPHICS.md](GRAPHICS.md)). GOP framebuffer console if the edk2 port drives DP |
| Other | Watchdog (`0xD4014000`, K1/MMP IP), RTC (`0xD4010000`, MMP IP; also via RPMI), carrier EEPROM at I2C1 `0x50`, CAN-FD, a fan header, a power-sequencing MCU | Watchdog early, as on the other machines. The rest is later |

**FreeBSD on this board.** FreeBSD's riscv64 kernel is devicetree-only (`options FDT`) and has no
APLIC MSI mode, so it does not run here today. There is no FreeBSD ground truth to capture and no
FreeBSD performance baseline on the board (ADR-0017). Linux on the shipped firmware is the
reference for what the hardware does, used as observation only.

