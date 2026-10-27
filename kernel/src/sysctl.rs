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
        Number::Fixed(kern::HOSTNAME),
        "hostname",
        "Hostname",
        // Unset until something sets it, as at FreeBSD's boot.
        Value::text(""),
    );
    add(
        k,
        Number::Auto,
        "vm_guest",
        "Virtual machine guest detected?",
        Value::text(vm_guest()),
    );
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
