// SPDX-License-Identifier: BSD-2-Clause
//! First-use telemetry (docs/MULTIUSER.md §2): the first time a program
//! reaches an interface FreeBSD has and the kernel lacks, one line is
//! logged:
//!
//! ```text
//! first use: <kind> <what> by <program>
//! ```
//!
//! once per interface and program. The multi-user boot test reads these
//! lines: each is work left, or an interface FreeBSD lacks too that the
//! program only probes.
//!
//! Callers report only what FreeBSD would have answered: a system call in
//! its table, a sysctl name the program looked up, a device request the
//! driver does not know (terminal requests aside, which `isatty` sends to
//! everything), an `fcntl` command or `sysarch` operation FreeBSD has.
//!
//! The record of what was reported is a fixed table of hashes, filled
//! without locks or allocation; once it is full, everything is reported.

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::proc::Process;

/// What kind of interface was reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    Syscall,
    Sysctl,
    Ioctl,
    Fcntl,
    Sysarch,
    Procctl,
    #[expect(dead_code, reason = "reported once socket options land (P6.5)")]
    Sockopt,
}

impl Kind {
    const fn name(self) -> &'static str {
        match self {
            Self::Syscall => "syscall",
            Self::Sysctl => "sysctl",
            Self::Ioctl => "ioctl",
            Self::Fcntl => "fcntl",
            Self::Sysarch => "sysarch",
            Self::Procctl => "procctl",
            Self::Sockopt => "sockopt",
        }
    }
}

const SLOTS: usize = 4096;
/// How far an insertion probes before giving up on deduplication.
const PROBES: usize = 16;

static SEEN: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];

/// FNV-1a, folding in each byte.
struct Fnv(u64);

impl Fnv {
    fn add(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
    }
}

impl fmt::Write for Fnv {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.add(s.as_bytes());
        Ok(())
    }
}

/// Whether `hash` is new; records it if so.
fn first(hash: u64) -> bool {
    // 0 marks a free slot.
    let hash = hash.max(1);
    let start = usize::try_from(hash % SLOTS as u64).unwrap_or(0);
    for i in 0..PROBES {
        let slot = &SEEN[(start + i) % SLOTS];
        // Relaxed: the table only deduplicates log lines; a race at worst
        // logs one twice.
        match slot.compare_exchange(0, hash, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return true,
            Err(h) if h == hash => return false,
            Err(_) => {}
        }
    }
    true
}

/// Reports a use of `what`, of `kind`, by the current process (or the
/// kernel, outside one), unless already reported for this program.
pub(crate) fn report(kind: Kind, what: fmt::Arguments<'_>) {
    let mut comm = [0u8; abi::kinfo::MAXCOMLEN + 1];
    let len = Process::current().map_or(0, |p| {
        p.with_comm(|c| {
            let n = c.len().min(comm.len());
            comm[..n].copy_from_slice(&c[..n]);
            n
        })
    });
    let program = match core::str::from_utf8(&comm[..len]) {
        Ok("") | Err(_) => "kernel",
        Ok(s) => s,
    };
    let mut h = Fnv(0xcbf2_9ce4_8422_2325);
    h.add(kind.name().as_bytes());
    let _ = fmt::write(&mut h, what);
    h.add(program.as_bytes());
    if first(h.0) {
        kprintln!("first use: {} {} by {}", kind.name(), what, program);
    }
}

/// A request number in FreeBSD's `_IOC` form, as `<dir>('<group>', <num>,
/// <len>)`, as `sys/ioccom.h` would write it.
pub(crate) struct IoctlCmd(pub(crate) u32);

impl fmt::Display for IoctlCmd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let c = self.0;
        let dir = match c & 0xe000_0000 {
            0x2000_0000 => "_IO",
            0x4000_0000 => "_IOR",
            0x8000_0000 => "_IOW",
            0xc000_0000 => "_IOWR",
            _ => return write!(f, "{c:#010x}"),
        };
        let group = char::from(u8::try_from((c >> 8) & 0xff).unwrap_or(b'?'));
        let len = (c >> 16) & 0x1fff;
        let num = c & 0xff;
        if group.is_ascii_graphic() {
            write!(f, "{dir}('{group}', {num}, {len}) {c:#010x}")
        } else {
            write!(f, "{dir}({:#x}, {num}, {len}) {c:#010x}", (c >> 8) & 0xff)
        }
    }
}
