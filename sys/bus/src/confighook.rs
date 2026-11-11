// SPDX-License-Identifier: BSD-2-Clause
//! Configuration hooks: work a driver defers until the boot's attach pass
//! is done and interrupts run (FreeBSD's `config_intrhook_establish` and
//! `config_intrhook_oneshot`).
//!
//! - A hook established while the system is cold waits. Once the boot
//!   pass has attached every device it found ([`run_config_hooks`], which
//!   the kernel calls after it), the hooks run one after another, in the
//!   order they were established, in the caller's thread; one
//!   established while they run is run too.
//! - Each hook holds the root mount back until it has run, as FreeBSD's
//!   boot waits for its hooks before mounting root.
//! - Established after boot, a hook runs at once, in the establishing
//!   thread.
//!
//! What a driver puts in one is what FreeBSD's counterpart does from its
//! hook: an NVMe controller's namespaces and CAM bus
//! (`nvme_ctrlr_start_config_hook`), registered after the buses that
//! attach registered directly (AHCI channels), so CAM numbers them as
//! FreeBSD does. USB explores in threads of its own and is not ordered
//! by them, as on FreeBSD.
//!
//! Behavior reference: sys/kern/subr_autoconf.c.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::string::String;

use sched::Sleepable;
use sync::SpinLock;

use crate::roothold::{RootHold, root_mount_hold};

/// A queued hook: what it runs, and the root mount it holds back.
struct Hook {
    run: Box<dyn FnOnce(&Sleepable) + Send>,
    _hold: RootHold,
}

struct Hooks {
    /// Before [`run_config_hooks`]: hooks wait.
    cold: bool,
    queue: VecDeque<Hook>,
    /// A thread is running the queue (`running`).
    running: bool,
}

static HOOKS: SpinLock<Hooks> = SpinLock::new(Hooks {
    cold: true,
    queue: VecDeque::new(),
    running: false,
});

/// Runs `f` once the boot's attach pass is done (at once after boot), in
/// establishment order (`config_intrhook_oneshot`); `name` is what the
/// root mount waits for meanwhile.
pub fn config_intrhook_oneshot(
    s: &Sleepable,
    name: &str,
    f: impl FnOnce(&Sleepable) + Send + 'static,
) {
    let cold = {
        let mut h = HOOKS.lock();
        h.queue.push_back(Hook {
            run: Box::new(f),
            _hold: root_mount_hold(String::from(name)),
        });
        h.cold
    };
    if !cold {
        run(s);
    }
}

/// Ends the cold period and runs the hooks established so far, and any
/// established while they run (`boot_run_interrupt_driven_config_hooks`).
pub fn run_config_hooks(s: &Sleepable) {
    HOOKS.lock().cold = false;
    run(s);
}

/// Runs the queue, unless another thread is already running it (which
/// will run what was just added).
fn run(s: &Sleepable) {
    {
        let mut h = HOOKS.lock();
        if h.running {
            return;
        }
        h.running = true;
    }
    loop {
        // Taken in its own statement: no spinlock held while it runs.
        let next = HOOKS.lock().queue.pop_front();
        let Some(hook) = next else {
            break;
        };
        let Hook {
            run: f,
            _hold: hold,
        } = hook;
        f(s);
        // Its root-mount hold goes once it has run.
        drop(hold);
    }
    let mut h = HOOKS.lock();
    h.running = false;
    // One added between the last look and here: run it too.
    let again = !h.queue.is_empty();
    drop(h);
    if again {
        run(s);
    }
}
