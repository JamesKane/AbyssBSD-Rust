// SPDX-License-Identifier: BSD-2-Clause
//! The loopback network (docs/MULTIUSER.md §8.4): `lo0` attached at boot
//! with the addresses `rc.d/netif` would give it, the [`inet`] crate's
//! protocols wired to the kernel's clock and random source, the interface
//! ioctls, netlink sockets, and the `net` sysctls.
//!
//! Behavior reference: sys/net/if_loop.c (`vnet_loif_init`),
//! sys/netinet6/in6_ifattach.c.

pub(crate) mod bpf;
pub(crate) mod ioctl;
#[cfg(feature = "ktest")]
mod ktests;
pub(crate) mod netlink;
pub(crate) mod splice;
mod sysctl;

use sched::Sleepable;

// SAFETY: `if_data` is integers with no padding.
unsafe impl crate::uaccess::Plain for abi::net::IfData {}
// SAFETY: `nd_ifinfo` is integers and byte arrays, its tail padding an
// explicit field.
unsafe impl crate::uaccess::Plain for abi::net::NdIfinfo {}
// SAFETY: `in6_addrpolicy` is two `sockaddr_in6`s (integers and byte
// arrays) and integers, with no padding between or after them.
unsafe impl crate::uaccess::Plain for abi::net::In6Addrpolicy {}

fn random_u32() -> u32 {
    let mut b = [0u8; 4];
    // Unseeded early on: a port still comes, from the clock.
    if crate::random::fill(&mut b).is_err() {
        #[allow(clippy::cast_possible_truncation, reason = "the low bits")]
        return crate::time::now_ns() as u32;
    }
    u32::from_ne_bytes(b)
}

/// The current thread's VNET (`curvnet`).
pub(crate) fn curvnet() -> alloc::sync::Arc<net::Vnet> {
    alloc::sync::Arc::clone(crate::prison::current().vnet())
}

/// The time in seconds, for interfaces' attach times.
pub(crate) fn now_s() -> i64 {
    i64::try_from(crate::time::realtime_ns().unwrap_or(0) / 1_000_000_000).unwrap_or(0)
}

/// Work for the `netisr` thread, and where it waits.
static NETISR_WORK: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static NETISR_WQ: sched::WaitQueue = sched::WaitQueue::new();

fn netisr_kick() {
    NETISR_WORK.store(true, core::sync::atomic::Ordering::Release);
    NETISR_WQ.wake_one();
}

/// The `netisr` thread (`swi1: netisr 0`): hands each VNET's queued
/// packets to their protocols.
fn start_netisr() {
    net::netisr::set_kick(netisr_kick);
    let _ = sched::spawn("netisr", sched::Priority::KERNEL, |s| {
        loop {
            NETISR_WQ.wait_until(s, || {
                NETISR_WORK.swap(false, core::sync::atomic::Ordering::Acquire)
            });
            for v in net::Vnet::all() {
                net::netisr::drain(&v);
            }
        }
    });
}

/// Seconds since boot (`time_uptime`).
pub(crate) fn uptime_s() -> u64 {
    crate::time::now_ns() / 1_000_000_000
}

/// Deletes routes as they expire (`rt_expire`'s callout), and ages the
/// link-layer table's entries (their `lle_timer`s), once a second.
fn start_route_expiry() {
    let _ = sched::spawn("rtexpire", sched::Priority::KERNEL, |s| {
        loop {
            crate::time::sleep_ns(s, 1_000_000_000);
            let now = u64::try_from(now_s()).unwrap_or(0);
            let up = uptime_s();
            for v in net::Vnet::all() {
                let mut n = v.net.lock();
                if n.routes.iter().any(|r| r.expire != 0) {
                    n.expire(now);
                }
                if n.lle.iter().any(|e| e.timer.is_some_and(|t| t <= up)) {
                    net::arp::tick(&mut n, up);
                }
            }
        }
    });
}

/// `devd`'s interface events, from the system's VNET (`if_attach`'s,
/// `ether_ifattach`'s, `if_detach`'s, and `do_link_state_change`'s
/// `devctl_notify`).
struct Devd;

impl net::announce::Listener for Devd {
    fn event(&self, e: &net::announce::Event) {
        use net::announce::Event;
        let m = bus::manager();
        match e {
            Event::Announce { ifp, arrival } => {
                let kind = if *arrival { "ATTACH" } else { "DETACH" };
                m.notify("IFNET", &ifp.name, kind, None);
                if *arrival && ifp.ty == abi::net::IFT_ETHER {
                    m.notify("ETHERNET", &ifp.name, "IFATTACH", None);
                }
            }
            Event::Link { ifp, up } => {
                let kind = if *up { "LINK_UP" } else { "LINK_DOWN" };
                m.notify("IFNET", &ifp.name, kind, None);
            }
            _ => {}
        }
    }
}

/// The listener, kept for the system's life.
static DEVD: sync::Once<alloc::sync::Arc<dyn net::announce::Listener>> = sync::Once::new();

/// What a clone needs from the creating thread (`ether_gen_addr_byname`'s
/// host UUID and jail name).
pub(crate) fn clone_ctx() -> net::clone::Ctx {
    let pr = crate::prison::current().settings();
    let mut random = [0u8; 12];
    let _ = crate::random::fill(&mut random);
    net::clone::Ctx {
        now: now_s(),
        hostuuid: pr.hostuuid,
        jail: pr.name,
        random,
    }
}

/// Configures `lo0` and adds the sysctls. Call once, after the sysctl tree
/// and `kern.features` exist.
pub(crate) fn init(s: &Sleepable) {
    // `net.fibs` (`CTLFLAG_RDTUN`).
    if let Some(n) = crate::kenv::get(b"net.fibs").and_then(|v| {
        core::str::from_utf8(&v)
            .ok()
            .and_then(|t| t.trim().parse::<u32>().ok())
    }) {
        net::set_numfibs(n);
    }
    net::set_uptime(uptime_s);
    net::set_realtime(now_s);
    net::init();
    let _ = DEVD.set(alloc::sync::Arc::new(Devd));
    if let Some(l) = DEVD.get() {
        net::vnet0().net.listen(l);
    }
    bpf::init();
    inet::set_hooks(inet::Hooks {
        now: crate::time::now_ns,
        random: random_u32,
    });
    let now = i64::try_from(crate::time::realtime_ns().unwrap_or(0) / 1_000_000_000).unwrap_or(0);
    inet::ifnet::configure_loopback(&mut net::vnet0().net.lock(), now);
    splice::init();
    start_netisr();
    start_route_expiry();
    sysctl::init(s);
}
