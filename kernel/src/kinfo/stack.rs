// SPDX-License-Identifier: BSD-2-Clause
//! Kernel stacks of a process's threads, symbolized: `kern.proc.kstack`
//! (`procstat -k`), one `struct kinfo_kstack` per thread.
//!
//! A thread off every CPU is walked from the frame pointer it was
//! switched out with; one running is reported as running, its stack
//! changing under the reader (`KKST_STATE_RUNNING`), as FreeBSD's
//! `stack_save_td` refuses it. Frames are printed as
//! `stack_sbuf_print` prints them (`#N 0xaddr at name+0xoff`), which
//! `procstat` takes apart.
//!
//! Behavior reference: sys/kern/kern_proc.c (`sysctl_kern_proc_kstack`),
//! sys/kern/subr_stack.c (`stack_sbuf_print_flags`).

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::Write;

use abi::kinfo::*;
use sysctl::{Number, Tree};

use super::{Access, arg, process};
use crate::proc::Process;
use crate::uaccess::{Plain, bytes_of};

// SAFETY: integers and a byte array, no padding (size asserted in abi).
unsafe impl Plain for KinfoKstack {}

/// Frames recorded at most (`STACK_MAX`).
const STACK_MAX: usize = 18;

/// A fixed buffer that drops what does not fit (`SBUF_FIXEDLEN`).
struct Fixed<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl Write for Fixed<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let room = self.buf.len().saturating_sub(self.len + 1);
        let n = s.len().min(room);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

/// A symbol's name as `stack_symbol` gives it: at most 63 bytes, with no
/// blanks (which `procstat` splits frames at).
fn name_of(raw: &[u8]) -> alloc::string::String {
    let mut s = alloc::format!("{}", demangle::Demangled(raw));
    s = s.replace(' ', "_");
    s.truncate(63);
    s
}

/// Thread `t`'s record.
fn kstack(t: &crate::uthread::UserThread) -> alloc::boxed::Box<KinfoKstack> {
    let mut k: alloc::boxed::Box<KinfoKstack> = super::zeroed();
    k.kkst_tid = t.tid();
    let Some(fp) = t.sched().and_then(|s| s.saved_frame_pointer()) else {
        k.kkst_state = KKST_STATE_RUNNING;
        return k;
    };
    let mut w = Fixed {
        buf: &mut k.kkst_trace,
        len: 0,
    };
    for (i, ret) in crate::arch::backtrace::walk(fp).take(STACK_MAX).enumerate() {
        // A return address names the call before it.
        let pc = ret.saturating_sub(1);
        let (name, off) = crate::symbols::lookup(pc).map_or_else(
            || (alloc::string::String::from("??"), 0),
            |(n, o)| (name_of(n), o + 1),
        );
        let _ = writeln!(w, "#{i} {ret:#x} at {name}+{off:#x}");
    }
    k.kkst_state = KKST_STATE_STACKOK;
    k
}

/// Each of `p`'s threads' records.
pub(crate) fn kstacks(p: &Process) -> Vec<u8> {
    let mut out = Vec::new();
    for t in p.threads() {
        out.extend_from_slice(bytes_of(&*kstack(&t)));
    }
    out
}

/// Adds `kern.proc.kstack`.
pub(crate) fn add_nodes(t: &mut Tree, procs: sysctl::NodeId) {
    t.add_handler(
        procs,
        Number::Fixed(node::KSTACK),
        "kstack",
        "Process kernel stacks",
        Arc::new(|r: &[i32], out: &mut Vec<u8>| {
            let p = process(arg(r)?, Access::Debug)?;
            out.extend(kstacks(&p));
            Ok(())
        }),
    )
    .expect("kern.proc.kstack");
}
