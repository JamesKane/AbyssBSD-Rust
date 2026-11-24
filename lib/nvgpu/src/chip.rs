// SPDX-License-Identifier: BSD-2-Clause
//! Identifying the chip (specification 01, sections 1 and 2): the two
//! identification registers at the start of the register aperture, the
//! chipset table, and where the video memory aperture's base address
//! register is.

/// The legacy identification register's offset in BAR0.
pub const NV_PMC_BOOT_0: usize = 0x0000_0000;
/// The extended identification register's offset in BAR0.
pub const NV_PMC_BOOT_42: usize = 0x0000_0a00;

/// A GPU architecture (`NV_PMC_BOOT_42` bits 29:24).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Architecture {
    /// Turing.
    Turing,
    /// Ampere.
    Ampere,
    /// Hopper.
    Hopper,
    /// Ada Lovelace.
    Ada,
    /// Blackwell, data center (`GB10x`).
    BlackwellGb10x,
    /// Blackwell, consumer (`GB20x`).
    BlackwellGb20x,
}

impl Architecture {
    /// The architecture code the register holds.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Turing => 0x16,
            Self::Ampere => 0x17,
            Self::Hopper => 0x18,
            Self::Ada => 0x19,
            Self::BlackwellGb10x => 0x1a,
            Self::BlackwellGb20x => 0x1b,
        }
    }

    /// Whether the Foundation Security Processor starts the GPU system
    /// processor (Hopper on), rather than the Video BIOS's firmware
    /// security image and the second security engine's booter.
    #[must_use]
    pub const fn uses_fsp(self) -> bool {
        !matches!(self, Self::Turing | Self::Ampere | Self::Ada)
    }
}

macro_rules! chipsets {
    ($($name:ident = $id:literal, $arch:ident, $text:literal;)*) => {
        /// A supported chipset: `(architecture << 4) | implementation`.
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub enum Chipset {
            $(
                #[doc = $text]
                $name,
            )*
        }

        impl Chipset {
            /// The chipset for 12-bit identifier `id`, if it is one the
            /// specification lists.
            #[must_use]
            pub const fn of_id(id: u16) -> Option<Self> {
                match id {
                    $($id => Some(Self::$name),)*
                    _ => None,
                }
            }

            /// The 12-bit chipset identifier.
            #[must_use]
            pub const fn id(self) -> u16 {
                match self {
                    $(Self::$name => $id,)*
                }
            }

            /// Its architecture.
            #[must_use]
            pub const fn architecture(self) -> Architecture {
                match self {
                    $(Self::$name => Architecture::$arch,)*
                }
            }

            /// Its name, as the part is called.
            #[must_use]
            pub const fn name(self) -> &'static str {
                match self {
                    $(Self::$name => $text,)*
                }
            }
        }
    };
}

chipsets! {
    Tu102 = 0x162, Turing, "TU102";
    Tu104 = 0x164, Turing, "TU104";
    Tu106 = 0x166, Turing, "TU106";
    Tu117 = 0x167, Turing, "TU117";
    Tu116 = 0x168, Turing, "TU116";
    Ga100 = 0x170, Ampere, "GA100";
    Ga102 = 0x172, Ampere, "GA102";
    Ga103 = 0x173, Ampere, "GA103";
    Ga104 = 0x174, Ampere, "GA104";
    Ga106 = 0x176, Ampere, "GA106";
    Ga107 = 0x177, Ampere, "GA107";
    Gh100 = 0x180, Hopper, "GH100";
    Ad102 = 0x192, Ada, "AD102";
    Ad103 = 0x193, Ada, "AD103";
    Ad104 = 0x194, Ada, "AD104";
    Ad106 = 0x196, Ada, "AD106";
    Ad107 = 0x197, Ada, "AD107";
    Gb100 = 0x1a0, BlackwellGb10x, "GB100";
    Gb102 = 0x1a2, BlackwellGb10x, "GB102";
    Gb202 = 0x1b2, BlackwellGb20x, "GB202";
    Gb203 = 0x1b3, BlackwellGb20x, "GB203";
    Gb205 = 0x1b5, BlackwellGb20x, "GB205";
    Gb206 = 0x1b6, BlackwellGb20x, "GB206";
    Gb207 = 0x1b7, BlackwellGb20x, "GB207";
}

impl Chipset {
    /// The address width the GPU drives for host memory: 47 bits before
    /// Hopper, 52 from Hopper on.
    #[must_use]
    pub const fn dma_address_bits(self) -> u8 {
        if self.architecture().uses_fsp() {
            52
        } else {
            47
        }
    }
}

/// Why a device is not a chip this code knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChipError {
    /// A register read all ones: the device is not answering.
    NotResponding,
    /// An architecture before Fermi, which has no extended register.
    BeforeFermi,
    /// A chipset the specification does not list.
    Unsupported {
        /// Its 12-bit identifier.
        id: u16,
    },
}

/// What the identification registers say.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Identity {
    /// The chipset.
    pub chipset: Chipset,
    /// Silicon major revision.
    pub major: u8,
    /// Silicon minor revision.
    pub minor: u8,
}

/// Identifies the chip from the two registers. `boot_42` is asked for only
/// if `boot_0` shows a chip that has it, as the specification requires.
///
/// # Errors
///
/// [`ChipError`] if either register reads all ones, the chip predates
/// Fermi, or its chipset is not listed.
pub fn identify(boot_0: u32, boot_42: impl FnOnce() -> u32) -> Result<Identity, ChipError> {
    if boot_0 == u32::MAX {
        return Err(ChipError::NotResponding);
    }
    let architecture_0 = boot_0 >> 24 & 0x1f;
    let architecture_1 = boot_0 >> 8 & 1;
    if architecture_1 == 0 && architecture_0 < 0x0c {
        return Err(ChipError::BeforeFermi);
    }
    let r = boot_42();
    if r == u32::MAX {
        return Err(ChipError::NotResponding);
    }
    let architecture = r >> 24 & 0x3f;
    let implementation = r >> 20 & 0xf;
    let id = u16::try_from(architecture << 4 | implementation).unwrap_or(0);
    let chipset = Chipset::of_id(id).ok_or(ChipError::Unsupported { id })?;
    Ok(Identity {
        chipset,
        major: u8::try_from(r >> 16 & 0xf).unwrap_or(0),
        minor: u8::try_from(r >> 12 & 0xf).unwrap_or(0),
    })
}

/// Where the video memory aperture (BAR1) is, given whether BAR0 is a
/// 64-bit memory BAR, which takes two base address register slots.
#[must_use]
pub const fn bar1_index(bar0_is_64_bit: bool) -> usize {
    if bar0_is_64_bit { 2 } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rtx_4070() {
        // Architecture 0x19, implementation 4, revision a.1.
        let id = identify(0x1940_00a1, || 0x194a_1000).unwrap();
        assert_eq!(id.chipset, Chipset::Ad104);
        assert_eq!(id.chipset.id(), 0x194);
        assert_eq!(id.chipset.name(), "AD104");
        assert_eq!(id.chipset.architecture(), Architecture::Ada);
        assert_eq!((id.major, id.minor), (0xa, 1));
        assert_eq!(id.chipset.dma_address_bits(), 47);
        assert!(!Architecture::Ada.uses_fsp());
    }

    #[test]
    fn blackwell_uses_the_security_processor() {
        let id = identify(0x1b30_0000, || 0x1b30_0000).unwrap();
        assert_eq!(id.chipset, Chipset::Gb203);
        assert!(id.chipset.architecture().uses_fsp());
        assert_eq!(id.chipset.dma_address_bits(), 52);
    }

    #[test]
    fn a_chip_before_fermi_is_rejected_without_the_second_read() {
        let r = identify(0x0400_0000, || panic!("read BOOT_42"));
        assert_eq!(r, Err(ChipError::BeforeFermi));
        // The most significant architecture bit set: not before Fermi.
        assert!(identify(0x0400_0100, || 0x1940_0000).is_ok());
    }

    #[test]
    fn a_device_that_does_not_answer_is_not_a_chip() {
        assert_eq!(
            identify(u32::MAX, || panic!("read BOOT_42")),
            Err(ChipError::NotResponding)
        );
        assert_eq!(
            identify(0x1940_0000, || u32::MAX),
            Err(ChipError::NotResponding)
        );
    }

    #[test]
    fn unknown_chipsets_are_errors() {
        // Architecture 0x19, implementation 5: no such part listed.
        assert_eq!(
            identify(0x1950_0000, || 0x1950_0000),
            Err(ChipError::Unsupported { id: 0x195 })
        );
        // Architecture 0x3f, implementation 0xf.
        assert_eq!(
            identify(0x1f00_0000, || 0x3ff0_0000),
            Err(ChipError::Unsupported { id: 0x3ff })
        );
    }

    #[test]
    fn every_chipset_round_trips_and_agrees_with_its_architecture() {
        for id in 0..0x400u16 {
            if let Some(c) = Chipset::of_id(id) {
                assert_eq!(c.id(), id);
                assert_eq!(u16::from(c.architecture().code()), id >> 4);
            }
        }
    }

    #[test]
    fn the_video_memory_aperture_follows_the_register_aperture() {
        assert_eq!(bar1_index(false), 1);
        assert_eq!(bar1_index(true), 2);
    }
}
