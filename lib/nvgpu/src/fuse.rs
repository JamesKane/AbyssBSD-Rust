// SPDX-License-Identifier: BSD-2-Clause
//! Which signature of a signed microcode a chip accepts (specification 02,
//! sections 3.3 and 5.4): the chip's fuses record the lowest microcode
//! version it still runs, and a microcode carries one signature per version.

/// The security engine 2's fuse array: sixteen registers, one per microcode.
pub const SEC2_UCODE1_VERSION: usize = 0x0082_4140;
/// The video decoder's.
pub const NVDEC_UCODE1_VERSION: usize = 0x0082_4100;
/// The GPU system processor's.
pub const GSP_UCODE1_VERSION: usize = 0x0082_41c0;

/// Why a microcode cannot be matched to the fuses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FuseError {
    /// The microcode ID is not in `1..=16`.
    BadUcodeId,
    /// The engine mask names no engine with a fuse array.
    BadEngine,
    /// The chip's fuse version is newer than the microcode's.
    Revoked,
    /// There is no signature for the version, or the signatures do not
    /// divide evenly.
    NoSignature,
}

/// The register to read for a microcode on the engines in `engine_id_mask`
/// with ID `ucode_id`: the lowest-numbered engine bit that has an array
/// (second security engine `0x1`, video decoder `0x4`, GPU system
/// processor `0x400`), then the array's entry for the ID.
///
/// # Errors
///
/// [`FuseError::BadUcodeId`] or [`FuseError::BadEngine`].
pub const fn register(engine_id_mask: u16, ucode_id: u8) -> Result<usize, FuseError> {
    if ucode_id == 0 || ucode_id > 16 {
        return Err(FuseError::BadUcodeId);
    }
    let base = if engine_id_mask & 0x0001 != 0 {
        SEC2_UCODE1_VERSION
    } else if engine_id_mask & 0x0004 != 0 {
        NVDEC_UCODE1_VERSION
    } else if engine_id_mask & 0x0400 != 0 {
        GSP_UCODE1_VERSION
    } else {
        return Err(FuseError::BadEngine);
    };
    Ok(base + (ucode_id as usize - 1) * 4)
}

/// The chip's fuse version from the register: the position of the highest
/// set bit among the low 16 (zero if none).
#[must_use]
pub const fn version(register: u32) -> u32 {
    16 - ((register & 0xffff) as u16).leading_zeros()
}

/// Which of `num_sigs` equal signatures to use: the last if the chip's
/// fuse version is zero, else `fuse_version - register_version` (the
/// microcode's version minus the chip's).
///
/// # Errors
///
/// [`FuseError::Revoked`] if the chip's version is newer than the
/// microcode's, [`FuseError::NoSignature`] if the index is past the
/// signatures or there are none.
pub const fn signature_index(
    num_sigs: u32,
    fuse_version: u32,
    register_version: u32,
) -> Result<u32, FuseError> {
    if num_sigs == 0 {
        return Err(FuseError::NoSignature);
    }
    if register_version == 0 {
        return Ok(num_sigs - 1);
    }
    let Some(index) = fuse_version.checked_sub(register_version) else {
        return Err(FuseError::Revoked);
    };
    if index >= num_sigs {
        return Err(FuseError::NoSignature);
    }
    Ok(index)
}

/// The byte range of signature `index` among `num_sigs` equal chunks of a
/// `total`-byte block, or `None` if they do not divide evenly.
#[must_use]
pub const fn signature_range(total: usize, num_sigs: u32, index: u32) -> Option<(usize, usize)> {
    if num_sigs == 0 || index >= num_sigs {
        return None;
    }
    let n = num_sigs as usize;
    if !total.is_multiple_of(n) {
        return None;
    }
    let size = total / n;
    Some((size * index as usize, size * (index as usize + 1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_by_engine_and_id() {
        assert_eq!(register(0x0001, 1), Ok(0x0082_4140));
        assert_eq!(register(0x0004, 16), Ok(0x0082_4100 + 15 * 4));
        // A GPU system processor microcode, as the 4070's firmware
        // security image (mask 0x400, ID 9).
        assert_eq!(register(0x0400, 9), Ok(0x0082_41c0 + 8 * 4));
        // The lowest engine bit wins.
        assert_eq!(register(0x0405, 3), Ok(0x0082_4140 + 8));
        assert_eq!(register(0x0400, 0), Err(FuseError::BadUcodeId));
        assert_eq!(register(0x0400, 17), Err(FuseError::BadUcodeId));
        assert_eq!(register(0x0002, 1), Err(FuseError::BadEngine));
        assert_eq!(register(0, 1), Err(FuseError::BadEngine));
    }

    #[test]
    fn versions_are_the_highest_set_bit() {
        assert_eq!(version(0), 0);
        assert_eq!(version(1), 1);
        assert_eq!(version(0b100), 3);
        assert_eq!(version(0xffff), 16);
        // Only the low 16 bits are the field.
        assert_eq!(version(0xffff_0000), 0);
    }

    #[test]
    fn choosing_a_signature() {
        // A fuse of zero takes the last.
        assert_eq!(signature_index(2, 5, 0), Ok(1));
        // The chip at version 4, the microcode at 5, two signatures: the
        // second (index 1).
        assert_eq!(signature_index(2, 5, 4), Ok(1));
        assert_eq!(signature_index(2, 5, 5), Ok(0));
        assert_eq!(signature_index(2, 4, 5), Err(FuseError::Revoked));
        assert_eq!(signature_index(2, 9, 1), Err(FuseError::NoSignature));
        assert_eq!(signature_index(0, 1, 1), Err(FuseError::NoSignature));
        assert_eq!(signature_index(0, 1, 0), Err(FuseError::NoSignature));
    }

    #[test]
    fn signature_chunks() {
        assert_eq!(signature_range(768, 2, 0), Some((0, 384)));
        assert_eq!(signature_range(768, 2, 1), Some((384, 768)));
        assert_eq!(signature_range(768, 2, 2), None);
        assert_eq!(signature_range(769, 2, 0), None, "uneven");
        assert_eq!(signature_range(768, 0, 0), None);
        assert_eq!(signature_range(usize::MAX, 1, 0), Some((0, usize::MAX)));
    }
}
