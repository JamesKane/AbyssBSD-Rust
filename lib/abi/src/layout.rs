// SPDX-License-Identifier: BSD-2-Clause
//! Machine-readable layouts of every boundary structure, for
//! `cargo xtask abi-check`.
//!
//! Offsets and sizes come from the Rust definitions themselves
//! (`offset_of!`, `size_of`), so the check compares what the kernel will
//! actually use against what FreeBSD's C compiler computes.

use crate::{signal, stat, time};

/// Architectures a layout applies to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arches {
    /// Identical on every supported 64-bit architecture.
    All,
    /// amd64 only.
    Amd64,
    /// arm64 only.
    Arm64,
}

/// The layout of one field.
#[derive(Clone, Copy, Debug)]
pub struct FieldLayout {
    /// Member designator in C (may be a macro such as `sa_handler`).
    pub c_name: &'static str,
    /// Byte offset.
    pub offset: usize,
    /// Size in bytes.
    pub size: usize,
}

/// The layout of one structure.
#[derive(Clone, Copy, Debug)]
pub struct StructLayout {
    /// The C type, e.g. `struct stat`.
    pub c_type: &'static str,
    /// Headers that declare it, in include order.
    pub headers: &'static [&'static str],
    /// Which architectures share this layout.
    pub arches: Arches,
    /// `sizeof`.
    pub size: usize,
    /// `_Alignof`.
    pub align: usize,
    /// Every field the kernel reads or writes.
    pub fields: &'static [FieldLayout],
}

/// Size of the field selected by `_f`. The function pointer only carries
/// the field type; it is never called.
#[doc(hidden)]
pub const fn field_size<T, F>(_f: fn(&T) -> &F) -> usize {
    size_of::<F>()
}

macro_rules! layout {
    ($ty:ty, $c_type:literal, [$($h:literal),*], $arches:ident, { $($field:ident => $c:literal),* $(,)? }) => {
        StructLayout {
            c_type: $c_type,
            headers: &[$($h),*],
            arches: Arches::$arches,
            size: size_of::<$ty>(),
            align: align_of::<$ty>(),
            fields: &[$(FieldLayout {
                c_name: $c,
                offset: core::mem::offset_of!($ty, $field),
                size: field_size(|s: &$ty| &s.$field),
            }),*],
        }
    };
}

/// Every structure checked by `abi-check`.
pub static LAYOUTS: &[StructLayout] = &[
    layout!(time::Timespec, "struct timespec", ["sys/types.h", "time.h"], All, {
        tv_sec => "tv_sec",
        tv_nsec => "tv_nsec",
    }),
    layout!(stat::Stat, "struct stat", ["sys/types.h", "sys/stat.h"], All, {
        st_dev => "st_dev",
        st_ino => "st_ino",
        st_nlink => "st_nlink",
        st_mode => "st_mode",
        st_bsdflags => "st_bsdflags",
        st_uid => "st_uid",
        st_gid => "st_gid",
        st_padding1 => "st_padding1",
        st_rdev => "st_rdev",
        st_atim => "st_atim",
        st_mtim => "st_mtim",
        st_ctim => "st_ctim",
        st_birthtim => "st_birthtim",
        st_size => "st_size",
        st_blocks => "st_blocks",
        st_blksize => "st_blksize",
        st_flags => "st_flags",
        st_gen => "st_gen",
        st_filerev => "st_filerev",
        st_spare => "st_spare",
    }),
    layout!(signal::Sigset, "sigset_t", ["sys/types.h", "signal.h"], All, {
        bits => "__bits",
    }),
    layout!(signal::Sigaction, "struct sigaction", ["sys/types.h", "signal.h"], All, {
        sa_handler => "sa_handler",
        sa_flags => "sa_flags",
        sa_mask => "sa_mask",
    }),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_sizes() {
        // Values computed by FreeBSD 15.1's clang for amd64; abi-check
        // re-verifies them against the real headers.
        assert_eq!(size_of::<time::Timespec>(), 16);
        assert_eq!(size_of::<stat::Stat>(), 224);
        assert_eq!(size_of::<signal::Sigaction>(), 32);
    }

    #[test]
    fn fields_are_in_bounds() {
        for l in LAYOUTS {
            for f in l.fields {
                assert!(f.offset + f.size <= l.size, "{}.{}", l.c_type, f.c_name);
            }
        }
    }
}
