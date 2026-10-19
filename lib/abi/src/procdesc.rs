// SPDX-License-Identifier: BSD-2-Clause
//! Process descriptors (`sys/procdesc.h`): `pdfork`'s flags.

use crate::layout::{Arches, ConstValue};

/// The process lives on when its last descriptor closes.
pub const PD_DAEMON: u32 = 0x1;
/// The descriptor is closed on exec.
pub const PD_CLOEXEC: u32 = 0x2;
/// The flags `pdfork` takes.
pub const PD_ALLOWED_AT_FORK: u32 = PD_DAEMON | PD_CLOEXEC;

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("PD_DAEMON", PD_DAEMON),
    c("PD_CLOEXEC", PD_CLOEXEC),
    c("PD_ALLOWED_AT_FORK", PD_ALLOWED_AT_FORK),
];

const fn c(c_name: &'static str, value: u32) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/procdesc.h",
        value: value as i64,
        arches: Arches::All,
    }
}
