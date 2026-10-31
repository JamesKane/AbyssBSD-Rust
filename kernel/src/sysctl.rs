// SPDX-License-Identifier: BSD-2-Clause
//! The kernel's sysctl tree (`sys/sysctl`), behind an sx lock, with the
//! nodes every boot has: the identity userland checks first
//! (docs/COMPATIBILITY.md §2.3) and basic hardware facts. Subsystems add
//! their own nodes through [`with`].

use alloc::string::String;

use abi::sysctl::{hw, identity, kern};
use sched::Sleepable;
use sched::lock::{LockClass, SxLock};
use sync::Once;
use sysctl::{Number, Tree, Value};

use crate::arch;

static SYSCTL_LOCK: LockClass = LockClass::new("sysctl", 700);
static TREE: Once<SxLock<Tree>> = Once::new();

/// `hw.machine`: the machine class.
const MACHINE: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else {
    "arm64"
};
/// `hw.machine_arch`: the architecture.
const MACHINE_ARCH: &str = if cfg!(target_arch = "x86_64") {
    "amd64"
} else {
    "aarch64"
};

/// `kern.vm_guest`: the hypervisor FreeBSD's `identcpu` would find, by
/// the CPUID hypervisor bit and the vendor signature at 0x40000000 (and
/// the leaves past it, where one may stand in for another); "generic"
/// for one it does not know, "none" on bare metal. arm64 says "none", as
/// FreeBSD's does.
fn vm_guest() -> &'static str {
    #[cfg(target_arch = "x86_64")]
    {
        use core::arch::x86_64::__cpuid;
        const KNOWN: &[(&[u8; 12], &str)] = &[
            (b"XenVMMXenVMM", "xen"),
            (b"Microsoft Hv", "hv"),
            (b"VMwareVMware", "vmware"),
            (b"KVMKVMKVM\0\0\0", "kvm"),
            (b"bhyve bhyve ", "bhyve"),
            (b"VBoxVBoxVBox", "vbox"),
            (b"___ NVMM ___", "nvmm"),
        ];
        if __cpuid(1).ecx & (1 << 31) == 0 {
            return "none";
        }
        let mut guest = "generic";
        for leaf in (0x4000_0000u32..0x4001_0000).step_by(0x100) {
            let r = __cpuid(leaf);
            if r.eax < leaf {
                continue;
            }
            let mut sig = [0u8; 12];
            sig[..4].copy_from_slice(&r.ebx.to_le_bytes());
            sig[4..8].copy_from_slice(&r.ecx.to_le_bytes());
            sig[8..].copy_from_slice(&r.edx.to_le_bytes());
            if let Some((_, name)) = KNOWN.iter().find(|(s, _)| **s == sig) {
                guest = name;
                // Xen and others may show Hyper-V's signature first.
                if *name != "hv" {
                    break;
                }
            }
        }
        guest
    }
    #[cfg(target_arch = "aarch64")]
    "none"
}

/// Builds the tree. Call once at boot, with the heap up.
pub(crate) fn init() {
    let mut t = Tree::new();
    let root = t.root();
    let k = t.child(root, "kern").expect("kern node");
    let h = t.child(root, "hw").expect("hw node");
    // `vm.max_user_wired`: four fifths of the memory free once the system
    // is up (`vm_pageout_init`); writable (`SYSCTL_ULONG`, `CTLFLAG_RW`).
    {
        use crate::mm::space::MAX_USER_WIRED;
        use core::sync::atomic::Ordering;
        let v = t.child(root, "vm").expect("vm node");
        MAX_USER_WIRED.store(crate::mm::free_frames() * 4 / 5, Ordering::Relaxed);
        t.add_writable(
            v,
            Number::Auto,
            "max_user_wired",
            "system-wide limit to user-wired page count",
            0,
            Value::Ulong(alloc::boxed::Box::new(|| {
                MAX_USER_WIRED.load(Ordering::Relaxed)
            })),
            alloc::sync::Arc::new(|b: &[u8]| {
                let v = b
                    .first_chunk::<8>()
                    .map(|c| u64::from_ne_bytes(*c))
                    .ok_or(abi::errno::Errno::EINVAL)?;
                MAX_USER_WIRED.store(v, Ordering::Relaxed);
                Ok(())
            }),
        )
        .expect("vm.max_user_wired");
        let stats = t
            .add_node(v, Number::Auto, "stats", "VM meter stats")
            .expect("vm.stats");
        let svm = t
            .add_node(stats, Number::Auto, "vm", "VM meter vm stats")
            .expect("vm.stats.vm");
        t.add_value(
            svm,
            Number::Auto,
            "v_free_count",
            "Free pages",
            Value::Uint(alloc::boxed::Box::new(|| {
                u32::try_from(crate::mm::free_frames()).unwrap_or(u32::MAX)
            })),
        )
        .expect("vm.stats.vm.v_free_count");
        t.add_value(
            svm,
            Number::Auto,
            "v_user_wire_count",
            "User-wired virtual memory",
            Value::Ulong(alloc::boxed::Box::new(|| {
                crate::mm::space::USER_WIRED.load(core::sync::atomic::Ordering::Relaxed)
            })),
        )
        .expect("vm.stats.vm.v_user_wire_count");
    }
    let mut add = |parent, number, name, descr, value| {
        t.add_value(parent, number, name, descr, value)
            .expect("boot sysctl node");
    };
    add(
        k,
        Number::Fixed(kern::OSTYPE),
        "ostype",
        "Operating system type",
        Value::text(identity::OSTYPE),
    );
    add(
        k,
        Number::Fixed(kern::OSRELEASE),
        "osrelease",
        "Operating system release",
        Value::text(identity::OSRELEASE),
    );
    add(
        k,
        Number::Fixed(kern::OSREV),
        "osrevision",
        "Operating system revision",
        Value::int(identity::OSREVISION),
    );
    add(
        k,
        Number::Fixed(kern::VERSION),
        "version",
        "Kernel version",
        Value::Str(alloc::boxed::Box::new(|out: &mut String| {
            use core::fmt::Write;
            let _ = writeln!(
                out,
                "AbyssBSD {} ({}, {} ABI)",
                env!("CARGO_PKG_VERSION"),
                arch::NAME,
                identity::OSRELEASE
            );
        })),
    );
    add(
        k,
        Number::Fixed(kern::OSRELDATE),
        "osreldate",
        "Kernel version",
        Value::int(identity::OSRELDATE),
    );
    add(
        k,
        Number::Fixed(kern::NGROUPS),
        "ngroups",
        "Maximum number of supplemental groups a user can belong to",
        Value::int(i32::try_from(crate::cred::NGROUPS_MAX).unwrap_or(i32::MAX)),
    );
    add(
        h,
        Number::Fixed(hw::NCPU),
        "ncpu",
        "Number of active CPUs",
        Value::Int(alloc::boxed::Box::new(|| {
            i32::try_from(sched::online_cpus()).unwrap_or(i32::MAX)
        })),
    );
    add(
        h,
        Number::Fixed(hw::MACHINE),
        "machine",
        "Machine class",
        Value::text(MACHINE),
    );
    add(
        h,
        Number::Fixed(hw::PAGESIZE),
        "pagesize",
        "System memory page size",
        Value::int(4096),
    );
    add(
        h,
        Number::Fixed(hw::MACHINE_ARCH),
        "machine_arch",
        "System architecture",
        Value::text(MACHINE_ARCH),
    );
    add(
        k,
        Number::Auto,
        "vm_guest",
        "Virtual machine guest detected?",
        Value::text(vm_guest()),
    );
    // System V IPC's limits (`kern.ipc`).
    {
        use crate::sysv::limits as l;
        let ipc = t.add_node(k, Number::Auto, "ipc", "IPC").expect("kern.ipc");
        let ulongs: [(&str, &str, u64); 5] = [
            ("shmmax", "Maximum shared memory segment size", l::SHMMAX),
            ("shmmin", "Minimum shared memory segment size", l::SHMMIN),
            (
                "shmmni",
                "Number of shared memory identifiers",
                l::SHMMNI as u64,
            ),
            ("shmseg", "Number of segments per process", l::SHMSEG as u64),
            (
                "shmall",
                "Maximum number of pages available for shared memory",
                l::SHMALL,
            ),
        ];
        for (name, descr, v) in ulongs {
            t.add_value(
                ipc,
                Number::Auto,
                name,
                descr,
                Value::Ulong(alloc::boxed::Box::new(move || v)),
            )
            .expect("kern.ipc.shm*");
        }
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_possible_wrap,
            reason = "small limits"
        )]
        let ints: [(&str, &str, i32); 15] = [
            (
                "semmni",
                "Number of semaphore identifiers",
                l::SEMMNI as i32,
            ),
            (
                "semmns",
                "Maximum number of semaphores in the system",
                l::SEMMNS as i32,
            ),
            (
                "semmnu",
                "Maximum number of undo structures in the system",
                l::SEMMNU as i32,
            ),
            ("semmsl", "Max semaphores per id", l::SEMMSL as i32),
            ("semopm", "Max operations per semop call", l::SEMOPM as i32),
            ("semume", "Max undo entries per process", l::SEMUME as i32),
            ("semvmx", "Semaphore maximum value", l::SEMVMX),
            ("semaem", "Adjust on exit max value", l::SEMAEM),
            ("msgmax", "Maximum message size", l::MSGMAX as i32),
            (
                "msgmni",
                "Number of message queue identifiers",
                l::MSGMNI as i32,
            ),
            (
                "msgmnb",
                "Maximum number of bytes in a queue",
                l::MSGMNB as i32,
            ),
            (
                "msgtql",
                "Maximum number of messages in the system",
                l::MSGTQL as i32,
            ),
            ("msgssz", "Size of a message segment", l::MSGSSZ as i32),
            ("msgseg", "Number of message segments", l::MSGSEG as i32),
            (
                "shm_allow_removed",
                "Enable/Disable attachment to attached segments marked for removal",
                1,
            ),
        ];
        for (name, descr, v) in ints {
            t.add_value(ipc, Number::Auto, name, descr, Value::int(v))
                .expect("kern.ipc.sem*, msg*");
        }
    }
    // The mbuf zones' limits, from memory (`tunable_mbinit`). Each may
    // only grow, and a cluster zone's only while the mbufs cover every
    // cluster (`sysctl_nmbclusters`).
    {
        use abi::errno::Errno;
        use mbuf::zone::{self, Limits, Zone};
        let ipc = t.child(k, "ipc").expect("kern.ipc");
        let l = Limits::for_memory(crate::mm::phys_stats().managed * 4096);
        l.apply();
        let maxmbufmem = i64::try_from(l.maxmbufmem).unwrap_or(i64::MAX);
        t.add_value(
            ipc,
            Number::Auto,
            "maxmbufmem",
            "Maximum real memory allocatable to various mbuf types",
            Value::Long(alloc::boxed::Box::new(move || maxmbufmem)),
        )
        .expect("kern.ipc.maxmbufmem");
        fn covered() -> bool {
            let clusters = [&zone::CLUSTER, &zone::JUMBOP, &zone::JUMBO9, &zone::JUMBO16]
                .iter()
                .map(|z| z.limit())
                .fold(0u64, u64::saturating_add);
            zone::MBUF.limit() >= clusters
        }
        let limits: [(&str, &str, &'static Zone, bool); 5] = [
            (
                "nmbclusters",
                "Maximum number of mbuf clusters allowed",
                &zone::CLUSTER,
                true,
            ),
            (
                "nmbjumbop",
                "Maximum number of mbuf page size jumbo clusters allowed",
                &zone::JUMBOP,
                true,
            ),
            (
                "nmbjumbo9",
                "Maximum number of mbuf 9k jumbo clusters allowed",
                &zone::JUMBO9,
                true,
            ),
            (
                "nmbjumbo16",
                "Maximum number of mbuf 16k jumbo clusters allowed",
                &zone::JUMBO16,
                true,
            ),
            (
                "nmbufs",
                "Maximum number of mbufs allowed",
                &zone::MBUF,
                false,
            ),
        ];
        for (name, descr, z, cluster) in limits {
            t.add_writable(
                ipc,
                Number::Auto,
                name,
                descr,
                0,
                Value::Int(alloc::boxed::Box::new(|| {
                    i32::try_from(z.limit()).unwrap_or(i32::MAX)
                })),
                sysctl::int_writer(move |v| {
                    let old = i64::try_from(z.limit()).unwrap_or(i64::MAX);
                    if i64::from(v) == old {
                        return Ok(());
                    }
                    if i64::from(v) > old && (!cluster || covered()) {
                        z.set_limit(u64::try_from(v).map_err(|_| Errno::EINVAL)?);
                        return Ok(());
                    }
                    Err(Errno::EINVAL)
                }),
            )
            .expect("kern.ipc.nmb*");
        }
        let hdrs: [(&str, &str, u32); 3] = [
            (
                "max_linkhdr",
                "Size of largest link layer header",
                mbuf::MAX_LINKHDR,
            ),
            (
                "max_protohdr",
                "Size of largest protocol layer header",
                mbuf::MAX_PROTOHDR,
            ),
            (
                "max_hdr",
                "Size of largest link plus protocol header",
                mbuf::MAX_HDR,
            ),
        ];
        for (name, descr, v) in hdrs {
            #[allow(clippy::cast_possible_wrap, reason = "under 100")]
            t.add_value(ipc, Number::Auto, name, descr, Value::int(v as i32))
                .expect("kern.ipc.max_*hdr");
        }
    }
    // UNIX-domain sockets' sizes (`net.local`, read-only here) and the
    // socket layer's limits.
    {
        use core::sync::atomic::Ordering::Relaxed;
        use socket::space;
        let ipc = t.child(k, "ipc").expect("kern.ipc");
        let ulong = |v: u64| Value::Ulong(alloc::boxed::Box::new(move || v));
        t.add_value(
            ipc,
            Number::Auto,
            "maxsockbuf",
            "Maximum socket buffer size",
            ulong(8 << 20),
        )
        .expect("kern.ipc.maxsockbuf");
        t.add_value(
            ipc,
            Number::Auto,
            "sockbuf_waste_factor",
            "Socket buffer size waste factor",
            ulong(8),
        )
        .expect("kern.ipc.sockbuf_waste_factor");
        for name in ["somaxconn", "soacceptqueue"] {
            t.add_value(
                ipc,
                Number::Auto,
                name,
                "Maximum listen socket pending connection accept queue size",
                Value::int(128),
            )
            .expect("kern.ipc.somaxconn");
        }
        let net = t.child(root, "net").expect("net node");
        let local = t
            .add_node(net, Number::Fixed(1), "local", "Local domain")
            .expect("net.local");
        let nodes = [
            (abi::socket::SOCK_STREAM, "stream", "SOCK_STREAM"),
            (abi::socket::SOCK_DGRAM, "dgram", "SOCK_DGRAM"),
            (abi::socket::SOCK_SEQPACKET, "seqpacket", "SOCK_SEQPACKET"),
        ];
        let read = |a: &'static core::sync::atomic::AtomicU64| {
            Value::Ulong(alloc::boxed::Box::new(move || a.load(Relaxed)))
        };
        for (num, name, descr) in nodes {
            let n = t
                .add_node(local, Number::Fixed(num.cast_signed()), name, descr)
                .expect("net.local.*");
            let vals: &[(&str, &str, &'static core::sync::atomic::AtomicU64)] = match name {
                "stream" => &[
                    (
                        "sendspace",
                        "Default stream send space.",
                        &space::STREAM_SEND,
                    ),
                    (
                        "recvspace",
                        "Default stream receive space.",
                        &space::STREAM_RECV,
                    ),
                ],
                "dgram" => &[
                    ("maxdgram", "Maximum datagram size.", &space::DGRAM_MAX),
                    (
                        "recvspace",
                        "Default datagram receive space.",
                        &space::DGRAM_RECV,
                    ),
                ],
                _ => &[
                    (
                        "maxseqpacket",
                        "Default seqpacket send space.",
                        &space::SEQPACKET_SEND,
                    ),
                    (
                        "recvspace",
                        "Default seqpacket receive space.",
                        &space::SEQPACKET_RECV,
                    ),
                ],
            };
            for &(v, d, a) in vals {
                t.add_value(n, Number::Auto, v, d, read(a))
                    .expect("net.local.*.*");
            }
        }
        t.add_value(
            local,
            Number::Auto,
            "inflight",
            "File descriptors in flight.",
            Value::Int(alloc::boxed::Box::new(|| {
                i32::try_from(socket::inflight()).unwrap_or(i32::MAX)
            })),
        )
        .expect("net.local.inflight");
        t.add_value(
            local,
            Number::Auto,
            "deferred",
            "File descriptors deferred to taskqueue for close.",
            Value::int(0),
        )
        .expect("net.local.deferred");
    }
    let sched_node = t
        .add_node(k, Number::Auto, "sched", "Scheduler")
        .expect("kern.sched");
    t.add_value(
        sched_node,
        Number::Auto,
        "cpusetsize",
        "sizeof(cpuset_t)",
        Value::int(i32::try_from(abi::thr::CPUSET_BYTES).unwrap_or(i32::MAX)),
    )
    .expect("kern.sched.cpusetsize");
    t.add_value(
        sched_node,
        Number::Auto,
        "cpusetsizemin",
        "The minimum size of cpuset_t allowed by the kernel",
        // `howmany(mp_maxid + 1, NBBY)`.
        Value::Uint(alloc::boxed::Box::new(|| {
            u32::try_from(sched::online_cpus().div_ceil(8)).unwrap_or(u32::MAX)
        })),
    )
    .expect("kern.sched.cpusetsizemin");
    // `kern.vty`: the console's terminal driver, which `kbdcontrol` and
    // `vidcontrol` check for `vt`.
    t.add_value(
        k,
        Number::Auto,
        "vty",
        "Console vty driver",
        Value::text("vt"),
    )
    .expect("kern.vty");
    let vt = t
        .add_node(k, Number::Auto, "vt", "vt(4) parameters")
        .expect("kern.vt node");
    t.add_value(
        vt,
        Number::Auto,
        "enable_bell",
        "Enable bell",
        Value::int(i32::from(crate::vtcons::enable_bell())),
    )
    .expect("kern.vt.enable_bell");
    let abyss = t
        .add_node(k, Number::Auto, "abyss", "AbyssBSD extensions")
        .expect("kern.abyss node");
    t.add_value(
        abyss,
        Number::Auto,
        "version",
        "AbyssBSD version",
        Value::text(env!("CARGO_PKG_VERSION")),
    )
    .expect("kern.abyss.version");
    assert!(
        TREE.set(SxLock::new(&SYSCTL_LOCK, t)).is_ok(),
        "sysctl::init called twice"
    );
}

/// Runs `f` on the tree, exclusively (to add or remove nodes).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))]
pub(crate) fn with<R>(s: &Sleepable, f: impl FnOnce(&mut Tree) -> R) -> R {
    f(&mut tree().write(s))
}

/// Reads the value at `oid` as the syscall will: a node handler is called
/// after the tree's lock is dropped, so it may take lower-ordered locks.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))]
pub(crate) fn read_oid(
    s: &Sleepable,
    oid: &[i32],
    out: &mut alloc::vec::Vec<u8>,
) -> Result<(), sysctl::ReadError> {
    let handler = {
        let t = tree().read(s);
        match t.handler(oid) {
            Some(h) => h,
            None => return t.read(oid, out),
        }
    };
    let (h, rest) = handler;
    h(&rest, out)
}

/// Runs `f` on the tree, shared (to read).
#[cfg_attr(not(feature = "ktest"), allow(dead_code))]
pub(crate) fn read<R>(s: &Sleepable, f: impl FnOnce(&Tree) -> R) -> R {
    f(&tree().read(s))
}

fn tree() -> &'static SxLock<Tree> {
    TREE.get().expect("sysctl tree used before init")
}

/// The `kern.features` AbyssBSD has, of those FreeBSD 15.1's GENERIC
/// lists (`FEATURE()`), which `feature_present(3)` asks about.
const FEATURES: &[(&str, &str)] = &[
    ("security_capabilities", "Capsicum Capabilities"),
    ("security_capability_mode", "Capsicum Capability Mode"),
    ("process_descriptors", "Process Descriptors"),
    ("posix_shm", "POSIX shared memory"),
    ("sysv_shm", "System V shared memory segments support"),
    ("sysv_sem", "System V semaphores support"),
    ("sysv_msg", "System V message queues support"),
    (
        "kposix_priority_scheduling",
        "POSIX P1003.1B realtime extensions",
    ),
    ("softupdates", "FFS soft-updates support"),
    ("evdev", "Input event devices support"),
    ("evdev_support", "evdev support in legacy drivers"),
    ("vimage", "VIMAGE kernel virtualization"),
];

/// Adds `kern.features`.
pub(crate) fn features_init(s: &Sleepable) {
    with(s, |t| {
        let root = t.root();
        let k = t.child(root, "kern").expect("kern node");
        let f = t
            .add_node(k, Number::Auto, "features", "Kernel Features")
            .expect("kern.features");
        for (name, descr) in FEATURES {
            t.add_value(f, Number::Auto, name, descr, Value::int(1))
                .expect("a feature");
        }
    });
}

/// What a program in capability mode may read (FreeBSD's `CTLFLAG_CAPRD`
/// nodes): the system's identity and shape, which libc and the run-time
/// linker ask for.
const CAPABILITY_READABLE: &[&str] = &[
    "kern.ostype",
    "kern.osrelease",
    "kern.osrevision",
    "kern.version",
    "kern.argmax",
    "kern.posix1version",
    "kern.ngroups",
    "kern.job_control",
    "kern.saved_ids",
    "kern.osreldate",
    "kern.arandom",
    "kern.ps_strings",
    "kern.usrstack",
    "kern.maxphys",
    "kern.supported_archs",
    "kern.boot_id",
    "kern.build_id",
    "kern.iov_max",
    "kern.proc.nfds",
    "kern.sched.cpusetsize",
    "kern.smp.maxid",
    "kern.smp.maxcpus",
    "kern.smp.disabled",
    "kern.smp.cpus",
    "kern.smp.threads_per_core",
    "kern.smp.cores",
    "hw.machine",
    "hw.model",
    "hw.ncpu",
    "hw.byteorder",
    "hw.pagesize",
    "hw.pagesizes",
    "hw.machine_arch",
    "hw.floatingpoint",
    "vfs.aio.max_aio_queue_per_proc",
];

/// Marks [`CAPABILITY_READABLE`] and every `kern.features` and `p1003_1b`
/// node readable in capability mode, once the tree is built.
pub(crate) fn mark_capability_readable(s: &Sleepable) {
    with(s, |t| {
        let mut ids: alloc::vec::Vec<_> = CAPABILITY_READABLE
            .iter()
            .filter_map(|n| t.name2oid(n))
            .filter_map(|o| t.lookup(&o))
            .collect();
        for parent in ["kern.features", "p1003_1b", "kern.p1003_1b"] {
            let Some(o) = t.name2oid(parent) else {
                continue;
            };
            let mut next = t.next(&o, false);
            while let Some(n) = next {
                if !n.starts_with(&o) {
                    break;
                }
                ids.extend(t.lookup(&n));
                next = t.next(&n, false);
            }
        }
        for id in ids {
            t.set_flags(id, abi::sysctl::ctlflag::CAPRD);
        }
    });
}
