# ACPI table corpus

Raw ACPI tables (`<signature>.dat`, header included; repeated signatures numbered `ssdt2.dat`,
…), for the host tests of `lib/acpi-tables` and `lib/aml`.

| Directory | Source |
|---|---|
| `qemu-q35/` | QEMU 10.2, `-machine q35`, edk2, 4 CPUs, 1 GiB, the boot-test devices (virtio-blk, virtio-rng). `cargo xtask acpidump --arch x86_64 --machine q35` |
| `qemu-pc/` | As above with `-machine pc` (i440FX). `--machine pc` |
| `qemu-virt/` | QEMU 10.2, `-machine virt,gic-version=3,acpi=on`, edk2. `cargo xtask acpidump --arch aarch64` |
| `qemu-virt-smmuv3/` | As `qemu-virt/` with `iommu=smmuv3` (an SMMUv3 in front of the PCIe root complex, buses 0 and 1) and the virtio devices `disable-legacy=on,iommu_platform=on`. `cargo xtask acpidump --arch aarch64 --iommu` |
| `bhyve/` | bhyve on FreeBSD 15.1 (the build server), `edk2-bhyve`, the boot-test configuration. `cargo xtask --remote acpidump --arch x86_64 --vmm bhyve` |
| `opi6p/` | Orange Pi 6 Plus (CIX P1 / Sky1), firmware as shipped, copied from Linux's `/sys/firmware/acpi/tables` on 2026-11-09. Linux's `SSDT1`/`SSDT2` are `ssdt1.dat`/`ssdt2.dat`. See [../opi6p/README.md](../opi6p/README.md) |
| `x870e/` | Ryzen 9 7900X / Gigabyte X870E AORUS ELITE WIFI7 (AMI firmware), copied from Linux's `/sys/firmware/acpi/tables` on 2026-11-09, without `VFCT` (AMD's video BIOS). Linux's `SSDT1`-`SSDT15` are `ssdt1.dat`-`ssdt15.dat`. See [../x870e/README.md](../x870e/README.md) |
| `radxa-q8b/` | Radxa Dragon Q8B (Qualcomm), captured with `acpidump` under Linux during the earlier AbyssBSD project (`../AbyssBSD/docs/boards/radxa-dragon-q8b/dumps/acpi/`). A large vendor DSDT |

`cargo xtask acpidump` boots the kernel with `abyss.acpidump`, which prints every table the XSDT
lists, plus the DSDT, hex-encoded on the console; xtask decodes them. Captures from the physical
reference machines are added at P6 (ADR-0010).
