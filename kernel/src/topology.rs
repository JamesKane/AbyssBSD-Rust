// SPDX-License-Identifier: BSD-2-Clause
//! CPU identification and topology: what each CPU is (MIDR / CPUID), where
//! it sits (package, cluster, core, SMT thread: from the PPTT on aarch64,
//! CPUID leaf 0xB on x86), and its relative capacity, which the scheduler
//! uses to prefer faster CPUs.
//!
//! Capacities are a first approximation by core type (little cores, and
//! x86 hybrid efficiency cores, at about half); CPPC (`_CPC`) replaces
//! them with each processor's highest performance level, scaled so the
//! fastest is 1024, once ACPI is up.
//!
//! Behavior reference: sys/arm64/arm64/identcpu.c, sys/x86/x86/mp_x86.c
//! (topology), sys/kern/subr_smp.c (cpu_topo).

use alloc::vec::Vec;

use hal::{CpuId, MAX_CPUS};
use sync::SpinLock;

/// What kind of CPU this is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ident {
    /// `MIDR_EL1`'s fields.
    #[cfg_attr(target_arch = "x86_64", allow(dead_code))]
    Arm {
        implementer: u8,
        part: u16,
        variant: u8,
        revision: u8,
    },
    /// CPUID's vendor, signature, and (on hybrid parts) core type.
    #[cfg_attr(target_arch = "aarch64", allow(dead_code))]
    X86 {
        vendor: [u8; 12],
        family: u32,
        model: u32,
        stepping: u32,
        core_type: Option<u8>,
    },
}

impl Ident {
    /// This CPU's identity.
    pub(crate) fn this_cpu() -> Self {
        #[cfg(target_arch = "aarch64")]
        {
            let m = crate::arch::cpu::midr();
            let field = |shift: u32, mask: u64| (m >> shift) & mask;
            Self::Arm {
                implementer: u8::try_from(field(24, 0xff)).unwrap_or(0),
                part: u16::try_from(field(4, 0xfff)).unwrap_or(0),
                variant: u8::try_from(field(20, 0xf)).unwrap_or(0),
                revision: u8::try_from(field(0, 0xf)).unwrap_or(0),
            }
        }
        #[cfg(target_arch = "x86_64")]
        {
            use core::arch::x86_64::{__cpuid, __cpuid_count};
            let v = __cpuid(0);
            let mut vendor = [0u8; 12];
            vendor[..4].copy_from_slice(&v.ebx.to_le_bytes());
            vendor[4..8].copy_from_slice(&v.edx.to_le_bytes());
            vendor[8..].copy_from_slice(&v.ecx.to_le_bytes());
            let sig = __cpuid(1).eax;
            let base_family = (sig >> 8) & 0xf;
            let family = if base_family == 0xf {
                base_family + ((sig >> 20) & 0xff)
            } else {
                base_family
            };
            let model = if base_family >= 6 {
                ((sig >> 4) & 0xf) | (((sig >> 16) & 0xf) << 4)
            } else {
                (sig >> 4) & 0xf
            };
            let hybrid = v.eax >= 7 && __cpuid_count(7, 0).edx & (1 << 15) != 0;
            let core_type = (hybrid && v.eax >= 0x1a)
                .then(|| u8::try_from(__cpuid_count(0x1a, 0).eax >> 24).unwrap_or(0));
            Self::X86 {
                vendor,
                family,
                model,
                stepping: sig & 0xf,
                core_type,
            }
        }
    }

    /// A human-readable name.
    pub(crate) fn name(&self) -> &'static str {
        match *self {
            Self::Arm {
                implementer: 0x41,
                part,
                ..
            } => match part {
                0xd03 => "Cortex-A53",
                0xd05 => "Cortex-A55",
                0xd07 => "Cortex-A57",
                0xd08 => "Cortex-A72",
                0xd0b => "Cortex-A76",
                0xd0c => "Neoverse-N1",
                0xd40 => "Neoverse-V1",
                0xd41 => "Cortex-A78",
                0xd44 => "Cortex-X1",
                0xd46 => "Cortex-A510",
                0xd47 => "Cortex-A710",
                0xd48 => "Cortex-X2",
                0xd49 => "Neoverse-N2",
                0xd4d => "Cortex-A715",
                0xd4e => "Cortex-X3",
                0xd4f => "Neoverse-V2",
                0xd80 => "Cortex-A520",
                0xd81 => "Cortex-A720",
                0xd82 => "Cortex-X4",
                _ => "Arm core",
            },
            Self::Arm { implementer: 0, .. } => "QEMU emulated core",
            Self::Arm { .. } => "Arm-architecture core",
            Self::X86 { vendor, .. } if &vendor == b"AuthenticAMD" => "AMD",
            Self::X86 { vendor, .. } if &vendor == b"GenuineIntel" => "Intel",
            Self::X86 { .. } => "x86-64",
        }
    }

    /// Relative capacity (1024 for the fastest class).
    pub(crate) fn capacity(&self) -> u16 {
        match *self {
            // In-order little cores.
            Self::Arm {
                implementer: 0x41,
                part: 0xd03 | 0xd05 | 0xd46 | 0xd80,
                ..
            } => 512,
            // Hybrid efficiency cores (CPUID 0x1A core type 0x20).
            Self::X86 {
                core_type: Some(0x20),
                ..
            } => 600,
            _ => 1024,
        }
    }
}

impl core::fmt::Display for Ident {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            Self::Arm {
                implementer,
                part,
                variant,
                revision,
            } => write!(
                f,
                "{} (implementer {implementer:#04x}, part {part:#05x}, r{variant}p{revision})",
                self.name()
            ),
            Self::X86 {
                family,
                model,
                stepping,
                core_type,
                ..
            } => {
                write!(
                    f,
                    "{} family {family:#x} model {model:#x} stepping {stepping}",
                    self.name()
                )?;
                match core_type {
                    Some(t) => write!(f, ", core type {t:#x}"),
                    None => Ok(()),
                }
            }
        }
    }
}

/// One CPU's place in the machine.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CpuInfo {
    pub(crate) ident: Ident,
    pub(crate) package: u32,
    pub(crate) cluster: u32,
    pub(crate) core: u32,
    pub(crate) smt_thread: bool,
    pub(crate) capacity: u16,
}

static CPUS: SpinLock<[Option<CpuInfo>; MAX_CPUS]> = SpinLock::new([None; MAX_CPUS]);

/// Identifies this CPU, records its place, and tells the scheduler its
/// capacity. Runs on each CPU as it comes up.
pub(crate) fn record_this_cpu(cpu: CpuId) {
    let ident = Ident::this_cpu();
    let (package, cluster, core, smt_thread) = place();
    let info = CpuInfo {
        ident,
        package,
        cluster,
        core,
        smt_thread,
        capacity: ident.capacity(),
    };
    CPUS.lock()[cpu.index()] = Some(info);
    sched::set_capacity(cpu, info.capacity);
    sched::set_smt(cpu, smt_core(smt_thread));
    crate::errata::apply(cpu, &ident);
    #[cfg(target_arch = "aarch64")]
    crate::idregs::record_this_cpu(cpu);
}

/// (package, cluster, core, is an SMT thread) for this CPU.
#[cfg(target_arch = "aarch64")]
fn place() -> (u32, u32, u32, bool) {
    use acpi_tables::madt::MadtEntry;
    let mpidr = crate::arch::cpu::mpidr();
    let aff = |n: u32| u32::try_from((mpidr >> (8 * n)) & 0xff).unwrap_or(0);
    let fallback = (aff(2), aff(1), aff(0), false);
    let uid = crate::acpi::find(*b"APIC")
        .and_then(|t| acpi_tables::madt::parse(&t).ok())
        .and_then(|m| {
            m.entries().filter_map(Result::ok).find_map(|e| match e {
                MadtEntry::GicCpuInterface(g) if g.mpidr == Some(mpidr) => Some(g.processor_uid),
                _ => None,
            })
        });
    let pptt = crate::acpi::find(*b"PPTT").and_then(|t| acpi_tables::pptt::parse(&t).ok());
    match (uid, pptt) {
        (Some(uid), Some(pptt)) => match pptt.topology(uid) {
            Some(t) => (
                t.package.map_or(0, |n| n.offset),
                t.cluster.map_or(0, |n| n.offset),
                t.core.offset,
                t.is_thread(),
            ),
            None => fallback,
        },
        _ => fallback,
    }
}

/// The extended topology leaf: 0x1F where it is enumerated (as Intel
/// recommends and FreeBSD prefers), else 0xB. Under a hypervisor the two
/// can disagree (bhyve 15.1 passes the host's 0x1F through); reading the
/// same one keeps scheduling decisions such as idle spinning in step.
#[cfg(target_arch = "x86_64")]
fn topology_leaf() -> Option<u32> {
    use core::arch::x86_64::{__cpuid, __cpuid_count};
    let max = __cpuid(0).eax;
    [0x1f, 0xb]
        .into_iter()
        .find(|&leaf| max >= leaf && __cpuid_count(leaf, 0).ebx & 0xffff != 0)
}

/// (package, cluster, core, is an SMT thread) for this CPU.
#[cfg(target_arch = "x86_64")]
fn place() -> (u32, u32, u32, bool) {
    use core::arch::x86_64::__cpuid_count;
    let Some(leaf) = topology_leaf() else {
        return (0, 0, crate::arch::apic::id(), false);
    };
    // Subleaf 0 is the SMT level, 1 the core level; EAX[4:0] is how far to
    // shift the x2APIC ID to get the next level up.
    let smt = __cpuid_count(leaf, 0);
    let core = __cpuid_count(leaf, 1);
    let id = smt.edx;
    let smt_shift = smt.eax & 0x1f;
    let core_shift = core.eax & 0x1f;
    let package = id.checked_shr(core_shift).unwrap_or(0);
    (
        package,
        package,
        id.checked_shr(smt_shift).unwrap_or(id),
        id & ((1u32 << smt_shift) - 1) != 0,
    )
}

/// Whether this CPU's core runs more than one thread. `smt_thread` (this
/// CPU is a core's second or later thread) is enough on aarch64, where the
/// PPTT marks every thread of such a core.
#[cfg(target_arch = "aarch64")]
fn smt_core(smt_thread: bool) -> bool {
    smt_thread
}

/// Whether this CPU's core runs more than one thread: the topology leaf's
/// SMT level counts the logical processors per core.
#[cfg(target_arch = "x86_64")]
fn smt_core(smt_thread: bool) -> bool {
    smt_thread
        || topology_leaf()
            .is_some_and(|leaf| core::arch::x86_64::__cpuid_count(leaf, 0).ebx & 0xffff > 1)
}

/// Prints a summary once every CPU is up.
pub(crate) fn summarize() {
    let cpus = CPUS.lock();
    let infos: Vec<CpuInfo> = cpus.iter().flatten().copied().collect();
    drop(cpus);
    let distinct = |f: &dyn Fn(&CpuInfo) -> u64| {
        let mut v: Vec<u64> = infos.iter().map(f).collect();
        v.sort_unstable();
        v.dedup();
        v.len()
    };
    let packages = distinct(&|c| u64::from(c.package));
    let clusters = distinct(&|c| (u64::from(c.package) << 32) | u64::from(c.cluster));
    let cores = distinct(&|c| (u64::from(c.package) << 32) | u64::from(c.core));
    let threads = infos.iter().filter(|c| c.smt_thread).count();
    kprintln!(
        "topology: {} CPUs: {packages} package(s), {clusters} cluster(s), {cores} core(s), {threads} SMT sibling(s)",
        infos.len()
    );
    let mut classes: Vec<(&str, u16, usize)> = Vec::new();
    for c in &infos {
        let name = c.ident.name();
        match classes
            .iter_mut()
            .find(|k| k.0 == name && k.1 == c.capacity)
        {
            Some(k) => k.2 += 1,
            None => classes.push((name, c.capacity, 1)),
        }
    }
    for (name, capacity, n) in classes {
        kprintln!("topology:   {n} x {name} (capacity {capacity})");
    }
}

/// Sets capacities from CPPC: each CPU's highest performance, scaled so
/// the fastest is 1024. `levels` holds (ACPI processor UID, highest
/// performance); CPUs without a level keep their estimate.
pub(crate) fn apply_cppc(levels: &[(u32, u64)]) {
    let Some(max) = levels.iter().map(|&(_, h)| h).max().filter(|&m| m > 0) else {
        return;
    };
    let Some(madt) = crate::acpi::find(*b"APIC").and_then(|t| acpi_tables::madt::parse(&t).ok())
    else {
        return;
    };
    let mut changed = 0;
    for i in 0..u32::try_from(sched::online_cpus()).unwrap_or(0) {
        let cpu = CpuId(i);
        let Some(hw) = crate::intc::hw_id(cpu) else {
            continue;
        };
        let uid = madt.cpus().find_map(|c| {
            let id = match c.hw_id {
                acpi_tables::madt::HwId::Apic(a) => u64::from(a),
                acpi_tables::madt::HwId::Mpidr(m) => m,
                acpi_tables::madt::HwId::GicCpuInterface(_) => return None,
            };
            (id == hw).then_some(c.uid)
        });
        let Some(&(_, highest)) = uid.and_then(|u| levels.iter().find(|(l, _)| *l == u)) else {
            continue;
        };
        let capacity =
            u16::try_from((highest.saturating_mul(1024) / max).clamp(1, 1024)).unwrap_or(1024);
        if let Some(info) = CPUS.lock()[cpu.index()].as_mut() {
            info.capacity = capacity;
        }
        sched::set_capacity(cpu, capacity);
        changed += 1;
    }
    kprintln!("topology: CPPC levels for {changed} CPU(s), highest {max}");
}
