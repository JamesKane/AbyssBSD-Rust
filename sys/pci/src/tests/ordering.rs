// SPDX-License-Identifier: BSD-2-Clause
//! Command-register writes are in effect when the call that made them
//! returns: each is read back, so the next access (a driver's first BAR
//! read, the devices behind a bridge, a framebuffer after BAR sizing)
//! finds the decoding it expects. Without that, the first virtio register
//! read on QEMU `virt` intermittently took a synchronous external abort.

use alloc::sync::Arc;
use alloc::vec::Vec;

use super::std::sync::Mutex;
use super::{Fake, device};
use crate::PciAddress;
use crate::bar;
use crate::bus::{PciBusData, PciChild};
use crate::config::ConfigSpace;
use crate::header::COMMAND;

/// One function's configuration space, logging every access: (write?,
/// offset).
struct Recorder {
    regs: Mutex<[u32; 1024]>,
    log: Mutex<Vec<(bool, u16)>>,
}

impl ConfigSpace for Recorder {
    fn read32(&self, _: PciAddress, offset: u16) -> u32 {
        self.log.lock().unwrap().push((false, offset));
        self.regs.lock().unwrap()[usize::from(offset / 4)]
    }

    fn write32(&self, _: PciAddress, offset: u16, v: u32) {
        self.log.lock().unwrap().push((true, offset));
        self.regs.lock().unwrap()[usize::from(offset / 4)] = v;
    }
}

impl Recorder {
    /// Runs `f` and returns the accesses it made.
    fn during(&self, f: impl FnOnce()) -> Vec<(bool, u16)> {
        self.log.lock().unwrap().clear();
        f();
        self.log.lock().unwrap().clone()
    }
}

/// The indices of the command-register writes in `log`, each checked to
/// be followed at once by a read of the register.
fn read_back(what: &str, log: &[(bool, u16)]) -> usize {
    let writes: Vec<usize> = log
        .iter()
        .enumerate()
        .filter(|&(_, &(write, at))| write && at == COMMAND)
        .map(|(i, _)| i)
        .collect();
    for &w in &writes {
        assert_eq!(log.get(w + 1), Some(&(false, COMMAND)), "{what}: {log:?}");
    }
    writes.len()
}

#[test]
fn command_writes_are_in_effect_on_return() {
    let cfg: &'static Recorder = alloc::boxed::Box::leak(alloc::boxed::Box::new(Recorder {
        regs: Mutex::new([0; 1024]),
        log: Mutex::new(Vec::new()),
    }));
    let fake = Fake::default();
    fake.add(0, 4, 0, device(0x1af4, 0x1041, 0x02_00_00, 0));
    let function = crate::enumerate::scan(&fake, 0, 0, 0).remove(0);
    let a = function.addr;
    let child = PciChild {
        vpd: sync::Once::new(),
        cfg,
        function,
        bus: Arc::new(PciBusData {
            cfg,
            adr_paths: Vec::new(),
            intx: Vec::new(),
            depth: 0,
            upstream: crate::requester::Upstream::Host,
        }),
    };
    // A driver's enables: one write each, read back, nothing after.
    for (what, log) in [
        ("enable_memory", cfg.during(|| child.enable_memory())),
        ("enable_io", cfg.during(|| child.enable_io())),
        ("set_bus_master", cfg.during(|| child.set_bus_master(true))),
    ] {
        assert_eq!(read_back(what, &log), 1, "{what}: {log:?}");
        assert_eq!(log.last(), Some(&(false, COMMAND)), "{what}: {log:?}");
    }
    // BAR sizing turns decoding off and back on, each read back.
    cfg.write32(a, COMMAND, 0x2);
    let log = cfg.during(|| {
        let _ = bar::read_all(cfg, a, 6);
    });
    assert_eq!(read_back("BAR sizing", &log), 2);
}
