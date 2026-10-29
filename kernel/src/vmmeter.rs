// SPDX-License-Identifier: BSD-2-Clause
//! System-wide statistics, as `top`, `vmstat`, `systat`, `uptime`, `w`,
//! and `ps` read them: `vm.stats.*` (pages by state, the paging and fork
//! counters, interrupts and context switches), `vm.vmtotal`,
//! `vm.loadavg`, `kern.cp_time` and `kern.cp_times`, `vm.swap_info` and
//! `vm.nswapdev` (there is no swap), the memory sizes under `hw`,
//! `kern.boottime`, `kern.lastpid`, `kern.fscale`, `kern.ccpu`,
//! `kern.clockrate`, `kern.smp.*`, `machdep.consdev`, and
//! `kern.devname` (`devname(3)`).
//!
//! Pages are counted as FreeBSD's queues would hold them, from what is
//! known: those memory objects hold (anonymous memory and the file
//! cache) are active, the rest in use is wired; nothing is inactive or
//! laundered, as nothing is paged out.
//!
//! Behavior reference: sys/vm/vm_meter.c, sys/kern/kern_mib.c,
//! sys/kern/kern_clock.c (`sysctl_kern_cp_time`, `sysctl_kern_clockrate`),
//! sys/kern/kern_tc.c (`kern.boottime`), sys/vm/swap_pager.c
//! (`sysctl_vm_swap_info`), sys/fs/devfs/devfs_devs.c (`sysctl_devname`).

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use abi::errno::Errno;
use abi::kinfo::{Clockinfo, Loadavg, Vmtotal};
use abi::sysctl::{ctlflag, ctltype, hw, kern, vm as vmn};
use sched::Sleepable;
use sysctl::{Number, ReadError, Value};

use crate::statclock::{self, CPUSTATES};
use crate::uaccess::{Plain, bytes_of};

// SAFETY: integers and explicit padding, no implicit padding.
unsafe impl Plain for Loadavg {}
// SAFETY: as above.
unsafe impl Plain for Vmtotal {}
// SAFETY: as above.
unsafe impl Plain for Clockinfo {}

/// Pages in all, free, and held by memory objects.
fn pages() -> (u64, u64, u64) {
    let st = crate::mm::phys_stats();
    let held = u64::try_from(crate::mm::space::OBJECT_PAGES.sum(Ordering::Relaxed)).unwrap_or(0);
    (
        st.managed,
        st.free,
        held.min(st.managed.saturating_sub(st.free)),
    )
}

fn wired() -> u64 {
    let (all, free, active) = pages();
    all.saturating_sub(free).saturating_sub(active)
}

/// FreeBSD's paging thresholds for `all` pages (`vm_pageout_init`'s
/// defaults): `v_free_min`, `v_free_target`, `v_free_reserved`,
/// `v_inactive_target`, `v_pageout_free_min`, `v_interrupt_free_min`,
/// `v_free_severe`.
fn thresholds(all: u64) -> [u64; 7] {
    let interrupt_free_min = 2;
    let pageout_free_min = 34;
    let free_reserved = pageout_free_min + interrupt_free_min * 4 / 2 + all / 768;
    let free_min = free_reserved + all / 200;
    let free_severe = free_min * 4 / 10 + free_reserved * 6 / 10;
    let free_target = 4 * free_min + free_reserved;
    let inactive_target = free_target * 3 / 2;
    [
        free_min,
        free_target,
        free_reserved,
        inactive_target,
        pageout_free_min,
        interrupt_free_min,
        free_severe,
    ]
}

/// Context switches system-wide.
fn switches() -> u64 {
    crate::proc::all()
        .iter()
        .map(|p| {
            let mut n = 0;
            p.each_thread(|t| {
                n += t.sched().map_or(0, |s| s.switches());
                true
            });
            n
        })
        .sum()
}

/// `vm.vmtotal` (`vmtotal`): threads by state, and memory.
fn vmtotal() -> Vmtotal {
    let mut t = Vmtotal::default();
    for p in crate::proc::all() {
        p.each_thread(|ut| {
            if let Some(s) = ut.sched() {
                let r = s.report();
                match r.state {
                    sched::State::Running | sched::State::Runnable => t.t_rq += 1,
                    // `td_priority <= PZERO`: a disk wait.
                    sched::State::Sleeping if !r.interruptible => t.t_dw += 1,
                    sched::State::Sleeping => t.t_sl += 1,
                    _ => {}
                }
            }
            true
        });
    }
    let (all, free, active) = pages();
    let _ = all;
    t.t_vm = active;
    t.t_avm = active;
    t.t_rm = active;
    t.t_arm = active;
    t.t_free = free;
    t
}

/// `kern.devname`: the name of the device whose `dev_t` is written
/// (`sysctl_devname`), as `devname(3)` asks.
fn devname(new: Option<&[u8]>, out: &mut Vec<u8>) -> Result<(), Errno> {
    let dev = new
        .and_then(|b| b.first_chunk::<8>())
        .map(|c| u64::from_ne_bytes(*c))
        .ok_or(Errno::EINVAL)?;
    let name = crate::fs::devfs::name_of_rdev(dev).ok_or(Errno::ENOENT)?;
    out.extend_from_slice(&name);
    out.push(0);
    Ok(())
}

/// Adds the nodes.
pub(crate) fn init(s: &Sleepable) {
    crate::sysctl::with(s, |t| {
        let root = t.root();
        let k = t.child(root, "kern").expect("kern node");
        let h = t.child(root, "hw").expect("hw node");
        let v = t.child(root, "vm").expect("vm node");
        let stats = t.child(v, "stats").expect("vm.stats");
        let svm = t.child(stats, "vm").expect("vm.stats.vm");
        let ssys = t
            .add_node(stats, Number::Auto, "sys", "VM meter sys stats")
            .expect("vm.stats.sys");
        t.add_node(stats, Number::Auto, "misc", "VM meter misc stats")
            .expect("vm.stats.misc");
        let mut add = |parent, number, name: &str, descr: &str, value| {
            t.add_value(parent, number, name, descr, value)
                .expect("vm meter node");
        };
        let u32_of =
            |f: fn() -> u64| Value::Uint(Box::new(move || u32::try_from(f()).unwrap_or(u32::MAX)));
        // The counts (`VM_STATS_PROC`, `VM_STATS_UINT`).
        add(
            svm,
            Number::Auto,
            "v_page_size",
            "Page size in bytes",
            Value::Uint(Box::new(|| 4096)),
        );
        add(
            svm,
            Number::Auto,
            "v_page_count",
            "Total number of pages in system",
            u32_of(|| pages().0),
        );
        add(
            svm,
            Number::Auto,
            "v_wire_count",
            "Wired pages",
            u32_of(wired),
        );
        add(
            svm,
            Number::Auto,
            "v_nofree_count",
            "Permanently allocated pages",
            Value::Uint(Box::new(|| 0)),
        );
        add(
            svm,
            Number::Auto,
            "v_active_count",
            "Active pages",
            u32_of(|| pages().2),
        );
        add(
            svm,
            Number::Auto,
            "v_inactive_count",
            "Inactive pages",
            Value::Uint(Box::new(|| 0)),
        );
        add(
            svm,
            Number::Auto,
            "v_laundry_count",
            "Pages eligible for laundering",
            Value::Uint(Box::new(|| 0)),
        );
        for (i, (name, descr)) in [
            ("v_free_min", "Minimum low-free-pages threshold"),
            ("v_free_target", "Pages desired free"),
            ("v_free_reserved", "Pages reserved for deadlock"),
            ("v_inactive_target", "Desired inactive pages"),
            ("v_pageout_free_min", "Min pages reserved for kernel"),
            ("v_interrupt_free_min", "Reserved pages for interrupt code"),
            ("v_free_severe", "Severe page depletion point"),
        ]
        .into_iter()
        .enumerate()
        {
            add(
                svm,
                Number::Auto,
                name,
                descr,
                Value::Uint(Box::new(move || {
                    u32::try_from(thresholds(pages().0)[i]).unwrap_or(u32::MAX)
                })),
            );
        }
        // The counters (`VM_STATS_VM`): nothing is paged or swapped.
        for (name, descr) in [
            ("v_vm_faults", "Address memory faults"),
            ("v_io_faults", "Page faults requiring I/O"),
            ("v_cow_faults", "Copy-on-write faults"),
            ("v_cow_optim", "Optimized COW faults"),
            ("v_zfod", "Pages zero-filled on demand"),
            ("v_ozfod", "Optimized zero fill pages"),
            ("v_swapin", "Swap pager pageins"),
            ("v_swapout", "Swap pager pageouts"),
            ("v_swappgsin", "Swap pages swapped in"),
            ("v_swappgsout", "Swap pages swapped out"),
            ("v_vnodein", "Vnode pager pageins"),
            ("v_vnodeout", "Vnode pager pageouts"),
            ("v_vnodepgsin", "Vnode pages paged in"),
            ("v_vnodepgsout", "Vnode pages paged out"),
            ("v_intrans", "In transit page faults"),
            ("v_reactivated", "Pages reactivated by pagedaemon"),
            ("v_pdwakeups", "Pagedaemon wakeups"),
            ("v_pdpages", "Pages analyzed by pagedaemon"),
            ("v_pdshortfalls", "Page reclamation shortfalls"),
            ("v_dfree", "Pages freed by pagedaemon"),
            ("v_pfree", "Pages freed by exiting processes"),
            ("v_tfree", "Total pages freed"),
            ("v_forks", "Number of fork() calls"),
            ("v_vforks", "Number of vfork() calls"),
            ("v_rforks", "Number of rfork() calls"),
            ("v_kthreads", "Number of fork() calls by kernel"),
            ("v_forkpages", "VM pages affected by fork()"),
            ("v_vforkpages", "VM pages affected by vfork()"),
            ("v_rforkpages", "VM pages affected by rfork()"),
            ("v_kthreadpages", "VM pages affected by fork() by kernel"),
        ] {
            add(svm, Number::Auto, name, descr, Value::U64(Box::new(|| 0)));
        }
        // `vm.stats.sys`.
        add(
            ssys,
            Number::Auto,
            "v_swtch",
            "Context switches",
            Value::U64(Box::new(switches)),
        );
        add(
            ssys,
            Number::Auto,
            "v_trap",
            "Traps",
            Value::U64(Box::new(|| 0)),
        );
        add(
            ssys,
            Number::Auto,
            "v_syscall",
            "System calls",
            Value::U64(Box::new(|| 0)),
        );
        add(
            ssys,
            Number::Auto,
            "v_intr",
            "Device interrupts",
            Value::U64(Box::new(|| {
                statclock::INTERRUPTS
                    .iter()
                    .map(|c| c.load(Ordering::Relaxed))
                    .sum()
            })),
        );
        add(
            ssys,
            Number::Auto,
            "v_soft",
            "Software interrupts",
            Value::U64(Box::new(|| 0)),
        );
        // `vm.v_free_min` and the other fixed ones.
        for (number, name, i) in [
            (vmn::V_FREE_MIN, "v_free_min", 0),
            (vmn::V_FREE_TARGET, "v_free_target", 1),
            (vmn::V_FREE_RESERVED, "v_free_reserved", 2),
            (vmn::V_INACTIVE_TARGET, "v_inactive_target", 3),
            (vmn::V_PAGEOUT_FREE_MIN, "v_pageout_free_min", 4),
        ] {
            add(
                v,
                Number::Fixed(number),
                name,
                "",
                Value::Uint(Box::new(move || {
                    u32::try_from(thresholds(pages().0)[i]).unwrap_or(u32::MAX)
                })),
            );
        }
        add(
            v,
            Number::Fixed(vmn::LOADAVG),
            "loadavg",
            "Machine loadaverage history",
            Value::Opaque {
                format: "S,loadavg",
                read: Box::new(|out| {
                    let l = statclock::loadavg();
                    let la = Loadavg {
                        ldavg: l.map(|x| u32::try_from(x).unwrap_or(u32::MAX)),
                        _pad: 0,
                        fscale: i64::try_from(statclock::FSCALE).unwrap_or(0),
                    };
                    out.extend_from_slice(bytes_of(&la));
                }),
            },
        );
        add(
            v,
            Number::Fixed(vmn::TOTAL),
            "vmtotal",
            "System virtual memory statistics",
            Value::Opaque {
                format: "S,vmtotal",
                read: Box::new(|out| out.extend_from_slice(bytes_of(&vmtotal()))),
            },
        );
        // `dmmax`: swap's interleave, which libkvm reads at open.
        add(
            v,
            Number::Auto,
            "dmmax",
            "Maximum size of a swap block",
            Value::int(32),
        );
        add(
            v,
            Number::Auto,
            "nswapdev",
            "Number of swap devices",
            Value::int(0),
        );
        add(
            v,
            Number::Auto,
            "swap_total",
            "Total amount of available swap storage.",
            Value::U64(Box::new(|| 0)),
        );
        add(
            v,
            Number::Auto,
            "swap_reserved",
            "Amount of swap storage needed to back all allocated anonymous memory.",
            Value::U64(Box::new(|| 0)),
        );
        add(
            v,
            Number::Auto,
            "swap_maxpages",
            "Maximum amount of swap supported",
            Value::Ulong(Box::new(|| 0)),
        );
        add(
            v,
            Number::Auto,
            "swap_async_max",
            "Maximum running async swap ops",
            Value::int(4),
        );
        // `hw.physmem` and kin (`ctob(physmem)`).
        let bytes = |f: fn() -> u64| Value::Ulong(Box::new(move || f() * 4096));
        add(
            h,
            Number::Fixed(hw::PHYSMEM),
            "physmem",
            "Amount of physical memory (in bytes)",
            bytes(|| pages().0),
        );
        add(
            h,
            Number::Fixed(hw::REALMEM),
            "realmem",
            "Amount of memory (in bytes) reported by the firmware",
            bytes(|| pages().0),
        );
        add(
            h,
            Number::Fixed(hw::USERMEM),
            "usermem",
            "Amount of memory (in bytes) which is not wired",
            bytes(|| pages().0.saturating_sub(wired())),
        );
        add(
            h,
            Number::Auto,
            "availpages",
            "Amount of physical memory (in pages)",
            Value::Long(Box::new(|| i64::try_from(pages().0).unwrap_or(i64::MAX))),
        );
        // `kern`.
        add(
            k,
            Number::Fixed(kern::BOOTTIME),
            "boottime",
            "System boottime",
            Value::Opaque {
                format: "S,timeval",
                read: Box::new(|out| {
                    let tv = crate::kinfo::timeval(crate::time::realtime_base_ns() / 1000);
                    out.extend_from_slice(&tv.tv_sec.to_ne_bytes());
                    out.extend_from_slice(&tv.tv_usec.to_ne_bytes());
                }),
            },
        );
        add(
            k,
            Number::Auto,
            "lastpid",
            "Last used PID",
            Value::Int(Box::new(crate::proc::last_pid)),
        );
        add(
            k,
            Number::Auto,
            "fscale",
            "",
            Value::int(1 << statclock::FSHIFT),
        );
        // `ARG_MAX`.
        add(
            k,
            Number::Fixed(kern::ARGMAX),
            "argmax",
            "Maximum bytes of argument to execve(2)",
            Value::int(2 * 256 * 1024),
        );
        add(
            k,
            Number::Fixed(kern::BOOTFILE),
            "bootfile",
            "Name of kernel file booted",
            Value::text("/boot/kernel/kernel"),
        );
        // ULE leaves FreeBSD's decay factor 0.
        add(
            k,
            Number::Auto,
            "ccpu",
            "Decay factor used for updating %CPU",
            Value::Uint(Box::new(|| 0)),
        );
        add(
            k,
            Number::Fixed(kern::CLOCKRATE),
            "clockrate",
            "Rate and period of various kernel clocks",
            Value::Opaque {
                format: "S,clockinfo",
                read: Box::new(|out| {
                    let c = Clockinfo {
                        hz: statclock::HZ,
                        tick: 1_000_000 / statclock::HZ,
                        spare: 0,
                        stathz: i32::try_from(statclock::STATHZ).unwrap_or(0),
                        profhz: statclock::PROFHZ,
                    };
                    out.extend_from_slice(bytes_of(&c));
                }),
            },
        );
        let cpus = || sched::online_cpus();
        drop(add);
        t.add_formatted(
            k,
            Number::Auto,
            "cp_time",
            "CPU time statistics",
            ctltype::LONG,
            "LU",
            Value::Opaque {
                format: "LU",
                read: Box::new(move |out| {
                    let mut sum = [0u64; CPUSTATES];
                    for c in 0..cpus() {
                        for (s, v) in sum.iter_mut().zip(statclock::cp_time(c)) {
                            *s += v;
                        }
                    }
                    for v in sum {
                        out.extend_from_slice(&v.to_ne_bytes());
                    }
                }),
            },
        )
        .expect("kern.cp_time");
        t.add_formatted(
            k,
            Number::Auto,
            "cp_times",
            "per-CPU time statistics",
            ctltype::LONG,
            "LU",
            Value::Opaque {
                format: "LU",
                read: Box::new(move |out| {
                    for c in 0..cpus() {
                        for v in statclock::cp_time(c) {
                            out.extend_from_slice(&v.to_ne_bytes());
                        }
                    }
                }),
            },
        )
        .expect("kern.cp_times");
        // `kern.smp`.
        let smp = t
            .add_node(k, Number::Auto, "smp", "Kernel SMP")
            .expect("kern.smp");
        let n =
            |f: fn() -> usize| Value::Int(Box::new(move || i32::try_from(f()).unwrap_or(i32::MAX)));
        t.add_value(
            smp,
            Number::Auto,
            "maxid",
            "Max CPU ID.",
            n(|| sched::online_cpus().saturating_sub(1)),
        )
        .expect("kern.smp.maxid");
        t.add_value(
            smp,
            Number::Auto,
            "maxcpus",
            "Max number of CPUs that the system was compiled for.",
            Value::int(i32::try_from(hal::MAX_CPUS).unwrap_or(i32::MAX)),
        )
        .expect("kern.smp.maxcpus");
        t.add_value(
            smp,
            Number::Auto,
            "active",
            "Indicates system is running in SMP mode",
            n(|| usize::from(sched::online_cpus() > 1)),
        )
        .expect("kern.smp.active");
        t.add_value(
            smp,
            Number::Auto,
            "cpus",
            "Number of CPUs online",
            n(sched::online_cpus),
        )
        .expect("kern.smp.cpus");
        // `vm.swap_info`: no device, so every one is `ENOENT`.
        t.add_handler(
            v,
            Number::Auto,
            "swap_info",
            "Swap statistics by device",
            Arc::new(|r: &[i32], _out: &mut Vec<u8>| match r {
                [_] => Err(ReadError::Failed(Errno::ENOENT)),
                _ => Err(ReadError::Invalid),
            }),
        )
        .expect("vm.swap_info");
        // `vfs.bufspace`: the buffer cache's bytes.
        let vfs = t.child(root, "vfs").expect("vfs node");
        if t.child(vfs, "bufspace").is_none() {
            t.add_value(
                vfs,
                Number::Auto,
                "bufspace",
                "Physical memory used for buffers",
                Value::Long(Box::new(|| 0)),
            )
            .expect("vfs.bufspace");
        }
        // `machdep.consdev`: the console's terminal (`w` for a login on
        // `/dev/console`).
        let md = t.child(root, "machdep").expect("machdep node");
        t.add_value(
            md,
            Number::Auto,
            "consdev",
            "Console device",
            Value::Opaque {
                format: "T,dev_t",
                read: Box::new(|out| {
                    let dev = crate::tty::console()
                        .and_then(|c| crate::fs::devfs::rdev_of(c.name().as_bytes()))
                        .unwrap_or(abi::kinfo::NODEV);
                    out.extend_from_slice(&dev.to_ne_bytes());
                }),
            },
        )
        .expect("machdep.consdev");
        t.add_query(
            k,
            Number::Auto,
            "devname",
            "devname(3) handler",
            ctltype::OPAQUE | ctlflag::RW | ctlflag::ANYBODY,
            "",
            Arc::new(devname),
        )
        .expect("kern.devname");
    });
}
