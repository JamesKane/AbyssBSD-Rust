// SPDX-License-Identifier: BSD-2-Clause
//! Small nodes `rc` and the base's programs read at boot, each where
//! FreeBSD keeps it: `user.localbase` (`kern_mib.c`), `kern.module_path`
//! (`kern_linker.c`), `vfs.root_mount_hold` (`vfs_mountroot.c`), and
//! `kern.boottrace` (`kern_boottrace.c`), which is off unless the loader
//! enables it, as in GENERIC, so what init and `rc` trace is dropped.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use abi::fcntl::PATH_MAX;
use abi::sysctl::ctlflag;
use sched::Sleepable;
use sync::SpinLock;
use sysctl::{Number, Value};

/// `USER_LOCALBASE`: `user.localbase`'s number, which `sendmail` uses.
const USER_LOCALBASE_OID: i32 = 21;

/// `_PATH_LOCALBASE`.
const LOCALBASE: &[u8] = b"/usr/local";

/// The module path FreeBSD's loader hands the kernel: the kernel's own
/// directory, then `loader.conf`'s `module_path`.
const MODULE_PATH: &[u8] =
    b"/boot/kernel;/boot/modules;/boot/firmware;/boot/dtb;/boot/dtb/overlays";

/// `MAXPATHLEN` for `kern.module_path` (`linker_path`).
static MODULE: SpinLock<Vec<u8>> = SpinLock::new(Vec::new());
/// `user.localbase`.
static USER_LOCALBASE: SpinLock<Vec<u8>> = SpinLock::new(Vec::new());
/// `kern.boottrace.shutdown_trace`.
static SHUTDOWN_TRACE: AtomicBool = AtomicBool::new(false);
/// `kern.boottrace.shutdown_trace_threshold`.
static SHUTDOWN_THRESHOLD: AtomicI32 = AtomicI32::new(0);

pub(crate) fn sysctl_init(s: &Sleepable) {
    *MODULE.lock() = crate::kenv::get(b"module_path").unwrap_or_else(|| MODULE_PATH.to_vec());
    *USER_LOCALBASE.lock() = LOCALBASE.to_vec();
    crate::sysctl::with(s, |t| {
        let root = t.root();
        let kern = t.child(root, "kern").expect("kern node");
        let user = t.child(root, "user").expect("user node");
        let vfs = t.child(root, "vfs").expect("vfs node");
        let string = |t: &mut sysctl::Tree,
                      parent,
                      number: Number,
                      name: &str,
                      descr: &str,
                      v: &'static SpinLock<Vec<u8>>| {
            t.add_writable(
                parent,
                number,
                name,
                descr,
                0,
                Value::ByteStr(Box::new(move |out| out.extend_from_slice(&v.lock()))),
                sysctl::str_writer(PATH_MAX, move |b| {
                    *v.lock() = b.to_vec();
                    Ok(())
                }),
            )
            .expect("string node");
        };
        string(
            t,
            user,
            Number::Fixed(USER_LOCALBASE_OID),
            "localbase",
            "Prefix used to install and locate add-on packages",
            &USER_LOCALBASE,
        );
        string(
            t,
            kern,
            Number::Auto,
            "module_path",
            "module load search path",
            &MODULE,
        );
        // Nothing holds the root mount once init runs.
        t.add_value(
            vfs,
            Number::Auto,
            "root_mount_hold",
            "List of root mount hold tokens",
            Value::text(""),
        )
        .expect("vfs.root_mount_hold");

        let bt = t
            .add_node(kern, Number::Auto, "boottrace", "boottrace statistics")
            .expect("kern.boottrace");
        t.add_value(
            bt,
            Number::Auto,
            "log",
            "Print a log of the boottrace trace data",
            Value::text(""),
        )
        .expect("kern.boottrace.log");
        if let Some(log) = t.child(bt, "log") {
            t.set_flags(log, ctlflag::SKIP);
        }
        // Write-only events, dropped while tracing is off.
        for (name, descr) in [
            ("boottrace", "Capture a boot-time trace event"),
            ("runtrace", "Capture a run-time trace event"),
            ("shuttrace", "Capture a shutdown-time trace event"),
        ] {
            t.add_writable(
                bt,
                Number::Auto,
                name,
                descr,
                0,
                Value::text(""),
                Arc::new(|_| Ok(())),
            )
            .expect("kern.boottrace event");
        }
        t.add_writable(
            bt,
            Number::Auto,
            "reset",
            "Reset run-time tracing table",
            0,
            Value::int(0),
            Arc::new(|_| Ok(())),
        )
        .expect("kern.boottrace.reset");
        t.add_value(
            bt,
            Number::Auto,
            "enabled",
            "Boot-time and shutdown-time tracing enabled",
            Value::U8(Box::new(|| 0)),
        )
        .expect("kern.boottrace.enabled");
        t.add_writable(
            bt,
            Number::Auto,
            "shutdown_trace",
            "Enable kernel shutdown tracing to the console",
            0,
            Value::U8(Box::new(|| {
                u8::from(SHUTDOWN_TRACE.load(Ordering::Relaxed))
            })),
            sysctl::u8_writer(|v| {
                SHUTDOWN_TRACE.store(v != 0, Ordering::Relaxed);
                Ok(())
            }),
        )
        .expect("kern.boottrace.shutdown_trace");
        t.add_writable(
            bt,
            Number::Auto,
            "shutdown_trace_threshold",
            "Tracing threshold (ms) below which tracing is ignored",
            0,
            Value::Int(Box::new(|| SHUTDOWN_THRESHOLD.load(Ordering::Relaxed))),
            sysctl::int_writer(|v| {
                SHUTDOWN_THRESHOLD.store(v, Ordering::Relaxed);
                Ok(())
            }),
        )
        .expect("kern.boottrace.shutdown_trace_threshold");
        // No table while tracing is off.
        t.add_value(
            bt,
            Number::Auto,
            "table_size",
            "Boot-time tracing table size",
            Value::Uint(Box::new(|| 0)),
        )
        .expect("kern.boottrace.table_size");
    });
}
