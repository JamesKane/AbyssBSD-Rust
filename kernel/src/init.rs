// SPDX-License-Identifier: BSD-2-Clause
//! The first user process (`init_main.c`'s `start_init`): each path in
//! `kern.init_path` (the loader's `init_path` if set) is tried in turn and
//! the first that executes runs as pid 1, with `-s` for a single-user boot
//! (`boot_single`, `RB_SINGLE`). docs/MULTIUSER.md §11.1.

use alloc::vec::Vec;
use sched::Sleepable;
use sync::SpinLock;

use abi::errno::Errno;

/// `INIT_PATH`'s default.
const DEFAULT_PATH: &[u8] = b"/sbin/init:/sbin/oinit:/sbin/init.bak:/rescue/init";

/// `INIT_SHUTDOWN_TIMEOUT`: how long init waits for `rc.shutdown`.
const SHUTDOWN_TIMEOUT: i32 = 120;

/// `kern.init_path`.
static PATH: SpinLock<Vec<u8>> = SpinLock::new(Vec::new());

/// `kern.init_path` and `kern.init_shutdown_timeout`.
pub(crate) fn sysctl_init(s: &Sleepable) {
    use sysctl::{Number, Value};
    *PATH.lock() = DEFAULT_PATH.to_vec();
    crate::sysctl::with(s, |t| {
        let kern = t.child(t.root(), "kern").expect("kern node");
        t.add_value(
            kern,
            Number::Auto,
            "init_path",
            "Path used to search the init process",
            Value::ByteStr(alloc::boxed::Box::new(|out| {
                out.extend_from_slice(&PATH.lock());
            })),
        )
        .expect("kern.init_path");
        // Read by init(8) only; kept for it, as FreeBSD's.
        let timeout = core::sync::atomic::AtomicI32::new(SHUTDOWN_TIMEOUT);
        let timeout = alloc::boxed::Box::leak(alloc::boxed::Box::new(timeout));
        t.add_writable(
            kern,
            Number::Auto,
            "init_shutdown_timeout",
            "Shutdown timeout of init(8). Unused within kernel, but used to control init(8)",
            0,
            Value::Int(alloc::boxed::Box::new(|| {
                timeout.load(core::sync::atomic::Ordering::Relaxed)
            })),
            sysctl::int_writer(|n| {
                timeout.store(n, core::sync::atomic::Ordering::Relaxed);
                Ok(())
            }),
        )
        .expect("kern.init_shutdown_timeout");
    });
}

/// Whether the boot is single-user: the loader's `boot_single`, which
/// `boot_env_to_howto` turns into `RB_SINGLE` whatever its value.
#[cfg_attr(
    feature = "ktest",
    allow(dead_code, reason = "the ktest kernel starts no init")
)]
pub(crate) fn single_user() -> bool {
    crate::kenv::get(b"boot_single").is_some()
}

/// Starts init as pid 1: the first path in `kern.init_path` that
/// executes. `None` when none does: FreeBSD panics ("no init"), and the
/// boot shell is the fallback here.
#[cfg_attr(
    feature = "ktest",
    allow(dead_code, reason = "the ktest kernel starts no init")
)]
pub(crate) fn start(s: &Sleepable) -> Option<alloc::sync::Arc<crate::proc::Process>> {
    if let Some(v) = crate::kenv::get(b"init_path") {
        *PATH.lock() = v;
    }
    let path = PATH.lock().clone();
    for p in path.split(|&b| b == b':') {
        let mut argv: Vec<&[u8]> = alloc::vec![p];
        if single_user() {
            argv.push(b"-s");
        }
        let name = core::str::from_utf8(p).unwrap_or("?");
        match crate::proc::Process::spawn_session(s, p, &argv, &[], None, None) {
            Ok(init) => {
                if init.pid() != crate::proc::INIT {
                    kprintln!("init: {name} started as pid {}, not 1", init.pid());
                }
                return Some(init);
            }
            Err(Errno::ENOENT) => {}
            Err(e) => kprintln!("exec {name}: error {}", e as i32),
        }
    }
    kprintln!(
        "init: not found in path {}",
        core::str::from_utf8(&path).unwrap_or("?")
    );
    None
}
