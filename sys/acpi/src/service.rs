// SPDX-License-Identifier: BSD-2-Clause
//! The AML service: one thread owns the interpreter and runs evaluations
//! sent to it as jobs.
//!
//! The interpreter is single-threaded, so serializing on one thread costs
//! nothing, and that thread gets a stack sized for the deepest firmware we
//! know (the Radxa Q8B's AML needs about 13 KiB; DRIVER-MODEL.md §5.1).

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::sync::Arc;

use aml::{Config, Host, Interpreter, Limits};
use sched::{Sleepable, WaitQueue};
use sync::{Once, SpinLock};

use crate::Ops;
use crate::host::AmlHost;

/// The AML thread's stack.
const STACK: usize = 64 * 1024;

type Job = Box<dyn FnOnce(&mut Interpreter, &mut dyn Host) + Send>;

/// The running service.
pub struct Service {
    ops: &'static dyn Ops,
    jobs: SpinLock<VecDeque<Job>>,
    queue: WaitQueue,
}

impl core::fmt::Debug for Service {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Service").finish_non_exhaustive()
    }
}

static SERVICE: Once<Service> = Once::new();

/// What loading the namespace found.
#[derive(Clone, Debug, Default)]
pub struct LoadSummary {
    /// Definition blocks loaded.
    pub tables: usize,
    /// Tables that failed to load.
    pub failed: usize,
    /// Scope bodies skipped after an error while loading.
    pub warnings: usize,
    /// `_STA`/`_INI` evaluations that failed.
    pub init_errors: usize,
    /// Namespace nodes.
    pub nodes: usize,
}

/// The service, once [`init`] has run.
#[must_use]
pub fn service() -> Option<&'static Service> {
    SERVICE.get()
}

/// Starts the AML thread, loads the DSDT and SSDTs, and runs `_STA`/`_INI`
/// over the namespace. Returns `None` if the thread cannot start or there
/// is no DSDT.
pub fn init(ops: &'static dyn Ops, s: &Sleepable) -> Option<LoadSummary> {
    let blocks = ops.definition_blocks();
    if blocks.is_empty() {
        return None;
    }
    if SERVICE
        .set(Service {
            ops,
            jobs: SpinLock::new(VecDeque::new()),
            queue: WaitQueue::new(),
        })
        .is_err()
    {
        return None;
    }
    let svc = SERVICE.get()?;
    sched::Builder::new("acpi")
        .priority(sched::Priority::KERNEL)
        .stack(STACK)
        .spawn(move |s| svc.thread(s))
        .ok()?;
    Some(svc.run(s, move |interp, host| {
        let mut sum = LoadSummary::default();
        for b in &blocks {
            match interp.load_table(b, host) {
                Ok(r) => {
                    sum.tables += 1;
                    sum.warnings += r.warnings.len();
                }
                Err(_) => sum.failed += 1,
            }
        }
        sum.init_errors = interp.initialize(host).len();
        sum.nodes = interp.node_count();
        sum
    }))
}

impl Service {
    fn thread(&self, s: &Sleepable) {
        let config = Config {
            limits: Limits::for_stack(STACK),
            ..Config::default()
        };
        let mut interp = Interpreter::new(config);
        let mut host = AmlHost { ops: self.ops, s };
        loop {
            let mut job = None;
            self.queue.wait_until(s, || {
                job = self.jobs.lock().pop_front();
                job.is_some()
            });
            if let Some(j) = job {
                j(&mut interp, &mut host);
            }
        }
    }

    /// Runs `f` on the AML thread and waits for its result. Must not be
    /// called from the AML thread.
    pub fn run<R: Send + 'static>(
        &self,
        s: &Sleepable,
        f: impl FnOnce(&mut Interpreter, &mut dyn Host) -> R + Send + 'static,
    ) -> R {
        let slot: Arc<(SpinLock<Option<R>>, WaitQueue)> =
            Arc::new((SpinLock::new(None), WaitQueue::new()));
        let back = Arc::clone(&slot);
        self.jobs.lock().push_back(Box::new(move |i, h| {
            let r = f(i, h);
            *back.0.lock() = Some(r);
            back.1.wake_all();
        }));
        self.queue.wake_one();
        let mut out = None;
        slot.1.wait_until(s, || {
            out = slot.0.lock().take();
            out.is_some()
        });
        out.expect("job result")
    }

    /// The kernel services.
    #[must_use]
    pub fn ops(&self) -> &'static dyn Ops {
        self.ops
    }
}
