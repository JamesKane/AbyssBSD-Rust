// SPDX-License-Identifier: BSD-2-Clause
//! Message priorities (`sys/syslog.h`): the `<pri>` the kernel's message
//! buffer gives `log(9)` lines and console output, which `syslogd` reads
//! from `/dev/klog`.

use crate::layout::{Arches, ConstValue};

/// The system is unusable.
pub const LOG_EMERG: i32 = 0;
/// Error conditions.
pub const LOG_ERR: i32 = 3;
/// Normal but significant.
pub const LOG_NOTICE: i32 = 5;
/// Informational.
pub const LOG_INFO: i32 = 6;
/// The level within a priority.
pub const LOG_PRIMASK: i32 = 0x07;
/// Kernel messages (facility 0).
pub const LOG_KERN: i32 = 0;
/// `/dev/console` output (facility 14).
pub const LOG_CONSOLE: i32 = 14 << 3;
/// The facility within a priority.
pub const LOG_FACMASK: i32 = 0x03f8;

pub(crate) const CONSTANTS: &[ConstValue] = &[
    c("LOG_EMERG", LOG_EMERG),
    c("LOG_ERR", LOG_ERR),
    c("LOG_INFO", LOG_INFO),
    c("LOG_PRIMASK", LOG_PRIMASK),
    c("LOG_KERN", LOG_KERN),
    c("LOG_CONSOLE", LOG_CONSOLE),
    c("LOG_FACMASK", LOG_FACMASK),
];

const fn c(c_name: &'static str, value: i32) -> ConstValue {
    ConstValue {
        c_name,
        header: "sys/syslog.h",
        value: value as i64,
        arches: Arches::All,
    }
}
