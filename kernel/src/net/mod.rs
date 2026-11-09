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
pub(crate) mod memstat;
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
/// The thread is draining (`NWS_RUNNING`): what is queued meanwhile it
/// finds without being woken.
static NETISR_RUNNING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static NETISR_WQ: sched::WaitQueue = sched::WaitQueue::new();

/// `netisr_queue_workstream`'s signal: only when the thread is neither
/// scheduled (work already flagged) nor running.
fn netisr_kick() {
    use core::sync::atomic::Ordering::SeqCst;
    // SeqCst, paired with the thread's store of `NETISR_RUNNING` and its
    // swap of `NETISR_WORK`: either it sees the work, or this sees it
    // stopped and wakes it.
    if !NETISR_WORK.swap(true, SeqCst) && !NETISR_RUNNING.load(SeqCst) {
        NETISR_WQ.wake_one();
    }
}

/// The `ktls` threads' work, and where they wait.
static KTLS_WQ: sched::WaitQueue = sched::WaitQueue::new();
static KTLS_STARTED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

fn ktls_kick() {
    KTLS_WQ.wake_one();
}

/// `ktls_start_kthreads`: one `ktls` thread per CPU, the first time a
/// receiving session is set up. Each opens the records of a socket at a
/// time (a socket with work is on the queue once).
pub(crate) fn ktls_start() {
    if KTLS_STARTED.swap(true, core::sync::atomic::Ordering::Relaxed) {
        return;
    }
    inet::tls::set_kick(ktls_kick);
    for _ in 0..sched::online_cpus() {
        let r = sched::spawn("ktls", sched::Priority::KERNEL, |s| {
            loop {
                KTLS_WQ.wait_until(s, inet::tls::pending);
                while let Some(so) = inet::tls::next() {
                    so.ktls_decrypt();
                }
            }
        });
        if r.is_ok() {
            ktls::stats::THREADS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// The `netisr` thread (`swi1: netisr 0`): hands each VNET's queued
/// packets to their protocols, a queue's worth at a time per VNET, until
/// none is left (packets queued behind others did not kick).
fn start_netisr() {
    net::netisr::set_kick(netisr_kick);
    let _ = sched::spawn("netisr", sched::Priority::KERNEL, |s| {
        loop {
            use core::sync::atomic::Ordering::SeqCst;
            // SeqCst: see `netisr_kick`.
            NETISR_WQ.wait_until(s, || NETISR_WORK.swap(false, SeqCst));
            NETISR_RUNNING.store(true, SeqCst);
            loop {
                // Taken first, so a kick while draining is a pass more.
                NETISR_WORK.store(false, SeqCst);
                let mut more = false;
                for v in net::Vnet::all() {
                    net::netisr::drain(&v);
                    more |= !v.netisr.is_empty();
                }
                if !more {
                    break;
                }
            }
            NETISR_RUNNING.store(false, SeqCst);
        }
    });
}

/// The time of day in nanoseconds since the epoch.
fn realtime_ns() -> u64 {
    crate::time::realtime_ns().unwrap_or(0)
}

/// `log(LOG_NOTICE, ...)` for the protocols.
fn log_notice(msg: &str) {
    crate::klog::log(abi::syslog::LOG_NOTICE, format_args!("{msg}"));
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
            // The network epoch's callback task: views and the like
            // retired a grace period ago go now.
            net::view::reclaim(&mut || crate::time::sleep_ns(s, 1_000_000));
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
                if n.garp.iter().any(|g| g.at <= up) {
                    net::arp::garp_tick(&mut n, up);
                }
                drop(n);
                // `ipreass_callout`.
                if v.inet.get().is_some() {
                    inet::reass::tick(inet::state::of(&v), up);
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
    net::set_log(|pri, msg| crate::klog::log(pri, format_args!("{msg}")));
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
        realtime: realtime_ns,
        log: log_notice,
        hostname,
        timer_kick: ip6_timer_kick,
    });
    inet::init();
    inet::reass::set_nmbclusters(mbuf::zone::CLUSTER.limit());
    inet::frag6::set_nmbclusters(mbuf::zone::CLUSTER.limit());
    let now = i64::try_from(crate::time::realtime_ns().unwrap_or(0) / 1_000_000_000).unwrap_or(0);
    inet::ifnet::configure_loopback(&mut net::vnet0().net.lock(), now);
    splice::init();
    start_netisr();
    start_route_expiry();
    start_ip6_timer();
    sysctl::init(s);
}

/// The current jail's host name, for IPv6's node information group.
fn hostname() -> alloc::string::String {
    let h = crate::prison::current().settings().hostname;
    alloc::string::String::from_utf8_lossy(&h).into_owned()
}

/// The IPv6 timer thread's work flag and where it waits.
static IP6_KICKED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static IP6_WQ: sched::WaitQueue = sched::WaitQueue::new();

fn ip6_timer_kick() {
    IP6_KICKED.store(true, core::sync::atomic::Ordering::Release);
    IP6_WQ.wake_one();
}

/// IPv6's callouts (`nd6_llinfo_timer`, `nd6_dad_timer`,
/// `mld_fasttimo`, `nd6_timer`, `frag6_slowtimo`): one thread runs
/// whatever is due and sleeps until the next deadline, or until a sooner
/// one is armed.
fn start_ip6_timer() {
    let _ = sched::spawn("ip6timer", sched::Priority::KERNEL, |s| {
        loop {
            let next = inet::next_deadline();
            let now = crate::time::now_ns();
            if next > now {
                let wait = (next - now).min(60 * 1_000_000_000);
                crate::time::wait_timeout(
                    s,
                    &IP6_WQ,
                    || IP6_KICKED.swap(false, core::sync::atomic::Ordering::Acquire),
                    wait,
                );
            }
            inet::run_timers(crate::time::now_ns());
        }
    });
}
