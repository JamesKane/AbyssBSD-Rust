// SPDX-License-Identifier: BSD-2-Clause
//! A CPU's name from its `MIDR_EL1`, as `hw.model` gives it: the
//! implementer's and the part's names and the variant and revision
//! (`ARM Cortex-A720 r0p1`), or what is known of them.
//!
//! Behavior reference: sys/arm64/arm64/identcpu.c (`print_cpu_midr`,
//! `cpu_implementers`, `cpu_parts_*`), sys/arm64/include/cpu.h.

use core::fmt;

/// Part numbers and their names.
type Parts = &'static [(u16, &'static str)];

/// Each implementer FreeBSD names, with its parts' names.
const IMPLEMENTERS: &[(u8, &str, Parts)] = &[
    (
        0xc0,
        "Ampere",
        &[(0xac3, "AmpereOne AC03"), (0xac4, "AmpereOne AC04")],
    ),
    (
        0x61,
        "Apple",
        &[
            (0x022, "M1 Icestorm"),
            (0x023, "M1 Firestorm"),
            (0x024, "M1 Pro Icestorm"),
            (0x025, "M1 Pro Firestorm"),
            (0x028, "M1 Max Icestorm"),
            (0x029, "M1 Max Firestorm"),
            (0x032, "M2 Blizzard"),
            (0x033, "M2 Avalanche"),
            (0x034, "M2 Pro Blizzard"),
            (0x035, "M2 Pro Avalanche"),
            (0x038, "M2 Max Blizzard"),
            (0x039, "M2 Max Avalanche"),
        ],
    ),
    (0x50, "APM", &[(0x000, "eMAG 8180")]),
    (
        0x41,
        "ARM",
        &[
            (0xd0f, "AEMv8"),
            (0xd00, "Foundation-Model"),
            (0xd02, "Cortex-A34"),
            (0xd04, "Cortex-A35"),
            (0xd03, "Cortex-A53"),
            (0xd05, "Cortex-A55"),
            (0xd07, "Cortex-A57"),
            (0xd06, "Cortex-A65"),
            (0xd43, "Cortex-A65AE"),
            (0xd08, "Cortex-A72"),
            (0xd09, "Cortex-A73"),
            (0xd0a, "Cortex-A75"),
            (0xd0b, "Cortex-A76"),
            (0xd0e, "Cortex-A76AE"),
            (0xd0d, "Cortex-A77"),
            (0xd41, "Cortex-A78"),
            (0xd42, "Cortex-A78AE"),
            (0xd4b, "Cortex-A78C"),
            (0xd46, "Cortex-A510"),
            (0xd80, "Cortex-A520"),
            (0xd47, "Cortex-A710"),
            (0xd4d, "Cortex-A715"),
            (0xd81, "Cortex-A720"),
            (0xd87, "Cortex-A725"),
            // FreeBSD's name for the X925.
            (0xd85, "Cortex-A925"),
            (0xd44, "Cortex-X1"),
            (0xd4c, "Cortex-X1C"),
            (0xd48, "Cortex-X2"),
            (0xd4e, "Cortex-X3"),
            (0xd82, "Cortex-X4"),
            (0xd8a, "C1-Nano"),
            (0xd8b, "C1-Pro"),
            (0xd90, "C1-Premium"),
            (0xd8c, "C1-Ultra"),
            (0xd4a, "Neoverse-E1"),
            (0xd0c, "Neoverse-N1"),
            (0xd49, "Neoverse-N2"),
            (0xd8e, "Neoverse-N3"),
            (0xd40, "Neoverse-V1"),
            (0xd4f, "Neoverse-V2"),
            (0xd84, "Neoverse-V3"),
            (0xd83, "Neoverse-V3AE"),
        ],
    ),
    (0x42, "Broadcom", &[]),
    (0x43, "Cavium", &[(0x0a1, "ThunderX"), (0x0af, "ThunderX2")]),
    (0x44, "DEC", &[]),
    (0x4d, "Freescale", &[]),
    (0x46, "Fujitsu", &[]),
    (0x48, "HiSilicon", &[]),
    (0x49, "IFX", &[]),
    (0x69, "Intel", &[]),
    (0x56, "Marvell", &[]),
    (0x6d, "Microsoft", &[(0xd49, "Azure Cobalt 100")]),
    (0x4e, "NVIDIA", &[]),
    (
        0x51,
        "Qualcomm",
        &[(0x804, "Kryo 400 Gold"), (0x805, "Kryo 400 Silver")],
    ),
];

/// `hw.model` for a CPU with this `MIDR_EL1`; its `Display` is FreeBSD's
/// text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Model(pub u32);

impl Model {
    fn implementer(self) -> u8 {
        self.0.to_be_bytes()[0]
    }

    fn part(self) -> u16 {
        u16::try_from((self.0 >> 4) & 0xfff).unwrap_or(0)
    }

    fn variant(self) -> u32 {
        (self.0 >> 20) & 0xf
    }

    fn revision(self) -> u32 {
        self.0 & 0xf
    }
}

impl fmt::Display for Model {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let midr = self.0;
        let Some((_, name, parts)) = IMPLEMENTERS.iter().find(|i| i.0 == self.implementer()) else {
            return write!(f, "Unknown Implementer (midr: {midr:08x})");
        };
        let (var, rev) = (self.variant(), self.revision());
        match parts.iter().find(|p| p.0 == self.part()) {
            Some((_, part)) => write!(f, "{name} {part} r{var}p{rev}"),
            None => write!(f, "{name} Unknown CPU r{var}p{rev} (midr: {midr:08x})"),
        }
    }
}
