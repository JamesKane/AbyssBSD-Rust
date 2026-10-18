// SPDX-License-Identifier: BSD-2-Clause
//! Frame-pointer stack walking. The kernel is built with frame pointers;
//! every read goes through [`probe_read`](crate::fixup::probe_read) so a
//! corrupt chain ends the walk instead of faulting again.

use crate::fixup::probe_read;

/// Return addresses up the stack, starting from frame pointer `fp`.
pub fn walk(fp: u64) -> impl Iterator<Item = u64> {
    let mut fp = fp;
    core::iter::from_fn(move || {
        if fp == 0 || !fp.is_multiple_of(8) {
            return None;
        }
        let ret = probe_read(fp + 8).ok()?;
        let next = probe_read(fp).ok()?;
        // Frames grow down, so callers sit at higher addresses.
        fp = if next > fp { next } else { 0 };
        (ret != 0).then_some(ret)
    })
    .take(64)
}

/// The current frame pointer.
#[inline(always)]
pub fn here() -> u64 {
    crate::cpu::frame_pointer()
}
